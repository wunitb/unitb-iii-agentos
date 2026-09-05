use agentos_http_adapter::{CHAT_TIMEOUT_MS, TriggerBus};
use data_encoding::{BASE64, BASE64URL_NOPAD};
use hmac::{Hmac, Mac};
use iii_sdk::channels::{ChannelReader, StreamChannelRef};
use iii_sdk::errors::Error;
use iii_sdk::protocol::TriggerAction;
use iii_sdk::{RegisterFunction, protocol::TriggerRequest, register_worker};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

type HmacSha256 = Hmac<Sha256>;

const AUTH_URL: &str = "https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token";
const MAX_MESSAGE_LEN: usize = 4096;
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Two inbound authentication schemes, chosen by the `Authorization` scheme
/// Teams itself uses, each armed by its own configuration:
///
/// * `Bearer <jwt>` — an Azure Bot (Bot Framework). The Bot Connector service
///   signs a JWT for every activity; it is checked against Microsoft's
///   published keys, `iss`, `aud` (= `TEAMS_APP_ID`), validity window and the
///   `serviceurl` claim (spelled that way in the token; see [`SERVICE_URL_CLAIM`])
///   (https://learn.microsoft.com/en-us/azure/bot-service/rest-api/bot-framework-rest-connector-authentication#authenticate-requests-from-the-bot-connector-service-to-your-bot).
///   Replies go out through the activity's `serviceUrl`, as before.
/// * `HMAC <base64>` — an Outgoing Webhook. Teams sends the HMAC-SHA256 of the
///   raw body keyed by the base64 security token shown when the webhook was
///   created
///   (https://learn.microsoft.com/en-us/microsoftteams/platform/webhooks-and-connectors/how-to/add-outgoing-webhook#use-the-security-token).
///   An Outgoing Webhook has no `serviceUrl` credential; the reply is the
///   synchronous HTTP response, which Teams waits five seconds for.
const APP_ID_KEY: &str = "TEAMS_APP_ID";
const WEBHOOK_SECRET_KEY: &str = "TEAMS_WEBHOOK_SECRET";

/// Bot Connector OpenID metadata. "This is a static URL that you can hardcode
/// into your application." — the Bot Framework authentication guide.
const OPENID_CONFIGURATION_URL: &str =
    "https://login.botframework.com/v1/.well-known/openidconfiguration";
const BOT_CONNECTOR_ISSUER: &str = "https://api.botframework.com";
/// "Industry-standard clock-skew is 5 minutes."
const JWT_CLOCK_SKEW_SECS: i64 = 300;
/// "All bot instances should refresh their local cache of the document at
/// least once every 24 hours."
const KEY_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// An unknown `kid` triggers a refresh, but no more often than this, so a
/// flood of forged tokens cannot turn us into a request generator.
const KEY_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Allowed Bot Framework `serviceUrl` hosts. Outbound replies are only sent
/// when the inbound activity's `serviceUrl` resolves to one of these. Operators
/// can extend the list via `TEAMS_ALLOWED_SERVICE_URLS` (comma-separated).
const DEFAULT_ALLOWED_SERVICE_URL_SUFFIXES: &[&str] = &[
    ".botframework.com",
    ".botframework.azure.us",
    ".botframework.cn",
];
/// The host the Teams channel actually presents as `serviceUrl`
/// (`https://smba.trafficmanager.net/<region>/`). With JWT verification the
/// value is additionally bound to the token's `serviceurl` claim, so this list
/// is a second fence, not the only one.
const DEFAULT_ALLOWED_SERVICE_URL_HOSTS: &[&str] = &["smba.trafficmanager.net"];

/// Upper bound on a provider delivery we are willing to read before verifying it.
const MAX_RAW_BODY_BYTES: usize = 4 * 1024 * 1024;
/// The engine streams the body from local memory; anything slower is a fault.
const RAW_BODY_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Engine WebSocket base: the same address `main` connects to.
fn engine_ws_url() -> String {
    std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string())
}

/// Where the provider's original bytes come from, decided before any are read.
#[derive(Debug)]
enum RawBodySource {
    /// The engine's `request_body` stream channel: the HTTP path.
    Channel(StreamChannelRef),
    /// A `rawBody` string handed over by a bus caller or a test: no HTTP
    /// request was involved, so there is no channel to prefer.
    Inline(Vec<u8>),
}

/// Pick the source of the raw body. The engine's channel ref always outranks
/// an inline `rawBody`: the adapter no longer flattens `rawBody` out of the
/// request body, but a handler must not depend on that — a channel ref that is
/// present and unusable is a refusal, never a fall-through to caller-chosen
/// bytes.
fn raw_body_source(req: &Value) -> Result<RawBodySource, String> {
    if let Some(channel) = req.get("request_body") {
        let channel: StreamChannelRef = serde_json::from_value(channel.clone())
            .map_err(|e| format!("request_body channel ref is malformed: {e}"))?;
        return Ok(RawBodySource::Channel(channel));
    }
    if let Some(raw) = req.get("rawBody").and_then(Value::as_str) {
        return Ok(RawBodySource::Inline(raw.as_bytes().to_vec()));
    }
    Err("raw request body unavailable (no request_body channel, no rawBody)".into())
}

/// The request body exactly as the provider sent it.
///
/// iii 0.22.1 hands HTTP handlers a `body` that is already parsed and
/// re-serialised (verified: `{ "b" : 2 , "a" : 1 }` arrives as `{"a":1,"b":2}`),
/// so no signature can be checked against it. The original bytes are exposed as
/// the `request_body` stream channel (verified live on 0.22.1, with and without
/// bus RBAC armed: the channel is keyed by its own access key, not the bus
/// credential), which is read whenever the engine provides it. A `rawBody`
/// string is accepted only when there is no channel at all, so a bus caller or
/// a test can hand the bytes over directly. Absent both, the caller refuses the
/// request: nothing here guesses.
async fn raw_request_body(req: &Value) -> Result<Vec<u8>, String> {
    let channel = match raw_body_source(req)? {
        RawBodySource::Inline(bytes) => return Ok(bytes),
        RawBodySource::Channel(channel) => channel,
    };
    let reader = ChannelReader::new(&engine_ws_url(), &channel);
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::time::timeout(RAW_BODY_READ_TIMEOUT, reader.next_binary())
            .await
            .map_err(|_| "timed out reading the request_body channel".to_string())?
            .map_err(|e| format!("request_body channel read failed: {e}"))?;
        let Some(chunk) = chunk else {
            return Ok(bytes);
        };
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_RAW_BODY_BYTES {
            return Err(format!("request body exceeds {MAX_RAW_BODY_BYTES} bytes"));
        }
    }
}

/// One header by case-insensitive name. The engine lowercases header names;
/// matching case-insensitively costs nothing and survives an engine change.
fn header<'a>(req: &'a Value, name: &str) -> Option<&'a str> {
    req.get("headers")?
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value.as_str())
}

fn reject(status: u16, error: &str) -> Value {
    json!({ "status_code": status, "body": { "error": error } })
}

/// One RSA signing key from the Bot Connector JWKS document.
#[derive(Debug, Clone, Deserialize, PartialEq)]
struct Jwk {
    #[serde(default)]
    kty: String,
    #[serde(default)]
    kid: String,
    #[serde(default)]
    n: String,
    #[serde(default)]
    e: String,
}

struct KeyCache {
    keys: Vec<Jwk>,
    fetched_at: Instant,
}

/// Process-wide cache of the Bot Connector signing keys.
type SharedKeys = Arc<RwLock<Option<KeyCache>>>;

fn is_allowed_service_url(url: &str, extra: &[String]) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if DEFAULT_ALLOWED_SERVICE_URL_HOSTS.contains(&host)
        || DEFAULT_ALLOWED_SERVICE_URL_SUFFIXES
            .iter()
            .any(|suffix| host.ends_with(suffix))
    {
        return true;
    }
    extra.iter().any(|allowed| {
        let allowed = allowed.trim();
        if allowed.is_empty() {
            return false;
        }
        if let Ok(allowed_url) = reqwest::Url::parse(allowed) {
            allowed_url.host_str() == Some(host)
        } else {
            host == allowed || host.ends_with(&format!(".{}", allowed.trim_start_matches('.')))
        }
    })
}

async fn get_secret(iii: &dyn TriggerBus, key: &str) -> String {
    let result = iii
        .trigger(TriggerRequest {
            function_id: "vault::get".to_string(),
            payload: json!({ "key": key }),
            action: None,
            timeout_ms: None,
        })
        .await;
    if let Ok(value) = result
        && let Some(v) = value.get("value").and_then(|v| v.as_str())
        && !v.is_empty()
    {
        return v.to_string();
    }
    std::env::var(key).unwrap_or_default()
}

/// The inbound verification secret at boot, or `None` when neither the vault
/// nor the environment has one. `main` registers the webhook route only on
/// `Some`: a route that cannot verify its caller is refused, not exposed.
/// (`vault::get` fails instantly with `function_not_found` while the vault
/// worker is still starting, so this falls through to the environment — the
/// path `agentos up` and `dev-up.sh` populate — without waiting.)
async fn startup_secret(iii: &dyn TriggerBus, key: &str) -> Option<String> {
    let value = get_secret(iii, key).await;
    (!value.is_empty()).then_some(value)
}

async fn resolve_agent(iii: &dyn TriggerBus, channel: &str, channel_id: &str) -> String {
    let key = format!("{channel}:{channel_id}");
    let result = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({ "scope": "channel_agents", "key": key }),
            action: None,
            timeout_ms: None,
        })
        .await;
    if let Ok(value) = result
        && let Some(agent) = value.get("agentId").and_then(|v| v.as_str())
    {
        return agent.to_string();
    }
    "default".to_string()
}

fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.chars().count() <= max_len {
        return vec![text.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut remaining = text.to_string();
    while !remaining.is_empty() {
        if remaining.chars().count() <= max_len {
            chunks.push(remaining);
            break;
        }
        let cutoff = remaining
            .char_indices()
            .nth(max_len)
            .map(|(idx, _)| idx)
            .unwrap_or(remaining.len());
        let window = &remaining[..cutoff];
        let split_at = match window.rfind('\n') {
            Some(idx) if window[..idx].chars().count() > max_len / 2 => idx,
            _ => cutoff,
        };
        chunks.push(remaining[..split_at].to_string());
        remaining = if split_at < cutoff && remaining.as_bytes().get(split_at) == Some(&b'\n') {
            remaining[split_at + 1..].to_string()
        } else {
            remaining[split_at..].to_string()
        };
    }
    chunks
}

/// Verify an Outgoing Webhook delivery: `Authorization: HMAC <base64>` carries
/// HMAC-SHA256(key = base64-decoded security token, message = raw body),
/// base64-encoded. The comparison is `Mac::verify_slice` (constant-time).
fn verify_outgoing_webhook_hmac(
    secret_base64: &str,
    raw_body: &[u8],
    provided_base64: &str,
) -> Result<(), String> {
    if secret_base64.is_empty() {
        return Err("TEAMS_WEBHOOK_SECRET not configured".into());
    }
    let key = BASE64
        .decode(secret_base64.trim().as_bytes())
        .map_err(|_| "TEAMS_WEBHOOK_SECRET is not base64".to_string())?;
    let provided = BASE64
        .decode(provided_base64.trim().as_bytes())
        .map_err(|_| "Authorization HMAC value is not base64".to_string())?;
    let mut mac = HmacSha256::new_from_slice(&key).map_err(|e| format!("HMAC init error: {e}"))?;
    mac.update(raw_body);
    mac.verify_slice(&provided)
        .map_err(|_| "Invalid Teams HMAC".to_string())
}

fn base64url(segment: &str, what: &str) -> Result<Vec<u8>, String> {
    BASE64URL_NOPAD
        .decode(segment.trim_end_matches('=').as_bytes())
        .map_err(|_| format!("{what} is not base64url"))
}

/// Verify a Bot Connector JWT and return its claims.
///
/// Requirements 1–5 of the Bot Framework guide: `Bearer` scheme (checked by
/// the caller), well-formed JWT, `iss`, `aud` equal to the app id, validity
/// window with five minutes of skew, and an RS256 signature by a key in the
/// OpenID keys document. Requirement 6 (the `serviceurl` claim) needs the
/// activity and is checked by [`verify_service_url_claim`].
fn verify_bot_framework_jwt(
    token: &str,
    app_id: &str,
    keys: &[Jwk],
    now: i64,
) -> Result<Value, String> {
    if app_id.is_empty() {
        return Err("TEAMS_APP_ID not configured".into());
    }
    let mut parts = token.split('.');
    let (Some(header_b64), Some(payload_b64), Some(signature_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("token is not a three-part JWT".into());
    };
    let header: Value = serde_json::from_slice(&base64url(header_b64, "JWT header")?)
        .map_err(|_| "JWT header is not JSON".to_string())?;
    if header.get("alg").and_then(Value::as_str) != Some("RS256") {
        return Err("JWT alg is not RS256".into());
    }
    let kid = header
        .get("kid")
        .and_then(Value::as_str)
        .ok_or_else(|| "JWT has no kid".to_string())?;
    let key = keys
        .iter()
        .find(|key| key.kid == kid && key.kty == "RSA")
        .ok_or_else(|| format!("no RSA signing key with kid {kid}"))?;
    let n = base64url(&key.n, "JWK n")?;
    let e = base64url(&key.e, "JWK e")?;
    let signature = base64url(signature_b64, "JWT signature")?;
    let signed = format!("{header_b64}.{payload_b64}");
    RsaPublicKeyComponents { n: &n, e: &e }
        .verify(&RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &signature)
        .map_err(|_| "JWT signature is invalid".to_string())?;

    // Claims are read only after the signature has been verified.
    let claims: Value = serde_json::from_slice(&base64url(payload_b64, "JWT payload")?)
        .map_err(|_| "JWT payload is not JSON".to_string())?;
    if claims.get("iss").and_then(Value::as_str) != Some(BOT_CONNECTOR_ISSUER) {
        return Err("JWT issuer is not the Bot Connector service".into());
    }
    let audience_matches = match claims.get("aud") {
        Some(Value::String(aud)) => aud == app_id,
        Some(Value::Array(auds)) => auds.iter().any(|aud| aud.as_str() == Some(app_id)),
        _ => false,
    };
    if !audience_matches {
        return Err("JWT audience is not this bot's app id".into());
    }
    let exp = claims
        .get("exp")
        .and_then(Value::as_i64)
        .ok_or_else(|| "JWT has no exp".to_string())?;
    if now > exp + JWT_CLOCK_SKEW_SECS {
        return Err("JWT has expired".into());
    }
    if let Some(nbf) = claims.get("nbf").and_then(Value::as_i64)
        && now + JWT_CLOCK_SKEW_SECS < nbf
    {
        return Err("JWT is not yet valid".into());
    }
    Ok(claims)
}

/// The name the Bot Connector service actually gives requirement 6's claim.
///
/// Microsoft's prose writes `serviceUrl`, but the tokens carry `serviceurl`:
/// botbuilder-js `libraries/botframework-connector/src/auth/authenticationConstants.ts`
/// (`export const ServiceUrlClaim = 'serviceurl';`) and botbuilder-dotnet
/// `libraries/Microsoft.Bot.Connector/Authentication/AuthenticationConstants.cs`
/// (`public const string ServiceUrlClaim = "serviceurl";`). The lookup below is
/// case-insensitive so either spelling verifies; a token that carries both
/// must agree with the activity on both.
const SERVICE_URL_CLAIM: &str = "serviceurl";

/// Requirement 6: the token's `serviceurl` claim must equal the activity's
/// `serviceUrl`, so a token minted for one channel cannot be replayed to
/// redirect replies elsewhere.
fn verify_service_url_claim(claims: &Value, activity: &Value) -> Result<(), String> {
    let claimed: Vec<&str> = claims
        .as_object()
        .into_iter()
        .flat_map(|claims| claims.iter())
        .filter(|(name, _)| name.eq_ignore_ascii_case(SERVICE_URL_CLAIM))
        .map(|(_, value)| value.as_str().unwrap_or(""))
        .collect();
    if claimed.is_empty() {
        return Err(format!("JWT has no {SERVICE_URL_CLAIM} claim"));
    }
    let actual = activity
        .get("serviceUrl")
        .and_then(Value::as_str)
        .unwrap_or("");
    if actual.is_empty()
        || claimed
            .iter()
            .any(|claimed| claimed.trim_end_matches('/') != actual.trim_end_matches('/'))
    {
        return Err(format!(
            "JWT {SERVICE_URL_CLAIM} claim does not match the activity"
        ));
    }
    Ok(())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn fetch_signing_keys(client: &reqwest::Client) -> Result<Vec<Jwk>, String> {
    let metadata: Value = client
        .get(OPENID_CONFIGURATION_URL)
        .send()
        .await
        .map_err(|e| format!("OpenID metadata request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("OpenID metadata request failed: {e}"))?
        .json()
        .await
        .map_err(|e| format!("OpenID metadata is not JSON: {e}"))?;
    let jwks_uri = metadata
        .get("jwks_uri")
        .and_then(Value::as_str)
        .ok_or_else(|| "OpenID metadata has no jwks_uri".to_string())?;
    if !jwks_uri.starts_with("https://") {
        return Err("jwks_uri is not https".into());
    }
    let document: Value = client
        .get(jwks_uri)
        .send()
        .await
        .map_err(|e| format!("JWKS request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("JWKS request failed: {e}"))?
        .json()
        .await
        .map_err(|e| format!("JWKS is not JSON: {e}"))?;
    let keys: Vec<Jwk> = serde_json::from_value(document.get("keys").cloned().unwrap_or_default())
        .map_err(|e| format!("JWKS keys are malformed: {e}"))?;
    if keys.is_empty() {
        return Err("JWKS has no keys".into());
    }
    Ok(keys)
}

/// The cached Bot Connector keys, refreshed when stale or when `kid` is not
/// in the cache (rate-limited).
async fn signing_keys(
    client: &reqwest::Client,
    cache: &SharedKeys,
    kid: Option<&str>,
) -> Result<Vec<Jwk>, String> {
    {
        let guard = cache.read().await;
        if let Some(cached) = guard.as_ref() {
            let fresh = cached.fetched_at.elapsed() < KEY_CACHE_TTL;
            let has_kid = kid.is_none_or(|kid| cached.keys.iter().any(|key| key.kid == kid));
            let recently_fetched = cached.fetched_at.elapsed() < KEY_REFRESH_MIN_INTERVAL;
            if fresh && (has_kid || recently_fetched) {
                return Ok(cached.keys.clone());
            }
        }
    }
    let keys = fetch_signing_keys(client).await?;
    *cache.write().await = Some(KeyCache {
        keys: keys.clone(),
        fetched_at: Instant::now(),
    });
    Ok(keys)
}

/// `kid` from an unverified JWT header — only used to decide whether the key
/// cache needs a refresh before the real verification runs.
fn peek_kid(token: &str) -> Option<String> {
    let header_b64 = token.split('.').next()?;
    let header: Value = serde_json::from_slice(&base64url(header_b64, "JWT header").ok()?).ok()?;
    header.get("kid").and_then(Value::as_str).map(String::from)
}

/// Exchange app credentials for a Bot Framework access token.
async fn get_token(
    client: &reqwest::Client,
    app_id: &str,
    app_password: &str,
) -> Result<String, Error> {
    if app_id.is_empty() || app_password.is_empty() {
        return Err(Error::Handler("Missing Teams credentials".into()));
    }
    let params = [
        ("grant_type", "client_credentials"),
        ("client_id", app_id),
        ("client_secret", app_password),
        ("scope", "https://api.botframework.com/.default"),
    ];
    let resp = client
        .post(AUTH_URL)
        .form(&params)
        .send()
        .await
        .map_err(|e| Error::Handler(format!("Teams token request failed: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::Handler(format!("Token request failed: {status}")));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| Error::Handler(format!("Token decode: {e}")))?;
    body.get("access_token")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| Error::Handler("Token response missing access_token".into()))
}

async fn send_message(
    client: &reqwest::Client,
    token: &str,
    service_url: &str,
    conversation_id: &str,
    reply_to_id: &str,
    text: &str,
) -> Result<(), Error> {
    let url = format!(
        "{}/v3/conversations/{}/activities",
        service_url.trim_end_matches('/'),
        conversation_id
    );
    for chunk in split_message(text, MAX_MESSAGE_LEN) {
        let resp = client
            .post(&url)
            .bearer_auth(token)
            .json(&json!({
                "type": "message",
                "text": chunk,
                "replyToId": reply_to_id,
            }))
            .send()
            .await
            .map_err(|e| Error::Handler(format!("Teams send error: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Handler(format!(
                "Teams send failed ({status}): {}",
                body.chars().take(300).collect::<String>()
            )));
        }
    }
    Ok(())
}

/// How an authenticated delivery arrived, which decides how the reply leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inbound {
    /// Bot Connector JWT: reply through the activity's `serviceUrl`.
    BotFramework,
    /// Outgoing Webhook HMAC: reply in the HTTP response.
    OutgoingWebhook,
}

/// Authenticate one delivery and return the activity it carried.
///
/// The scheme in `Authorization` selects the check; each check needs its own
/// configuration and refuses (503) without it. For the HMAC scheme the body is
/// parsed from the verified raw bytes only. For the JWT scheme nothing signs
/// the body — the token authenticates the caller and TLS carries the payload —
/// so the parsed body is used, and the token's `serviceurl` claim is then bound
/// to it. Every failure is a refusal (`Err(response)`).
async fn authenticate(
    iii: &dyn TriggerBus,
    client: &reqwest::Client,
    keys: &SharedKeys,
    req: &Value,
) -> Result<(Inbound, Value), Value> {
    let Some((scheme, credential)) = header(req, "authorization")
        .and_then(|value| value.trim().split_once(' '))
        .map(|(scheme, credential)| (scheme, credential.trim()))
        .filter(|(_, credential)| !credential.is_empty())
    else {
        return Err(reject(401, "Missing Authorization header"));
    };

    if scheme.eq_ignore_ascii_case("hmac") {
        let secret = get_secret(iii, WEBHOOK_SECRET_KEY).await;
        if secret.is_empty() {
            return Err(reject(503, "TEAMS_WEBHOOK_SECRET not configured"));
        }
        let raw = match raw_request_body(req).await {
            Ok(raw) => raw,
            Err(e) => {
                tracing::warn!(error = %e, "teams: refusing delivery without its raw body");
                return Err(reject(
                    400,
                    "Raw request body unavailable for signature verification",
                ));
            }
        };
        if let Err(e) = verify_outgoing_webhook_hmac(&secret, &raw, credential) {
            tracing::warn!(error = %e, "teams outgoing-webhook HMAC rejected");
            return Err(reject(401, "Invalid Teams HMAC"));
        }
        let activity =
            serde_json::from_slice(&raw).map_err(|_| reject(400, "Body is not valid JSON"))?;
        return Ok((Inbound::OutgoingWebhook, activity));
    }

    if scheme.eq_ignore_ascii_case("bearer") {
        let app_id = get_secret(iii, APP_ID_KEY).await;
        if app_id.is_empty() {
            return Err(reject(503, "TEAMS_APP_ID not configured"));
        }
        let keys = match signing_keys(client, keys, peek_kid(credential).as_deref()).await {
            Ok(keys) => keys,
            Err(e) => {
                tracing::error!(error = %e, "teams: Bot Connector signing keys unavailable");
                return Err(reject(503, "Bot Connector signing keys unavailable"));
            }
        };
        let claims = match verify_bot_framework_jwt(credential, &app_id, &keys, unix_now()) {
            Ok(claims) => claims,
            Err(e) => {
                tracing::warn!(error = %e, "teams Bot Connector token rejected");
                return Err(reject(401, "Invalid Bot Connector token"));
            }
        };
        let activity = req.get("body").cloned().unwrap_or(Value::Null);
        if let Err(e) = verify_service_url_claim(&claims, &activity) {
            tracing::warn!(error = %e, "teams Bot Connector token rejected");
            return Err(reject(401, "Invalid Bot Connector token"));
        }
        return Ok((Inbound::BotFramework, activity));
    }

    Err(reject(401, "Unsupported Authorization scheme"))
}

async fn handle_webhook(
    iii: &dyn TriggerBus,
    client: &reqwest::Client,
    keys: &SharedKeys,
    req: Value,
) -> Result<Value, Error> {
    let (inbound, activity) = match authenticate(iii, client, keys, &req).await {
        Ok(verified) => verified,
        Err(response) => return Ok(response),
    };

    if activity.get("type").and_then(|v| v.as_str()) != Some("message") {
        return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
    }

    let conversation_id = activity
        .get("conversation")
        .and_then(|c| c.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let text = activity
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let user_id = activity
        .get("from")
        .and_then(|f| f.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let service_url = activity
        .get("serviceUrl")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let activity_id = activity
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let agent_id = resolve_agent(iii, "teams", &conversation_id).await;

    let chat = iii
        .trigger(TriggerRequest {
            function_id: "agent::chat".to_string(),
            payload: json!({
                "agentId": agent_id,
                "message": text,
                "sessionId": format!("teams:{conversation_id}"),
            }),
            action: None,
            timeout_ms: Some(CHAT_TIMEOUT_MS),
        })
        .await
        .map_err(|e| Error::Handler(format!("agent::chat failed: {e}")))?;

    let reply = chat.get("content").and_then(|v| v.as_str()).unwrap_or("");

    let _ = iii
        .trigger(TriggerRequest {
            function_id: "security::audit".to_string(),
            payload: json!({
                "type": "channel_message",
                "agentId": agent_id,
                "detail": {
                    "channel": "teams",
                    "conversationId": conversation_id,
                    "userId": user_id
                },
            }),
            action: Some(TriggerAction::Void),
            timeout_ms: None,
        })
        .await;

    if inbound == Inbound::OutgoingWebhook {
        // An Outgoing Webhook reads its reply from this response and nothing
        // else; there is no serviceUrl credential to post with.
        if reply.is_empty() {
            return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
        }
        return Ok(json!({
            "status_code": 200,
            "body": { "type": "message", "text": reply },
        }));
    }

    if !reply.is_empty() {
        let allowed_extra: Vec<String> = get_secret(iii, "TEAMS_ALLOWED_SERVICE_URLS")
            .await
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !is_allowed_service_url(&service_url, &allowed_extra) {
            tracing::warn!(
                service_url = %service_url,
                "rejecting Teams activity with untrusted serviceUrl"
            );
            return Ok(json!({
                "status_code": 401,
                "body": { "error": "Untrusted serviceUrl" }
            }));
        }
        let app_id = get_secret(iii, APP_ID_KEY).await;
        let app_password = get_secret(iii, "TEAMS_APP_PASSWORD").await;
        match get_token(client, &app_id, &app_password).await {
            Ok(token) => {
                if let Err(e) = send_message(
                    client,
                    &token,
                    &service_url,
                    &conversation_id,
                    &activity_id,
                    reply,
                )
                .await
                {
                    tracing::error!(conversation = %conversation_id, error = %e, "failed to send Teams reply");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to acquire Teams token");
            }
        }
    }

    Ok(json!({ "status_code": 200, "body": { "ok": true } }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = engine_ws_url();
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());
    let client = reqwest::Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .build()?;
    let keys: SharedKeys = Arc::new(RwLock::new(None));

    let iii_clone = iii.clone();
    let client_clone = client.clone();
    let keys_clone = keys.clone();
    iii.register_function(
        "channel::teams::webhook",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let client = client_clone.clone();
            let keys = keys_clone.clone();
            async move { handle_webhook(&iii, &client, &keys, input).await }
        })
        .description("Handle Microsoft Teams Bot Framework webhook"),
    );

    // The route is registered only when at least one inbound scheme can be
    // verified: a Bot Connector JWT needs the app id it is issued for, an
    // Outgoing Webhook needs its security token. Without either every delivery
    // would be refused anyway, and an unverifiable route is not worth exposing.
    let bot_framework = startup_secret(&iii, APP_ID_KEY).await.is_some();
    let outgoing_webhook = startup_secret(&iii, WEBHOOK_SECRET_KEY).await.is_some();
    if bot_framework || outgoing_webhook {
        agentos_http_adapter::register_http_trigger(
            &iii,
            "channel::teams::webhook".to_string(),
            json!({ "http_method": "POST", "api_path": "webhook/teams" }),
            None,
        )?;
        tracing::info!(
            bot_framework_jwt = bot_framework,
            outgoing_webhook_hmac = outgoing_webhook,
            "teams webhook route registered"
        );
    } else {
        tracing::error!(
            "neither {APP_ID_KEY} nor {WEBHOOK_SECRET_KEY} is configured: POST /webhook/teams is \
             NOT registered. Set the app id (Azure Bot) or the outgoing-webhook security token \
             and restart."
        );
    }

    tracing::info!("channel-teams worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_http_adapter::fake::FakeBus;
    use ring::rand::SystemRandom;
    use ring::signature::{KeyPair, RSA_PKCS1_SHA256, RsaKeyPair};

    #[test]
    fn engine_channel_outranks_an_inline_raw_body() {
        let channel = json!({ "channel_id": "ch-1", "access_key": "k", "direction": "read" });
        // Both present (what a request body carrying `rawBody` would produce if
        // the adapter let it through): the engine's channel is the source.
        let both = json!({ "request_body": channel, "rawBody": "{\"forged\":true}" });
        assert!(matches!(
            raw_body_source(&both).unwrap(),
            RawBodySource::Channel(StreamChannelRef { channel_id, .. }) if channel_id == "ch-1"
        ));
        // Inline alone (a bus caller or a test): accepted.
        assert!(matches!(
            raw_body_source(&json!({ "rawBody": "{}" })).unwrap(),
            RawBodySource::Inline(bytes) if bytes == b"{}"
        ));
        // A channel ref that cannot be used is a refusal, never a fall-through.
        assert!(raw_body_source(&json!({ "request_body": "junk", "rawBody": "{}" })).is_err());
        assert!(raw_body_source(&json!({})).is_err());
    }

    const APP_ID: &str = "3a1c6f3e-2b4d-4e5f-8a9b-0c1d2e3f4a5b";
    /// The base64 security token an Outgoing Webhook shows on creation
    /// (test value; shape matches the portal's, 32 random bytes in base64).
    const WEBHOOK_SECRET: &str = "5w0Y0e6y3qkYd6m1sWgs6xZq0m2M6fJ4mR7pQ9tUvXo=";
    const SERVICE_URL: &str = "https://smba.trafficmanager.net/amer/";
    /// A test-only 2048-bit RSA key (PKCS#8 DER) standing in for a Bot
    /// Connector signing key. Its public half is published to the tests as a
    /// JWK the way login.botframework.com publishes Microsoft's.
    const TEST_PKCS8_B64: &str = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7N5QFA03VuPhQOn+6AYy8KDZg2ELZErK1Fk/vxFgDQGWzwY714PjctKv43+obSnL7M80UiktcZnJx0jmUNfVEFk9Z6cYVSmRA/wP6e8ZKuZgNJ76U9PHA1RenEtaO0c4FEXDYYyEXoQmqmYc8AAXhQwMYIRCq5U3RQCbyM8Yr85+3U8LEozt2z9+J0bjgjS6U/hyA9YAaiUSUVWigi4vx/C/65Ra/b/g3EPn2Zo2kBPWulJnLRg7BCTioVmvST7syuUT85j/kJg1+V6q5yeOi1qcMId1A3y/zyJELcJQvOea25xu5d7a/8ZMZKbxBncTuVZepKn/SdA10Mq8a+DeJAgMBAAECggEAAl0nyc5qX633uK+caEFXwRJy6VMhuPLy/bVb6gedIuFfx17Eytb93W+MklZlctXEUOatCrraS77haA2C+5uYzrTHaLe6cA9h564woyuH6+6e/F+JmQDkwo4OP+ZNfj0o8Ehxl9Hcm5tFb3mDyx6m9FqvwdC9EKNXkbJRK5K6yFhED7Wz70XmdV65GpaY5caXnt0YIScb2Ye/tvb2SJ57g6hxU6HRrIBkP4z5MR/lf8sleTaUK/1zdLj8Qt9O7oG8Vfpf49f9jv/d/D1fSonJETFXEsZbbw2OzGtjNrm3x6Q3ViOXFXiBC+IC+nZNSC5SEzGQ8e2+lGjDfWMBfM6DzwKBgQD9y1Cq67VkcA5grsIQdg8rhhGFGmTfRCYM7ovYWc2r9LERjadaL8yNB/LZAQl/FYCmQmHfjcp/CZXGH7I+WxKpFqXGXzS3wSuaIvUp4dP6bSF/pZOYoGbhpY3tTGF8O8NXAEarOKwYrmy244ipECPs7OzilovKsgPykWevlU4juwKBgQC82CGFXsfe/H2ZDjLuEAGGPjDPApDv1ltoTn3YzCaNzzYWNJUJpgwPyM3F+YM5p4fJC/gjZzWBvc+kseuMCL5HkFUe9FpSJ8vlUzsMtFwfqjZi6vxWcXLwHF0XuTPAca990KLYblxlLh4KAPNKa5rd4QlhPHr5I7wV47jwa9TjiwKBgGqGkFFtpjGGJ0LFl4c5RpzKJUhtD7H29NGwvtoMt5tZlYj8oCXmskDv+SrEmKvS5rDiZBpldX1lFIyYeURbDbYTX3moNIR8fESyL51owIT4kXr2kMEbcpN73dqgmLqAizlVUFRF8VZawB7z2kS8FZg4yiVBc2Oc3LNP/OliDe5JAoGBAKCOgbGLLBQCSCbhU5vkL+ea6JSYcfH4Ji9AzO6OZBkdm7a1biGN86NX7tvrkA5syZ29d3NiRLPSVcCJJOMia+UcacKvrjs7arfHU+UxU0H4zdS8RV6ZhkdvVhbdd4qfHb2yrUGmUxgTZabLuA4F/t22fusVKNi58SgLPSnsBEyRAoGAXP0hpcRR1/QgQ8+P/rDoC8QAX8fmSW2eiE23mcgzDV9ihQWKlyM5TXZWxvB3tOLWlHmh4skr8vd/bY4fyTeanRpUxtToJNOnCoaEnEZTCqOZ/pPDD8coxQ+JCvGqicXaBAIxwBOjrdQ4ct5ZSnaDcI+9X4ohyaQYcVnZa/2VvVI=";
    const TEST_KID: &str = "test-kid-2026";

    /// A `message` activity as the Bot Connector service delivers it
    /// (https://learn.microsoft.com/en-us/azure/bot-service/rest-api/bot-framework-rest-connector-api-reference#activity-object).
    fn activity_json(text: &str) -> String {
        format!(
            r#"{{"type":"message","id":"1485983408511","timestamp":"2017-02-01T21:10:07.437Z","localTimestamp":"2017-02-01T14:10:07.437-07:00","serviceUrl":"{SERVICE_URL}","channelId":"msteams","from":{{"id":"29:1GIhh9K6ojx3S7Y9M5Z7aJvUQmYbEP5BZ1IjuLEbfsdFeHl9jPnv6LrNgkZUbnNcZ5nqFs2s6DzLbbNuHjZS9Bw","name":"Larry Jin","aadObjectId":"ea9c5f5b-2d8b-4d35-95d3-8dd3f6d7a5c6"}},"conversation":{{"isGroup":true,"conversationType":"channel","id":"19:253f6e2f5f4d4e3a9b2d0c1e8f7a6b5c@thread.skype;messageid=1485983194839"}},"recipient":{{"id":"28:c9e8d4f2-3b1a-4c5d-9e7f-1a2b3c4d5e6f","name":"AgentOS"}},"textFormat":"plain","text":"{text}","entities":[{{"locale":"en-US","country":"US","platform":"Windows","type":"clientInfo"}}]}}"#
        )
    }

    fn key_pair() -> RsaKeyPair {
        RsaKeyPair::from_pkcs8(&BASE64.decode(TEST_PKCS8_B64.as_bytes()).unwrap()).unwrap()
    }

    fn jwks() -> Vec<Jwk> {
        let components = RsaPublicKeyComponents::<Vec<u8>>::from(key_pair().public_key());
        vec![Jwk {
            kty: "RSA".into(),
            kid: TEST_KID.into(),
            n: BASE64URL_NOPAD.encode(&components.n),
            e: BASE64URL_NOPAD.encode(&components.e),
        }]
    }

    fn jwt(claims: Value) -> String {
        let header = json!({ "typ": "JWT", "alg": "RS256", "kid": TEST_KID, "x5t": TEST_KID });
        let signed = format!(
            "{}.{}",
            BASE64URL_NOPAD.encode(serde_json::to_vec(&header).unwrap().as_slice()),
            BASE64URL_NOPAD.encode(serde_json::to_vec(&claims).unwrap().as_slice())
        );
        let pair = key_pair();
        let mut signature = vec![0u8; pair.public().modulus_len()];
        pair.sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signed.as_bytes(),
            &mut signature,
        )
        .unwrap();
        format!("{signed}.{}", BASE64URL_NOPAD.encode(&signature))
    }

    /// Claims as the Bot Connector service mints them. The names are the ones
    /// the Bot Framework SDKs decode, not the prose spelling: `serviceurl` is
    /// `ServiceUrlClaim` in botbuilder-js
    /// (`libraries/botframework-connector/src/auth/authenticationConstants.ts`:
    /// `export const ServiceUrlClaim = 'serviceurl';`) and in botbuilder-dotnet
    /// (`libraries/Microsoft.Bot.Connector/Authentication/AuthenticationConstants.cs`:
    /// `public const string ServiceUrlClaim = "serviceurl";`); `iss` is
    /// `ToBotFromChannelTokenIssuer = 'https://api.botframework.com'` and `aud`
    /// is `AudienceClaim = 'aud'` in the same files. An earlier fixture wrote
    /// `serviceUrl`, which made the tests agree with an implementation that
    /// rejected every real token.
    fn claims(now: i64) -> Value {
        json!({
            "serviceurl": SERVICE_URL,
            "nbf": now - 60,
            "exp": now + 3600,
            "iss": BOT_CONNECTOR_ISSUER,
            "aud": APP_ID,
        })
    }

    fn hmac_header(secret_base64: &str, body: &[u8]) -> String {
        let key = BASE64.decode(secret_base64.as_bytes()).unwrap();
        let mut mac = HmacSha256::new_from_slice(&key).unwrap();
        mac.update(body);
        format!("HMAC {}", BASE64.encode(&mac.finalize().into_bytes()))
    }

    fn request(raw: &str, authorization: Option<&str>) -> Value {
        let mut headers = json!({ "content-type": "application/json" });
        if let Some(authorization) = authorization {
            headers["authorization"] = json!(authorization);
        }
        json!({
            "method": "POST",
            "headers": headers,
            "rawBody": raw,
            "body": serde_json::from_str::<Value>(raw).unwrap_or(Value::Null),
        })
    }

    fn bus(app_id: &str, webhook_secret: &str) -> FakeBus {
        let bus = FakeBus::new();
        let app_id = app_id.to_string();
        let webhook_secret = webhook_secret.to_string();
        bus.on("vault::get", move |payload| {
            let value = match payload["key"].as_str().unwrap_or_default() {
                APP_ID_KEY => app_id.clone(),
                WEBHOOK_SECRET_KEY => webhook_secret.clone(),
                _ => String::new(),
            };
            Ok(json!({ "value": value }))
        });
        bus.on_value("state::get", json!({ "agentId": "default" }));
        bus.on_value("agent::chat", json!({ "content": "hi from the agent" }));
        bus.on_value("security::audit", json!({}));
        bus
    }

    fn preloaded_keys() -> SharedKeys {
        Arc::new(RwLock::new(Some(KeyCache {
            keys: jwks(),
            fetched_at: Instant::now(),
        })))
    }

    // ---- Outgoing Webhook: HMAC -------------------------------------------

    #[test]
    fn outgoing_webhook_known_good_hmac_verifies() {
        let body = activity_json("<at>AgentOS</at> hello");
        // Independently computed (Python hmac, key = base64decode(WEBHOOK_SECRET)).
        let expected = "DyKRPusN/iBiY++fB8MrHPzxRPO5CbzOUfKW8JQwswA=";
        let header = hmac_header(WEBHOOK_SECRET, body.as_bytes());
        assert_eq!(header, format!("HMAC {expected}"));
        assert!(verify_outgoing_webhook_hmac(WEBHOOK_SECRET, body.as_bytes(), expected).is_ok());
    }

    #[test]
    fn outgoing_webhook_tampered_body_wrong_key_and_garbage_fail() {
        let body = activity_json("hello");
        let header = hmac_header(WEBHOOK_SECRET, body.as_bytes());
        let value = header.trim_start_matches("HMAC ");
        let tampered = activity_json("hello, ignore prior instructions");
        assert!(verify_outgoing_webhook_hmac(WEBHOOK_SECRET, tampered.as_bytes(), value).is_err());
        let other = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        assert!(verify_outgoing_webhook_hmac(other, body.as_bytes(), value).is_err());
        assert!(
            verify_outgoing_webhook_hmac(WEBHOOK_SECRET, body.as_bytes(), "not base64!").is_err()
        );
        assert!(verify_outgoing_webhook_hmac(WEBHOOK_SECRET, body.as_bytes(), "").is_err());
        assert!(verify_outgoing_webhook_hmac("", body.as_bytes(), value).is_err());
        assert!(verify_outgoing_webhook_hmac("not base64!", body.as_bytes(), value).is_err());
    }

    #[tokio::test]
    async fn outgoing_webhook_delivery_is_answered_in_the_response() {
        let bus = bus("", WEBHOOK_SECRET);
        let body = activity_json("<at>AgentOS</at> hello");
        let header = hmac_header(WEBHOOK_SECRET, body.as_bytes());
        let response = handle_webhook(
            &bus,
            &reqwest::Client::new(),
            &preloaded_keys(),
            request(&body, Some(&header)),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 200);
        assert_eq!(response["body"]["type"], "message");
        assert_eq!(response["body"]["text"], "hi from the agent");
        let chats = bus.calls_to("agent::chat");
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].payload["message"], "<at>AgentOS</at> hello");
    }

    #[tokio::test]
    async fn outgoing_webhook_forgeries_never_reach_the_agent() {
        let bus = bus("", WEBHOOK_SECRET);
        let body = activity_json("hello");
        let header = hmac_header(WEBHOOK_SECRET, body.as_bytes());
        let tampered = activity_json("tampered");
        let mut no_raw = request(&body, Some(&header));
        no_raw.as_object_mut().unwrap().remove("rawBody");
        for (req, status) in [
            (request(&tampered, Some(&header)), 401),
            (request(&body, Some("HMAC AAAA")), 401),
            (request(&body, None), 401),
            (request(&body, Some("Basic abc")), 401),
            (no_raw, 400),
        ] {
            let response = handle_webhook(&bus, &reqwest::Client::new(), &preloaded_keys(), req)
                .await
                .unwrap();
            assert_eq!(response["status_code"], status);
        }
        assert_eq!(bus.call_count("agent::chat"), 0);
        assert_eq!(bus.call_count("state::get"), 0);
    }

    #[tokio::test]
    async fn outgoing_webhook_refuses_without_its_secret() {
        let bus = bus(APP_ID, "");
        let body = activity_json("hello");
        let header = hmac_header(WEBHOOK_SECRET, body.as_bytes());
        let response = handle_webhook(
            &bus,
            &reqwest::Client::new(),
            &preloaded_keys(),
            request(&body, Some(&header)),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 503);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    // ---- Bot Framework: JWT ----------------------------------------------

    #[test]
    fn bot_connector_known_good_jwt_verifies() {
        let now = 1_700_000_000;
        let token = jwt(claims(now));
        let verified = verify_bot_framework_jwt(&token, APP_ID, &jwks(), now).unwrap();
        assert_eq!(verified["serviceurl"], SERVICE_URL);
        assert!(verify_service_url_claim(&verified, &json!({ "serviceUrl": SERVICE_URL })).is_ok());
    }

    #[test]
    fn bot_connector_token_with_microsofts_exact_claim_names_verifies() {
        // Exactly the claim set a Bot Connector token carries, spelled as the
        // botbuilder SDKs decode it (see `claims`): `serviceurl`, `nbf`, `exp`,
        // `iss`, `aud` — and nothing spelled `serviceUrl`.
        let now = 1_700_000_000;
        let microsoft = json!({
            "serviceurl": "https://smba.trafficmanager.net/emea/",
            "nbf": now - 300,
            "exp": now + 3600,
            "iss": "https://api.botframework.com",
            "aud": APP_ID,
        });
        assert!(microsoft.get("serviceUrl").is_none());
        let verified = verify_bot_framework_jwt(&jwt(microsoft), APP_ID, &jwks(), now).unwrap();
        let activity = json!({ "serviceUrl": "https://smba.trafficmanager.net/emea/" });
        assert!(verify_service_url_claim(&verified, &activity).is_ok());

        // The prose spelling is tolerated too, in case a channel ever emits it.
        let camel = json!({ "serviceUrl": "https://smba.trafficmanager.net/emea/" });
        assert!(verify_service_url_claim(&camel, &activity).is_ok());

        // A token carrying both spellings must agree with the activity on both:
        // the lookup cannot be steered to the matching one.
        let split = json!({
            "serviceurl": "https://smba.trafficmanager.net/emea/",
            "serviceUrl": "https://attacker.example/",
        });
        assert!(verify_service_url_claim(&split, &activity).is_err());
        let split = json!({
            "serviceurl": "https://attacker.example/",
            "serviceUrl": "https://smba.trafficmanager.net/emea/",
        });
        assert!(verify_service_url_claim(&split, &activity).is_err());

        // An activity without a serviceUrl cannot match any claim.
        assert!(verify_service_url_claim(&verified, &json!({ "serviceUrl": "" })).is_err());
    }

    #[test]
    fn bot_connector_jwt_rejects_every_broken_requirement() {
        let now = 1_700_000_000;
        let keys = jwks();
        let good = jwt(claims(now));

        // Signature: tamper with the payload after signing.
        let mut parts: Vec<&str> = good.split('.').collect();
        let forged_payload = BASE64URL_NOPAD.encode(
            serde_json::to_vec(&json!({
                "aud": APP_ID, "iss": BOT_CONNECTOR_ISSUER, "exp": now + 3600, "serviceurl": "https://attacker.example/"
            }))
            .unwrap()
            .as_slice(),
        );
        parts[1] = &forged_payload;
        let tampered = parts.join(".");
        assert!(verify_bot_framework_jwt(&tampered, APP_ID, &keys, now).is_err());

        // Unknown key id.
        let other_keys = vec![Jwk {
            kid: "other".into(),
            ..keys[0].clone()
        }];
        assert!(verify_bot_framework_jwt(&good, APP_ID, &other_keys, now).is_err());
        assert!(verify_bot_framework_jwt(&good, APP_ID, &[], now).is_err());

        // Wrong audience, wrong issuer, expired, not yet valid.
        assert!(verify_bot_framework_jwt(&good, "another-app-id", &keys, now).is_err());
        let mut wrong_issuer = claims(now);
        wrong_issuer["iss"] = json!("https://sts.windows.net/evil/");
        assert!(verify_bot_framework_jwt(&jwt(wrong_issuer), APP_ID, &keys, now).is_err());
        assert!(verify_bot_framework_jwt(&good, APP_ID, &keys, now + 3600 + 301).is_err());
        assert!(verify_bot_framework_jwt(&good, APP_ID, &keys, now + 3600 + 299).is_ok());
        assert!(verify_bot_framework_jwt(&good, APP_ID, &keys, now - 60 - 301).is_err());

        // alg confusion: an unsigned / HMAC-labelled token is refused outright.
        let none_header = BASE64URL_NOPAD.encode(br#"{"alg":"none","kid":"test-kid-2026"}"#);
        let none = format!("{none_header}.{}.", good.split('.').nth(1).unwrap());
        assert!(verify_bot_framework_jwt(&none, APP_ID, &keys, now).is_err());
        let hs_header = BASE64URL_NOPAD.encode(br#"{"alg":"HS256","kid":"test-kid-2026"}"#);
        let hs = format!("{hs_header}.{}.AAAA", good.split('.').nth(1).unwrap());
        assert!(verify_bot_framework_jwt(&hs, APP_ID, &keys, now).is_err());

        // Malformed.
        assert!(verify_bot_framework_jwt("", APP_ID, &keys, now).is_err());
        assert!(verify_bot_framework_jwt("a.b", APP_ID, &keys, now).is_err());
        assert!(verify_bot_framework_jwt("a.b.c.d", APP_ID, &keys, now).is_err());
        assert!(verify_bot_framework_jwt(&good, "", &keys, now).is_err());

        // Audience may be a list.
        let mut list_aud = claims(now);
        list_aud["aud"] = json!(["something-else", APP_ID]);
        assert!(verify_bot_framework_jwt(&jwt(list_aud), APP_ID, &keys, now).is_ok());
    }

    #[test]
    fn service_url_claim_must_match_the_activity() {
        let verified = json!({ "serviceurl": SERVICE_URL });
        assert!(verify_service_url_claim(&verified, &json!({ "serviceUrl": SERVICE_URL })).is_ok());
        assert!(
            verify_service_url_claim(
                &verified,
                &json!({ "serviceUrl": "https://smba.trafficmanager.net/amer" })
            )
            .is_ok(),
            "a trailing slash is not a different service"
        );
        assert!(
            verify_service_url_claim(
                &verified,
                &json!({ "serviceUrl": "https://attacker.example/" })
            )
            .is_err()
        );
        assert!(verify_service_url_claim(&verified, &json!({})).is_err());
        assert!(
            verify_service_url_claim(&json!({}), &json!({ "serviceUrl": SERVICE_URL })).is_err()
        );
    }

    #[tokio::test]
    async fn bot_connector_delivery_reaches_the_agent() {
        let bus = bus(APP_ID, "");
        let body = activity_json("hello bot");
        let token = jwt(claims(unix_now()));
        let response = handle_webhook(
            &bus,
            &reqwest::Client::new(),
            &preloaded_keys(),
            request(&body, Some(&format!("Bearer {token}"))),
        )
        .await
        .unwrap();
        // Verification passed and dispatch happened; the reply leg then failed
        // to mint a Bot Framework token because TEAMS_APP_PASSWORD is unset,
        // which is logged, not surfaced — Teams still gets its 200.
        assert_eq!(response["status_code"], 200);
        let chats = bus.calls_to("agent::chat");
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].payload["message"], "hello bot");
    }

    #[tokio::test]
    async fn bot_connector_forgeries_never_reach_the_agent() {
        let bus = bus(APP_ID, "");
        let body = activity_json("hello bot");
        let now = unix_now();
        let mut redirected = claims(now);
        redirected["serviceurl"] = json!("https://attacker.example/");
        let mut expired = claims(now);
        expired["exp"] = json!(now - 3600);
        for authorization in [
            format!("Bearer {}", jwt(redirected)),
            format!("Bearer {}", jwt(expired)),
            format!("Bearer {}.x", jwt(claims(now))),
            "Bearer not-a-jwt".to_string(),
        ] {
            let response = handle_webhook(
                &bus,
                &reqwest::Client::new(),
                &preloaded_keys(),
                request(&body, Some(&authorization)),
            )
            .await
            .unwrap();
            assert_eq!(response["status_code"], 401, "{authorization}");
        }
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn bot_connector_refuses_without_the_app_id() {
        let bus = bus("", WEBHOOK_SECRET);
        let body = activity_json("hello bot");
        let token = jwt(claims(unix_now()));
        let response = handle_webhook(
            &bus,
            &reqwest::Client::new(),
            &preloaded_keys(),
            request(&body, Some(&format!("Bearer {token}"))),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 503);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn startup_refuses_the_route_without_either_secret() {
        let none = bus("", "");
        assert_eq!(startup_secret(&none, APP_ID_KEY).await, None);
        assert_eq!(startup_secret(&none, WEBHOOK_SECRET_KEY).await, None);
        let jwt_only = bus(APP_ID, "");
        assert_eq!(
            startup_secret(&jwt_only, APP_ID_KEY).await.as_deref(),
            Some(APP_ID)
        );
        assert_eq!(startup_secret(&jwt_only, WEBHOOK_SECRET_KEY).await, None);
    }

    #[test]
    fn peek_kid_reads_only_the_header() {
        assert_eq!(peek_kid(&jwt(claims(0))).as_deref(), Some(TEST_KID));
        assert_eq!(peek_kid("garbage"), None);
    }

    // ---- unchanged behaviour ---------------------------------------------

    #[test]
    fn ignores_non_message_activity() {
        let activity = json!({ "type": "conversationUpdate" });
        assert_ne!(
            activity.get("type").and_then(|v| v.as_str()),
            Some("message")
        );
    }

    #[test]
    fn split_short_text_returns_single_chunk() {
        assert_eq!(split_message("hi", 4096), vec!["hi".to_string()]);
    }

    #[test]
    fn split_preserves_total_length() {
        let text = "x".repeat(10_000);
        let chunks = split_message(&text, 4096);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn allows_known_botframework_hosts() {
        assert!(is_allowed_service_url(
            "https://smba.trafficmanager.net.botframework.com/amer/",
            &[]
        ));
        assert!(is_allowed_service_url(SERVICE_URL, &[]));
        assert!(!is_allowed_service_url(
            "https://smba.trafficmanager.net.attacker.example/amer/",
            &[]
        ));
    }

    #[test]
    fn rejects_arbitrary_https_serviceurl() {
        assert!(!is_allowed_service_url(
            "https://attacker.example.com/",
            &[]
        ));
    }

    #[test]
    fn rejects_non_https_serviceurl() {
        assert!(!is_allowed_service_url(
            "http://smba.botframework.com/",
            &[]
        ));
    }

    #[test]
    fn allows_extra_configured_host() {
        assert!(is_allowed_service_url(
            "https://intranet.example.com/api/messages",
            &["intranet.example.com".to_string()]
        ));
    }
    #[tokio::test]
    async fn a_caller_supplied_raw_body_does_not_replace_the_engine_channel() {
        // A validly signed `rawBody` next to a `request_body` ref: the channel
        // is what gets read. Here the ref is unusable, so the request is
        // refused instead of being verified against the caller's bytes.
        let bus = bus("", WEBHOOK_SECRET);
        let body = activity_json("hello");
        let header = hmac_header(WEBHOOK_SECRET, body.as_bytes());
        let mut req = request(&body, Some(&header));
        req["request_body"] = json!("not-a-channel-ref");
        let response = handle_webhook(&bus, &reqwest::Client::new(), &preloaded_keys(), req)
            .await
            .unwrap();
        assert_eq!(response["status_code"], 400);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }
}
