use agentos_http_adapter::TriggerBus;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use iii_sdk::channels::{ChannelReader, StreamChannelRef};
use iii_sdk::errors::Error;
use iii_sdk::protocol::TriggerAction;
use iii_sdk::{RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type HmacSha256 = Hmac<Sha256>;

const TWITCH_API: &str = "https://api.twitch.tv/helix";
const MAX_MESSAGE_LEN: usize = 500;
const TWITCH_EVENTSUB_SECRET_KEY: &str = "TWITCH_EVENTSUB_SECRET";

/// EventSub's replay guidance (https://dev.twitch.tv/docs/eventsub/, "Guarding
/// against replay attacks"): "Make sure the value in the message_timestamp
/// field isn't older than 10 minutes" and "Make sure you haven't seen the ID in
/// the message_id field before". Both are signed, so neither can be adjusted
/// without failing the HMAC.
const MESSAGE_MAX_AGE: Duration = Duration::from_secs(10 * 60);
/// How long a processed message id is remembered: twice the timestamp window,
/// so an id whose signed timestamp sat at the future edge of the window is
/// still remembered for as long as that timestamp can be accepted.
const SEEN_MESSAGE_RETENTION: Duration = Duration::from_secs(2 * 10 * 60);
/// Hard cap on remembered ids, oldest evicted first. Well above what a busy
/// chat produces in twenty minutes; only memory is at stake past it.
const SEEN_MESSAGE_CAP: usize = 100_000;

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

/// Refuse a delivery whose signed `Twitch-Eventsub-Message-Timestamp` (RFC
/// 3339, UTC) is more than [`MESSAGE_MAX_AGE`] away from `now` in either
/// direction. Checked only after the signature, so the answer says nothing
/// about an unsigned request.
fn check_message_timestamp(timestamp: &str, now: DateTime<Utc>) -> Result<(), String> {
    let sent = DateTime::parse_from_rfc3339(timestamp.trim())
        .map_err(|_| "Twitch-Eventsub-Message-Timestamp is not RFC 3339".to_string())?;
    let offset_secs = now
        .signed_duration_since(sent.with_timezone(&Utc))
        .num_seconds();
    if offset_secs.unsigned_abs() > MESSAGE_MAX_AGE.as_secs() {
        return Err(format!(
            "Twitch-Eventsub-Message-Timestamp is {offset_secs}s from now; the window is \
             {}s either way",
            MESSAGE_MAX_AGE.as_secs()
        ));
    }
    Ok(())
}

/// Message ids already dispatched, so a retried or replayed notification is
/// acknowledged (Twitch retries until it sees a 2XX) without reaching the agent
/// again. Bounded two ways: an id is forgotten after [`SEEN_MESSAGE_RETENTION`],
/// by which time its signed timestamp is outside the window and the timestamp
/// check refuses the replay instead; and the set never holds more than
/// [`SEEN_MESSAGE_CAP`] ids, oldest evicted first.
#[derive(Default)]
struct ReplayGuard {
    seen: HashMap<String, Instant>,
    order: VecDeque<String>,
}

type SharedReplayGuard = Arc<Mutex<ReplayGuard>>;

impl ReplayGuard {
    /// Record `message_id` as dispatched. `false` when it already was.
    fn first_sight(&mut self, message_id: &str, now: Instant) -> bool {
        self.expire(now);
        if self.seen.contains_key(message_id) {
            return false;
        }
        self.seen.insert(message_id.to_owned(), now);
        self.order.push_back(message_id.to_owned());
        true
    }

    fn expire(&mut self, now: Instant) {
        while let Some(oldest) = self.order.front() {
            let stale = self
                .seen
                .get(oldest)
                .is_none_or(|seen| now.duration_since(*seen) > SEEN_MESSAGE_RETENTION);
            if !stale && self.order.len() < SEEN_MESSAGE_CAP {
                break;
            }
            if let Some(id) = self.order.pop_front() {
                self.seen.remove(&id);
            }
        }
    }
}

/// Authenticate one delivery and return the parsed body it was signed over.
///
/// Order matters: the secret, all EventSub headers and the raw bytes are checked
/// BEFORE any JSON is parsed; the signed timestamp is checked against `now`
/// only after the signature. The body handed back is parsed from the verified
/// bytes — never from the engine's pre-parsed `body`. Every failure is a refusal.
async fn authenticate(
    iii: &dyn TriggerBus,
    req: &Value,
    now: DateTime<Utc>,
) -> Result<Value, Value> {
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
    if let Err(e) = check_message_timestamp(timestamp, now) {
        tracing::warn!(error = %e, message_id, "twitch eventsub delivery outside the timestamp window");
        return Err(reject(
            401,
            "Twitch EventSub message timestamp outside the 10-minute window",
        ));
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
    seen: &SharedReplayGuard,
    input: Value,
) -> Result<Value, Error> {
    let body = match authenticate(iii, &input, Utc::now()).await {
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

    // Only a notification has side effects, so only a notification is deduped.
    // A repeat (Twitch's own retry, or a replay inside the timestamp window) is
    // acknowledged so Twitch stops resending, and goes no further.
    let message_id = header(&input, "twitch-eventsub-message-id").unwrap_or_default();
    let first_sight = seen
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .first_sight(message_id, Instant::now());
    if !first_sight {
        tracing::info!(
            message_id,
            "twitch: duplicate EventSub notification acknowledged, not dispatched"
        );
        return Ok(json!({ "status_code": 200, "body": { "ok": true, "duplicate": true } }));
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

    let seen: SharedReplayGuard = Arc::new(Mutex::new(ReplayGuard::default()));

    let iii_clone = iii.clone();
    let client_clone = client.clone();
    let seen_clone = seen.clone();
    iii.register_function(
        "channel::twitch::webhook",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let client = client_clone.clone();
            let seen = seen_clone.clone();
            async move { webhook_handler(&iii, &client, &seen, input).await }
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
    use std::sync::atomic::{AtomicU64, Ordering};

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

    const DELIVERY: &str = r#"{"subscription":{"type":"channel.chat.message"},"event":{"broadcaster_user_id":"b1","user_id":"u1","message":{"text":"hello"}}}"#;
    const SECRET: &str = "twitch-eventsub-test-secret";
    const MESSAGE_ID: &str = "msg-123";
    const TIMESTAMP: &str = "2026-09-03T12:34:56Z";
    const SIGNATURE: &str =
        "sha256=8a40e300e18578033ff7405c9caa87fe60a505e974359a57b7b28fd3dae4808d";

    /// A delivery produced by Twitch's own tooling: `twitch event trigger
    /// channel.follow --version 2 --transport webhook --secret <CLI_SECRET>
    /// --forward-address <capture server>` with twitch-cli v1.1.24, captured
    /// verbatim (headers and the 554 raw body bytes). Twitch publishes no
    /// fixed vector in the EventSub guide — its sample uses `<your secret goes
    /// here>` — so the CLI, which signs the way the service does, is the
    /// closest thing to one. The HMAC was recomputed independently in Python
    /// before being pinned here.
    const CLI_SECRET: &str = "agentos-twitch-vector-secret-2026";
    const CLI_MESSAGE_ID: &str = "fee9c059-5aac-6971-011c-1de221f89e0d";
    const CLI_TIMESTAMP: &str = "2026-09-03T17:44:39.737021409Z";
    const CLI_SIGNATURE: &str =
        "sha256=8e45b42956f970ba6dcfac2e82f65ce5cbaae74f91134e357ae3c3720741f0a2";
    const CLI_BODY: &str = r#"{"subscription":{"id":"14b3559d-688e-447a-fbd8-a3ed5abf1587","status":"enabled","type":"channel.follow","version":"2","condition":{"broadcaster_user_id":"54769673","moderator_user_id":"99502841"},"transport":{"method":"webhook","callback":"null"},"created_at":"2026-09-03T17:44:39.737021409Z","cost":0},"event":{"user_id":"99502841","user_login":"testFromUser","user_name":"testFromUser","broadcaster_user_id":"54769673","broadcaster_user_login":"testBroadcaster","broadcaster_user_name":"testBroadcaster","followed_at":"2026-09-03T17:44:39.737021409Z"}}"#;

    fn sign_with(secret: &str, message_id: &str, timestamp: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(message_id.as_bytes());
        mac.update(timestamp.as_bytes());
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        sign_with(secret, MESSAGE_ID, TIMESTAMP, body)
    }

    fn request_with(
        raw: Option<&str>,
        message_id: &str,
        timestamp: &str,
        signature: Option<&str>,
        message_type: &str,
    ) -> Value {
        let mut headers = json!({
            "twitch-eventsub-message-id": message_id,
            "twitch-eventsub-message-timestamp": timestamp,
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

    fn now_rfc3339() -> String {
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
    }

    /// A delivery signed under SECRET with a fresh timestamp and a message id
    /// no other test in this process has used, so the replay guard and the
    /// timestamp window let it through.
    fn fresh_request(raw: Option<&str>, message_type: &str) -> Value {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let message_id = format!("msg-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
        let timestamp = now_rfc3339();
        let signature = sign_with(
            SECRET,
            &message_id,
            &timestamp,
            raw.unwrap_or(DELIVERY).as_bytes(),
        );
        request_with(raw, &message_id, &timestamp, Some(&signature), message_type)
    }

    fn fresh_guard() -> SharedReplayGuard {
        Arc::new(Mutex::new(ReplayGuard::default()))
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
    fn twitch_cli_signature_vector_verifies() {
        assert_eq!(CLI_BODY.len(), 554);
        assert_eq!(
            sign_with(
                CLI_SECRET,
                CLI_MESSAGE_ID,
                CLI_TIMESTAMP,
                CLI_BODY.as_bytes()
            ),
            CLI_SIGNATURE
        );
        assert!(
            verify_eventsub_signature(
                CLI_SECRET,
                CLI_MESSAGE_ID,
                CLI_TIMESTAMP,
                CLI_BODY.as_bytes(),
                CLI_SIGNATURE
            )
            .is_ok()
        );
        // The CLI's timestamp was current when captured and is stale now.
        let captured = DateTime::parse_from_rfc3339(CLI_TIMESTAMP)
            .unwrap()
            .with_timezone(&Utc);
        assert!(
            check_message_timestamp(CLI_TIMESTAMP, captured + chrono::Duration::seconds(599))
                .is_ok()
        );
        assert!(
            check_message_timestamp(CLI_TIMESTAMP, captured + chrono::Duration::seconds(601))
                .is_err()
        );
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

    #[test]
    fn timestamp_window_is_ten_minutes_either_way() {
        let sent = DateTime::parse_from_rfc3339("2026-09-03T12:34:56Z")
            .unwrap()
            .with_timezone(&Utc);
        let secs = chrono::Duration::seconds;
        assert!(check_message_timestamp("2026-09-03T12:34:56Z", sent).is_ok());
        assert!(check_message_timestamp("2026-09-03T12:34:56Z", sent + secs(600)).is_ok());
        assert!(check_message_timestamp("2026-09-03T12:34:56Z", sent - secs(600)).is_ok());
        assert!(check_message_timestamp("2026-09-03T12:34:56Z", sent + secs(601)).is_err());
        assert!(check_message_timestamp("2026-09-03T12:34:56Z", sent - secs(601)).is_err());
        // Nanoseconds and offsets, as RFC 3339 allows and Twitch emits.
        assert!(check_message_timestamp("2026-09-03T12:34:56.737021409Z", sent).is_ok());
        assert!(check_message_timestamp("2026-09-03T14:34:56+02:00", sent).is_ok());
        assert!(check_message_timestamp("2026-09-03T12:34:56+02:00", sent).is_err());
        // Not a timestamp at all.
        for bad in ["", "1700000000", "2026-09-03 12:34:56", "yesterday"] {
            assert!(check_message_timestamp(bad, sent).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn replay_guard_forgets_ids_by_age_and_by_count() {
        let mut guard = ReplayGuard::default();
        let start = Instant::now();
        assert!(guard.first_sight("a", start));
        assert!(!guard.first_sight("a", start));
        assert!(guard.first_sight("b", start + Duration::from_secs(1)));
        assert_eq!(guard.order.len(), 2);
        // Still remembered just inside the retention window.
        assert!(!guard.first_sight("a", start + SEEN_MESSAGE_RETENTION));
        // Forgotten past it — by then its timestamp cannot pass the window.
        assert!(guard.first_sight("a", start + SEEN_MESSAGE_RETENTION + Duration::from_secs(2)));
        assert_eq!(
            guard.order.len(),
            1,
            "a and b both expired; only the re-recorded a remains"
        );

        let mut guard = ReplayGuard::default();
        for i in 0..SEEN_MESSAGE_CAP + 10 {
            assert!(guard.first_sight(&format!("id-{i}"), start));
        }
        assert!(guard.order.len() <= SEEN_MESSAGE_CAP);
        assert!(
            guard.first_sight("id-0", start),
            "the oldest id was evicted by the cap"
        );
        assert!(!guard.first_sight(&format!("id-{}", SEEN_MESSAGE_CAP + 9), start));
    }

    #[tokio::test]
    async fn valid_notification_reaches_agent_but_forgery_does_not() {
        let bus = bus_with_secret(SECRET);
        let seen = fresh_guard();
        let valid = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            &seen,
            fresh_request(Some(DELIVERY), "notification"),
        )
        .await
        .unwrap();
        assert_eq!(valid["status_code"], 200);
        assert_eq!(bus.call_count("agent::chat"), 1);

        let tampered = DELIVERY.replace("hello", "forged");
        let mut forged = fresh_request(Some(DELIVERY), "notification");
        forged["rawBody"] = json!(tampered);
        forged["body"] = serde_json::from_str::<Value>(&tampered).unwrap();
        let forged = webhook_handler(&bus, &reqwest::Client::new(), &seen, forged)
            .await
            .unwrap();
        assert_eq!(forged["status_code"], 401);
        assert_eq!(bus.call_count("agent::chat"), 1);
    }

    #[tokio::test]
    async fn a_delivery_outside_the_timestamp_window_never_reaches_the_agent() {
        let bus = bus_with_secret(SECRET);
        let seen = fresh_guard();
        // Correctly signed — the timestamp is inside the HMAC — but stale by
        // eleven minutes, then eleven minutes early.
        for offset in [
            -chrono::Duration::minutes(11),
            chrono::Duration::minutes(11),
        ] {
            let timestamp =
                (Utc::now() + offset).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            let signature = sign_with(SECRET, "msg-window", &timestamp, DELIVERY.as_bytes());
            let response = webhook_handler(
                &bus,
                &reqwest::Client::new(),
                &seen,
                request_with(
                    Some(DELIVERY),
                    "msg-window",
                    &timestamp,
                    Some(&signature),
                    "notification",
                ),
            )
            .await
            .unwrap();
            assert_eq!(response["status_code"], 401, "{timestamp}");
        }
        // The pinned vector's timestamp is long past its window, so even that
        // exact, correctly signed delivery is refused today.
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            &seen,
            request_with(
                Some(DELIVERY),
                MESSAGE_ID,
                TIMESTAMP,
                Some(SIGNATURE),
                "notification",
            ),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 401);
        assert_eq!(bus.call_count("agent::chat"), 0);
        assert!(
            seen.lock().unwrap().order.is_empty(),
            "a refused delivery is not recorded"
        );
    }

    #[tokio::test]
    async fn a_replayed_notification_is_acknowledged_but_not_dispatched() {
        let bus = bus_with_secret(SECRET);
        let seen = fresh_guard();
        let delivery = fresh_request(Some(DELIVERY), "notification");
        let first = webhook_handler(&bus, &reqwest::Client::new(), &seen, delivery.clone())
            .await
            .unwrap();
        assert_eq!(first["status_code"], 200);
        assert_eq!(bus.call_count("agent::chat"), 1);

        // Same bytes, same headers: Twitch's retry, or a captured replay.
        let again = webhook_handler(&bus, &reqwest::Client::new(), &seen, delivery)
            .await
            .unwrap();
        assert_eq!(
            again["status_code"], 200,
            "Twitch must see a 2XX or it keeps retrying"
        );
        assert_eq!(again["body"]["duplicate"], true);
        assert_eq!(bus.call_count("agent::chat"), 1);

        // A different message id is a different message.
        let other = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            &seen,
            fresh_request(Some(DELIVERY), "notification"),
        )
        .await
        .unwrap();
        assert_eq!(other["status_code"], 200);
        assert_eq!(bus.call_count("agent::chat"), 2);
    }

    #[tokio::test]
    async fn missing_signature_header_is_rejected() {
        let bus = bus_with_secret(SECRET);
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            &fresh_guard(),
            request_with(
                Some(DELIVERY),
                MESSAGE_ID,
                &now_rfc3339(),
                None,
                "notification",
            ),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 401);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn missing_raw_body_is_rejected() {
        let bus = bus_with_secret(SECRET);
        let mut req = fresh_request(Some(DELIVERY), "notification");
        req.as_object_mut().unwrap().remove("rawBody");
        let response = webhook_handler(&bus, &reqwest::Client::new(), &fresh_guard(), req)
            .await
            .unwrap();
        assert_eq!(response["status_code"], 400);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn a_caller_supplied_raw_body_does_not_replace_the_engine_channel() {
        // A validly signed `rawBody` next to a `request_body` ref: the channel
        // is what gets read. Here the ref is unusable, so the request is
        // refused instead of being verified against the caller's bytes.
        let bus = bus_with_secret(SECRET);
        let mut req = fresh_request(Some(DELIVERY), "notification");
        req["request_body"] = json!("not-a-channel-ref");
        let response = webhook_handler(&bus, &reqwest::Client::new(), &fresh_guard(), req)
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
            &fresh_guard(),
            fresh_request(Some(DELIVERY), "notification"),
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
        let bus = bus_with_secret(SECRET);
        let seen = fresh_guard();
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            &seen,
            fresh_request(Some(challenge), "webhook_callback_verification"),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 200);
        assert_eq!(response["body"], "challenge-token");
        assert_eq!(response["headers"]["content-type"], "text/plain");

        let revocation = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            &seen,
            fresh_request(Some(challenge), "revocation"),
        )
        .await
        .unwrap();
        assert_eq!(revocation["body"], json!({ "ok": true }));
        assert_eq!(bus.call_count("agent::chat"), 0);
        assert_eq!(
            seen.lock().unwrap().order.len(),
            0,
            "only notifications are deduped"
        );
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
