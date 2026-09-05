mod admission;

use admission::{Admission, AdmissionDecision};
use agentos_http_adapter::{CHAT_TIMEOUT_MS, TriggerBus};
use iii_sdk::errors::Error;
use iii_sdk::protocol::TriggerAction;
use iii_sdk::{RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;

const TELEGRAM_MAX_LEN: usize = 4096;
const SECRET_TOKEN_KEY: &str = "TELEGRAM_SECRET_TOKEN";
const MAX_IN_FLIGHT: usize = 32;
const DEDUPE_CAPACITY: usize = 4096;
const DEDUPE_RETENTION: Duration = Duration::from_secs(10 * 60);
const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

#[derive(Clone)]
struct TelegramApi {
    client: reqwest::Client,
    base_url: String,
}

impl TelegramApi {
    fn production(client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: TELEGRAM_API_BASE.to_string(),
        }
    }
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

fn safe_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
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

fn verify_telegram_update(secret_token: &str, input: &Value) -> bool {
    if secret_token.is_empty() {
        return false;
    }
    let provided = header(input, "x-telegram-bot-api-secret-token").unwrap_or_default();
    if provided.is_empty() {
        return false;
    }
    // Telegram authenticates webhooks with this shared-secret header rather
    // than a body signature, so there is no raw request body to read here.
    safe_eq(provided, secret_token)
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

fn transport_error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_request() {
        "request"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else {
        "transport"
    }
}

async fn send_message(
    iii: &dyn TriggerBus,
    api: &TelegramApi,
    chat_id: i64,
    text: &str,
) -> Result<(), Error> {
    let bot_token = get_secret(iii, "TELEGRAM_BOT_TOKEN").await;
    if bot_token.is_empty() {
        return Err(Error::Handler("TELEGRAM_BOT_TOKEN not configured".into()));
    }
    for chunk in split_message(text, TELEGRAM_MAX_LEN) {
        let url = format!("{}/bot{bot_token}/sendMessage", api.base_url);
        let res = api
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            // Send as plain text. Telegram Markdown would need every `_`,
            // `*`, `[`, `]`, and backtick in unescaped model output to be
            // escaped, otherwise the API rejects the message.
            .json(&json!({
                "chat_id": chat_id,
                "text": chunk,
            }))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|error| {
                // reqwest Display includes the request URL. Telegram embeds the
                // bot token in that URL, so expose only a bounded error kind.
                Error::Handler(format!(
                    "Telegram send failed (transport:{})",
                    transport_error_kind(&error)
                ))
            })?;
        if !res.status().is_success() {
            // Provider response bodies are untrusted and may reflect the token
            // or submitted text. Status is actionable without echoing the body.
            return Err(Error::Handler(format!(
                "Telegram send failed (HTTP {})",
                res.status()
            )));
        }
    }
    Ok(())
}

/// Complete one authenticated, admitted Telegram update in the background.
async fn process_update(
    iii: Arc<dyn TriggerBus>,
    api: TelegramApi,
    message: Value,
) -> Result<(), Error> {
    let text = message
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let chat_id = message
        .get("chat")
        .and_then(|chat| chat.get("id"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let user_id = message
        .get("from")
        .and_then(|from| from.get("id"))
        .and_then(Value::as_i64);
    let agent_id = resolve_agent(iii.as_ref(), "telegram", &chat_id.to_string()).await;

    let chat_response = iii
        .trigger(TriggerRequest {
            function_id: "agent::chat".to_string(),
            payload: json!({
                "agentId": &agent_id,
                "principal": { "agentId": &agent_id },
                "message": text,
                "sessionId": format!("telegram:{chat_id}"),
            }),
            action: None,
            timeout_ms: Some(CHAT_TIMEOUT_MS),
        })
        .await
        .map_err(|error| Error::Handler(format!("agent::chat failed: {error}")))?;

    let reply = chat_response
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !reply.is_empty() {
        send_message(iii.as_ref(), &api, chat_id, reply).await?;
    }

    let _ = iii
        .trigger(TriggerRequest {
            function_id: "security::audit".to_string(),
            payload: json!({
                "type": "channel_message",
                "agentId": agent_id,
                "detail": { "channel": "telegram", "chatId": chat_id, "userId": user_id },
            }),
            action: Some(TriggerAction::Void),
            timeout_ms: None,
        })
        .await;
    Ok(())
}

/// Authenticate before reading the update, then promptly acknowledge bounded
/// in-process admission. The owned task/dedupe set prevents duplicate turns
/// only during this process lifetime; it is not a durable/exactly-once queue.
async fn webhook_handler(
    iii: Arc<dyn TriggerBus>,
    api: TelegramApi,
    admission: Admission,
    input: Value,
) -> Result<Value, Error> {
    let secret_token = get_secret(iii.as_ref(), SECRET_TOKEN_KEY).await;
    if !verify_telegram_update(&secret_token, &input) {
        return Ok(json!({
            "status_code": 401,
            "body": { "error": "Missing or invalid webhook signature" },
        }));
    }

    // Telegram authenticates with a header rather than a body HMAC. Do not
    // inspect or admit the parsed body until that header has been verified.
    let update = input.get("body").cloned().unwrap_or_else(|| input.clone());
    let message = update
        .get("message")
        .or_else(|| update.get("edited_message"))
        .cloned()
        .unwrap_or(Value::Null);
    let text = message
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if text.is_empty() {
        return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
    }

    let Some(update_id) = update.get("update_id").and_then(Value::as_i64) else {
        return Ok(json!({
            "status_code": 400,
            "body": { "error": "Authenticated Telegram message is missing update_id" },
        }));
    };
    // This worker has one configured bot/webhook token, so the admission
    // instance itself scopes Telegram's per-bot update_id sequence.
    let delivery_key = format!("telegram:{update_id}");
    let task_iii = iii.clone();
    let decision = admission.admit(delivery_key, async move {
        if let Err(error) = process_update(task_iii, api, message).await {
            tracing::error!(%error, "accepted Telegram delivery failed");
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
        AdmissionDecision::Overloaded => Ok(json!({
            "status_code": 503,
            "body": { "error": "Webhook admission overloaded; retry delivery" },
        })),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = Arc::new(register_worker(&ws_url, agentos_bus_auth::init_options()));
    let api = TelegramApi::production(reqwest::Client::new());
    let admission = Admission::new(MAX_IN_FLIGHT, DEDUPE_CAPACITY, DEDUPE_RETENTION);

    let iii_clone = iii.clone();
    let api_clone = api.clone();
    let admission_clone = admission.clone();
    iii.register_function(
        "channel::telegram::webhook",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let api = api_clone.clone();
            let admission = admission_clone.clone();
            async move { webhook_handler(iii, api, admission, input).await }
        })
        .description("Handle Telegram webhook"),
    );

    // The route is registered only when the token that authenticates Telegram
    // deliveries exists. Without it every delivery would be refused anyway,
    // and an unauthenticated route is not worth exposing. The handler re-reads
    // the token per request, so a rotation takes effect without a restart.
    if startup_secret(iii.as_ref(), SECRET_TOKEN_KEY)
        .await
        .is_some()
    {
        agentos_http_adapter::register_http_trigger(
            &iii,
            "channel::telegram::webhook".to_string(),
            json!({ "http_method": "POST", "api_path": "webhook/telegram" }),
            None,
        )?;
        tracing::info!("telegram webhook route registered (secret token verified)");
    } else {
        tracing::error!(
            "{SECRET_TOKEN_KEY} is not configured: POST /webhook/telegram is NOT registered. \
             Set the webhook secret token and restart."
        );
    }

    tracing::info!("channel-telegram worker started");
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

    const SECRET: &str = "telegram-webhook-secret";

    fn request(token: Option<&str>) -> Value {
        let mut headers = json!({ "content-type": "application/json" });
        if let Some(token) = token {
            headers["x-telegram-bot-api-secret-token"] = json!(token);
        }
        json!({
            "method": "POST",
            "headers": headers,
            "body": {
                "update_id": 123,
                "message": {
                    "message_id": 456,
                    "from": { "id": 7 },
                    "chat": { "id": 42 },
                    "text": "hello"
                }
            }
        })
    }

    fn bus_with_secret(secret: &str) -> Arc<FakeBus> {
        let bus = FakeBus::new();
        let secret = secret.to_string();
        bus.on("vault::get", move |payload| {
            let key = payload["key"].as_str().unwrap_or_default();
            Ok(json!({
                "value": if key == "TELEGRAM_SECRET_TOKEN" {
                    secret.clone()
                } else {
                    String::new()
                }
            }))
        });
        bus.on_value("state::get", json!({ "agentId": "default" }));
        bus.on_value("agent::chat", json!({ "content": "" }));
        bus.on_value("security::audit", json!({}));
        Arc::new(bus)
    }

    fn test_admission() -> Admission {
        Admission::new(4, 64, Duration::from_secs(600))
    }

    fn test_api() -> TelegramApi {
        TelegramApi::production(reqwest::Client::new())
    }

    async fn test_handle(bus: &Arc<FakeBus>, admission: &Admission, input: Value) -> Value {
        webhook_handler(bus.clone(), test_api(), admission.clone(), input)
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

    async fn spawn_fake_provider(
        status: &'static str,
        response_body: &'static str,
        delay: Duration,
    ) -> FakeProvider {
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
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
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

    fn request_with_id(token: Option<&str>, update_id: i64) -> Value {
        let mut req = request(token);
        req["body"]["update_id"] = json!(update_id);
        req
    }

    #[test]
    fn safe_eq_matches_equal_strings() {
        assert!(safe_eq("abc", "abc"));
    }

    #[test]
    fn safe_eq_rejects_unequal_strings() {
        assert!(!safe_eq("abc", "abd"));
        assert!(!safe_eq("abc", "ab"));
    }

    #[test]
    fn verify_rejects_empty_secret() {
        let body = json!({ "headers": { "x-telegram-bot-api-secret-token": "x" } });
        assert!(!verify_telegram_update("", &body));
    }

    #[test]
    fn verify_rejects_missing_header() {
        let body = json!({ "headers": {} });
        assert!(!verify_telegram_update("secret", &body));
    }

    #[test]
    fn verify_accepts_matching_token() {
        let body = json!({ "headers": { "x-telegram-bot-api-secret-token": "secret" } });
        assert!(verify_telegram_update("secret", &body));
    }

    #[test]
    fn verify_rejects_mismatched_token() {
        let body = json!({ "headers": { "x-telegram-bot-api-secret-token": "wrong" } });
        assert!(!verify_telegram_update("secret", &body));
    }

    #[tokio::test]
    async fn valid_token_reaches_agent_chat() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let response = test_handle(&bus, &admission, request(Some(SECRET))).await;
        assert_eq!(response["status_code"], 200);
        wait_for_calls(bus.as_ref(), "agent::chat", 1).await;
        let chats = bus.calls_to("agent::chat");
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].payload["message"], "hello");
        assert_eq!(
            chats[0].payload["principal"],
            json!({ "agentId": "default" })
        );
        assert_eq!(chats[0].payload["sessionId"], "telegram:42");
    }

    #[tokio::test]
    async fn wrong_or_missing_token_is_rejected_before_dispatch() {
        for token in [Some("wrong"), None] {
            let bus = bus_with_secret(SECRET);
            let admission = test_admission();
            let response = test_handle(&bus, &admission, request(token)).await;
            assert_eq!(response["status_code"], 401);
            assert_eq!(bus.call_count("agent::chat"), 0);
            assert_eq!(bus.call_count("state::get"), 0);
        }
    }

    #[tokio::test]
    async fn missing_secret_refuses_delivery_and_route() {
        let bus = bus_with_secret("");
        let admission = test_admission();
        let response = test_handle(&bus, &admission, request(Some(SECRET))).await;
        assert_eq!(response["status_code"], 401);
        assert_eq!(bus.call_count("agent::chat"), 0);
        assert_eq!(
            startup_secret(bus.as_ref(), "TELEGRAM_SECRET_TOKEN").await,
            None
        );

        let configured = bus_with_secret(SECRET);
        assert_eq!(
            startup_secret(configured.as_ref(), "TELEGRAM_SECRET_TOKEN")
                .await
                .as_deref(),
            Some(SECRET)
        );
    }

    #[test]
    fn split_under_limit_returns_single_chunk() {
        let chunks = split_message("hello", 4096);
        assert_eq!(chunks, vec!["hello".to_string()]);
    }

    #[test]
    fn split_over_limit_chunks() {
        let text = "a".repeat(5000);
        let chunks = split_message(&text, 4096);
        assert!(chunks.len() >= 2);
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
    async fn duplicate_authenticated_update_starts_one_turn() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        for _ in 0..2 {
            let response = test_handle(&bus, &admission, request(Some(SECRET))).await;
            assert_eq!(response["status_code"], 200);
        }
        wait_for_calls(bus.as_ref(), "agent::chat", 1).await;
        assert_eq!(bus.call_count("agent::chat"), 1);
    }

    #[tokio::test]
    async fn slow_chat_is_acknowledged_before_the_turn_finishes() {
        let provider = spawn_fake_provider("200 OK", "{}", Duration::from_millis(200)).await;
        let bus = bus_with_secret(SECRET);
        bus.on("vault::get", |payload| {
            Ok(json!({
                "value": match payload["key"].as_str().unwrap_or_default() {
                    "TELEGRAM_SECRET_TOKEN" => SECRET,
                    "TELEGRAM_BOT_TOKEN" => "bot-token",
                    _ => "",
                }
            }))
        });
        bus.on_value("agent::chat", json!({ "content": "slow reply" }));
        let admission = Admission::new(1, 8, Duration::from_secs(600));
        let api = TelegramApi {
            client: reqwest::Client::new(),
            base_url: provider.base_url.clone(),
        };
        let started = std::time::Instant::now();
        let response = webhook_handler(bus.clone(), api, admission.clone(), request(Some(SECRET)))
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
        let req = request(Some(SECRET));
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let spawn_delivery = |req: Value| {
            let bus = bus.clone();
            let admission = admission.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                webhook_handler(bus, test_api(), admission, req)
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
            admission.admit("telegram:123".to_string(), std::future::pending()),
            AdmissionDecision::Accepted
        );
        let duplicate = test_handle(&bus, &admission, request_with_id(Some(SECRET), 123)).await;
        let overloaded = test_handle(&bus, &admission, request_with_id(Some(SECRET), 124)).await;
        assert_eq!(duplicate["status_code"], 200);
        assert_eq!(overloaded["status_code"], 503);
        assert_eq!(bus.call_count("agent::chat"), 0);
        tokio::time::timeout(Duration::from_secs(1), admission.shutdown())
            .await
            .expect("owned task shutdown timed out");
    }

    #[tokio::test]
    async fn reordered_unique_update_ids_are_both_admitted() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        for update_id in [200, 100] {
            let response =
                test_handle(&bus, &admission, request_with_id(Some(SECRET), update_id)).await;
            assert_eq!(response["status_code"], 200);
        }
        wait_for_calls(bus.as_ref(), "agent::chat", 2).await;
        assert_eq!(bus.call_count("agent::chat"), 2);
    }

    #[tokio::test]
    async fn invalid_token_does_not_poison_update_id() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let rejected = test_handle(&bus, &admission, request(Some("wrong"))).await;
        let accepted = test_handle(&bus, &admission, request(Some(SECRET))).await;
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
        for _ in 0..2 {
            let response = test_handle(&bus, &admission, request(Some(SECRET))).await;
            assert_eq!(response["status_code"], 200);
            assert_eq!(response["body"]["accepted"], true);
            assert!(response["body"].get("ok").is_none());
        }
        wait_for_calls(bus.as_ref(), "agent::chat", 1).await;
        assert_eq!(bus.call_count("agent::chat"), 1);
        assert_eq!(bus.call_count("security::audit"), 0);
    }

    #[tokio::test]
    async fn positive_reply_posts_once_to_fake_provider() {
        let provider = spawn_fake_provider("200 OK", "{}", Duration::ZERO).await;
        let bus = bus_with_secret(SECRET);
        bus.on("vault::get", |payload| {
            Ok(json!({
                "value": match payload["key"].as_str().unwrap_or_default() {
                    "TELEGRAM_SECRET_TOKEN" => SECRET,
                    "TELEGRAM_BOT_TOKEN" => "bot-token",
                    _ => "",
                }
            }))
        });
        bus.on_value("agent::chat", json!({ "content": "the reply" }));
        let admission = test_admission();
        let api = TelegramApi {
            client: reqwest::Client::new(),
            base_url: provider.base_url.clone(),
        };
        let response = webhook_handler(bus.clone(), api, admission.clone(), request(Some(SECRET)))
            .await
            .unwrap();
        assert_eq!(response["status_code"], 200);
        wait_for_provider(&provider, 1).await;
        wait_for_calls(bus.as_ref(), "security::audit", 1).await;
        let requests = provider
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("POST /botbot-token/sendMessage"));
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
    async fn authenticated_non_message_update_keeps_compatibility() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let mut req = request(Some(SECRET));
        req["body"] = json!({ "update_id": 123, "callback_query": { "id": "q1" } });
        let response = test_handle(&bus, &admission, req).await;
        assert_eq!(response["status_code"], 200);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn authenticated_message_without_update_id_is_refused_without_dispatch() {
        let bus = bus_with_secret(SECRET);
        let admission = test_admission();
        let mut req = request(Some(SECRET));
        req["body"].as_object_mut().unwrap().remove("update_id");
        let response = test_handle(&bus, &admission, req).await;
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

    #[tokio::test]
    async fn telegram_transport_error_never_exposes_bot_token_or_url_path() {
        const FAKE_BOT_TOKEN: &str = "literal-fake-bot-token-for-red-test";
        // TCP port 0 is reserved and cannot have a listening peer.
        let base_url = "http://127.0.0.1:0".to_string();
        let bus = bus_with_secret(SECRET);
        bus.on_value("vault::get", json!({ "value": FAKE_BOT_TOKEN }));
        let api = TelegramApi {
            client: reqwest::Client::new(),
            base_url,
        };
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            send_message(bus.as_ref(), &api, 42, "safe text"),
        )
        .await
        .expect("refused loopback request timed out")
        .unwrap_err()
        .to_string();
        assert!(
            !error.contains(FAKE_BOT_TOKEN),
            "transport error exposed configured bot token"
        );
        assert!(
            !error.contains("sendMessage"),
            "transport error exposed credential-bearing URL path"
        );
        assert!(
            error.contains("transport:connect"),
            "transport error lost bounded failure kind"
        );
    }

    #[tokio::test]
    async fn telegram_http_error_never_echoes_provider_body() {
        const REFLECTED_SECRET: &str = "reflected-secret-marker";
        const REFLECTED_CHAT: &str = "reflected-chat-marker";
        let body = r#"{"error":"reflected-secret-marker reflected-chat-marker"}"#;
        let provider = spawn_fake_provider("502 Bad Gateway", body, Duration::ZERO).await;
        let bus = bus_with_secret(SECRET);
        bus.on_value("vault::get", json!({ "value": "fake-token" }));
        let api = TelegramApi {
            client: reqwest::Client::new(),
            base_url: provider.base_url.clone(),
        };
        let error = send_message(bus.as_ref(), &api, 42, REFLECTED_CHAT)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            !error.contains(REFLECTED_SECRET),
            "HTTP error echoed provider-controlled secret marker"
        );
        assert!(
            !error.contains(REFLECTED_CHAT),
            "HTTP error echoed submitted chat marker"
        );
        assert!(error.contains("502"), "HTTP error lost actionable status");
    }
}
