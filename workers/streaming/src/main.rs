//! Chat transports for the HTTP edge.
//!
//! All three endpoints delegate to `agent::chat`. This worker used to run a
//! second, weaker pipeline for `POST /api/chat/stream` (no tools, no injection
//! scan, no memory write, no metering, and `toolCalls` dropped on the floor),
//! which is the branch the TUI actually used. There is one pipeline now.
//!
//! Honesty note: none of this is incremental delivery. `agent::chat` is a
//! request/response call on the bus and `agentos::llm::complete` does not
//! stream, so `stream::sse` frames a *finished* answer as SSE events. The
//! framing is real, the streaming is not; the response says so in
//! `x-agentos-stream: buffered`.

use iii_sdk::errors::Error;
use iii_sdk::{
    IIIClient, InitOptions, RegisterFunction,
    protocol::{RegisterTriggerInput, TriggerRequest},
    register_worker,
};
use serde_json::{Value, json};

mod types;

use types::chunk_markdown_aware;

/// A chat turn may legitimately take minutes: a ReAct loop is several provider
/// calls plus the tools between them. The SDK default is 30 s, and it applied
/// to the whole outer turn, so the HTTP edge advertised 300 s while the bus cut
/// the turn at thirty seconds.
const CHAT_TIMEOUT_MS: u64 = 300_000;

fn chat_trigger(function_id: &str, payload: Value) -> TriggerRequest {
    TriggerRequest {
        function_id: function_id.to_string(),
        payload,
        action: None,
        timeout_ms: Some(CHAT_TIMEOUT_MS),
    }
}

/// Deny-by-default authorization for `iii-stream` WebSocket joins.
///
/// The engine cannot refuse the handshake. `iii/engine/src/workers/stream/stream.rs`
/// logs a failing `auth_function` ("Failed to invoke auth function" /
/// "No result from auth function" / "Failed to convert result to context") and
/// then calls `ws.on_upgrade(... None)` anyway, so an unauthenticated socket is
/// upgraded with `context: None`. The first place the engine reads a verdict is
/// the `stream:join` trigger: its handler's `{"unauthorized": true}` makes the
/// connection answer `stream.unauthorized` and return **before** the
/// subscription is inserted (`workers/stream/connection.rs`).
///
/// So the rule here is: a join is authorized only when the handshake produced an
/// authenticated auth context. `security::stream_auth` returns
/// `{"context": {"authenticated": true, "subject": ..., "addr": ...}}`; the
/// engine hands the inner object to this trigger. No context, a context that is
/// not an object, `authenticated` that is not literally `true`, or an empty
/// `subject` -> refused.
///
/// Known limits, stated rather than papered over:
/// * The engine fails **open** if this handler cannot be called at all (an
///   `Err` from the trigger call is logged and the join proceeds), so this is
///   defence in depth behind the loopback bind, not a replacement for it.
/// * Nothing in this repository joins an engine stream today, so the allow
///   branch has no production consumer to regress; the deny branch is the one
///   that matters.
const STREAM_JOIN_TRIGGER_TYPE: &str = "stream:join";
const STREAM_JOIN_FUNCTION: &str = "stream::authorize_join";

/// The auth context the engine stamped on the join event, wherever it carries
/// it. Deny-by-default means an unrecognised envelope simply has no context.
fn stream_join_context(event: &Value) -> Option<&Value> {
    event
        .get("context")
        .or_else(|| event.get("data").and_then(|data| data.get("context")))
        .filter(|context| !context.is_null())
}

fn join_context_is_authenticated(context: Option<&Value>) -> bool {
    let Some(Value::Object(context)) = context else {
        return false;
    };
    let authenticated = matches!(context.get("authenticated"), Some(Value::Bool(true)));
    let subject = context
        .get("subject")
        .and_then(Value::as_str)
        .is_some_and(|subject| !subject.trim().is_empty());
    authenticated && subject
}

fn stream_join_decision(event: &Value) -> Value {
    if join_context_is_authenticated(stream_join_context(event)) {
        return json!({ "unauthorized": false });
    }
    let stream_name = event
        .get("stream_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<unknown>");
    let subscription_id = event
        .get("subscription_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<unknown>");
    tracing::warn!(
        stream_name,
        subscription_id,
        "refused an iii-stream join without an authenticated connection context"
    );
    json!({ "unauthorized": true })
}

fn stream_join_trigger() -> RegisterTriggerInput {
    RegisterTriggerInput {
        trigger_type: STREAM_JOIN_TRIGGER_TYPE.to_string(),
        function_id: STREAM_JOIN_FUNCTION.to_string(),
        // The engine matches join triggers on trigger type only, so an empty
        // config gates every stream instead of one named stream.
        config: json!({}),
        metadata: None,
    }
}

fn payload_body(input: &Value) -> Value {
    input.get("body").cloned().unwrap_or_else(|| input.clone())
}

fn chat_message(body: &Value) -> Result<String, Error> {
    body["message"]
        .as_str()
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::Handler("a non-empty message is required".into()))
}

fn agent_chat_payload(body: &Value, message: &str) -> Value {
    json!({
        "agentId": body["agentId"].as_str().unwrap_or("default"),
        "message": message,
        "sessionId": body.get("sessionId").cloned().unwrap_or(Value::Null),
        "systemPrompt": body.get("systemPrompt").cloned().unwrap_or(Value::Null),
        "provider": body.get("provider").cloned().unwrap_or(Value::Null),
        "model": body.get("model").cloned().unwrap_or(Value::Null),
    })
}

fn completion_id() -> String {
    format!(
        "chatcmpl-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}

/// Transport-neutral view of an `agent::chat` answer. Everything the agent
/// reported is carried through: dropping `toolCalls` here is what turned a
/// tool-calling turn into an empty assistant bubble.
fn stream_chat_response(response: &Value) -> Value {
    let mut body = json!({
        "content": response.get("content").cloned().unwrap_or(Value::Null),
        "model": response.get("model").cloned().unwrap_or(Value::Null),
        "usage": response.get("usage").cloned().unwrap_or(Value::Null),
    });
    for field in ["toolCalls", "iterations", "durationMs", "sessionId"] {
        if let Some(value) = response.get(field) {
            body[field] = value.clone();
        }
    }
    body
}

async fn agent_chat(iii: &IIIClient, body: &Value, message: &str) -> Result<Value, Error> {
    iii.trigger(chat_trigger(
        "agent::chat",
        agent_chat_payload(body, message),
    ))
    .await
    .map_err(|error| Error::Handler(error.to_string()))
}

/// `POST /api/chat/stream`. One pipeline: `agent::chat` owns tools, the
/// injection scan, memory and metering.
async fn stream_chat(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let body = payload_body(&input);
    let message = chat_message(&body)?;
    let response = agent_chat(iii, &body, &message).await?;
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

    let response = agent_chat(iii, &body, &message).await?;

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

/// SSE events for a finished answer. Pure so the framing can be asserted
/// without a bus.
fn sse_events(content: &str, model: &str) -> Result<String, Error> {
    let chunks = chunk_markdown_aware(content, 20, 100);
    let chunks_len = chunks.len();
    let created = chrono::Utc::now().timestamp();

    let mut sse_body = String::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let finish_reason = if index + 1 == chunks_len {
            json!("stop")
        } else {
            Value::Null
        };
        let delta = if index == 0 {
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
        let encoded =
            serde_json::to_string(&event).map_err(|error| Error::Handler(error.to_string()))?;
        sse_body.push_str("data: ");
        sse_body.push_str(&encoded);
        sse_body.push_str("\n\n");
    }
    sse_body.push_str("data: [DONE]\n\n");
    Ok(sse_body)
}

fn sse_envelope(sse_body: String) -> Value {
    json!({
        "status_code": 200,
        "headers": {
            "Content-Type": "text/event-stream",
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
            // The events are framed after the answer is complete. Say so
            // instead of letting a client believe it is reading tokens live.
            "x-agentos-stream": "buffered",
        },
        "body": sse_body,
    })
}

/// `POST /v1/chat/completions/stream`. SSE framing of an answer that is already
/// complete — see the module note; the response labels itself `buffered`.
async fn stream_sse(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let body = payload_body(&input);
    let message = chat_message(&body)?;
    let response = agent_chat(iii, &body, &message).await?;

    let content = response["content"].as_str().unwrap_or("");
    let model = response["model"].as_str().unwrap_or("agentos-default");

    Ok(sse_envelope(sse_events(content, model)?))
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
        .description("Chat endpoint backed by agent::chat (buffered, not incremental)"),
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
        .description("SSE framing of a completed agent::chat answer (buffered, not incremental)"),
    );

    iii.register_function(
        STREAM_JOIN_FUNCTION,
        RegisterFunction::new_async(|input: Value| async move {
            Ok::<Value, Error>(stream_join_decision(&input))
        })
        .description(
            "Authorize an iii-stream WebSocket join; refuses any join whose connection \
             carries no authenticated auth context",
        ),
    );

    // A failure here must not take the chat transport down: the engine keeps
    // serving the stream port either way, so refusing to boot would close
    // nothing and cost chat. Log it loudly instead.
    if let Err(error) = iii.register_trigger(stream_join_trigger()) {
        tracing::error!(
            error = %error,
            trigger_type = STREAM_JOIN_TRIGGER_TYPE,
            "could not register the stream join gate; iii-stream joins are NOT authorized by this worker"
        );
    }

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
    use super::*;
    use serde_json::json;

    #[test]
    fn every_chat_hop_carries_the_full_turn_budget() {
        // The SDK default is 30_000 ms and it applied to the whole outer turn:
        // a ReAct loop with tools could not finish inside it.
        let trigger = chat_trigger("agent::chat", json!({ "message": "hello" }));
        assert_eq!(trigger.function_id, "agent::chat");
        assert_eq!(trigger.timeout_ms, Some(CHAT_TIMEOUT_MS));
        assert_eq!(CHAT_TIMEOUT_MS, 300_000);
        assert!(trigger.timeout_ms.is_some_and(|budget| budget > 30_000));
    }

    #[test]
    fn agent_chat_payload_forwards_route_preferences() {
        let payload = agent_chat_payload(
            &json!({
                "agentId": "agent-1",
                "provider": "codex",
                "model": "gpt-5.6-sol",
            }),
            "hello",
        );
        assert_eq!(payload["agentId"], "agent-1");
        assert_eq!(payload["message"], "hello");
        assert_eq!(payload["provider"], "codex");
        assert_eq!(payload["model"], "gpt-5.6-sol");
    }

    #[test]
    fn agent_chat_payload_carries_the_session_and_defaults_the_agent() {
        let with_session = agent_chat_payload(
            &json!({ "agentId": "agent-1", "sessionId": "tui-1", "systemPrompt": "be brief" }),
            "hello",
        );
        assert_eq!(with_session["sessionId"], "tui-1");
        assert_eq!(with_session["systemPrompt"], "be brief");

        let without_session = agent_chat_payload(&json!({}), "hello");
        assert_eq!(without_session["agentId"], "default");
        assert_eq!(without_session["sessionId"], Value::Null);
        assert_eq!(without_session["provider"], Value::Null);
    }

    #[test]
    fn chat_message_rejects_absent_empty_and_blank_messages() {
        for body in [
            json!({}),
            json!({ "message": "" }),
            json!({ "message": "   " }),
            json!({ "message": 42 }),
            json!({ "message": { "text": "hello" } }),
        ] {
            assert!(chat_message(&body).is_err(), "accepted {body}");
        }
        assert_eq!(chat_message(&json!({ "message": " hi " })).unwrap(), "hi");
    }

    #[test]
    fn stream_chat_response_keeps_tool_calls() {
        // The second pipeline dropped `toolCalls`, so a turn that answered with
        // a tool call reached the client as an empty assistant bubble.
        let response = stream_chat_response(&json!({
            "content": "",
            "model": "test-model",
            "usage": { "total": 7 },
            "toolCalls": [{ "callId": "call-1", "id": "memory::recall", "arguments": { "query": "x" } }],
            "iterations": 2,
            "durationMs": 1234,
        }));

        assert_eq!(
            response["toolCalls"],
            json!([{ "callId": "call-1", "id": "memory::recall", "arguments": { "query": "x" } }])
        );
        assert_eq!(response["iterations"], 2);
        assert_eq!(response["durationMs"], 1234);
        assert_eq!(response["usage"], json!({ "total": 7 }));
    }

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
        assert!(response.get("toolCalls").is_none());
    }

    #[test]
    fn stream_chat_response_reports_a_missing_answer_as_null_not_as_a_silent_string() {
        let response = stream_chat_response(&json!({}));
        assert_eq!(response["content"], Value::Null);
        assert_eq!(response["model"], Value::Null);
        assert_eq!(response["usage"], Value::Null);
    }

    #[test]
    fn sse_framing_declares_that_it_is_buffered() {
        let envelope = sse_envelope(sse_events("hello", "test-model").expect("events"));
        assert_eq!(envelope["headers"]["x-agentos-stream"], "buffered");
        assert_eq!(envelope["headers"]["Content-Type"], "text/event-stream");
        let body = envelope["body"].as_str().expect("body");
        assert!(body.ends_with("data: [DONE]\n\n"), "{body}");
        assert!(body.contains("\"role\":\"assistant\""), "{body}");
        assert!(body.contains("\"finish_reason\":\"stop\""), "{body}");
    }

    #[test]
    fn sse_framing_closes_the_last_chunk_of_a_long_answer() {
        let long = "word ".repeat(200);
        let body = sse_events(&long, "test-model").expect("events");
        let events: Vec<&str> = body
            .split("\n\n")
            .filter(|event| !event.is_empty() && *event != "data: [DONE]")
            .collect();
        assert!(events.len() > 1, "expected several chunks, got {events:?}");
        let stops = events
            .iter()
            .filter(|event| event.contains("\"finish_reason\":\"stop\""))
            .count();
        assert_eq!(stops, 1, "exactly the last chunk closes the stream");
        assert!(
            events
                .last()
                .expect("last event")
                .contains("\"finish_reason\":\"stop\"")
        );
    }

    #[test]
    fn stream_join_is_refused_without_an_authenticated_context() {
        // engine 0.22.1 upgrades the socket even when the auth function fails,
        // leaving `context: null`. That join must not subscribe.
        for event in [
            json!({ "subscription_id": "s1", "stream_name": "chat", "group_id": "g" }),
            json!({ "stream_name": "chat", "context": Value::Null }),
            json!({ "stream_name": "chat", "context": "agentos-bearer" }),
            json!({ "stream_name": "chat", "context": [] }),
            json!({ "stream_name": "chat", "context": { "subject": "agentos-bearer" } }),
            json!({ "stream_name": "chat", "context": { "authenticated": true } }),
            json!({ "stream_name": "chat", "context": { "authenticated": "true", "subject": "x" } }),
            json!({ "stream_name": "chat", "context": { "authenticated": 1, "subject": "x" } }),
            json!({ "stream_name": "chat", "context": { "authenticated": false, "subject": "x" } }),
            json!({ "stream_name": "chat", "context": { "authenticated": true, "subject": "" } }),
            json!({ "stream_name": "chat", "context": { "authenticated": true, "subject": "   " } }),
            json!({ "stream_name": "chat", "context": { "authenticated": true, "subject": 7 } }),
        ] {
            assert_eq!(
                stream_join_decision(&event),
                json!({ "unauthorized": true }),
                "allowed {event}"
            );
        }
    }

    #[test]
    fn stream_join_is_allowed_for_an_authenticated_connection() {
        // The context `security::stream_auth` returns, as the engine hands it on.
        let authenticated = json!({
            "authenticated": true,
            "subject": "agentos-bearer",
            "addr": "127.0.0.1:51000",
        });

        assert_eq!(
            stream_join_decision(&json!({
                "subscription_id": "s1",
                "stream_name": "chat",
                "group_id": "g",
                "context": authenticated,
            })),
            json!({ "unauthorized": false })
        );

        // Same verdict if the engine wraps the event payload in `data`.
        assert_eq!(
            stream_join_decision(&json!({
                "type": "stream:join",
                "data": { "stream_name": "chat", "context": authenticated },
            })),
            json!({ "unauthorized": false })
        );
    }

    #[test]
    fn stream_join_gate_is_registered_for_every_stream() {
        let trigger = stream_join_trigger();
        assert_eq!(trigger.trigger_type, "stream:join");
        assert_eq!(trigger.function_id, STREAM_JOIN_FUNCTION);
        // No `stream_name` filter: the engine matches join triggers on type
        // only, so an empty config covers every stream.
        assert_eq!(trigger.config, json!({}));
        assert!(trigger.metadata.is_none());
    }
}
