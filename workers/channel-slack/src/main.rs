mod admission;

use admission::{Admission, AdmissionDecision};
use agentos_http_adapter::{CHAT_TIMEOUT_MS, TriggerBus};
use hmac::{Hmac, Mac};
use iii_sdk::channels::{ChannelReader, StreamChannelRef};
use iii_sdk::errors::Error;
use iii_sdk::{RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};
use sha2::Sha256;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

const SLACK_API_BASE: &str = "https://slack.com/api";
const MAX_MESSAGE_LEN: usize = 4000;
const SIGNING_SECRET_KEY: &str = "SLACK_SIGNING_SECRET";
const MAX_IN_FLIGHT: usize = 32;
const DEDUPE_CAPACITY: usize = 4096;
const DEDUPE_RETENTION: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
struct SlackApi {
    client: reqwest::Client,
    base_url: String,
}

impl SlackApi {
    fn production(client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: SLACK_API_BASE.to_string(),
        }
    }
}

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

/// Resolve which agent should handle a given Slack channel message.
/// Mirrors `src/shared/utils.ts::resolveAgent`.
async fn resolve_agent(iii: &dyn TriggerBus, channel_id: &str) -> String {
    let key = format!("slack:{channel_id}");
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

/// Split text into Slack-safe chunks, preferring newline boundaries.
/// Character-aware (UTF-8 safe): never slices mid-codepoint.
/// Mirrors `src/shared/utils.ts::splitMessage`.
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
        remaining = remaining[split_at..].to_string();
    }
    chunks
}

/// Verify Slack's request signature: `v0=` followed by the lowercase hex
/// HMAC-SHA256 of `v0:{timestamp}:{raw_body}`, keyed by the signing secret
/// (https://api.slack.com/authentication/verifying-requests-from-slack).
/// The timestamp must be within five minutes and `Mac::verify_slice` performs
/// the digest comparison in constant time.
fn verify_slack_signature(
    signing_secret: &str,
    timestamp: &str,
    signature: &str,
    raw_body: &[u8],
) -> Result<(), String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    verify_slack_signature_at(signing_secret, timestamp, signature, raw_body, now)
}

fn verify_slack_signature_at(
    signing_secret: &str,
    timestamp: &str,
    signature: &str,
    raw_body: &[u8],
    now: i64,
) -> Result<(), String> {
    if signing_secret.is_empty() {
        return Err("SLACK_SIGNING_SECRET not configured".into());
    }
    let ts: i64 = timestamp
        .parse()
        .map_err(|_| "Invalid timestamp".to_string())?;
    if (now - ts).abs() > 300 {
        return Err("Stale Slack timestamp".to_string());
    }

    let provided_hex = signature
        .trim()
        .strip_prefix("v0=")
        .ok_or_else(|| "Invalid Slack signature version".to_string())?;
    let provided =
        hex::decode(provided_hex).map_err(|_| "X-Slack-Signature digest is not hex".to_string())?;
    let mut mac = HmacSha256::new_from_slice(signing_secret.as_bytes())
        .map_err(|e| format!("HMAC init error: {e}"))?;
    mac.update(format!("v0:{timestamp}:").as_bytes());
    mac.update(raw_body);
    mac.verify_slice(&provided)
        .map_err(|_| "Invalid Slack signature".to_string())
}

/// POST to `chat.postMessage`. Splits text > 4000 chars into multiple messages.
/// Returns Slack's response from the LAST chunk.
/// Slack docs: https://api.slack.com/methods/chat.postMessage
async fn slack_post_message(
    api: &SlackApi,
    bot_token: &str,
    channel: &str,
    text: &str,
    thread_ts: Option<&str>,
) -> Result<Value, Error> {
    if bot_token.is_empty() {
        return Err(Error::Handler("SLACK_BOT_TOKEN not configured".into()));
    }
    let chunks = split_message(text, MAX_MESSAGE_LEN);
    let mut last: Value = json!({ "ok": false });
    for chunk in chunks {
        let mut body = json!({ "channel": channel, "text": chunk });
        if let Some(ts) = thread_ts {
            body["thread_ts"] = json!(ts);
        }
        let resp = api
            .client
            .post(format!("{}/chat.postMessage", api.base_url))
            .bearer_auth(bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Handler(format!("Slack API error: {e}")))?;
        let status = resp.status();
        last = resp
            .json::<Value>()
            .await
            .map_err(|e| Error::Handler(format!("Slack response decode: {e}")))?;
        if !status.is_success() || last.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let error = last
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown_error");
            return Err(Error::Handler(format!(
                "Slack chat.postMessage failed ({status}): {error}"
            )));
        }
    }
    Ok(last)
}

/// Authenticate one delivery and return the parsed body it was signed over.
///
/// Order matters: the secret, both headers and the raw bytes are all checked
/// BEFORE any JSON is parsed, and the body handed back is parsed from the
/// verified bytes — never from the engine's pre-parsed `body`, which was not
/// what Slack signed. Every failure is a refusal (`Err(response)`).
async fn authenticate(iii: &dyn TriggerBus, req: &Value) -> Result<Value, Value> {
    let signing_secret = get_secret(iii, SIGNING_SECRET_KEY).await;
    if signing_secret.is_empty() {
        return Err(reject(503, "SLACK_SIGNING_SECRET not configured"));
    }
    let timestamp = header(req, "x-slack-request-timestamp").unwrap_or_default();
    let signature = header(req, "x-slack-signature").unwrap_or_default();
    if timestamp.is_empty() || signature.is_empty() {
        return Err(reject(401, "Missing Slack signature headers"));
    }
    let raw = match raw_request_body(req).await {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(error = %e, "slack: refusing delivery without its raw body");
            return Err(reject(
                400,
                "Raw request body unavailable for signature verification",
            ));
        }
    };
    if let Err(e) = verify_slack_signature(&signing_secret, timestamp, signature, &raw) {
        tracing::warn!(error = %e, "slack signature rejected");
        return Err(reject(401, "Invalid Slack signature"));
    }
    serde_json::from_slice(&raw).map_err(|_| reject(400, "Body is not valid JSON"))
}

/// Complete one authenticated, admitted Slack message in the background.
async fn process_message(
    iii: Arc<dyn TriggerBus>,
    api: SlackApi,
    event: Value,
) -> Result<(), Error> {
    let channel = event
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let text = event
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let ts = event
        .get("ts")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let thread_ts = event
        .get("thread_ts")
        .and_then(Value::as_str)
        .map(String::from);
    let session_anchor = thread_ts.clone().unwrap_or_else(|| ts.clone());
    let agent_id = resolve_agent(iii.as_ref(), &channel).await;

    let chat = iii
        .trigger(TriggerRequest {
            function_id: "agent::chat".to_string(),
            payload: json!({
                "agentId": &agent_id,
                "principal": { "agentId": &agent_id },
                "message": text,
                "sessionId": format!("slack:{channel}:{session_anchor}"),
            }),
            action: None,
            timeout_ms: Some(CHAT_TIMEOUT_MS),
        })
        .await
        .map_err(|error| Error::Handler(format!("agent::chat failed: {error}")))?;

    let reply = chat
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !reply.is_empty() {
        let bot_token = get_secret(iii.as_ref(), "SLACK_BOT_TOKEN").await;
        slack_post_message(&api, &bot_token, &channel, reply, thread_ts.as_deref()).await?;
    }
    Ok(())
}

/// Handle Slack Events API webhook delivery.
///
/// Authentication and parsing of the signed bytes finish before admission.
/// Accepted user-message events are acknowledged after bounded in-process
/// admission; their turn continues in an owned task. The acknowledgment is not
/// a durable/exactly-once guarantee: a process crash can lose accepted work.
async fn handle_events(
    iii: Arc<dyn TriggerBus>,
    api: SlackApi,
    admission: Admission,
    req: Value,
) -> Result<Value, Error> {
    let body = match authenticate(iii.as_ref(), &req).await {
        Ok(body) => body,
        Err(response) => return Ok(response),
    };

    if body.get("type").and_then(Value::as_str) == Some("url_verification") {
        let challenge = body
            .get("challenge")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return Ok(json!({
            "status_code": 200,
            "body": { "challenge": challenge }
        }));
    }

    let event = body.get("event").cloned().unwrap_or_else(|| json!({}));
    let is_user_message = event.get("type").and_then(Value::as_str) == Some("message")
        && event.get("subtype").is_none()
        && event.get("bot_id").is_none()
        && event.get("user").and_then(Value::as_str).is_some()
        && event.get("text").and_then(Value::as_str).is_some();
    if !is_user_message {
        return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
    }

    let Some(event_id) = body
        .get("event_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return Ok(reject(
            400,
            "Authenticated Slack message is missing event_id",
        ));
    };
    // Slack documents event_id as globally unique. Team and app still scope it
    // defensively when one process serves a single configured Slack app.
    let team_id = body.get("team_id").and_then(Value::as_str).unwrap_or("-");
    let app_id = body
        .get("api_app_id")
        .and_then(Value::as_str)
        .unwrap_or("-");
    let delivery_key = format!("slack:{team_id}:{app_id}:{event_id}");

    let task_iii = iii.clone();
    let decision = admission.admit(delivery_key, async move {
        if let Err(error) = process_message(task_iii, api, event).await {
            tracing::error!(%error, "accepted Slack delivery failed");
        }
    });
    match decision {
        AdmissionDecision::Accepted => Ok(json!({
            "status_code": 200,
            "body": { "accepted": true },
        })),
        AdmissionDecision::Duplicate => Ok(json!({
            "status_code": 200,
            "body": { "accepted": true, "duplicate": true },
        })),
        AdmissionDecision::Overloaded => {
            Ok(reject(503, "Webhook admission overloaded; retry delivery"))
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = engine_ws_url();
    let iii = Arc::new(register_worker(&ws_url, agentos_bus_auth::init_options()));
    let api = SlackApi::production(reqwest::Client::new());
    let admission = Admission::new(MAX_IN_FLIGHT, DEDUPE_CAPACITY, DEDUPE_RETENTION);

    // channel::slack::events — preserve the exact ID registered by the TS port.
    let iii_clone = iii.clone();
    let api_clone = api.clone();
    let admission_clone = admission.clone();
    iii.register_function(
        "channel::slack::events",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let api = api_clone.clone();
            let admission = admission_clone.clone();
            async move { handle_events(iii, api, admission, input).await }
        })
        .description("Handle Slack Events API webhook"),
    );

    // channel::slack::send — outbound helper for other workers (agent::chat etc).
    let iii_clone = iii.clone();
    let api_clone = api.clone();
    iii.register_function(
        "channel::slack::send",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let api = api_clone.clone();
            async move {
                let channel = input
                    .get("channel")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Handler("missing channel".into()))?
                    .to_string();
                let text = input
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Handler("missing text".into()))?
                    .to_string();
                let thread_ts = input
                    .get("thread_ts")
                    .and_then(Value::as_str)
                    .map(String::from);
                let bot_token = get_secret(iii.as_ref(), "SLACK_BOT_TOKEN").await;
                slack_post_message(&api, &bot_token, &channel, &text, thread_ts.as_deref()).await
            }
        })
        .description("Post a message to a Slack channel via chat.postMessage"),
    );

    // The route is registered only when the secret that verifies Slack's
    // signature exists. Without it every delivery would be refused anyway, and
    // an unverifiable route is not worth exposing. The handler re-reads the
    // secret per request, so a rotation takes effect without a restart.
    if startup_secret(iii.as_ref(), SIGNING_SECRET_KEY)
        .await
        .is_some()
    {
        agentos_http_adapter::register_http_trigger(
            &iii,
            "channel::slack::events".to_string(),
            json!({ "http_method": "POST", "api_path": "webhook/slack/events" }),
            None,
        )?;
        tracing::info!("slack webhook route registered (HMAC-SHA256 signature verified)");
    } else {
        tracing::error!(
            "{SIGNING_SECRET_KEY} is not configured: POST /webhook/slack/events is NOT registered. \
             Set the app signing secret and restart."
        );
    }

    tracing::info!("channel-slack worker started");
    tokio::signal::ctrl_c().await?;
    admission.shutdown().await;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_http_adapter::fake::FakeBus;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

    const SECRET: &str = "slack-signing-secret";
    const URL_VERIFICATION: &str =
        r#"{"type":"url_verification","challenge":"abc123","token":"deprecated"}"#;
    const MESSAGE: &str = r#"{"type":"event_callback","event_id":"Ev-123","team_id":"T1","api_app_id":"A1","event":{"type":"message","user":"U1","text":"hello","channel":"C1","ts":"1710000000.000100"}}"#;

    #[test]
    fn split_short_text_returns_single_chunk() {
        let chunks = split_message("hello", 4000);
        assert_eq!(chunks, vec!["hello".to_string()]);
    }

    #[test]
    fn split_long_text_breaks_on_newline() {
        let text = format!("{}\n{}", "a".repeat(50), "b".repeat(50));
        let chunks = split_message(&text, 80);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].ends_with('a'));
        assert!(chunks[1].starts_with('\n'));
    }

    #[test]
    fn split_long_text_with_no_newline_falls_back_to_max() {
        let text = "x".repeat(150);
        let chunks = split_message(&text, 80);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 80);
        assert_eq!(chunks[1].len(), 70);
    }

    #[test]
    fn split_preserves_total_length() {
        let text = "line1\nline2\n".repeat(500);
        let chunks = split_message(&text, 4000);
        let joined: String = chunks.concat();
        assert_eq!(joined, text);
    }

    fn sign(secret: &str, ts: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("v0:{ts}:").as_bytes());
        mac.update(body);
        format!("v0={}", hex::encode(mac.finalize().into_bytes()))
    }

    fn now_ts() -> String {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string()
    }

    fn request(raw: &str, signature: Option<&str>, timestamp: Option<&str>) -> Value {
        let mut headers = json!({ "content-type": "application/json" });
        if let Some(signature) = signature {
            headers["x-slack-signature"] = json!(signature);
        }
        if let Some(timestamp) = timestamp {
            headers["x-slack-request-timestamp"] = json!(timestamp);
        }
        json!({
            "method": "POST",
            "headers": headers,
            "rawBody": raw,
            // The engine's parsed body is deliberately present. Signed routes
            // must ignore it and parse the verified raw bytes instead.
            "body": serde_json::from_str::<Value>(raw).unwrap_or(Value::Null),
        })
    }

    fn bus_with_secret(secret: &str) -> Arc<FakeBus> {
        let bus = FakeBus::new();
        let secret = secret.to_string();
        bus.on("vault::get", move |payload| {
            let key = payload["key"].as_str().unwrap_or_default();
            Ok(json!({
                "value": if key == "SLACK_SIGNING_SECRET" {
                    secret.clone()
                } else {
                    String::new()
                }
            }))
        });
        bus.on_value("state::get", json!({ "agentId": "default" }));
        bus.on_value("agent::chat", json!({ "content": "" }));
        Arc::new(bus)
    }

    fn test_admission() -> Admission {
        Admission::new(4, 64, Duration::from_secs(600))
    }

    fn test_api() -> SlackApi {
        SlackApi::production(reqwest::Client::new())
    }

    async fn test_handle(bus: &Arc<FakeBus>, admission: &Admission, req: Value) -> Value {
        handle_events(bus.clone(), test_api(), admission.clone(), req)
            .await
            .unwrap()
    }

    async fn wait_for_calls(bus: &FakeBus, function_id: &str, count: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while bus.call_count(function_id) < count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background task did not make expected bus call");
    }

    struct FakeProvider {
        base_url: String,
        calls: Arc<AtomicUsize>,
        requests: Arc<std::sync::Mutex<Vec<String>>>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for FakeProvider {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn spawn_fake_provider(response_body: &'static str, delay: Duration) -> FakeProvider {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake provider");
        let addr = listener.local_addr().expect("fake provider address");
        let calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let task_calls = calls.clone();
        let task_requests = requests.clone();
        let handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut bytes = vec![0_u8; 16 * 1024];
                let count = stream.read(&mut bytes).await.unwrap_or_default();
                task_requests
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(String::from_utf8_lossy(&bytes[..count]).to_string());
                tokio::time::sleep(delay).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
                task_calls.fetch_add(1, Ordering::SeqCst);
            }
        });
        FakeProvider {
            base_url: format!("http://{addr}"),
            calls,
            requests,
            handle,
        }
    }

    async fn wait_for_provider(provider: &FakeProvider, count: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while provider.calls.load(Ordering::SeqCst) < count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background task did not reach fake provider");
    }

    #[test]
    fn slack_documentation_signature_vector_verifies() {
        // Published Slack vector from
        // https://api.slack.com/authentication/verifying-requests-from-slack.
        let secret = "8f742231b10e8888abcd99yyyzzz85a5";
        let timestamp = "1531420618";
        let body = b"token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
        let signature = "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503";
        assert_eq!(sign(secret, timestamp, body), signature);
        assert!(
            verify_slack_signature_at(secret, timestamp, signature, body, 1_531_420_618).is_ok()
        );
    }

    #[test]
    fn signature_verifies_when_correct() {
        let body = br#"{"type":"event_callback"}"#;
        let ts = now_ts();
        let sig = sign(SECRET, &ts, body);
        assert!(verify_slack_signature(SECRET, &ts, &sig, body).is_ok());
    }

    #[test]
    fn signature_rejects_when_body_tampered() {
        let ts = now_ts();
        let sig = sign(SECRET, &ts, br#"{"type":"event_callback"}"#);
        let result = verify_slack_signature(SECRET, &ts, &sig, br#"{"type":"tampered"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn signature_rejects_stale_timestamp() {
        let body = br#"{"type":"event_callback"}"#;
        let ts = "1000000000";
        let sig = sign(SECRET, ts, body);
        let result = verify_slack_signature(SECRET, ts, &sig, body);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Stale"));
    }

    #[test]
    fn signature_rejects_garbage_signature() {
        let body = br#"{"type":"event_callback"}"#;
        let ts = now_ts();
        let result = verify_slack_signature(SECRET, &ts, "v0=deadbeef", body);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn signed_url_verification_echoes_challenge() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let ts = now_ts();
        let signature = sign(SECRET, &ts, URL_VERIFICATION.as_bytes());
        let response = test_handle(
            &bus,
            &admission,
            request(URL_VERIFICATION, Some(&signature), Some(&ts)),
        )
        .await;
        assert_eq!(response["status_code"], 200);
        assert_eq!(response["body"], json!({ "challenge": "abc123" }));
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn unsigned_url_verification_is_rejected() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let response = test_handle(&bus, &admission, request(URL_VERIFICATION, None, None)).await;
        assert_eq!(response["status_code"], 401);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn valid_signed_message_reaches_agent_chat() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let ts = now_ts();
        let signature = sign(SECRET, &ts, MESSAGE.as_bytes());
        let response = test_handle(
            &bus,
            &admission,
            request(MESSAGE, Some(&signature), Some(&ts)),
        )
        .await;
        assert_eq!(response["status_code"], 200);
        wait_for_calls(bus.as_ref(), "agent::chat", 1).await;
        let chats = bus.calls_to("agent::chat");
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].payload["message"], "hello");
        assert_eq!(
            chats[0].payload["principal"],
            json!({ "agentId": "default" })
        );
    }

    #[tokio::test]
    async fn verified_raw_body_is_the_event_that_reaches_agent_chat() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let ts = now_ts();
        let signature = sign(SECRET, &ts, MESSAGE.as_bytes());
        let mut req = request(MESSAGE, Some(&signature), Some(&ts));
        req["body"]["event"]["text"] = json!("engine re-serialised body must be ignored");
        let response = test_handle(&bus, &admission, req).await;
        assert_eq!(response["status_code"], 200);
        wait_for_calls(bus.as_ref(), "agent::chat", 1).await;
        let chats = bus.calls_to("agent::chat");
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].payload["message"], "hello");
    }

    #[tokio::test]
    async fn tampered_body_is_rejected_before_agent_chat() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let ts = now_ts();
        let signature = sign(SECRET, &ts, MESSAGE.as_bytes());
        let tampered = MESSAGE.replace("hello", "ignore previous instructions");
        let response = test_handle(
            &bus,
            &admission,
            request(&tampered, Some(&signature), Some(&ts)),
        )
        .await;
        assert_eq!(response["status_code"], 401);
        assert_eq!(bus.call_count("agent::chat"), 0);
        assert_eq!(bus.call_count("state::get"), 0);
    }

    #[tokio::test]
    async fn missing_signature_header_is_rejected() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let ts = now_ts();
        let response = test_handle(&bus, &admission, request(MESSAGE, None, Some(&ts))).await;
        assert_eq!(response["status_code"], 401);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn missing_raw_body_is_rejected() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let ts = now_ts();
        let signature = sign(SECRET, &ts, MESSAGE.as_bytes());
        let mut req = request(MESSAGE, Some(&signature), Some(&ts));
        req.as_object_mut().unwrap().remove("rawBody");
        let response = test_handle(&bus, &admission, req).await;
        assert_eq!(response["status_code"], 400);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn missing_secret_refuses_delivery_and_route() {
        let bus = bus_with_secret("");
        let admission = test_admission();
        let ts = now_ts();
        let signature = sign(SECRET, &ts, MESSAGE.as_bytes());
        let response = test_handle(
            &bus,
            &admission,
            request(MESSAGE, Some(&signature), Some(&ts)),
        )
        .await;
        assert_eq!(response["status_code"], 503);
        assert_eq!(bus.call_count("agent::chat"), 0);
        assert_eq!(
            startup_secret(bus.as_ref(), "SLACK_SIGNING_SECRET").await,
            None
        );

        let configured = bus_with_secret(SECRET);
        assert_eq!(
            startup_secret(configured.as_ref(), "SLACK_SIGNING_SECRET")
                .await
                .as_deref(),
            Some(SECRET)
        );
    }

    fn classify(event: &Value) -> bool {
        event.get("type").and_then(|v| v.as_str()) == Some("message")
            && event.get("subtype").is_none()
            && event.get("bot_id").is_none()
            && event.get("user").and_then(|v| v.as_str()).is_some()
            && event.get("text").and_then(|v| v.as_str()).is_some()
    }

    #[test]
    fn ignores_bot_messages() {
        let event = json!({
            "type": "message",
            "text": "from bot",
            "user": "U1",
            "channel": "C1",
            "ts": "1.0",
            "bot_id": "B123"
        });
        assert!(!classify(&event));
    }

    #[test]
    fn detects_user_messages() {
        let event = json!({
            "type": "message",
            "text": "hi",
            "user": "U1",
            "channel": "C1",
            "ts": "1.0"
        });
        assert!(classify(&event));
    }

    #[test]
    fn ignores_message_changed_subtype() {
        let event = json!({
            "type": "message",
            "subtype": "message_changed",
            "channel": "C1",
            "ts": "1.0",
            "message": { "text": "edited" }
        });
        assert!(!classify(&event));
    }

    #[test]
    fn ignores_message_deleted_subtype() {
        let event = json!({
            "type": "message",
            "subtype": "message_deleted",
            "channel": "C1",
            "ts": "1.0",
            "deleted_ts": "0.5"
        });
        assert!(!classify(&event));
    }

    #[test]
    fn ignores_message_missing_user() {
        let event = json!({
            "type": "message",
            "text": "hi",
            "channel": "C1",
            "ts": "1.0"
        });
        assert!(!classify(&event));
    }

    #[test]
    fn split_handles_multibyte_chars_without_panic() {
        let text: String = "🦀".repeat(10);
        let chunks = split_message(&text, 3);
        let joined: String = chunks.concat();
        assert_eq!(joined, text);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 3);
        }
    }
    #[tokio::test]
    async fn a_caller_supplied_raw_body_does_not_replace_the_engine_channel() {
        // A validly signed `rawBody` next to a `request_body` ref: the channel
        // is what gets read. Here the ref is unusable, so the request is
        // refused instead of being verified against the caller's bytes.
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let ts = now_ts();
        let signature = sign(SECRET, &ts, MESSAGE.as_bytes());
        let mut req = request(MESSAGE, Some(&signature), Some(&ts));
        req["request_body"] = json!("not-a-channel-ref");
        let response = test_handle(&bus, &admission, req).await;
        assert_eq!(response["status_code"], 400);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn admission_deduplicates_concurrent_delivery_ids() {
        let admission = Admission::new(1, 8, Duration::from_secs(600));
        let first = admission.admit("delivery-1".to_string(), async {});
        let duplicate = admission.admit("delivery-1".to_string(), async {});
        assert_eq!(first, AdmissionDecision::Accepted);
        assert_eq!(duplicate, AdmissionDecision::Duplicate);
        admission.shutdown().await;
    }

    #[tokio::test]
    async fn duplicate_signed_delivery_starts_one_turn() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let ts = now_ts();
        let signature = sign(SECRET, &ts, MESSAGE.as_bytes());
        for _ in 0..2 {
            let response = test_handle(
                &bus,
                &admission,
                request(MESSAGE, Some(&signature), Some(&ts)),
            )
            .await;
            assert_eq!(response["status_code"], 200);
        }
        wait_for_calls(bus.as_ref(), "agent::chat", 1).await;
        assert_eq!(bus.call_count("agent::chat"), 1);
    }

    #[tokio::test]
    async fn slow_chat_is_acknowledged_before_the_turn_finishes() {
        let provider = spawn_fake_provider(r#"{"ok":true}"#, Duration::from_millis(200)).await;
        let bus = bus_with_secret(SECRET);
        bus.on("vault::get", |payload| {
            Ok(json!({
                "value": match payload["key"].as_str().unwrap_or_default() {
                    "SLACK_SIGNING_SECRET" => SECRET,
                    "SLACK_BOT_TOKEN" => "bot-token",
                    _ => "",
                }
            }))
        });
        bus.on_value("agent::chat", json!({ "content": "slow reply" }));
        let admission = Admission::new(1, 8, Duration::from_secs(600));
        let api = SlackApi {
            client: reqwest::Client::new(),
            base_url: provider.base_url.clone(),
        };
        let ts = now_ts();
        let signature = sign(SECRET, &ts, MESSAGE.as_bytes());
        let started = std::time::Instant::now();
        let response = handle_events(
            bus.clone(),
            api,
            admission.clone(),
            request(MESSAGE, Some(&signature), Some(&ts)),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 200);
        assert!(started.elapsed() < Duration::from_millis(100));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !provider
                    .requests
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("accepted task did not reach slow fake provider");
        tokio::time::timeout(Duration::from_secs(1), admission.shutdown())
            .await
            .expect("owned task shutdown timed out");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn simultaneous_retries_start_one_turn() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let ts = now_ts();
        let signature = sign(SECRET, &ts, MESSAGE.as_bytes());
        let req = request(MESSAGE, Some(&signature), Some(&ts));
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let spawn_delivery = |req: Value| {
            let bus = bus.clone();
            let admission = admission.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                handle_events(bus, test_api(), admission, req)
                    .await
                    .unwrap()
            })
        };
        let first = spawn_delivery(req.clone());
        let second = spawn_delivery(req);
        barrier.wait().await;
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap()["status_code"], 200);
        assert_eq!(second.unwrap()["status_code"], 200);
        wait_for_calls(bus.as_ref(), "agent::chat", 1).await;
        assert_eq!(bus.call_count("agent::chat"), 1);
    }

    #[tokio::test]
    async fn overload_is_refused_before_ack_and_duplicate_still_acks() {
        let bus = bus_with_secret(SECRET);
        let admission = Admission::new(1, 8, Duration::from_secs(600));
        assert_eq!(
            admission.admit("slack:T1:A1:Ev-123".to_string(), std::future::pending(),),
            AdmissionDecision::Accepted
        );
        let ts = now_ts();
        let first_body = MESSAGE.to_string();
        let second_body = MESSAGE.replace("Ev-123", "Ev-124");
        let first_sig = sign(SECRET, &ts, first_body.as_bytes());
        let second_sig = sign(SECRET, &ts, second_body.as_bytes());
        let duplicate = test_handle(
            &bus,
            &admission,
            request(&first_body, Some(&first_sig), Some(&ts)),
        )
        .await;
        let overloaded = test_handle(
            &bus,
            &admission,
            request(&second_body, Some(&second_sig), Some(&ts)),
        )
        .await;
        assert_eq!(duplicate["status_code"], 200);
        assert_eq!(overloaded["status_code"], 503);
        assert_eq!(bus.call_count("agent::chat"), 0);
        tokio::time::timeout(Duration::from_secs(1), admission.shutdown())
            .await
            .expect("owned task shutdown timed out");
    }

    #[tokio::test]
    async fn reordered_unique_event_ids_are_both_admitted() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let ts = now_ts();
        for id in ["Ev-200", "Ev-100"] {
            let body = MESSAGE.replace("Ev-123", id);
            let signature = sign(SECRET, &ts, body.as_bytes());
            let response = test_handle(
                &bus,
                &admission,
                request(&body, Some(&signature), Some(&ts)),
            )
            .await;
            assert_eq!(response["status_code"], 200);
        }
        wait_for_calls(bus.as_ref(), "agent::chat", 2).await;
        assert_eq!(bus.call_count("agent::chat"), 2);
    }

    #[tokio::test]
    async fn invalid_signature_does_not_poison_delivery_id() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let ts = now_ts();
        let rejected = test_handle(
            &bus,
            &admission,
            request(MESSAGE, Some("v0=deadbeef"), Some(&ts)),
        )
        .await;
        let signature = sign(SECRET, &ts, MESSAGE.as_bytes());
        let accepted = test_handle(
            &bus,
            &admission,
            request(MESSAGE, Some(&signature), Some(&ts)),
        )
        .await;
        assert_eq!(rejected["status_code"], 401);
        assert_eq!(accepted["status_code"], 200);
        wait_for_calls(bus.as_ref(), "agent::chat", 1).await;
        assert_eq!(bus.call_count("agent::chat"), 1);
    }

    #[tokio::test]
    async fn chat_failure_stays_accepted_and_is_not_retried() {
        let bus = bus_with_secret(SECRET);
        bus.on_error("agent::chat", "provider failed");
        let admission = test_admission();
        let ts = now_ts();
        let signature = sign(SECRET, &ts, MESSAGE.as_bytes());
        for _ in 0..2 {
            let response = test_handle(
                &bus,
                &admission,
                request(MESSAGE, Some(&signature), Some(&ts)),
            )
            .await;
            assert_eq!(response["status_code"], 200);
            assert_eq!(response["body"]["accepted"], true);
            assert!(response["body"].get("ok").is_none());
        }
        wait_for_calls(bus.as_ref(), "agent::chat", 1).await;
        assert_eq!(bus.call_count("agent::chat"), 1);
    }

    #[tokio::test]
    async fn positive_reply_posts_once_to_fake_provider() {
        let provider = spawn_fake_provider(r#"{"ok":true}"#, Duration::ZERO).await;
        let bus = bus_with_secret(SECRET);
        bus.on("vault::get", |payload| {
            Ok(json!({
                "value": match payload["key"].as_str().unwrap_or_default() {
                    "SLACK_SIGNING_SECRET" => SECRET,
                    "SLACK_BOT_TOKEN" => "bot-token",
                    _ => "",
                }
            }))
        });
        bus.on_value("agent::chat", json!({ "content": "the reply" }));
        let admission = test_admission();
        let api = SlackApi {
            client: reqwest::Client::new(),
            base_url: provider.base_url.clone(),
        };
        let ts = now_ts();
        let signature = sign(SECRET, &ts, MESSAGE.as_bytes());
        let response = handle_events(
            bus.clone(),
            api,
            admission.clone(),
            request(MESSAGE, Some(&signature), Some(&ts)),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 200);
        wait_for_provider(&provider, 1).await;
        let requests = provider
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("POST /chat.postMessage"));
        assert!(requests[0].contains("the reply"));
        let chats = bus.calls_to("agent::chat");
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].timeout_ms, Some(CHAT_TIMEOUT_MS));
    }

    #[tokio::test]
    async fn dedupe_cache_refuses_early_eviction_and_expires_by_ttl() {
        let admission = Admission::new(2, 1, Duration::from_millis(20));
        assert_eq!(
            admission.admit("first".to_string(), async {}),
            AdmissionDecision::Accepted
        );
        tokio::task::yield_now().await;
        assert_eq!(
            admission.admit("second".to_string(), async {}),
            AdmissionDecision::Overloaded
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(
            admission.admit("second".to_string(), async {}),
            AdmissionDecision::Accepted
        );
        admission.shutdown().await;
    }

    #[tokio::test]
    async fn signed_non_message_event_keeps_compatibility_without_delivery_id() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let body = r#"{"type":"event_callback","event":{"type":"reaction_added"}}"#;
        let ts = now_ts();
        let signature = sign(SECRET, &ts, body.as_bytes());
        let response =
            test_handle(&bus, &admission, request(body, Some(&signature), Some(&ts))).await;
        assert_eq!(response["status_code"], 200);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn signed_message_without_event_id_is_refused_without_dispatch() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let mut body: Value = serde_json::from_str(MESSAGE).unwrap();
        body.as_object_mut().unwrap().remove("event_id");
        let body = serde_json::to_string(&body).unwrap();
        let ts = now_ts();
        let signature = sign(SECRET, &ts, body.as_bytes());
        let response = test_handle(
            &bus,
            &admission,
            request(&body, Some(&signature), Some(&ts)),
        )
        .await;
        assert_eq!(response["status_code"], 400);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn in_flight_delivery_never_expires_into_a_duplicate_turn() {
        let admission = Admission::new(2, 8, Duration::from_millis(10));
        assert_eq!(
            admission.admit("still-running".to_string(), std::future::pending()),
            AdmissionDecision::Accepted
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            admission.admit("still-running".to_string(), async {}),
            AdmissionDecision::Duplicate
        );
        tokio::time::timeout(Duration::from_secs(1), admission.shutdown())
            .await
            .expect("owned task shutdown timed out");
    }
}
