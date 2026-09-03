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

const LINKEDIN_API: &str = "https://api.linkedin.com/v2";
const MAX_MESSAGE_LEN: usize = 4096;
const MESSAGE_EVENT_KEY: &str = "com.linkedin.voyager.messaging.event.MessageEvent";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const NOTIFICATION_DEDUPE_TTL_SECS: u64 = 24 * 60 * 60;
const CLIENT_SECRET_KEY: &str = "LINKEDIN_CLIENT_SECRET";

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

/// Compute hex-encoded HMAC-SHA256 over `body` using `secret` as the key.
fn hmac_sha256_hex(secret: &str, body: &[u8]) -> Result<String, String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|e| format!("HMAC init error: {e}"))?;
    mac.update(body);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// Verify LinkedIn's `X-LI-Signature`: lowercase hex HMAC-SHA256 of
/// `b"hmacsha256=" || raw_body`, keyed by the client secret. The header itself
/// contains only the digest
/// (https://learn.microsoft.com/en-us/linkedin/shared/api-guide/webhook-validation).
/// The comparison is `Mac::verify_slice`, which is constant-time.
fn verify_linkedin_signature(secret: &str, raw_body: &[u8], signature: &str) -> Result<(), String> {
    if secret.is_empty() {
        return Err("LINKEDIN_CLIENT_SECRET not configured".into());
    }
    let provided = hex::decode(signature.trim())
        .map_err(|_| "X-LI-Signature is not lowercase hex".to_string())?;
    if signature.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err("X-LI-Signature is not lowercase hex".into());
    }
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|e| format!("HMAC init error: {e}"))?;
    mac.update(b"hmacsha256=");
    mac.update(raw_body);
    mac.verify_slice(&provided)
        .map_err(|_| "Invalid LinkedIn signature".to_string())
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

/// Authenticate a POST delivery and return the parsed body it was signed over.
///
/// Order matters: the secret, signature header and raw bytes are all checked
/// BEFORE any JSON is parsed. The body handed back is parsed from the verified
/// bytes — never from the engine's pre-parsed `body`. Every failure is a refusal.
async fn authenticate(iii: &dyn TriggerBus, req: &Value) -> Result<Value, Value> {
    let secret = get_secret(iii, CLIENT_SECRET_KEY).await;
    if secret.is_empty() {
        return Err(reject(503, "LINKEDIN_CLIENT_SECRET not configured"));
    }
    let Some(signature) = header(req, "x-li-signature").filter(|s| !s.is_empty()) else {
        return Err(reject(401, "Missing X-LI-Signature header"));
    };
    let raw = match raw_request_body(req).await {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(error = %e, "linkedin: refusing delivery without its raw body");
            return Err(reject(
                400,
                "Raw request body unavailable for signature verification",
            ));
        }
    };
    if let Err(e) = verify_linkedin_signature(&secret, &raw, signature) {
        tracing::warn!(error = %e, "linkedin signature rejected");
        return Err(reject(401, "Invalid LinkedIn signature"));
    }
    serde_json::from_slice(&raw).map_err(|_| reject(400, "Body is not valid JSON"))
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

async fn send_message(
    iii: &dyn TriggerBus,
    client: &reqwest::Client,
    thread_id: &str,
    text: &str,
) -> Result<(), Error> {
    let token = get_secret(iii, "LINKEDIN_TOKEN").await;
    if token.is_empty() {
        return Err(Error::Handler("LINKEDIN_TOKEN not configured".into()));
    }
    for chunk in split_message(text, MAX_MESSAGE_LEN) {
        let url = format!("{LINKEDIN_API}/messages");
        let res = client
            .post(&url)
            .bearer_auth(&token)
            .header("Content-Type", "application/json")
            .header("X-Restli-Protocol-Version", "2.0.0")
            .json(&json!({
                "recipients": [],
                "threadId": thread_id,
                "body": chunk,
            }))
            .send()
            .await
            .map_err(|e| Error::Handler(format!("LinkedIn send error: {e}")))?;
        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            return Err(Error::Handler(format!(
                "LinkedIn send failed ({status}): {}",
                body.chars().take(300).collect::<String>()
            )));
        }
    }
    Ok(())
}

fn extract_message_text(msg_event: &Value) -> Option<String> {
    msg_event
        .get("messageBody")
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .map(String::from)
        .or_else(|| {
            msg_event
                .get("attributedBody")
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str())
                .map(String::from)
        })
}

/// Check whether `notification_id` was already processed via the `state::*`
/// dedup scope. Returns true if newly recorded, false if duplicate.
async fn record_notification_id(iii: &dyn TriggerBus, notification_id: &str) -> bool {
    if notification_id.is_empty() {
        return true;
    }
    let key = format!("linkedin:{notification_id}");
    let existing = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({ "scope": "channel_dedupe", "key": key }),
            action: None,
            timeout_ms: None,
        })
        .await;
    if let Ok(v) = existing
        && v.get("seen").and_then(|s| s.as_bool()) == Some(true)
    {
        return false;
    }
    let _ = iii
        .trigger(TriggerRequest {
            function_id: "state::set".to_string(),
            payload: json!({
                "scope": "channel_dedupe",
                "key": key,
                "value": { "seen": true },
                "ttl": NOTIFICATION_DEDUPE_TTL_SECS,
            }),
            action: Some(TriggerAction::Void),
            timeout_ms: None,
        })
        .await;
    true
}

async fn process_element(
    iii: &dyn TriggerBus,
    client: &reqwest::Client,
    element: &Value,
) -> Result<(), Error> {
    let msg_event = element.get("event").and_then(|e| e.get(MESSAGE_EVENT_KEY));
    let Some(msg_event) = msg_event else {
        return Ok(());
    };
    let Some(text) = extract_message_text(msg_event) else {
        return Ok(());
    };
    if text.is_empty() {
        return Ok(());
    }
    let thread_id = element
        .get("entityUrn")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let sender_id = element
        .get("from")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let notification_id = element
        .get("notificationId")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !record_notification_id(iii, notification_id).await {
        tracing::info!(notification_id, "linkedin: skipping duplicate notification");
        return Ok(());
    }

    let agent_id = resolve_agent(iii, "linkedin", &thread_id).await;

    let chat = iii
        .trigger(TriggerRequest {
            function_id: "agent::chat".to_string(),
            payload: json!({
                "agentId": agent_id,
                "message": text,
                "sessionId": format!("linkedin:{thread_id}"),
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
        && !thread_id.is_empty()
        && let Err(e) = send_message(iii, client, &thread_id, &reply).await
    {
        tracing::error!(error = %e, "failed to post LinkedIn reply");
    }

    let _ = iii
        .trigger(TriggerRequest {
            function_id: "security::audit".to_string(),
            payload: json!({
                "type": "channel_message",
                "agentId": agent_id,
                "detail": {
                    "channel": "linkedin",
                    "threadId": thread_id,
                    "senderId": sender_id,
                },
            }),
            action: Some(TriggerAction::Void),
            timeout_ms: None,
        })
        .await;
    Ok(())
}

async fn webhook_handler(
    iii: &dyn TriggerBus,
    client: &reqwest::Client,
    input: Value,
) -> Result<Value, Error> {
    let method = input
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("POST")
        .to_uppercase();

    // LinkedIn's GET challenge is query-authenticated and has no signed body.
    if method == "GET" {
        let challenge = query_param(&input, "challengeCode").unwrap_or("");
        if challenge.is_empty() {
            return Ok(reject(400, "Missing challengeCode"));
        }
        let secret = get_secret(iii, CLIENT_SECRET_KEY).await;
        if secret.is_empty() {
            return Ok(reject(503, "LINKEDIN_CLIENT_SECRET not configured"));
        }
        let response = match hmac_sha256_hex(&secret, challenge.as_bytes()) {
            Ok(hex) => hex,
            Err(e) => {
                tracing::error!(error = %e, "linkedin: challenge HMAC failed");
                return Ok(reject(500, "Challenge HMAC failed"));
            }
        };
        return Ok(json!({
            "status_code": 200,
            "body": {
                "challengeCode": challenge,
                "challengeResponse": response,
            }
        }));
    }

    let body = match authenticate(iii, &input).await {
        Ok(body) => body,
        Err(response) => return Ok(response),
    };

    let elements: Vec<Value> = body
        .get("elements")
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default();

    if elements.is_empty() {
        return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
    }

    for element in &elements {
        if let Err(e) = process_element(iii, client, element).await {
            tracing::error!(error = %e, "failed to process LinkedIn element");
        }
    }

    Ok(json!({ "status_code": 200, "body": { "ok": true } }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let ws_url = engine_ws_url();
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());
    let client = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build()?;

    let iii_clone = iii.clone();
    let client_clone = client.clone();
    iii.register_function(
        "channel::linkedin::webhook",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let client = client_clone.clone();
            async move { webhook_handler(&iii, &client, input).await }
        })
        .description("Handle LinkedIn messaging webhook"),
    );

    if startup_secret(&iii, CLIENT_SECRET_KEY).await.is_some() {
        agentos_http_adapter::register_http_trigger(
            &iii,
            "channel::linkedin::webhook".to_string(),
            json!({ "http_method": "POST", "api_path": "webhook/linkedin" }),
            None,
        )?;
        agentos_http_adapter::register_http_trigger(
            &iii,
            "channel::linkedin::webhook".to_string(),
            json!({ "http_method": "GET", "api_path": "webhook/linkedin" }),
            None,
        )?;
        tracing::info!("linkedin webhook routes registered (X-LI-Signature verified)");
    } else {
        tracing::error!(
            "LINKEDIN_CLIENT_SECRET is not configured: LinkedIn webhook routes are NOT registered"
        );
    }

    tracing::info!("channel-linkedin worker started");
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

    const DELIVERY: &str = r#"{"elements":[{"notificationId":"n1","entityUrn":"urn:li:msg:thread1","from":"urn:li:person:u1","event":{"com.linkedin.voyager.messaging.event.MessageEvent":{"messageBody":{"text":"hello"}}}}]}"#;
    const SECRET: &str = "linkedin-client-test-secret";
    const SIGNATURE: &str = "122d08d3b6b55e1847e717bdde67b0219ae0f4f2d77fa07a623cdb9cdd1db04f";

    fn sign_post(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(b"hmacsha256=");
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    fn post_request(raw: Option<&str>, signature: Option<&str>) -> Value {
        let mut headers = json!({ "content-type": "application/json" });
        if let Some(signature) = signature {
            headers["x-li-signature"] = json!(signature);
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
            Ok(json!({ "value": if key == CLIENT_SECRET_KEY { secret.clone() } else { String::new() } }))
        });
        bus.on_value("state::get", json!({}));
        bus.on_value("state::set", json!({}));
        bus.on_value("agent::chat", json!({ "content": "" }));
        bus.on_value("security::audit", json!({}));
        bus
    }

    #[test]
    fn known_good_post_signature_verifies() {
        // Computed independently with Python's hmac/hashlib over
        // b"hmacsha256=" || DELIVERY, keyed by SECRET.
        assert_eq!(sign_post(SECRET, DELIVERY.as_bytes()), SIGNATURE);
        assert!(verify_linkedin_signature(SECRET, DELIVERY.as_bytes(), SIGNATURE).is_ok());
    }

    #[test]
    fn bare_body_or_prefixed_header_scheme_is_rejected() {
        let bare_body_digest = hmac_sha256_hex(SECRET, DELIVERY.as_bytes()).unwrap();
        assert!(verify_linkedin_signature(SECRET, DELIVERY.as_bytes(), &bare_body_digest).is_err());
        assert!(
            verify_linkedin_signature(
                SECRET,
                DELIVERY.as_bytes(),
                &format!("hmacsha256={SIGNATURE}")
            )
            .is_err()
        );
    }

    #[test]
    fn tampered_body_fails_signature_verification() {
        let tampered = DELIVERY.replace("hello", "forged");
        assert!(verify_linkedin_signature(SECRET, tampered.as_bytes(), SIGNATURE).is_err());
    }

    #[tokio::test]
    async fn valid_delivery_reaches_agent_but_forgery_does_not() {
        let bus = bus_with_secret(SECRET);
        let valid = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            post_request(Some(DELIVERY), Some(SIGNATURE)),
        )
        .await
        .unwrap();
        assert_eq!(valid["status_code"], 200);
        assert_eq!(bus.call_count("agent::chat"), 1);

        let tampered = DELIVERY.replace("hello", "forged");
        let forged = webhook_handler(
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
        let bus = bus_with_secret(SECRET);
        let response = webhook_handler(
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
        let bus = bus_with_secret(SECRET);
        let response = webhook_handler(
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
    async fn missing_secret_refuses_delivery_and_startup() {
        let bus = bus_with_secret("");
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            post_request(Some(DELIVERY), Some(SIGNATURE)),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 503);
        assert_eq!(bus.call_count("agent::chat"), 0);
        assert_eq!(startup_secret(&bus, CLIENT_SECRET_KEY).await, None);
    }

    #[tokio::test]
    async fn get_challenge_remains_json_hmac() {
        let bus = bus_with_secret(SECRET);
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            json!({
                "method": "GET",
                "query": { "challengeCode": "challenge-123" }
            }),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 200);
        assert_eq!(response["body"]["challengeCode"], "challenge-123");
        // Computed independently with Python's hmac/hashlib over challengeCode.
        assert_eq!(
            response["body"]["challengeResponse"],
            "6789db0b29ee94adaa98a816dc583d86e0eea6df1799517f702f40bd912b5020"
        );
    }

    #[test]
    fn split_short_text_returns_single_chunk() {
        let chunks = split_message("hello", 4096);
        assert_eq!(chunks, vec!["hello".to_string()]);
    }

    #[test]
    fn split_long_text_chunks_at_limit() {
        let text = "a".repeat(5000);
        let chunks = split_message(&text, 4096);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().all(|c| c.chars().count() <= 4096));
    }

    #[test]
    fn extracts_message_body_text() {
        let event = json!({ "messageBody": { "text": "hello" } });
        assert_eq!(extract_message_text(&event), Some("hello".to_string()));
    }

    #[test]
    fn extracts_attributed_body_fallback() {
        let event = json!({ "attributedBody": { "text": "fallback" } });
        assert_eq!(extract_message_text(&event), Some("fallback".to_string()));
    }

    #[test]
    fn returns_none_when_neither_field_present() {
        let event = json!({});
        assert_eq!(extract_message_text(&event), None);
    }
}
