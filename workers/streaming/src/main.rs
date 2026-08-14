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

    let model_config = config
        .as_ref()
        .and_then(|c| c.get("model").cloned())
        .unwrap_or(Value::Null);

    let model = iii
        .trigger(TriggerRequest {
            function_id: "llm::route".into(),
            payload: json!({ "message": &message, "toolCount": 0, "config": model_config }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

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
            payload: json!({
                "model": model,
                "systemPrompt": system_prompt,
                "messages": messages,
            }),
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
    let agent_id = body["agentId"].as_str().unwrap_or("default");
    let requested_model = body["model"].as_str().unwrap_or("agentos-default");
    let session_id = body.get("sessionId").cloned().unwrap_or(Value::Null);

    let response = iii
        .trigger(TriggerRequest {
            function_id: "agent::chat".into(),
            payload: json!({
                "agentId": agent_id,
                "message": message,
                "sessionId": session_id,
            }),
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
    let agent_id = body["agentId"].as_str().unwrap_or("default").to_string();
    let message = body["message"]
        .as_str()
        .ok_or_else(|| Error::Handler("message required".into()))?
        .to_string();
    let session_id = body.get("sessionId").cloned().unwrap_or(Value::Null);

    let response = iii
        .trigger(TriggerRequest {
            function_id: "agent::chat".into(),
            payload: json!({
                "agentId": &agent_id,
                "message": &message,
                "sessionId": session_id,
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let content = response["content"].as_str().unwrap_or("").to_string();
    let model = response["model"]
        .as_str()
        .unwrap_or("claude-sonnet-4-6")
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
    use super::stream_chat_response;
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
}
