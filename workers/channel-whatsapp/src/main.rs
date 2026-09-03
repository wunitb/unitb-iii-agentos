use agentos_http_adapter::TriggerBus;
use hmac::{Hmac, Mac};
use iii_sdk::channels::{ChannelReader, StreamChannelRef};
use iii_sdk::errors::Error;
use iii_sdk::protocol::TriggerAction;
use iii_sdk::{RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};
use sha2::Sha256;
use std::time::Duration;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

const WHATSAPP_API_BASE: &str = "https://graph.facebook.com/v18.0";
const MAX_MESSAGE_LEN: usize = 4096;
const APP_SECRET_KEY: &str = "WHATSAPP_APP_SECRET";
const VERIFY_TOKEN_KEY: &str = "WHATSAPP_VERIFY_TOKEN";

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

/// A query parameter as the adapter delivers it: under `query` (a string, or a
/// single-element list when the engine reports repeated keys) or scalarised at
/// the top level of the payload.
fn query_param<'a>(req: &'a Value, name: &str) -> Option<&'a str> {
    let nested = req
        .get("query")
        .or_else(|| req.get("query_params"))
        .and_then(|query| query.get(name))
        .and_then(|value| match value {
            Value::Array(items) if items.len() == 1 => items[0].as_str(),
            other => other.as_str(),
        });
    nested.or_else(|| req.get(name).and_then(Value::as_str))
}

/// Verify Meta's `X-Hub-Signature-256`: the hex HMAC-SHA256 of the raw
/// body keyed by the WhatsApp app secret
/// (https://developers.facebook.com/docs/graph-api/webhooks/getting-started).
/// The comparison is `Mac::verify_slice`, which is constant-time.
fn verify_meta_signature(secret: &str, raw_body: &[u8], signature: &str) -> Result<(), String> {
    if secret.is_empty() {
        return Err("WHATSAPP_APP_SECRET not configured".into());
    }
    let Some(provided_hex) = signature.trim().strip_prefix("sha256=") else {
        return Err("X-Hub-Signature-256 has no sha256= prefix".into());
    };
    let provided =
        hex::decode(provided_hex).map_err(|_| "X-Hub-Signature-256 is not hex".to_string())?;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|e| format!("HMAC init error: {e}"))?;
    mac.update(raw_body);
    mac.verify_slice(&provided)
        .map_err(|_| "Invalid X-Hub-Signature-256".to_string())
}

/// SHA256(prefix-8) hash of a phone number for non-identifying error logs.
fn redact(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(value.as_bytes());
    format!("sha256:{}", hex::encode(&digest[..8]))
}

/// Get a secret from `vault::get` first, falling back to env var.
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

/// Authenticate a POST delivery and return the parsed body it was signed over.
///
/// Order matters: the secret, signature header and raw bytes are all checked
/// BEFORE any JSON is parsed. The body handed back is parsed from the verified
/// bytes — never from the engine's pre-parsed `body`. Every failure is a refusal.
async fn authenticate(iii: &dyn TriggerBus, req: &Value) -> Result<Value, Value> {
    let secret = get_secret(iii, APP_SECRET_KEY).await;
    if secret.is_empty() {
        return Err(reject(503, "WHATSAPP_APP_SECRET not configured"));
    }
    let Some(signature) = header(req, "x-hub-signature-256").filter(|s| !s.is_empty()) else {
        return Err(reject(401, "Missing X-Hub-Signature-256 header"));
    };
    let raw = match raw_request_body(req).await {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(error = %e, "whatsapp: refusing delivery without its raw body");
            return Err(reject(
                400,
                "Raw request body unavailable for signature verification",
            ));
        }
    };
    if let Err(e) = verify_meta_signature(&secret, &raw, signature) {
        tracing::warn!(error = %e, "whatsapp signature rejected");
        return Err(reject(401, "Invalid X-Hub-Signature-256"));
    }
    serde_json::from_slice(&raw).map_err(|_| reject(400, "Body is not valid JSON"))
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

/// UTF-8-safe split into max-`max_len` char chunks, breaking on newline when reasonable.
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

async fn send_message(
    client: &reqwest::Client,
    token: &str,
    phone_id: &str,
    to: &str,
    text: &str,
) -> Result<(), Error> {
    if token.is_empty() {
        return Err(Error::Handler("WHATSAPP_TOKEN not configured".into()));
    }
    if phone_id.is_empty() {
        return Err(Error::Handler("WHATSAPP_PHONE_ID not configured".into()));
    }
    let url = format!("{WHATSAPP_API_BASE}/{phone_id}/messages");
    for chunk in split_message(text, MAX_MESSAGE_LEN) {
        let resp = client
            .post(&url)
            .bearer_auth(token)
            .json(&json!({
                "messaging_product": "whatsapp",
                "to": to,
                "type": "text",
                "text": { "body": chunk },
            }))
            .send()
            .await
            .map_err(|e| Error::Handler(format!("WhatsApp send error: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Handler(format!(
                "WhatsApp send failed ({status}): {}",
                body.chars().take(300).collect::<String>()
            )));
        }
    }
    Ok(())
}

async fn handle_webhook(
    iii: &dyn TriggerBus,
    client: &reqwest::Client,
    req: Value,
) -> Result<Value, Error> {
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("POST")
        .to_uppercase();

    // Meta's GET subscription handshake is token-authenticated, not body-signed.
    if method == "GET" {
        let expected = get_secret(iii, VERIFY_TOKEN_KEY).await;
        if expected.is_empty() {
            return Ok(reject(503, "WHATSAPP_VERIFY_TOKEN not configured"));
        }
        let mode = query_param(&req, "hub.mode").unwrap_or("");
        let token = query_param(&req, "hub.verify_token").unwrap_or("");
        let challenge = query_param(&req, "hub.challenge").unwrap_or("");
        if mode == "subscribe" && bool::from(token.as_bytes().ct_eq(expected.as_bytes())) {
            return Ok(json!({
                "status_code": 200,
                "headers": { "content-type": "text/plain" },
                "body": challenge,
            }));
        }
        return Ok(reject(403, "Verification failed"));
    }

    let body = match authenticate(iii, &req).await {
        Ok(body) => body,
        Err(response) => return Ok(response),
    };

    if body.get("object").and_then(|v| v.as_str()) != Some("whatsapp_business_account") {
        return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
    }

    let message = body
        .get("entry")
        .and_then(|e| e.get(0))
        .and_then(|e| e.get("changes"))
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("value"))
        .and_then(|v| v.get("messages"))
        .and_then(|m| m.get(0));

    let Some(message) = message else {
        return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
    };

    let text = message
        .get("text")
        .and_then(|t| t.get("body"))
        .and_then(|b| b.as_str())
        .unwrap_or("");
    if text.is_empty() {
        return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
    }

    let from = message
        .get("from")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let agent_id = resolve_agent(iii, "whatsapp", &from).await;

    let chat_result = iii
        .trigger(TriggerRequest {
            function_id: "agent::chat".to_string(),
            payload: json!({
                "agentId": agent_id,
                "message": text,
                "sessionId": format!("whatsapp:{from}"),
            }),
            action: None,
            timeout_ms: None,
        })
        .await;

    let reply = match &chat_result {
        Ok(chat) => chat
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Err(e) => {
            tracing::error!(
                agent = %agent_id,
                from_hash = %redact(&from),
                error = %e,
                "agent::chat failed"
            );
            String::new()
        }
    };

    if !reply.is_empty() {
        let token = get_secret(iii, "WHATSAPP_TOKEN").await;
        let phone_id = get_secret(iii, "WHATSAPP_PHONE_ID").await;
        if let Err(e) = send_message(client, &token, &phone_id, &from, &reply).await {
            tracing::error!(
                to_hash = %redact(&from),
                error = %e,
                "failed to send WhatsApp reply"
            );
        }
    }

    let _ = iii
        .trigger(TriggerRequest {
            function_id: "security::audit".to_string(),
            payload: json!({
                "type": "channel_message",
                "agentId": agent_id,
                "detail": { "channel": "whatsapp", "from": from },
            }),
            action: Some(TriggerAction::Void),
            timeout_ms: None,
        })
        .await;

    Ok(json!({ "status_code": 200, "body": { "ok": true } }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = engine_ws_url();
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());
    let client = reqwest::Client::new();

    let iii_clone = iii.clone();
    let client_clone = client.clone();
    iii.register_function(
        "channel::whatsapp::webhook",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let client = client_clone.clone();
            async move { handle_webhook(&iii, &client, input).await }
        })
        .description("Handle WhatsApp Business API webhook"),
    );

    let app_secret = startup_secret(&iii, APP_SECRET_KEY).await;
    let verify_token = startup_secret(&iii, VERIFY_TOKEN_KEY).await;
    if app_secret.is_some() {
        agentos_http_adapter::register_http_trigger(
            &iii,
            "channel::whatsapp::webhook".to_string(),
            json!({ "http_method": "POST", "api_path": "webhook/whatsapp" }),
            None,
        )?;
        agentos_http_adapter::register_http_trigger(
            &iii,
            "channel::whatsapp::webhook".to_string(),
            json!({ "http_method": "GET", "api_path": "webhook/whatsapp" }),
            None,
        )?;
        tracing::info!("whatsapp webhook routes registered (Meta signature verified)");
        if verify_token.is_none() {
            tracing::error!(
                "WHATSAPP_VERIFY_TOKEN is not configured: GET /webhook/whatsapp will refuse with 503"
            );
        }
    } else {
        tracing::error!(
            "WHATSAPP_APP_SECRET is not configured: WhatsApp webhook routes are NOT registered"
        );
        if verify_token.is_none() {
            tracing::error!("WHATSAPP_VERIFY_TOKEN is also not configured");
        }
    }

    tracing::info!("channel-whatsapp worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_params_are_read_in_every_shape_the_adapter_produces() {
        let nested = json!({ "query": { "k": "v" } });
        assert_eq!(query_param(&nested, "k"), Some("v"));
        let listed = json!({ "query_params": { "k": ["v"] } });
        assert_eq!(query_param(&listed, "k"), Some("v"));
        let scalarised = json!({ "k": "v" });
        assert_eq!(query_param(&scalarised, "k"), Some("v"));
        assert_eq!(
            query_param(&json!({ "query": { "k": ["a", "b"] } }), "k"),
            None
        );
        assert_eq!(query_param(&json!({}), "k"), None);
    }
    use agentos_http_adapter::fake::FakeBus;

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

    const DELIVERY: &str = r#"{"object":"whatsapp_business_account","entry":[{"changes":[{"value":{"messages":[{"from":"15551234567","text":{"body":"hello"}}]}}]}]}"#;
    const APP_SECRET: &str = "whatsapp-app-test-secret";
    const VERIFY_TOKEN: &str = "whatsapp-verify-test-token";
    const SIGNATURE: &str =
        "sha256=9acf53dad6453e8a280ba2b416d8e8f794e5acd52444156f3d5c8dea8acd6e2e";

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    fn post_request(raw: Option<&str>, signature: Option<&str>) -> Value {
        let mut headers = json!({ "content-type": "application/json" });
        if let Some(signature) = signature {
            headers["x-hub-signature-256"] = json!(signature);
        }
        let mut req = json!({
            "method": "POST",
            "headers": headers,
            "body": serde_json::from_str::<Value>(raw.unwrap_or(DELIVERY)).unwrap(),
        });
        if let Some(raw) = raw {
            req["rawBody"] = json!(raw);
        }
        req
    }

    fn get_request(token: &str) -> Value {
        json!({
            "method": "GET",
            "query": {
                "hub.mode": "subscribe",
                "hub.verify_token": token,
                "hub.challenge": "challenge-token",
            }
        })
    }

    fn bus_with_secrets(app_secret: &str, verify_token: &str) -> FakeBus {
        let bus = FakeBus::new();
        let app_secret = app_secret.to_string();
        let verify_token = verify_token.to_string();
        bus.on("vault::get", move |payload| {
            let value = match payload["key"].as_str().unwrap_or_default() {
                APP_SECRET_KEY => app_secret.clone(),
                VERIFY_TOKEN_KEY => verify_token.clone(),
                _ => String::new(),
            };
            Ok(json!({ "value": value }))
        });
        bus.on_value("state::get", json!({ "agentId": "default" }));
        bus.on_value("agent::chat", json!({ "content": "" }));
        bus.on_value("security::audit", json!({}));
        bus
    }

    #[test]
    fn known_good_signature_verifies() {
        // Computed independently with Python's hmac/hashlib over DELIVERY,
        // keyed by APP_SECRET.
        assert_eq!(sign(APP_SECRET, DELIVERY.as_bytes()), SIGNATURE);
        assert!(verify_meta_signature(APP_SECRET, DELIVERY.as_bytes(), SIGNATURE).is_ok());
    }

    #[test]
    fn tampered_body_fails_signature_verification() {
        let tampered = DELIVERY.replace("hello", "forged");
        assert!(verify_meta_signature(APP_SECRET, tampered.as_bytes(), SIGNATURE).is_err());
    }

    #[tokio::test]
    async fn valid_delivery_reaches_agent_but_forgery_does_not() {
        let bus = bus_with_secrets(APP_SECRET, VERIFY_TOKEN);
        let valid = handle_webhook(
            &bus,
            &reqwest::Client::new(),
            post_request(Some(DELIVERY), Some(SIGNATURE)),
        )
        .await
        .unwrap();
        assert_eq!(valid["status_code"], 200);
        assert_eq!(bus.call_count("agent::chat"), 1);

        let tampered = DELIVERY.replace("hello", "forged");
        let forged = handle_webhook(
            &bus,
            &reqwest::Client::new(),
            post_request(Some(&tampered), Some(SIGNATURE)),
        )
        .await
        .unwrap();
        assert_eq!(forged["status_code"], 401);
        assert_eq!(bus.call_count("agent::chat"), 1);
    }

    #[tokio::test]
    async fn missing_signature_header_is_rejected() {
        let bus = bus_with_secrets(APP_SECRET, VERIFY_TOKEN);
        let response = handle_webhook(
            &bus,
            &reqwest::Client::new(),
            post_request(Some(DELIVERY), None),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 401);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn missing_raw_body_is_rejected() {
        let bus = bus_with_secrets(APP_SECRET, VERIFY_TOKEN);
        let response = handle_webhook(
            &bus,
            &reqwest::Client::new(),
            post_request(None, Some(SIGNATURE)),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 400);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn missing_app_secret_refuses_delivery_and_startup() {
        let bus = bus_with_secrets("", VERIFY_TOKEN);
        let response = handle_webhook(
            &bus,
            &reqwest::Client::new(),
            post_request(Some(DELIVERY), Some(SIGNATURE)),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 503);
        assert_eq!(bus.call_count("agent::chat"), 0);
        assert_eq!(startup_secret(&bus, APP_SECRET_KEY).await, None);
    }

    #[tokio::test]
    async fn get_handshake_is_constant_time_guarded_and_text_plain() {
        let bus = bus_with_secrets(APP_SECRET, VERIFY_TOKEN);
        let valid = handle_webhook(&bus, &reqwest::Client::new(), get_request(VERIFY_TOKEN))
            .await
            .unwrap();
        assert_eq!(valid["status_code"], 200);
        assert_eq!(valid["body"], "challenge-token");
        assert_eq!(valid["headers"]["content-type"], "text/plain");

        let invalid = handle_webhook(&bus, &reqwest::Client::new(), get_request("wrong"))
            .await
            .unwrap();
        assert_eq!(invalid["status_code"], 403);
    }

    #[tokio::test]
    async fn get_handshake_missing_verify_token_is_503() {
        let bus = bus_with_secrets(APP_SECRET, "");
        let response = handle_webhook(&bus, &reqwest::Client::new(), get_request(VERIFY_TOKEN))
            .await
            .unwrap();
        assert_eq!(response["status_code"], 503);
    }

    #[test]
    fn split_short_text_returns_single_chunk() {
        let chunks = split_message("hi", 4096);
        assert_eq!(chunks, vec!["hi".to_string()]);
    }

    #[test]
    fn split_preserves_total_length() {
        let text = "x".repeat(10_000);
        let chunks = split_message(&text, 4096);
        let joined: String = chunks.concat();
        assert_eq!(joined, text);
    }

    #[test]
    fn split_handles_multibyte_chars() {
        let text: String = "🦀".repeat(10);
        let chunks = split_message(&text, 3);
        let joined: String = chunks.concat();
        assert_eq!(joined, text);
    }

    #[test]
    fn ignores_non_whatsapp_object() {
        let body = json!({ "object": "page" });
        assert_ne!(
            body.get("object").and_then(|v| v.as_str()),
            Some("whatsapp_business_account")
        );
    }

    #[test]
    fn extracts_text_from_nested_payload() {
        let body = json!({
            "object": "whatsapp_business_account",
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "15551234567",
                            "text": { "body": "hello" }
                        }]
                    }
                }]
            }]
        });
        let text = body
            .get("entry")
            .and_then(|e| e.get(0))
            .and_then(|e| e.get("changes"))
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("value"))
            .and_then(|v| v.get("messages"))
            .and_then(|m| m.get(0))
            .and_then(|m| m.get("text"))
            .and_then(|t| t.get("body"))
            .and_then(|b| b.as_str())
            .unwrap_or("");
        assert_eq!(text, "hello");
    }

    #[test]
    fn missing_text_yields_empty() {
        let body = json!({
            "object": "whatsapp_business_account",
            "entry": [{ "changes": [{ "value": { "messages": [{ "from": "1" }] } }] }]
        });
        let text = body
            .pointer("/entry/0/changes/0/value/messages/0/text/body")
            .and_then(|b| b.as_str())
            .unwrap_or("");
        assert!(text.is_empty());
    }
    #[tokio::test]
    async fn a_caller_supplied_raw_body_does_not_replace_the_engine_channel() {
        // A validly signed `rawBody` next to a `request_body` ref: the channel
        // is what gets read. Here the ref is unusable, so the request is
        // refused instead of being verified against the caller's bytes.
        let bus = bus_with_secrets(APP_SECRET, VERIFY_TOKEN);
        let mut req = post_request(Some(DELIVERY), Some(SIGNATURE));
        req["request_body"] = json!("not-a-channel-ref");
        let response = handle_webhook(&bus, &reqwest::Client::new(), req)
            .await
            .unwrap();
        assert_eq!(response["status_code"], 400);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }
}
