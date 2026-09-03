use agentos_http_adapter::TriggerBus;
use hmac::{Hmac, Mac};
use iii_sdk::channels::{ChannelReader, StreamChannelRef};
use iii_sdk::errors::Error;
use iii_sdk::protocol::TriggerAction;
use iii_sdk::{RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};
use sha2::Sha256;
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

const TWITCH_API: &str = "https://api.twitch.tv/helix";
const MAX_MESSAGE_LEN: usize = 500;
const TWITCH_EVENTSUB_SECRET_KEY: &str = "TWITCH_EVENTSUB_SECRET";

/// Upper bound on a provider delivery we are willing to read before verifying it.
const MAX_RAW_BODY_BYTES: usize = 4 * 1024 * 1024;
/// The engine streams the body from local memory; anything slower is a fault.
const RAW_BODY_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Engine WebSocket base: the same address `main` connects to.
fn engine_ws_url() -> String {
    std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string())
}

/// The request body exactly as the provider sent it.
///
/// iii 0.22.1 hands HTTP handlers a `body` that is already parsed and
/// re-serialised (verified: `{ "b" : 2 , "a" : 1 }` arrives as `{"a":1,"b":2}`),
/// so no signature can be checked against it. The original bytes are exposed as
/// the `request_body` stream channel (verified live on 0.22.1, with and without
/// bus RBAC armed: the channel is keyed by its own access key, not the bus
/// credential). A `rawBody` string, when present, wins so an adapter that has
/// already drained the channel — or a test — can hand the bytes over directly.
/// Absent both, the caller refuses the request: nothing here guesses.
async fn raw_request_body(req: &Value) -> Result<Vec<u8>, String> {
    if let Some(raw) = req.get("rawBody").and_then(Value::as_str) {
        return Ok(raw.as_bytes().to_vec());
    }
    let Some(channel) = req.get("request_body") else {
        return Err("raw request body unavailable (no rawBody, no request_body channel)".into());
    };
    let channel: StreamChannelRef = serde_json::from_value(channel.clone())
        .map_err(|e| format!("request_body channel ref is malformed: {e}"))?;
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

fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.chars().count() <= max_len {
        return vec![text.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut remaining: &str = text;
    while !remaining.is_empty() {
        if remaining.chars().count() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }
        let split_idx = remaining
            .char_indices()
            .nth(max_len)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());
        chunks.push(remaining[..split_idx].to_string());
        remaining = &remaining[split_idx..];
    }
    chunks
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
    if let Ok(v) = result
        && let Some(value) = v.get("value").and_then(|s| s.as_str())
        && !value.is_empty()
    {
        return value.to_string();
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
    let result = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({
                "scope": "channel_agents",
                "key": format!("{channel}:{channel_id}"),
            }),
            action: None,
            timeout_ms: None,
        })
        .await;
    match result {
        Ok(v) => v
            .get("agentId")
            .and_then(|a| a.as_str())
            .unwrap_or("default")
            .to_string(),
        Err(_) => "default".to_string(),
    }
}

/// Verify Twitch EventSub's `Twitch-Eventsub-Message-Signature`: the hex
/// HMAC-SHA256 of `message_id || timestamp || raw_body`, keyed by the EventSub
/// secret (https://dev.twitch.tv/docs/eventsub/handling-webhook-events).
/// The comparison is `Mac::verify_slice`, which is constant-time.
fn verify_eventsub_signature(
    secret: &str,
    message_id: &str,
    timestamp: &str,
    raw_body: &[u8],
    signature: &str,
) -> Result<(), String> {
    if secret.is_empty() {
        return Err("TWITCH_EVENTSUB_SECRET not configured".into());
    }
    let Some(provided_hex) = signature.trim().strip_prefix("sha256=") else {
        return Err("Twitch EventSub signature has no sha256= prefix".into());
    };
    let provided = hex::decode(provided_hex)
        .map_err(|_| "Twitch EventSub signature is not hex".to_string())?;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|e| format!("HMAC init error: {e}"))?;
    mac.update(message_id.as_bytes());
    mac.update(timestamp.as_bytes());
    mac.update(raw_body);
    mac.verify_slice(&provided)
        .map_err(|_| "Invalid Twitch EventSub signature".to_string())
}

/// Authenticate one delivery and return the parsed body it was signed over.
///
/// Order matters: the secret, all EventSub headers and the raw bytes are checked
/// BEFORE any JSON is parsed. The body handed back is parsed from the verified
/// bytes — never from the engine's pre-parsed `body`. Every failure is a refusal.
async fn authenticate(iii: &dyn TriggerBus, req: &Value) -> Result<Value, Value> {
    let secret = get_secret(iii, TWITCH_EVENTSUB_SECRET_KEY).await;
    if secret.is_empty() {
        return Err(reject(503, "TWITCH_EVENTSUB_SECRET not configured"));
    }
    let message_id = header(req, "twitch-eventsub-message-id").unwrap_or_default();
    let timestamp = header(req, "twitch-eventsub-message-timestamp").unwrap_or_default();
    let signature = header(req, "twitch-eventsub-message-signature").unwrap_or_default();
    let message_type = header(req, "twitch-eventsub-message-type").unwrap_or_default();
    if message_id.is_empty()
        || timestamp.is_empty()
        || signature.is_empty()
        || message_type.is_empty()
    {
        return Err(reject(401, "Missing Twitch EventSub headers"));
    }
    let raw = match raw_request_body(req).await {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(error = %e, "twitch: refusing delivery without its raw body");
            return Err(reject(
                400,
                "Raw request body unavailable for signature verification",
            ));
        }
    };
    if let Err(e) = verify_eventsub_signature(&secret, message_id, timestamp, &raw, signature) {
        tracing::warn!(error = %e, "twitch eventsub signature rejected");
        return Err(reject(401, "Invalid Twitch EventSub signature"));
    }
    serde_json::from_slice(&raw).map_err(|_| reject(400, "Body is not valid JSON"))
}

async fn send_message(
    iii: &dyn TriggerBus,
    client: &reqwest::Client,
    broadcaster_id: &str,
    text: &str,
) -> Result<(), Error> {
    let token = get_secret(iii, "TWITCH_TOKEN").await;
    if token.is_empty() {
        return Err(Error::Handler("TWITCH_TOKEN not configured".into()));
    }
    let client_id = get_secret(iii, "TWITCH_CLIENT_ID").await;
    if client_id.is_empty() {
        return Err(Error::Handler("TWITCH_CLIENT_ID not configured".into()));
    }
    let bot_id = get_secret(iii, "TWITCH_BOT_USER_ID").await;
    if bot_id.is_empty() {
        return Err(Error::Handler("TWITCH_BOT_USER_ID not configured".into()));
    }
    for chunk in split_message(text, MAX_MESSAGE_LEN) {
        let url = format!("{TWITCH_API}/chat/messages");
        let res = client
            .post(&url)
            .bearer_auth(&token)
            .header("Client-Id", &client_id)
            .header("Content-Type", "application/json")
            .json(&json!({
                "broadcaster_id": broadcaster_id,
                "sender_id": bot_id,
                "message": chunk,
            }))
            .send()
            .await
            .map_err(|e| Error::Handler(format!("Twitch send error: {e}")))?;
        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            return Err(Error::Handler(format!(
                "Twitch send failed ({status}): {}",
                body.chars().take(300).collect::<String>()
            )));
        }
    }
    Ok(())
}

async fn webhook_handler(
    iii: &dyn TriggerBus,
    client: &reqwest::Client,
    input: Value,
) -> Result<Value, Error> {
    let body = match authenticate(iii, &input).await {
        Ok(body) => body,
        Err(response) => return Ok(response),
    };
    let message_type = header(&input, "twitch-eventsub-message-type").unwrap_or_default();

    match message_type {
        "webhook_callback_verification" => {
            let Some(challenge) = body.get("challenge").and_then(Value::as_str) else {
                return Ok(reject(400, "Missing EventSub challenge"));
            };
            return Ok(json!({
                "status_code": 200,
                "headers": { "content-type": "text/plain" },
                "body": challenge,
            }));
        }
        "revocation" => {
            return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
        }
        "notification" => {}
        _ => return Ok(json!({ "status_code": 200, "body": { "ok": true } })),
    }

    let event = body.get("event").cloned().unwrap_or_else(|| json!({}));
    let text = event
        .get("message")
        .and_then(|m| m.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();

    if text.is_empty() {
        return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
    }

    let channel_id = event
        .get("broadcaster_user_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let user_id = event
        .get("user_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let agent_id = resolve_agent(iii, "twitch", &channel_id).await;

    let chat = iii
        .trigger(TriggerRequest {
            function_id: "agent::chat".to_string(),
            payload: json!({
                "agentId": agent_id,
                "message": text,
                "sessionId": format!("twitch:{channel_id}"),
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(format!("agent::chat failed: {e}")))?;

    let reply = chat
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if !reply.is_empty()
        && !channel_id.is_empty()
        && let Err(e) = send_message(iii, client, &channel_id, &reply).await
    {
        tracing::error!(error = %e, "failed to post Twitch reply");
    }

    let _ = iii
        .trigger(TriggerRequest {
            function_id: "security::audit".to_string(),
            payload: json!({
                "type": "channel_message",
                "agentId": agent_id,
                "detail": {
                    "channel": "twitch",
                    "channelId": channel_id,
                    "userId": user_id,
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

    let iii_clone = iii.clone();
    let client_clone = client.clone();
    iii.register_function(
        "channel::twitch::webhook",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let client = client_clone.clone();
            async move { webhook_handler(&iii, &client, input).await }
        })
        .description("Handle Twitch EventSub webhook"),
    );

    if startup_secret(&iii, TWITCH_EVENTSUB_SECRET_KEY)
        .await
        .is_some()
    {
        agentos_http_adapter::register_http_trigger(
            &iii,
            "channel::twitch::webhook".to_string(),
            json!({ "http_method": "POST", "api_path": "webhook/twitch" }),
            None,
        )?;
        tracing::info!("twitch webhook route registered (EventSub signature verified)");
    } else {
        tracing::error!(
            "TWITCH_EVENTSUB_SECRET is not configured: POST /webhook/twitch is NOT registered"
        );
    }

    tracing::info!("channel-twitch worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_http_adapter::fake::FakeBus;

    const DELIVERY: &str = r#"{"subscription":{"type":"channel.chat.message"},"event":{"broadcaster_user_id":"b1","user_id":"u1","message":{"text":"hello"}}}"#;
    const SECRET: &str = "twitch-eventsub-test-secret";
    const MESSAGE_ID: &str = "msg-123";
    const TIMESTAMP: &str = "2026-09-03T12:34:56Z";
    const SIGNATURE: &str =
        "sha256=8a40e300e18578033ff7405c9caa87fe60a505e974359a57b7b28fd3dae4808d";

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(MESSAGE_ID.as_bytes());
        mac.update(TIMESTAMP.as_bytes());
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    fn request(raw: Option<&str>, signature: Option<&str>, message_type: &str) -> Value {
        let mut headers = json!({
            "twitch-eventsub-message-id": MESSAGE_ID,
            "twitch-eventsub-message-timestamp": TIMESTAMP,
            "twitch-eventsub-message-type": message_type,
        });
        if let Some(signature) = signature {
            headers["twitch-eventsub-message-signature"] = json!(signature);
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

    fn bus_with_secret(secret: &str) -> FakeBus {
        let bus = FakeBus::new();
        let secret = secret.to_string();
        bus.on("vault::get", move |payload| {
            let key = payload["key"].as_str().unwrap_or_default();
            Ok(json!({ "value": if key == TWITCH_EVENTSUB_SECRET_KEY { secret.clone() } else { String::new() } }))
        });
        bus.on_value("state::get", json!({ "agentId": "default" }));
        bus.on_value("agent::chat", json!({ "content": "" }));
        bus.on_value("security::audit", json!({}));
        bus
    }

    #[test]
    fn known_good_signature_verifies() {
        // Computed independently with Python's hmac/hashlib over
        // MESSAGE_ID || TIMESTAMP || DELIVERY, keyed by SECRET.
        assert_eq!(sign(SECRET, DELIVERY.as_bytes()), SIGNATURE);
        assert!(
            verify_eventsub_signature(
                SECRET,
                MESSAGE_ID,
                TIMESTAMP,
                DELIVERY.as_bytes(),
                SIGNATURE
            )
            .is_ok()
        );
    }

    #[test]
    fn tampered_body_fails_signature_verification() {
        let tampered = DELIVERY.replace("hello", "forged");
        assert!(
            verify_eventsub_signature(
                SECRET,
                MESSAGE_ID,
                TIMESTAMP,
                tampered.as_bytes(),
                SIGNATURE
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn valid_notification_reaches_agent_but_forgery_does_not() {
        let bus = bus_with_secret(SECRET);
        let valid = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            request(Some(DELIVERY), Some(SIGNATURE), "notification"),
        )
        .await
        .unwrap();
        assert_eq!(valid["status_code"], 200);
        assert_eq!(bus.call_count("agent::chat"), 1);

        let tampered = DELIVERY.replace("hello", "forged");
        let forged = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            request(Some(&tampered), Some(SIGNATURE), "notification"),
        )
        .await
        .unwrap();
        assert_eq!(forged["status_code"], 401);
        assert_eq!(bus.call_count("agent::chat"), 1);
    }

    #[tokio::test]
    async fn missing_signature_header_is_rejected() {
        let bus = bus_with_secret(SECRET);
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            request(Some(DELIVERY), None, "notification"),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 401);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn missing_raw_body_is_rejected() {
        let bus = bus_with_secret(SECRET);
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            request(None, Some(SIGNATURE), "notification"),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 400);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn missing_secret_refuses_delivery_and_startup() {
        let bus = bus_with_secret("");
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            request(Some(DELIVERY), Some(SIGNATURE), "notification"),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 503);
        assert_eq!(bus.call_count("agent::chat"), 0);
        assert_eq!(startup_secret(&bus, TWITCH_EVENTSUB_SECRET_KEY).await, None);
    }

    #[tokio::test]
    async fn verified_challenge_is_text_plain_and_message_type_driven() {
        let challenge = r#"{"challenge":"challenge-token","subscription":{}}"#;
        let signature = sign(SECRET, challenge.as_bytes());
        let bus = bus_with_secret(SECRET);
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            request(
                Some(challenge),
                Some(&signature),
                "webhook_callback_verification",
            ),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 200);
        assert_eq!(response["body"], "challenge-token");
        assert_eq!(response["headers"]["content-type"], "text/plain");

        let revocation = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            request(Some(challenge), Some(&signature), "revocation"),
        )
        .await
        .unwrap();
        assert_eq!(revocation["body"], json!({ "ok": true }));
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[test]
    fn split_short_returns_single_chunk() {
        let chunks = split_message("hello", 500);
        assert_eq!(chunks, vec!["hello".to_string()]);
    }

    #[test]
    fn split_long_chunks_at_limit() {
        let text = "x".repeat(1200);
        let chunks = split_message(&text, 500);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.chars().count() <= 500));
    }

    #[test]
    fn split_handles_multibyte_chars() {
        let text: String = "🦀".repeat(10);
        let chunks = split_message(&text, 3);
        let joined: String = chunks.concat();
        assert_eq!(joined, text);
    }
}
