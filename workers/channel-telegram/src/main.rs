use agentos_http_adapter::{CHAT_TIMEOUT_MS, TriggerBus};
use iii_sdk::errors::Error;
use iii_sdk::protocol::TriggerAction;
use iii_sdk::{RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};
use std::time::Duration;
use subtle::ConstantTimeEq;

const TELEGRAM_MAX_LEN: usize = 4096;
const SECRET_TOKEN_KEY: &str = "TELEGRAM_SECRET_TOKEN";

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

async fn send_message(
    iii: &dyn TriggerBus,
    client: &reqwest::Client,
    chat_id: i64,
    text: &str,
) -> Result<(), Error> {
    let bot_token = get_secret(iii, "TELEGRAM_BOT_TOKEN").await;
    if bot_token.is_empty() {
        return Err(Error::Handler("TELEGRAM_BOT_TOKEN not configured".into()));
    }
    for chunk in split_message(text, TELEGRAM_MAX_LEN) {
        let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");
        let res = client
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
            .map_err(|e| Error::Handler(e.to_string()))?;
        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            return Err(Error::Handler(format!(
                "Telegram send failed ({status}): {}",
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
    let secret_token = get_secret(iii, SECRET_TOKEN_KEY).await;
    if !verify_telegram_update(&secret_token, &input) {
        return Ok(json!({
            "status_code": 401,
            "body": { "error": "Missing or invalid webhook signature" },
        }));
    }

    let update = input.get("body").cloned().unwrap_or_else(|| input.clone());
    let message = update
        .get("message")
        .or_else(|| update.get("edited_message"))
        .cloned()
        .unwrap_or(Value::Null);

    let text = message.get("text").and_then(|t| t.as_str()).unwrap_or("");
    if text.is_empty() {
        return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
    }

    let chat_id = message
        .get("chat")
        .and_then(|c| c.get("id"))
        .and_then(|i| i.as_i64())
        .unwrap_or(0);
    let user_id = message
        .get("from")
        .and_then(|f| f.get("id"))
        .and_then(|i| i.as_i64());

    let agent_id = resolve_agent(iii, "telegram", &chat_id.to_string()).await;

    let chat_response = iii
        .trigger(TriggerRequest {
            function_id: "agent::chat".to_string(),
            payload: json!({
                "agentId": agent_id,
                "message": text,
                "sessionId": format!("telegram:{chat_id}"),
            }),
            action: None,
            timeout_ms: Some(CHAT_TIMEOUT_MS),
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let reply = chat_response
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    if !reply.is_empty() {
        send_message(iii, client, chat_id, &reply).await?;
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

    Ok(json!({ "status_code": 200, "body": { "ok": true } }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());
    let client = reqwest::Client::new();

    let iii_clone = iii.clone();
    let client_clone = client.clone();
    iii.register_function(
        "channel::telegram::webhook",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let client = client_clone.clone();
            async move { webhook_handler(&iii, &client, input).await }
        })
        .description("Handle Telegram webhook"),
    );

    // The route is registered only when the token that authenticates Telegram
    // deliveries exists. Without it every delivery would be refused anyway,
    // and an unauthenticated route is not worth exposing. The handler re-reads
    // the token per request, so a rotation takes effect without a restart.
    if startup_secret(&iii, SECRET_TOKEN_KEY).await.is_some() {
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
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_http_adapter::fake::FakeBus;

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

    fn bus_with_secret(secret: &str) -> FakeBus {
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
        bus
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
        let response = webhook_handler(&bus, &reqwest::Client::new(), request(Some(SECRET)))
            .await
            .unwrap();
        assert_eq!(response["status_code"], 200);
        let chats = bus.calls_to("agent::chat");
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].payload["message"], "hello");
        assert_eq!(chats[0].payload["sessionId"], "telegram:42");
    }

    #[tokio::test]
    async fn wrong_or_missing_token_is_rejected_before_dispatch() {
        for token in [Some("wrong"), None] {
            let bus = bus_with_secret(SECRET);
            let response = webhook_handler(&bus, &reqwest::Client::new(), request(token))
                .await
                .unwrap();
            assert_eq!(response["status_code"], 401);
            assert_eq!(bus.call_count("agent::chat"), 0);
            assert_eq!(bus.call_count("state::get"), 0);
        }
    }

    #[tokio::test]
    async fn missing_secret_refuses_delivery_and_route() {
        let bus = bus_with_secret("");
        let response = webhook_handler(&bus, &reqwest::Client::new(), request(Some(SECRET)))
            .await
            .unwrap();
        assert_eq!(response["status_code"], 401);
        assert_eq!(bus.call_count("agent::chat"), 0);
        assert_eq!(startup_secret(&bus, "TELEGRAM_SECRET_TOKEN").await, None);

        let configured = bus_with_secret(SECRET);
        assert_eq!(
            startup_secret(&configured, "TELEGRAM_SECRET_TOKEN")
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
}
