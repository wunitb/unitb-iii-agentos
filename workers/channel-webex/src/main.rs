use agentos_http_adapter::{CHAT_TIMEOUT_MS, TriggerBus};
use hmac::{Hmac, Mac};
use iii_sdk::channels::{ChannelReader, StreamChannelRef};
use iii_sdk::errors::Error;
use iii_sdk::protocol::TriggerAction;
use iii_sdk::{RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};
use sha1::Sha1;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

type HmacSha1 = Hmac<Sha1>;

const API_URL: &str = "https://webexapis.com/v1";
const MAX_MESSAGE_LEN: usize = 7439;

/// Name of the secret that authenticates Webex deliveries. It is the `secret`
/// given to `POST /v1/webhooks` when the webhook was created; Webex signs every
/// delivery with it. Distinct from `WEBEX_TOKEN`, which authenticates OUR calls
/// to Webex and never appears on the wire inbound.
const WEBHOOK_SECRET_KEY: &str = "WEBEX_WEBHOOK_SECRET";

/// Upper bound on a provider delivery we are willing to read before verifying it.
const MAX_RAW_BODY_BYTES: usize = 4 * 1024 * 1024;
/// The engine streams the body from local memory; anything slower is a fault.
const RAW_BODY_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Process-local cache of the bot's own `personId`, populated lazily so we do
/// not hit `/v1/people/me` on every webhook delivery.
type BotIdCache = Arc<RwLock<Option<String>>>;

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

/// Verify Webex's `X-Spark-Signature` header: the lowercase hex HMAC-SHA1 of the
/// raw request body keyed by the webhook `secret`
/// (https://developer.webex.com/docs/api/guides/webhooks#handling-requests-from-webex).
/// The comparison is `Mac::verify_slice`, which is constant-time.
fn verify_webex_signature(secret: &str, raw_body: &[u8], signature: &str) -> Result<(), String> {
    if secret.is_empty() {
        return Err("WEBEX_WEBHOOK_SECRET not configured".into());
    }
    let provided =
        hex::decode(signature.trim()).map_err(|_| "X-Spark-Signature is not hex".to_string())?;
    let mut mac =
        HmacSha1::new_from_slice(secret.as_bytes()).map_err(|e| format!("HMAC init error: {e}"))?;
    mac.update(raw_body);
    mac.verify_slice(&provided)
        .map_err(|_| "Invalid X-Spark-Signature".to_string())
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

async fn fetch_message(
    client: &reqwest::Client,
    token: &str,
    message_id: &str,
) -> Result<Option<String>, Error> {
    let resp = client
        .get(format!("{API_URL}/messages/{message_id}"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| Error::Handler(format!("Webex fetch error: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Handler(format!(
            "Webex fetch failed ({status}): {}",
            body.chars().take(300).collect::<String>()
        )));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| Error::Handler(format!("Webex decode: {e}")))?;
    Ok(body.get("text").and_then(|v| v.as_str()).map(String::from))
}

/// Fetch the bot's own personId from `/v1/people/me` so we can drop self-posted
/// webhook events. Returns `None` on any failure so the caller can skip the
/// guard and continue processing.
async fn fetch_bot_person_id(client: &reqwest::Client, token: &str) -> Option<String> {
    let resp = client
        .get(format!("{API_URL}/people/me"))
        .bearer_auth(token)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    body.get("id").and_then(|v| v.as_str()).map(String::from)
}

async fn send_message(
    client: &reqwest::Client,
    token: &str,
    room_id: &str,
    text: &str,
) -> Result<(), Error> {
    for chunk in split_message(text, MAX_MESSAGE_LEN) {
        let resp = client
            .post(format!("{API_URL}/messages"))
            .bearer_auth(token)
            .json(&json!({ "roomId": room_id, "text": chunk }))
            .send()
            .await
            .map_err(|e| Error::Handler(format!("Webex send error: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Handler(format!(
                "Webex send failed ({status}): {}",
                body.chars().take(300).collect::<String>()
            )));
        }
    }
    Ok(())
}

/// Authenticate one delivery and return the parsed body it was signed over.
///
/// Order matters: the secret, the header and the raw bytes are all checked
/// BEFORE any JSON is parsed, and the body handed back is parsed from the
/// verified bytes — never from the engine's pre-parsed `body`, which was not
/// what Webex signed. Every failure is a refusal (`Err(response)`).
async fn authenticate(iii: &dyn TriggerBus, req: &Value) -> Result<Value, Value> {
    let secret = get_secret(iii, WEBHOOK_SECRET_KEY).await;
    if secret.is_empty() {
        return Err(reject(503, "WEBEX_WEBHOOK_SECRET not configured"));
    }
    let Some(signature) = header(req, "x-spark-signature").filter(|s| !s.is_empty()) else {
        return Err(reject(401, "Missing X-Spark-Signature header"));
    };
    let raw = match raw_request_body(req).await {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(error = %e, "webex: refusing delivery without its raw body");
            return Err(reject(
                400,
                "Raw request body unavailable for signature verification",
            ));
        }
    };
    if let Err(e) = verify_webex_signature(&secret, &raw, signature) {
        tracing::warn!(error = %e, "webex signature rejected");
        return Err(reject(401, "Invalid X-Spark-Signature"));
    }
    serde_json::from_slice(&raw).map_err(|_| reject(400, "Body is not valid JSON"))
}

async fn handle_webhook(
    iii: &dyn TriggerBus,
    client: &reqwest::Client,
    bot_id_cache: &BotIdCache,
    req: Value,
) -> Result<Value, Error> {
    let body = match authenticate(iii, &req).await {
        Ok(body) => body,
        Err(response) => return Ok(response),
    };

    let resource = body.get("resource").and_then(|v| v.as_str()).unwrap_or("");
    let event = body.get("event").and_then(|v| v.as_str()).unwrap_or("");
    if resource != "messages" || event != "created" {
        return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
    }

    let message_id = body
        .pointer("/data/id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let room_id = body
        .pointer("/data/roomId")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let person_id = body
        .pointer("/data/personId")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let webex_token = get_secret(iii, "WEBEX_TOKEN").await;
    if webex_token.is_empty() {
        return Ok(json!({
            "status_code": 500,
            "body": { "error": "WEBEX_TOKEN not configured" }
        }));
    }

    // Drop self-posted messages so the bot does not loop on its own replies.
    let bot_id = {
        let cached = bot_id_cache.read().await.clone();
        match cached {
            Some(id) => Some(id),
            None => {
                let fetched = fetch_bot_person_id(client, &webex_token).await;
                if let Some(id) = &fetched {
                    let mut guard = bot_id_cache.write().await;
                    if guard.is_none() {
                        *guard = Some(id.clone());
                    }
                }
                fetched
            }
        }
    };
    if let Some(id) = bot_id.as_deref()
        && id == person_id
    {
        return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
    }

    let text = match fetch_message(client, &webex_token, &message_id).await? {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(json!({ "status_code": 200, "body": { "ok": true } })),
    };

    let agent_id = resolve_agent(iii, "webex", &room_id).await;

    let chat = iii
        .trigger(TriggerRequest {
            function_id: "agent::chat".to_string(),
            payload: json!({
                "agentId": agent_id,
                "message": text,
                "sessionId": format!("webex:{room_id}"),
            }),
            action: None,
            timeout_ms: Some(CHAT_TIMEOUT_MS),
        })
        .await
        .map_err(|e| Error::Handler(format!("agent::chat failed: {e}")))?;

    let reply = chat.get("content").and_then(|v| v.as_str()).unwrap_or("");
    if !reply.is_empty()
        && let Err(e) = send_message(client, &webex_token, &room_id, reply).await
    {
        tracing::error!(room = %room_id, error = %e, "failed to send Webex reply");
    }

    let _ = iii
        .trigger(TriggerRequest {
            function_id: "security::audit".to_string(),
            payload: json!({
                "type": "channel_message",
                "agentId": agent_id,
                "detail": {
                    "channel": "webex",
                    "roomId": room_id,
                    "personId": person_id
                },
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
    let bot_id_cache: BotIdCache = Arc::new(RwLock::new(None));

    let iii_clone = iii.clone();
    let client_clone = client.clone();
    let cache_clone = bot_id_cache.clone();
    iii.register_function(
        "channel::webex::webhook",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let client = client_clone.clone();
            let cache = cache_clone.clone();
            async move { handle_webhook(&iii, &client, &cache, input).await }
        })
        .description("Handle Cisco Webex webhook"),
    );

    // The route is registered only when the secret that verifies Webex's
    // signature exists. Without it every delivery would be refused anyway, and
    // an unverifiable route is not worth exposing. The handler re-reads the
    // secret per request, so a rotation takes effect without a restart.
    if startup_secret(&iii, WEBHOOK_SECRET_KEY).await.is_some() {
        agentos_http_adapter::register_http_trigger(
            &iii,
            "channel::webex::webhook".to_string(),
            json!({ "http_method": "POST", "api_path": "webhook/webex" }),
            None,
        )?;
        tracing::info!("webex webhook route registered (X-Spark-Signature verified)");
    } else {
        tracing::error!(
            "{WEBHOOK_SECRET_KEY} is not configured: POST /webhook/webex is NOT registered. \
             Set it to the `secret` the Webex webhook was created with and restart."
        );
    }

    tracing::info!("channel-webex worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// A real Webex `messages/created` delivery shape
    /// (https://developer.webex.com/docs/api/guides/webhooks#webhook-payload).
    const DELIVERY: &str = r#"{"id":"Y2lzY29zcGFyazovL3VzL1dFQkhPT0svOTZhYmMyYWEtM2RjYy0xMWU1LWExNTItZmUzNDgxOWNkYzlh","name":"My Attachment Action Webhook","targetUrl":"https://example.com/mywebhook","resource":"messages","event":"created","filter":"roomId=Y2lzY29zcGFyazovL3VzL1JPT00vYmJjZWIxYWQtNDNmMS0zYjU4LTkxNDctZjE0YmIwYzRkMTU0","orgId":"OTZhYmMyYWEtM2RjYy0xMWU1LWExNTItZmUzNDgxOWNkYzlh","createdBy":"Y2lzY29zcGFyazovL3VzL1BFT1BMRS9mNWIzNjE4Ny1jOGRkLTQ3MjctOGIyZi1mOWM0NDdmMjkwNDY","appId":"Y2lzY29zcGFyazovL3VzL0FQUExJQ0FUSU9OL0MyNzljYjMwYzAyOTE4MGJiNGJkYWViYjA2MWI3OTY1Y2RhMzliNjAyOTdjODUwM2YyNjZhYmY2NmM5OTllYzFm","ownedBy":"creator","status":"active","actorId":"Y2lzY29zcGFyazovL3VzL1BFT1BMRS9mNWIzNjE4Ny1jOGRkLTQ3MjctOGIyZi1mOWM0NDdmMjkwNDY","data":{"id":"Y2lzY29zcGFyazovL3VzL01FU1NBR0UvOTJkYjNiZTAtNDNiZC0xMWU2LThhZTktZGQ1YjNkZmM1NjVk","roomId":"Y2lzY29zcGFyazovL3VzL1JPT00vYmJjZWIxYWQtNDNmMS0zYjU4LTkxNDctZjE0YmIwYzRkMTU0","personId":"Y2lzY29zcGFyazovL3VzL1BFT1BMRS9mNWIzNjE4Ny1jOGRkLTQ3MjctOGIyZi1mOWM0NDdmMjkwNDY","personEmail":"matt@example.com","created":"2015-10-18T14:26:16.000Z"}}"#;
    const SECRET: &str = "webhook-secret-from-post-v1-webhooks";

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha1::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    fn request(raw: &str, signature: Option<&str>) -> Value {
        let mut headers = json!({ "content-type": "application/json" });
        if let Some(signature) = signature {
            headers["x-spark-signature"] = json!(signature);
        }
        json!({
            "method": "POST",
            "headers": headers,
            "rawBody": raw,
            // What the engine would hand over after its own parse: present so a
            // handler that wrongly trusted it instead of rawBody would be caught.
            "body": serde_json::from_str::<Value>(raw).unwrap_or(Value::Null),
        })
    }

    fn bus_with_secret(secret: &str) -> FakeBus {
        let bus = FakeBus::new();
        let secret = secret.to_string();
        bus.on("vault::get", move |payload| {
            let key = payload["key"].as_str().unwrap_or_default();
            Ok(json!({ "value": if key == WEBHOOK_SECRET_KEY { secret.clone() } else { String::new() } }))
        });
        bus
    }

    #[test]
    fn known_good_signature_verifies() {
        // HMAC-SHA1 vector computed independently (Python hmac, key=SECRET,
        // msg=DELIVERY) so the test does not merely agree with itself.
        let expected = "4a7d8a9f8adc87192c16b10d8e4345022c74f2ab";
        assert_eq!(sign(SECRET, DELIVERY.as_bytes()), expected);
        assert!(verify_webex_signature(SECRET, DELIVERY.as_bytes(), expected).is_ok());
    }

    #[test]
    fn tampered_body_fails() {
        let signature = sign(SECRET, DELIVERY.as_bytes());
        let tampered = DELIVERY.replace("\"event\":\"created\"", "\"event\":\"deleted\"");
        assert!(verify_webex_signature(SECRET, tampered.as_bytes(), &signature).is_err());
    }

    #[test]
    fn wrong_secret_garbage_and_non_hex_signatures_fail() {
        let signature = sign(SECRET, DELIVERY.as_bytes());
        assert!(verify_webex_signature("other-secret", DELIVERY.as_bytes(), &signature).is_err());
        assert!(verify_webex_signature(SECRET, DELIVERY.as_bytes(), "deadbeef").is_err());
        assert!(verify_webex_signature(SECRET, DELIVERY.as_bytes(), "not hex at all").is_err());
        assert!(verify_webex_signature(SECRET, DELIVERY.as_bytes(), "").is_err());
    }

    #[test]
    fn empty_secret_refuses_even_a_matching_signature() {
        let signature = sign("", DELIVERY.as_bytes());
        assert!(verify_webex_signature("", DELIVERY.as_bytes(), &signature).is_err());
    }

    #[tokio::test]
    async fn handler_accepts_a_signed_delivery_and_reaches_the_bus() {
        let bus = bus_with_secret(SECRET);
        let signature = sign(SECRET, DELIVERY.as_bytes());
        // Verification passed and the handler moved on to the next required
        // secret; no network call was made before that point.
        let response = handle_webhook(
            &bus,
            &reqwest::Client::new(),
            &Arc::new(RwLock::new(None)),
            request(DELIVERY, Some(&signature)),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 500);
        assert_eq!(response["body"]["error"], "WEBEX_TOKEN not configured");
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn handler_rejects_a_tampered_body_before_anything_else() {
        let bus = bus_with_secret(SECRET);
        let signature = sign(SECRET, DELIVERY.as_bytes());
        let tampered = DELIVERY.replace("matt@example.com", "mallory@example.com");
        let response = handle_webhook(
            &bus,
            &reqwest::Client::new(),
            &Arc::new(RwLock::new(None)),
            request(&tampered, Some(&signature)),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 401);
        assert_eq!(
            bus.calls_to("vault::get").len(),
            1,
            "only the signing secret was read"
        );
        assert_eq!(bus.call_count("agent::chat"), 0);
        assert_eq!(bus.call_count("state::get"), 0);
    }

    #[tokio::test]
    async fn handler_rejects_a_missing_signature() {
        let bus = bus_with_secret(SECRET);
        let response = handle_webhook(
            &bus,
            &reqwest::Client::new(),
            &Arc::new(RwLock::new(None)),
            request(DELIVERY, None),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 401);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn handler_rejects_when_the_raw_body_is_unavailable() {
        let bus = bus_with_secret(SECRET);
        let signature = sign(SECRET, DELIVERY.as_bytes());
        let mut req = request(DELIVERY, Some(&signature));
        req.as_object_mut().unwrap().remove("rawBody");
        let response = handle_webhook(
            &bus,
            &reqwest::Client::new(),
            &Arc::new(RwLock::new(None)),
            req,
        )
        .await
        .unwrap();
        assert_eq!(
            response["status_code"], 400,
            "a body that cannot be verified is refused, not trusted"
        );
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn handler_refuses_when_the_secret_is_missing() {
        let bus = bus_with_secret("");
        let signature = sign(SECRET, DELIVERY.as_bytes());
        let response = handle_webhook(
            &bus,
            &reqwest::Client::new(),
            &Arc::new(RwLock::new(None)),
            request(DELIVERY, Some(&signature)),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 503);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn startup_refuses_the_route_without_a_secret() {
        // The env fallback is not set in any test environment for this name.
        let bus = FakeBus::new();
        bus.on_error("vault::get", "function_not_found");
        assert_eq!(startup_secret(&bus, WEBHOOK_SECRET_KEY).await, None);

        let configured = bus_with_secret(SECRET);
        assert_eq!(
            startup_secret(&configured, WEBHOOK_SECRET_KEY)
                .await
                .as_deref(),
            Some(SECRET)
        );
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let req = json!({ "headers": { "X-Spark-Signature": "abc" } });
        assert_eq!(header(&req, "x-spark-signature"), Some("abc"));
        assert_eq!(header(&json!({}), "x-spark-signature"), None);
    }

    #[test]
    fn ignores_non_message_resource() {
        let body = json!({ "resource": "memberships", "event": "created" });
        let resource = body.get("resource").and_then(|v| v.as_str()).unwrap_or("");
        assert_ne!(resource, "messages");
    }

    #[test]
    fn ignores_non_created_event() {
        let body = json!({ "resource": "messages", "event": "deleted" });
        let event = body.get("event").and_then(|v| v.as_str()).unwrap_or("");
        assert_ne!(event, "created");
    }

    #[test]
    fn extracts_data_fields() {
        let body = json!({
            "resource": "messages",
            "event": "created",
            "data": { "id": "M1", "roomId": "R1", "personId": "P1" }
        });
        assert_eq!(
            body.pointer("/data/id").and_then(|v| v.as_str()),
            Some("M1")
        );
        assert_eq!(
            body.pointer("/data/roomId").and_then(|v| v.as_str()),
            Some("R1")
        );
        assert_eq!(
            body.pointer("/data/personId").and_then(|v| v.as_str()),
            Some("P1")
        );
    }

    #[test]
    fn split_short_text_returns_single_chunk() {
        assert_eq!(split_message("hi", 7439), vec!["hi".to_string()]);
    }

    #[test]
    fn split_preserves_total_length() {
        let text = "x".repeat(20_000);
        let chunks = split_message(&text, 7439);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn session_id_format() {
        let room = "R1";
        assert_eq!(format!("webex:{room}"), "webex:R1");
    }
    #[tokio::test]
    async fn a_caller_supplied_raw_body_does_not_replace_the_engine_channel() {
        // A validly signed `rawBody` next to a `request_body` ref: the channel
        // is what gets read. Here the ref is unusable, so the request is
        // refused instead of being verified against the caller's bytes.
        let bus = bus_with_secret(SECRET);
        let signature = sign(SECRET, DELIVERY.as_bytes());
        let mut req = request(DELIVERY, Some(&signature));
        req["request_body"] = json!("not-a-channel-ref");
        let response = handle_webhook(
            &bus,
            &reqwest::Client::new(),
            &Arc::new(RwLock::new(None)),
            req,
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 400);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }
}
