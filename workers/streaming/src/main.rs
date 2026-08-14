use iii_sdk::errors::Error;
use iii_sdk::{
    IIIClient, InitOptions, RegisterFunction, protocol::TriggerRequest, register_worker,
};
use serde_json::{Value, json};

mod types;

use types::chunk_markdown_aware;

fn payload_body(input: &Value) -> Value {
    input.get("body").cloned().unwrap_or_else(|| input.clone())
}

fn valid_route_preference(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && *value != "agentos-default")
        .map(str::to_owned)
}

fn stream_route_preferences(
    body: &Value,
    model_config: Option<&Value>,
) -> (Option<String>, Option<String>) {
    let request_provider = valid_route_preference(body.get("provider"));
    let request_model = valid_route_preference(body.get("model"));
    if request_provider.is_some() || request_model.is_some() {
        return (request_provider, request_model);
    }

    let config_provider =
        model_config.and_then(|model| valid_route_preference(model.get("provider")));
    let config_model = model_config.and_then(|model| valid_route_preference(model.get("model")));
    if config_provider.is_some() && config_model.is_some() {
        (config_provider, config_model)
    } else {
        (None, None)
    }
}

fn stream_route_fields(route: &Value) -> Result<(&str, &str), Error> {
    let provider = route["provider"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Handler("llm::route omitted provider".into()))?;
    let model = route["model"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Handler("llm::route omitted model".into()))?;
    Ok((provider, model))
}

fn agent_chat_payload(body: &Value, message: &str) -> Value {
    json!({
        "agentId": body["agentId"].as_str().unwrap_or("default"),
        "message": message,
        "sessionId": body.get("sessionId").cloned().unwrap_or(Value::Null),
        "provider": body.get("provider").cloned().unwrap_or(Value::Null),
        "model": body.get("model").cloned().unwrap_or(Value::Null),
    })
}

fn stream_route_payload(
    message: &str,
    provider: Option<&str>,
    model: Option<&str>,
) -> Value {
    let mut payload = json!({
        "messages": [{ "role": "user", "content": message }],
        "tools": [],
    });
    if let Some(provider) = provider {
        payload["provider"] = json!(provider);
    }
    if let Some(model) = model {
        payload["model"] = json!(model);
    }
    payload
}

fn stream_completion_payload(
    provider: &str,
    model: &str,
    system_prompt: &str,
    messages: &[Value],
) -> Value {
    json!({
        "provider": provider,
        "model": model,
        "systemPrompt": system_prompt,
        "messages": messages,
    })
}

fn completion_id() -> String {
    format!(
        "chatcmpl-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}

fn stream_chat_response(response: &Value) -> Value {
    json!({
        "content": response["content"],
        "model": response["model"],
        "usage": response["usage"],
    })
}

async fn stream_chat(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let body = payload_body(&input);
    let agent_id = body["agentId"].as_str().unwrap_or("default").to_string();
    let message = body["message"]
        .as_str()
        .ok_or_else(|| Error::Handler("message required".into()))?
        .to_string();

    let config = iii
        .trigger(TriggerRequest {
            function_id: "state::get".into(),
            payload: json!({ "scope": "agents", "key": &agent_id }),
            action: None,
            timeout_ms: None,
        })
        .await
        .ok();

    let memories = iii
        .trigger(TriggerRequest {
            function_id: "memory::recall".into(),
            payload: json!({ "agentId": &agent_id, "query": &message, "limit": 10 }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or_else(|_| json!([]));

    let model_config = config.as_ref().and_then(|c| c.get("model"));
    let (preferred_provider, preferred_model) =
        stream_route_preferences(&body, model_config);

    let route = iii
        .trigger(TriggerRequest {
            function_id: "llm::route".into(),
            payload: stream_route_payload(
                &message,
                preferred_provider.as_deref(),
                preferred_model.as_deref(),
            ),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let (provider, model) = stream_route_fields(&route)?;

    let system_prompt = config
        .as_ref()
        .and_then(|c| c["systemPrompt"].as_str())
        .unwrap_or("")
        .to_string();

    let mut messages: Vec<Value> = memories.as_array().cloned().unwrap_or_default();
    messages.push(json!({ "role": "user", "content": &message }));

    let response = iii
        .trigger(TriggerRequest {
            function_id: "llm::complete".into(),
            payload: stream_completion_payload(provider, model, &system_prompt, &messages),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    Ok(stream_chat_response(&response))
}

fn latest_user_message(body: &Value) -> Result<String, Error> {
    body["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message["role"] == "user")
        })
        .and_then(|message| message["content"].as_str())
        .filter(|content| !content.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::Handler("a non-empty user message is required".into()))
}

async fn chat_completion(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let body = payload_body(&input);
    let message = latest_user_message(&body)?;
    let requested_model = body["model"].as_str().unwrap_or("agentos-default");

    let response = iii
        .trigger(TriggerRequest {
            function_id: "agent::chat".into(),
            payload: agent_chat_payload(&body, &message),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|error| Error::Handler(error.to_string()))?;

    let content = response.get("content").cloned().unwrap_or(Value::Null);
    let model = response["model"].as_str().unwrap_or(requested_model);
    let usage = response.get("usage").cloned().unwrap_or_else(
        || json!({ "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 }),
    );

    Ok(json!({
        "id": completion_id(),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop",
        }],
        "usage": usage,
    }))
}

async fn stream_sse(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let body = payload_body(&input);
    let message = body["message"]
        .as_str()
        .ok_or_else(|| Error::Handler("message required".into()))?
        .to_string();

    let response = iii
        .trigger(TriggerRequest {
            function_id: "agent::chat".into(),
            payload: agent_chat_payload(&body, &message),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let content = response["content"].as_str().unwrap_or("").to_string();
    let model = response["model"]
        .as_str()
        .unwrap_or("agentos-default")
        .to_string();

    let chunks = chunk_markdown_aware(&content, 20, 100);
    let chunks_len = chunks.len();
    let created = chrono::Utc::now().timestamp();

    let mut sse_body = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let finish_reason = if i == chunks_len - 1 {
            json!("stop")
        } else {
            Value::Null
        };
        let delta = if i == 0 {
            json!({ "role": "assistant", "content": chunk })
        } else {
            json!({ "content": chunk })
        };
        let event = json!({
            "id": completion_id(),
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }]
        });
        let encoded = serde_json::to_string(&event).map_err(|e| Error::Handler(e.to_string()))?;
        sse_body.push_str("data: ");
        sse_body.push_str(&encoded);
        sse_body.push_str("\n\n");
    }
    sse_body.push_str("data: [DONE]\n\n");

    Ok(json!({
        "status_code": 200,
        "headers": {
            "Content-Type": "text/event-stream",
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
        },
        "body": sse_body,
    }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, InitOptions::default());

    let iii_ref = iii.clone();
    iii.register_function(
        "stream::chat",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { stream_chat(&iii, input).await }
        })
        .description("SSE streaming chat endpoint"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "stream::completion",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { chat_completion(&iii, input).await }
        })
        .description("OpenAI-compatible chat completion endpoint"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "stream::sse",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { stream_sse(&iii, input).await }
        })
        .description("SSE event stream for chat completions"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "stream::chat",
        json!({ "http_method": "POST", "api_path": "api/chat/stream" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "stream::completion",
        json!({ "http_method": "POST", "api_path": "v1/chat/completions" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "stream::sse",
        json!({ "http_method": "POST", "api_path": "v1/chat/completions/stream" }),
        None,
    )?;

    tracing::info!("streaming worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        agent_chat_payload, stream_chat_response, stream_completion_payload, stream_route_fields,
        stream_route_payload, stream_route_preferences,
    };
    use serde_json::json;

    #[test]
    fn stream_chat_response_is_transport_neutral() {
        let response = stream_chat_response(&json!({
            "content": "ready",
            "model": "test-model",
            "usage": { "total_tokens": 4 },
        }));

        assert_eq!(
            response,
            json!({
                "content": "ready",
                "model": "test-model",
                "usage": { "total_tokens": 4 },
            })
        );
        assert!(response.get("status_code").is_none());
        assert!(response.get("body").is_none());
    }

    #[test]
    fn stream_route_payload_uses_top_level_model_strings() {
        let payload = stream_route_payload("hello", Some("codex"), Some("gpt-5.6-sol"));

        assert_eq!(payload["provider"], "codex");
        assert_eq!(payload["model"], "gpt-5.6-sol");
        assert_eq!(
            payload["messages"],
            json!([{ "role": "user", "content": "hello" }])
        );
        assert_eq!(payload["tools"], json!([]));
        assert!(payload.get("config").is_none());
        assert!(payload.get("message").is_none());
        assert!(payload.get("toolCount").is_none());
    }

    #[test]
    fn stream_completion_payload_consumes_route_fields() {
        let payload = stream_completion_payload(
            "codex",
            "gpt-5.6-sol",
            "system",
            &[json!({"role": "user", "content": "hello"})],
        );

        assert_eq!(payload["provider"], "codex");
        assert_eq!(payload["model"], "gpt-5.6-sol");
        assert!(payload["provider"].is_string());
        assert!(payload["model"].is_string());
        assert!(payload.get("route").is_none());
    }
    #[test]
    fn agent_chat_payload_forwards_provider_and_model() {
        let body = json!({
            "agentId": "agent-2",
            "sessionId": "sess-42",
            "provider": "codex",
            "model": "gpt-5.6-sol",
        });
        let payload = agent_chat_payload(&body, "hello");

        assert_eq!(payload["provider"], "codex");
        assert_eq!(payload["model"], "gpt-5.6-sol");
    }

    fn configured_route() -> serde_json::Value {
        json!({
            "provider": "config-provider",
            "model": "config-model",
        })
    }

    #[test]
    fn stream_route_preferences_use_request_pair() {
        let body = json!({
            "provider": "codex",
            "model": "gpt-5.6-sol",
        });
        assert_eq!(
            stream_route_preferences(&body, Some(&configured_route())),
            (Some("codex".into()), Some("gpt-5.6-sol".into()))
        );
    }

    #[test]
    fn stream_route_preferences_model_only_does_not_combine_config_provider() {
        let body = json!({ "model": "gpt-5.6-sol" });
        assert_eq!(
            stream_route_preferences(&body, Some(&configured_route())),
            (None, Some("gpt-5.6-sol".into()))
        );
    }

    #[test]
    fn stream_route_preferences_filter_agentos_default() {
        let body = json!({
            "provider": "agentos-default",
            "model": "agentos-default",
        });
        assert_eq!(
            stream_route_preferences(&body, Some(&configured_route())),
            (Some("config-provider".into()), Some("config-model".into()))
        );
    }

    #[test]
    fn stream_route_preferences_filter_empty_strings() {
        let body = json!({ "provider": "", "model": "" });
        assert_eq!(
            stream_route_preferences(&body, Some(&configured_route())),
            (Some("config-provider".into()), Some("config-model".into()))
        );
    }

    #[test]
    fn stream_route_preferences_fallback_to_complete_config_pair() {
        let body = json!({});
        assert_eq!(
            stream_route_preferences(&body, Some(&configured_route())),
            (Some("config-provider".into()), Some("config-model".into()))
        );
    }

    #[test]
    fn stream_route_preferences_ignore_non_string_request_values() {
        let body = json!({ "provider": null, "model": 42 });
        assert_eq!(
            stream_route_preferences(&body, Some(&configured_route())),
            (Some("config-provider".into()), Some("config-model".into()))
        );
    }

    #[test]
    fn stream_route_fields_extract_provider_and_model() {
        let route = json!({
            "provider": "codex",
            "model": "gpt-5.6-sol",
        });
        let fields = stream_route_fields(&route).unwrap();
        assert_eq!(fields, ("codex", "gpt-5.6-sol"));
    }

    #[test]
    fn stream_route_fields_reject_missing_empty_or_non_string_values() {
        assert!(stream_route_fields(&json!({ "provider": "", "model": "x" })).is_err());
        assert!(stream_route_fields(&json!({ "provider": "codex" })).is_err());
        assert!(stream_route_fields(&json!({ "provider": 42, "model": "x" })).is_err());
        assert!(
            stream_route_fields(&json!({ "provider": "codex", "model": { "id": "nested" } }))
                .is_err()
        );
    }

    #[test]
    fn agent_chat_payload_omitted_preferences_remain_null() {
        let payload = agent_chat_payload(&json!({ "agentId": "agent-2" }), "hello");

        assert!(payload["provider"].is_null());
        assert!(payload["model"].is_null());
        assert_ne!(payload["model"], "agentos-default");
    }
}
