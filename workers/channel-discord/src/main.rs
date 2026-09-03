use agentos_http_adapter::TriggerBus;
use ed25519_dalek::{Signature, VerifyingKey};
use iii_sdk::channels::{ChannelReader, StreamChannelRef};
use iii_sdk::errors::Error;
use iii_sdk::protocol::TriggerAction;
use iii_sdk::{RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};
use std::time::Duration;

const DISCORD_API: &str = "https://discord.com/api/v10";
const DISCORD_MAX_LEN: usize = 2000;

/// Name of the value that authenticates Discord deliveries: the application's
/// Ed25519 public key (64 hex characters) shown on the Developer Portal. Every
/// interaction and webhook event Discord POSTs is signed with the matching
/// private key over `timestamp + body`
/// (https://discord.com/developers/docs/interactions/overview#setting-up-an-endpoint-validating-security-request-headers).
/// Distinct from `DISCORD_BOT_TOKEN`, which authenticates OUR calls to Discord.
const PUBLIC_KEY_KEY: &str = "DISCORD_PUBLIC_KEY";

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
        let mut split_idx = remaining
            .char_indices()
            .take(max_len + 1)
            .last()
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());
        if let Some(nl) = remaining[..split_idx].rfind('\n')
            && nl >= max_len / 2
        {
            split_idx = nl;
        }
        chunks.push(remaining[..split_idx].to_string());
        remaining = &remaining[split_idx..];
    }
    chunks
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

/// Verify Discord's request signature: Ed25519 over `timestamp || raw_body`
/// with the application public key, both hex-encoded in the request headers.
/// `verify_strict` rejects non-canonical signatures and weak keys on top of the
/// plain check; Ed25519 verification is constant-time in the signature.
fn verify_discord_signature(
    public_key_hex: &str,
    timestamp: &str,
    raw_body: &[u8],
    signature_hex: &str,
) -> Result<(), String> {
    if public_key_hex.is_empty() {
        return Err("DISCORD_PUBLIC_KEY not configured".into());
    }
    let key: [u8; 32] = hex::decode(public_key_hex.trim())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| "DISCORD_PUBLIC_KEY is not a 32-byte hex key".to_string())?;
    let verifying_key = VerifyingKey::from_bytes(&key)
        .map_err(|_| "DISCORD_PUBLIC_KEY is not a valid Ed25519 key".to_string())?;
    let signature: [u8; 64] = hex::decode(signature_hex.trim())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| "X-Signature-Ed25519 is not a 64-byte hex signature".to_string())?;
    let signature = Signature::from_bytes(&signature);
    let mut message = Vec::with_capacity(timestamp.len() + raw_body.len());
    message.extend_from_slice(timestamp.as_bytes());
    message.extend_from_slice(raw_body);
    verifying_key
        .verify_strict(&message, &signature)
        .map_err(|_| "Invalid Discord signature".to_string())
}

/// Authenticate one delivery and return the parsed body it was signed over.
///
/// Order matters: the key, both headers and the raw bytes are all checked
/// BEFORE any JSON is parsed, and the body handed back is parsed from the
/// verified bytes — never from the engine's pre-parsed `body`, which is not
/// what Discord signed. Every failure is a refusal (`Err(response)`).
async fn authenticate(iii: &dyn TriggerBus, req: &Value) -> Result<Value, Value> {
    let public_key = get_secret(iii, PUBLIC_KEY_KEY).await;
    if public_key.is_empty() {
        return Err(reject(503, "DISCORD_PUBLIC_KEY not configured"));
    }
    let signature = header(req, "x-signature-ed25519").unwrap_or_default();
    let timestamp = header(req, "x-signature-timestamp").unwrap_or_default();
    if signature.is_empty() || timestamp.is_empty() {
        return Err(reject(401, "Missing Discord signature headers"));
    }
    let raw = match raw_request_body(req).await {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(error = %e, "discord: refusing delivery without its raw body");
            return Err(reject(
                400,
                "Raw request body unavailable for signature verification",
            ));
        }
    };
    if let Err(e) = verify_discord_signature(&public_key, timestamp, &raw, signature) {
        tracing::warn!(error = %e, "discord signature rejected");
        return Err(reject(401, "Invalid Discord signature"));
    }
    serde_json::from_slice(&raw).map_err(|_| reject(400, "Body is not valid JSON"))
}

/// Discord's endpoint validation pings, answered before anything else.
///
/// The Interactions endpoint sends `type: 1` (PING) and expects `200 {"type":1}`
/// (PONG); a Webhook Events URL sends `type: 0` and expects `204` with an empty
/// body. Both share the signature scheme, so one route can serve either. A
/// webhook EVENT is also `type: 1`, but it wraps an `event` object and has no
/// interaction `token`; an interaction PING is the other way round.
fn ping_response(body: &Value) -> Option<Value> {
    match body.get("type").and_then(Value::as_u64) {
        Some(0) => Some(json!({
            "status_code": 204,
            "headers": { "content-type": "text/plain" },
            "body": "",
        })),
        Some(1) if body.get("token").is_some() || body.get("event").is_none() => Some(json!({
            "status_code": 200,
            "headers": { "content-type": "application/json" },
            "body": { "type": 1 },
        })),
        _ => None,
    }
}

async fn send_message(
    iii: &dyn TriggerBus,
    client: &reqwest::Client,
    channel_id: &str,
    content: &str,
) -> Result<(), Error> {
    let bot_token = get_secret(iii, "DISCORD_BOT_TOKEN").await;
    if bot_token.is_empty() {
        return Err(Error::Handler("DISCORD_BOT_TOKEN not configured".into()));
    }
    for chunk in split_message(content, DISCORD_MAX_LEN) {
        let url = format!("{DISCORD_API}/channels/{channel_id}/messages");
        let res = client
            .post(&url)
            .header("Authorization", format!("Bot {bot_token}"))
            .header("Content-Type", "application/json")
            // Suppress all mentions so prompt-injected `@everyone`/role/user
            // pings in model output never reach Discord.
            .json(&json!({
                "content": chunk,
                "allowed_mentions": { "parse": [] },
            }))
            .send()
            .await
            .map_err(|e| Error::Handler(e.to_string()))?;
        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            return Err(Error::Handler(format!(
                "Discord send failed ({status}): {}",
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
    let event = match authenticate(iii, &input).await {
        Ok(body) => body,
        Err(response) => return Ok(response),
    };
    if let Some(pong) = ping_response(&event) {
        return Ok(pong);
    }

    if event.get("t").and_then(|t| t.as_str()) == Some("MESSAGE_CREATE") {
        let msg = event.get("d").cloned().unwrap_or_else(|| json!({}));
        let is_bot = msg
            .get("author")
            .and_then(|a| a.get("bot"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        if is_bot {
            return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
        }

        let channel_id = msg
            .get("channel_id")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let content = msg
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        // Skip MESSAGE_CREATE events with no text (attachments, stickers,
        // system events) to avoid generating an LLM reply to nothing.
        if content.trim().is_empty() {
            return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
        }

        let agent_id = resolve_agent(iii, "discord", &channel_id).await;

        let chat_response = iii
            .trigger(TriggerRequest {
                function_id: "agent::chat".to_string(),
                payload: json!({
                    "agentId": agent_id,
                    "message": content,
                    "sessionId": format!("discord:{channel_id}"),
                }),
                action: None,
                timeout_ms: None,
            })
            .await
            .map_err(|e| Error::Handler(e.to_string()))?;

        let reply = chat_response
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        if !channel_id.is_empty() && !reply.is_empty() {
            send_message(iii, client, &channel_id, &reply).await?;
        }

        // Emit channel_message audit event so Discord traffic is captured by
        // the same monitoring pipeline as the other channel adapters.
        let _ = iii
            .trigger(TriggerRequest {
                function_id: "security::audit".to_string(),
                payload: json!({
                    "type": "channel_message",
                    "agentId": agent_id,
                    "detail": { "channel": "discord", "channelId": channel_id },
                }),
                action: Some(TriggerAction::Void),
                timeout_ms: None,
            })
            .await;
    }

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
        "channel::discord::webhook",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let client = client_clone.clone();
            async move { webhook_handler(&iii, &client, input).await }
        })
        .description("Handle Discord interaction/webhook"),
    );

    // The route is registered only when the application public key that
    // verifies Discord's signature exists. Without it every delivery would be
    // refused anyway, and an unverifiable route is not worth exposing. The
    // handler re-reads the key per request, so a rotation needs no restart.
    if startup_secret(&iii, PUBLIC_KEY_KEY).await.is_some() {
        agentos_http_adapter::register_http_trigger(
            &iii,
            "channel::discord::webhook".to_string(),
            json!({ "http_method": "POST", "api_path": "webhook/discord" }),
            None,
        )?;
        tracing::info!("discord webhook route registered (Ed25519 signature verified)");
    } else {
        tracing::error!(
            "{PUBLIC_KEY_KEY} is not configured: POST /webhook/discord is NOT registered. \
             Set it to the application's public key from the Developer Portal and restart."
        );
    }

    tracing::info!("channel-discord worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_http_adapter::fake::FakeBus;
    use ed25519_dalek::{Signer, SigningKey};

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

    /// The PING Discord sends when an Interactions Endpoint URL is saved
    /// (https://discord.com/developers/docs/interactions/overview#setting-up-an-endpoint-acknowledging-ping-requests).
    const INTERACTION_PING: &str = r#"{"app_permissions":"0","application_id":"1234567890","entitlements":[],"id":"9876543210","token":"aW50ZXJhY3Rpb246OTg3NjU0MzIxMDp0b2tlbg","type":1,"user":{"avatar":null,"discriminator":"0","global_name":null,"id":"1111","public_flags":0,"username":"discord"},"version":1}"#;
    /// The PING Discord sends when a Webhook Events URL is saved
    /// (https://discord.com/developers/docs/events/webhook-events#payload-structure).
    const WEBHOOK_EVENTS_PING: &str = r#"{"version":1,"application_id":"1234567890","type":0}"#;
    const MESSAGE_CREATE: &str = r#"{"t":"MESSAGE_CREATE","d":{"channel_id":"c1","content":"hello","author":{"id":"u1","bot":false}}}"#;
    const TIMESTAMP: &str = "1700000000";

    fn keypair() -> (SigningKey, String) {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let public_hex = hex::encode(signing.verifying_key().to_bytes());
        (signing, public_hex)
    }

    fn sign(signing: &SigningKey, timestamp: &str, body: &str) -> String {
        let mut message = timestamp.as_bytes().to_vec();
        message.extend_from_slice(body.as_bytes());
        hex::encode(signing.sign(&message).to_bytes())
    }

    fn request(raw: &str, signature: Option<&str>, timestamp: Option<&str>) -> Value {
        let mut headers = json!({ "content-type": "application/json" });
        if let Some(signature) = signature {
            headers["x-signature-ed25519"] = json!(signature);
        }
        if let Some(timestamp) = timestamp {
            headers["x-signature-timestamp"] = json!(timestamp);
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

    fn bus_with_key(public_key_hex: &str) -> FakeBus {
        let bus = FakeBus::new();
        let key = public_key_hex.to_string();
        bus.on("vault::get", move |payload| {
            let name = payload["key"].as_str().unwrap_or_default();
            Ok(json!({ "value": if name == PUBLIC_KEY_KEY { key.clone() } else { String::new() } }))
        });
        bus.on_value("state::get", json!({ "agentId": "default" }));
        bus.on_value("agent::chat", json!({ "content": "" }));
        bus.on_value("security::audit", json!({}));
        bus
    }

    #[test]
    fn known_good_signature_verifies() {
        let (signing, public_hex) = keypair();
        // Interaction PING signed the way Discord does it: Ed25519 over
        // timestamp || body. Key derived from a fixed seed so the vector is
        // stable; the hex signature below was produced by ed25519-dalek from it.
        assert_eq!(
            public_hex,
            "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c"
        );
        let signature = sign(&signing, TIMESTAMP, INTERACTION_PING);
        assert_eq!(
            signature,
            "e27a0e164334c5ad3fd1d581e9508525bbba00728d52bd75c66ced9dd56a8b6e6ea884865af3f033e1ce4d27b93b594f93f21c252016f95e3aca85d718647f09"
        );
        assert!(
            verify_discord_signature(
                &public_hex,
                TIMESTAMP,
                INTERACTION_PING.as_bytes(),
                &signature
            )
            .is_ok()
        );
    }

    #[test]
    fn rfc_8032_published_vectors_verify_through_the_discord_path() {
        // Discord publishes no fixed (key, timestamp, body, signature) vector:
        // the interactions guide shows code with `APPLICATION_PUBLIC_KEY` as a
        // placeholder, and discord-interactions-js generates a key pair per
        // test run. The scheme is Ed25519 over `timestamp || body`, so the
        // primitive — hex key and signature decoding, `verify_strict` — is
        // pinned here to RFC 8032 §7.1's published test vectors instead
        // (https://www.rfc-editor.org/rfc/rfc8032#section-7.1), split across
        // the two inputs the way the handler concatenates them.
        //
        // TEST 2: message 0x72 ("r").
        let public_key = "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c";
        let signature = "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00";
        assert!(verify_discord_signature(public_key, "r", b"", signature).is_ok());
        assert!(verify_discord_signature(public_key, "", b"r", signature).is_ok());
        assert!(verify_discord_signature(public_key, "r", b"r", signature).is_err());
        assert!(verify_discord_signature(public_key, "", b"", signature).is_err());
        // TEST 3: message 0xaf82 (not UTF-8, so it can only be the body).
        let public_key = "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025";
        let signature = "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a";
        assert!(verify_discord_signature(public_key, "", &[0xaf, 0x82], signature).is_ok());
        assert!(verify_discord_signature(public_key, "", &[0xaf, 0x83], signature).is_err());
        // TEST 2's key does not verify TEST 3's signature.
        assert!(
            verify_discord_signature(
                "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
                "",
                &[0xaf, 0x82],
                signature
            )
            .is_err()
        );
    }

    #[test]
    fn tampered_body_or_timestamp_fails() {
        let (signing, public_hex) = keypair();
        let signature = sign(&signing, TIMESTAMP, INTERACTION_PING);
        let tampered = INTERACTION_PING.replace("\"type\":1", "\"type\":2");
        assert!(
            verify_discord_signature(&public_hex, TIMESTAMP, tampered.as_bytes(), &signature)
                .is_err()
        );
        assert!(
            verify_discord_signature(
                &public_hex,
                "1700000001",
                INTERACTION_PING.as_bytes(),
                &signature
            )
            .is_err()
        );
    }

    #[test]
    fn wrong_key_garbage_and_malformed_signatures_fail() {
        let (signing, public_hex) = keypair();
        let signature = sign(&signing, TIMESTAMP, INTERACTION_PING);
        let other_hex = hex::encode(
            SigningKey::from_bytes(&[8u8; 32])
                .verifying_key()
                .to_bytes(),
        );
        let body = INTERACTION_PING.as_bytes();
        assert!(verify_discord_signature(&other_hex, TIMESTAMP, body, &signature).is_err());
        assert!(verify_discord_signature(&public_hex, TIMESTAMP, body, "deadbeef").is_err());
        assert!(verify_discord_signature(&public_hex, TIMESTAMP, body, "zz").is_err());
        assert!(verify_discord_signature(&public_hex, TIMESTAMP, body, "").is_err());
        assert!(verify_discord_signature("", TIMESTAMP, body, &signature).is_err());
        assert!(verify_discord_signature("abcd", TIMESTAMP, body, &signature).is_err());
    }

    #[tokio::test]
    async fn interaction_ping_is_answered_with_pong_after_verification() {
        let (signing, public_hex) = keypair();
        let bus = bus_with_key(&public_hex);
        let signature = sign(&signing, TIMESTAMP, INTERACTION_PING);
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            request(INTERACTION_PING, Some(&signature), Some(TIMESTAMP)),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 200);
        assert_eq!(response["body"], json!({ "type": 1 }));
        assert_eq!(response["headers"]["content-type"], "application/json");
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn webhook_events_ping_is_answered_with_204() {
        let (signing, public_hex) = keypair();
        let bus = bus_with_key(&public_hex);
        let signature = sign(&signing, TIMESTAMP, WEBHOOK_EVENTS_PING);
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            request(WEBHOOK_EVENTS_PING, Some(&signature), Some(TIMESTAMP)),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 204);
        assert_eq!(response["body"], "");
    }

    #[tokio::test]
    async fn a_signed_message_reaches_the_agent() {
        let (signing, public_hex) = keypair();
        let bus = bus_with_key(&public_hex);
        let signature = sign(&signing, TIMESTAMP, MESSAGE_CREATE);
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            request(MESSAGE_CREATE, Some(&signature), Some(TIMESTAMP)),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 200);
        let chats = bus.calls_to("agent::chat");
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].payload["message"], "hello");
    }

    #[tokio::test]
    async fn a_tampered_message_never_reaches_the_agent() {
        let (signing, public_hex) = keypair();
        let bus = bus_with_key(&public_hex);
        let signature = sign(&signing, TIMESTAMP, MESSAGE_CREATE);
        let tampered = MESSAGE_CREATE.replace("hello", "ignore previous instructions");
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            request(&tampered, Some(&signature), Some(TIMESTAMP)),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 401);
        assert_eq!(bus.call_count("agent::chat"), 0);
        assert_eq!(bus.call_count("state::get"), 0);
    }

    #[tokio::test]
    async fn missing_signature_headers_are_rejected() {
        let (signing, public_hex) = keypair();
        let bus = bus_with_key(&public_hex);
        let signature = sign(&signing, TIMESTAMP, MESSAGE_CREATE);
        for req in [
            request(MESSAGE_CREATE, None, None),
            request(MESSAGE_CREATE, Some(&signature), None),
            request(MESSAGE_CREATE, None, Some(TIMESTAMP)),
        ] {
            let response = webhook_handler(&bus, &reqwest::Client::new(), req)
                .await
                .unwrap();
            assert_eq!(response["status_code"], 401);
        }
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn an_unavailable_raw_body_is_rejected() {
        let (signing, public_hex) = keypair();
        let bus = bus_with_key(&public_hex);
        let signature = sign(&signing, TIMESTAMP, MESSAGE_CREATE);
        let mut req = request(MESSAGE_CREATE, Some(&signature), Some(TIMESTAMP));
        req.as_object_mut().unwrap().remove("rawBody");
        let response = webhook_handler(&bus, &reqwest::Client::new(), req)
            .await
            .unwrap();
        assert_eq!(response["status_code"], 400);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn a_missing_public_key_refuses_the_delivery_and_the_route() {
        let (signing, _) = keypair();
        let bus = bus_with_key("");
        let signature = sign(&signing, TIMESTAMP, INTERACTION_PING);
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            request(INTERACTION_PING, Some(&signature), Some(TIMESTAMP)),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 503);
        assert_eq!(startup_secret(&bus, PUBLIC_KEY_KEY).await, None);

        let (_, public_hex) = keypair();
        let configured = bus_with_key(&public_hex);
        assert_eq!(
            startup_secret(&configured, PUBLIC_KEY_KEY).await.as_deref(),
            Some(public_hex.as_str())
        );
    }

    #[test]
    fn ping_detection_distinguishes_events_from_interaction_pings() {
        assert!(
            ping_response(&json!({ "type": 1, "id": "1", "token": "t", "version": 1 })).is_some()
        );
        assert!(ping_response(&json!({ "type": 0, "version": 1 })).is_some());
        assert!(
            ping_response(
                &json!({ "type": 1, "version": 1, "event": { "type": "APPLICATION_AUTHORIZED" } })
            )
            .is_none()
        );
        assert!(ping_response(&json!({ "type": 2 })).is_none());
        assert!(ping_response(&json!({ "t": "MESSAGE_CREATE" })).is_none());
    }

    #[test]
    fn split_short_message_returns_single_chunk() {
        let chunks = split_message("hello", 2000);
        assert_eq!(chunks, vec!["hello".to_string()]);
    }

    #[test]
    fn split_long_message_chunks_at_limit() {
        let text = "a".repeat(2500);
        let chunks = split_message(&text, 2000);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().all(|c| c.chars().count() <= 2000));
    }

    #[test]
    fn split_prefers_newline_break() {
        let text = format!("{}\n{}", "a".repeat(1500), "b".repeat(800));
        let chunks = split_message(&text, 2000);
        assert!(chunks.len() >= 2);
        assert!(chunks[0].ends_with(&"a".repeat(1500)));
    }
    #[tokio::test]
    async fn a_caller_supplied_raw_body_does_not_replace_the_engine_channel() {
        // A validly signed `rawBody` next to a `request_body` ref: the channel
        // is what gets read. Here the ref is unusable, so the request is
        // refused instead of being verified against the caller's bytes.
        let (signing, public_hex) = keypair();
        let bus = bus_with_key(&public_hex);
        let signature = sign(&signing, TIMESTAMP, MESSAGE_CREATE);
        let mut req = request(MESSAGE_CREATE, Some(&signature), Some(TIMESTAMP));
        req["request_body"] = json!("not-a-channel-ref");
        let response = webhook_handler(&bus, &reqwest::Client::new(), req)
            .await
            .unwrap();
        assert_eq!(response["status_code"], 400);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }
}
