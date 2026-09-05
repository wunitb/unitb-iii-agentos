use agentos_http_adapter::CHAT_TIMEOUT_MS;
use iii_sdk::errors::Error;
use iii_sdk::protocol::TriggerAction;
use iii_sdk::{IIIClient, RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};

const MASTODON_MAX_LEN: usize = 500;

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

fn strip_html_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for c in input.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

async fn resolve_agent(iii: &IIIClient, channel: &str, channel_id: &str) -> String {
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

async fn get_secret(iii: &IIIClient, key: &str) -> String {
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

async fn send_message(
    iii: &IIIClient,
    client: &reqwest::Client,
    text: &str,
    in_reply_to_id: Option<String>,
) -> Result<(), Error> {
    let instance = get_secret(iii, "MASTODON_INSTANCE").await;
    if instance.is_empty() {
        return Err(Error::Handler("MASTODON_INSTANCE not configured".into()));
    }
    let token = get_secret(iii, "MASTODON_TOKEN").await;
    if token.is_empty() {
        return Err(Error::Handler("MASTODON_TOKEN not configured".into()));
    }
    let mut reply_id = in_reply_to_id;
    for chunk in split_message(text, MASTODON_MAX_LEN) {
        let mut body = json!({ "status": chunk });
        if let Some(id) = &reply_id {
            body["in_reply_to_id"] = json!(id);
        }
        let url = format!("{instance}/api/v1/statuses");
        let res = client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Handler(e.to_string()))?;
        if !res.status().is_success() {
            return Err(Error::Handler(format!(
                "Mastodon post failed: {}",
                res.status()
            )));
        }
        let resp: Value = res
            .json()
            .await
            .map_err(|e| Error::Handler(e.to_string()))?;
        let new_id = resp
            .get("id")
            .and_then(|i| i.as_str())
            .ok_or_else(|| Error::Handler("Mastodon response missing status id".into()))?;
        reply_id = Some(new_id.to_string());
    }
    Ok(())
}

/// Mastodon delivers a bot's mentions over the streaming API
/// (`/api/v1/streaming/user`, WebSocket or SSE) or by polling
/// `/api/v1/notifications`; there is no per-app webhook for them. The two
/// push-shaped features that exist are not this: Web Push subscriptions are
/// end-to-end encrypted for browser/mobile clients, and admin webhooks
/// (4.0+, `X-Hub-Signature`) carry instance-admin events for all users. The
/// `{account, status}` payload this handler reads is a streaming
/// `notification` object. A `POST /webhook/mastodon` route therefore had
/// nothing it could verify, so no HTTP route is registered;
/// `channel::mastodon::webhook` stays on the bus for a streaming client that
/// hands notifications in over the authenticated bus.
async fn webhook_handler(
    iii: &IIIClient,
    client: &reqwest::Client,
    input: Value,
) -> Result<Value, Error> {
    let body = input.get("body").cloned().unwrap_or(input);
    let account = body.get("account").cloned().unwrap_or(Value::Null);
    let status = body.get("status").cloned().unwrap_or(Value::Null);

    let content = status.get("content").and_then(|c| c.as_str()).unwrap_or("");
    if content.is_empty() {
        return Ok(json!({ "status_code": 200, "body": { "ok": true } }));
    }

    let acct = account
        .get("acct")
        .and_then(|a| a.as_str())
        .map(String::from)
        .or_else(|| {
            account.get("id").map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
        })
        .unwrap_or_default();

    let text = strip_html_tags(content);
    let agent_id = resolve_agent(iii, "mastodon", &acct).await;

    let chat_response = iii
        .trigger(TriggerRequest {
            function_id: "agent::chat".to_string(),
            payload: json!({
                "agentId": agent_id,
                "message": text,
                "sessionId": format!("mastodon:{acct}"),
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

    let status_id = status.get("id").and_then(|i| i.as_str()).map(String::from);

    if !reply.is_empty() {
        send_message(iii, client, &reply, status_id).await?;
    }

    let _ = iii
        .trigger(TriggerRequest {
            function_id: "security::audit".to_string(),
            payload: json!({
                "type": "channel_message",
                "agentId": agent_id,
                "detail": { "channel": "mastodon", "acct": acct },
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
        "channel::mastodon::webhook",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let client = client_clone.clone();
            async move { webhook_handler(&iii, &client, input).await }
        })
        .description("Handle a Mastodon mention (bus-only: mentions arrive over the streaming API, not a webhook)"),
    );

    tracing::info!("channel-mastodon worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard: this worker registers NO HTTP route. See the module note above
    /// the handler: the provider has no inbound webhook, so a route here can
    /// never verify its caller. Bringing one back requires a verifier first.
    #[test]
    fn registers_no_inbound_http_route() {
        let source = include_str!("main.rs");
        let call = concat!("register_http_", "trigger(");
        assert!(
            !source.contains(call),
            "an HTTP route without a provider verifier was reintroduced"
        );
        assert!(!source.contains(concat!("agentos_http_", "adapter")));
    }

    #[test]
    fn strip_html_removes_tags() {
        assert_eq!(strip_html_tags("<p>Hello</p>"), "Hello");
        assert_eq!(
            strip_html_tags("<p>Plain <b>text</b> here</p>"),
            "Plain text here"
        );
    }

    #[test]
    fn strip_html_passes_through_plain_text() {
        assert_eq!(strip_html_tags("just text"), "just text");
    }

    #[test]
    fn split_short_returns_single() {
        assert_eq!(split_message("short", 500), vec!["short".to_string()]);
    }

    #[test]
    fn split_long_chunks() {
        let text = "a".repeat(700);
        let chunks = split_message(&text, 500);
        assert!(chunks.len() >= 2);
    }
}
