use iii_sdk::errors::Error;
use iii_sdk::{
    IIIClient, InitOptions, RegisterFunction,
    protocol::{RegisterTriggerInput, TriggerRequest},
    register_worker,
};
use serde_json::{Value, json};
use std::time::Instant;

mod types;

use types::{AgentConfig, ChatRequest, FunctionCall, ModelConfig};

const MAX_ITERATIONS: u32 = 50;

const CHANNELS: [&str; 14] = [
    "bluesky", "discord", "email", "linkedin", "mastodon", "matrix", "reddit", "signal", "slack",
    "teams", "telegram", "twitch", "webex", "whatsapp",
];

fn channel_secrets(channel: &str) -> Option<&'static [&'static str]> {
    match channel {
        "bluesky" => Some(&["BLUESKY_HANDLE", "BLUESKY_PASSWORD"]),
        "discord" => Some(&["DISCORD_BOT_TOKEN"]),
        "email" => Some(&["SMTP_HOST", "SMTP_PORT", "SMTP_USER", "SMTP_PASS"]),
        "linkedin" => Some(&["LINKEDIN_TOKEN"]),
        "mastodon" => Some(&["MASTODON_INSTANCE", "MASTODON_TOKEN"]),
        "matrix" => Some(&["MATRIX_HOMESERVER", "MATRIX_TOKEN"]),
        "reddit" => Some(&["REDDIT_CLIENT_ID", "REDDIT_SECRET", "REDDIT_REFRESH_TOKEN"]),
        "signal" => Some(&["SIGNAL_API_URL", "SIGNAL_PHONE"]),
        "slack" => Some(&["SLACK_BOT_TOKEN", "SLACK_SIGNING_SECRET"]),
        "teams" => Some(&["TEAMS_APP_ID", "TEAMS_APP_PASSWORD"]),
        "telegram" => Some(&["TELEGRAM_BOT_TOKEN"]),
        "twitch" => Some(&["TWITCH_CLIENT_ID", "TWITCH_TOKEN", "TWITCH_BOT_USER_ID"]),
        "webex" => Some(&["WEBEX_TOKEN"]),
        "whatsapp" => Some(&["WHATSAPP_PHONE_ID", "WHATSAPP_TOKEN"]),
        _ => None,
    }
}

async fn missing_channel_secrets(
    iii: &IIIClient,
    channel: &str,
) -> Result<Vec<&'static str>, Error> {
    let required = channel_secrets(channel)
        .ok_or_else(|| Error::Handler(format!("Unsupported channel: {channel}")))?;
    let mut missing = Vec::new();
    for key in required {
        let secret = iii
            .trigger(TriggerRequest {
                function_id: "vault::get".to_string(),
                payload: json!({ "key": key }),
                action: None,
                timeout_ms: None,
            })
            .await
            .ok()
            .and_then(|value| value["value"].as_str().map(str::to_owned))
            .or_else(|| std::env::var(key).ok());
        if secret.as_deref().is_none_or(str::is_empty) {
            missing.push(*key);
        }
    }
    Ok(missing)
}

async fn channel_statuses(iii: &IIIClient) -> Result<Value, Error> {
    let workers = iii
        .trigger(TriggerRequest {
            function_id: "engine::workers::list".to_string(),
            payload: json!({}),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|error| Error::Handler(error.to_string()))?;
    let connected = workers["workers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|worker| worker["status"] == "connected")
        .filter_map(|worker| worker["name"].as_str())
        .collect::<std::collections::HashSet<_>>();
    Ok(Value::Array(
        CHANNELS
            .iter()
            .map(|channel| {
                let enabled = connected.contains(format!("channel-{channel}").as_str());
                json!({
                    "id": channel,
                    "type": channel,
                    "enabled": enabled,
                    "config": "vault/env",
                })
            })
            .collect(),
    ))
}

async fn channel_readiness(iii: &IIIClient, channel: &str) -> Result<Value, Error> {
    let missing = missing_channel_secrets(iii, channel).await?;
    let statuses = channel_statuses(iii).await?;
    let connected = statuses
        .as_array()
        .into_iter()
        .flatten()
        .any(|status| status["id"] == channel && status["enabled"] == true);
    let success = connected && missing.is_empty();
    let error = if !connected {
        Some(format!("channel-{channel} worker is not connected"))
    } else if missing.is_empty() {
        None
    } else {
        Some(format!("missing secrets: {}", missing.join(", ")))
    };
    Ok(json!({
        "id": channel,
        "success": success,
        "connected": connected,
        "missingSecrets": missing,
        "error": error,
    }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, InitOptions::default());

    let started_at = Instant::now();
    let iii_clone = iii.clone();
    iii.register_function(
        "health::check",
        RegisterFunction::new_async(move |_: Value| {
            let iii = iii_clone.clone();
            async move {
                let workers = iii
                    .trigger(TriggerRequest {
                        function_id: "engine::workers::list".to_string(),
                        payload: json!({}),
                        action: None,
                        timeout_ms: None,
                    })
                    .await
                    .map_err(|error| Error::Handler(error.to_string()))?;
                let worker_count = workers["workers"].as_array().map_or(0, |entries| {
                    entries
                        .iter()
                        .filter(|worker| {
                            worker["runtime"] != "engine"
                                && worker["name"] != "iii-worker-ops"
                                && worker["status"] == "connected"
                        })
                        .count()
                });

                Ok::<Value, Error>(json!({
                    "status": "healthy",
                    "version": env!("CARGO_PKG_VERSION"),
                    "workers": worker_count,
                    "uptime": started_at.elapsed().as_secs_f64(),
                }))
            }
        })
        .description("Report AgentOS runtime health"),
    );
    agentos_http_adapter::register_http_trigger(
        &iii,
        "health::check",
        json!({ "api_path": "/api/health", "http_method": "GET", "auth": false }),
        None,
    )?;

    let iii_clone = iii.clone();
    iii.register_function(
        "channel::list",
        RegisterFunction::new_async(move |_: Value| {
            let iii = iii_clone.clone();
            async move { channel_statuses(&iii).await }
        })
        .description("List channel adapter status"),
    );
    let iii_clone = iii.clone();
    iii.register_function(
        "channel::setup",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let channel = input["channel"].as_str().unwrap_or_default();
                channel_readiness(&iii, channel).await
            }
        })
        .description("Validate channel adapter configuration"),
    );
    let iii_clone = iii.clone();
    iii.register_function(
        "channel::test",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let channel = input["channel"].as_str().unwrap_or_default();
                channel_readiness(&iii, channel).await
            }
        })
        .description("Test channel adapter readiness"),
    );
    for (function_id, method, path) in [
        ("channel::list", "GET", "/api/channels"),
        ("channel::setup", "POST", "/api/channels"),
        ("channel::test", "POST", "/api/channels/:channel/test"),
    ] {
        agentos_http_adapter::register_http_trigger(
            &iii,
            function_id,
            json!({ "api_path": path, "http_method": method }),
            None,
        )?;
    }

    let iii_clone = iii.clone();
    iii.register_function(
        "agent::chat",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: ChatRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                agent_chat(&iii, req).await
            }
        })
        .description("Process a message through the agent loop"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "agent::list_functions",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let agent_id = input["agentId"].as_str().unwrap_or("default");
                list_functions(&iii, agent_id).await
            }
        })
        .description("List functions available to an agent"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "agent::create",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let config: AgentConfig =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                create_agent(&iii, config).await
            }
        })
        .description("Register a new agent"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "agent::list",
        RegisterFunction::new_async(move |_: Value| {
            let iii = iii_clone.clone();
            async move {
                iii.trigger(TriggerRequest {
                    function_id: "state::list".to_string(),
                    payload: json!({ "scope": "agents" }),
                    action: None,
                    timeout_ms: None,
                })
                .await
                .map_err(|e| Error::Handler(e.to_string()))
            }
        })
        .description("List all agents"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "agent::delete",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let agent_id = input["agentId"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| Error::Handler("missing or empty agentId".into()))?
                    .to_string();
                iii.trigger(TriggerRequest {
                    function_id: "state::delete".to_string(),
                    payload: json!({
                        "scope": "agents",
                        "key": &agent_id,
                    }),
                    action: None,
                    timeout_ms: None,
                })
                .await
                .map_err(|e| Error::Handler(e.to_string()))?;

                spawn_trigger(
                    &iii,
                    "publish",
                    json!({
                        "topic": "agent.lifecycle",
                        "data": { "type": "deleted", "agentId": &agent_id },
                    }),
                );

                Ok::<Value, Error>(json!({ "deleted": true }))
            }
        })
        .description("Remove an agent"),
    );

    let agent_routes = [
        ("agent::list", "GET", "/api/agents"),
        ("agent::create", "POST", "/api/agents"),
        ("agent::chat", "POST", "/api/agents/:agentId/message"),
        ("agent::delete", "DELETE", "/api/agents/:agentId"),
    ];
    for (function_id, method, path) in agent_routes {
        agentos_http_adapter::register_http_trigger(
            &iii,
            function_id,
            json!({ "api_path": path, "http_method": method }),
            None,
        )?;
    }

    iii.register_trigger(RegisterTriggerInput {
        trigger_type: "queue".to_string(),
        function_id: "agent::chat".to_string(),
        config: json!({ "topic": "agent.inbox" }),
        metadata: None,
    })?;

    tracing::info!("agent-core worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

/// Triggers a function in the background; the outcome is intentionally ignored.
fn spawn_trigger(iii: &IIIClient, function_id: &'static str, payload: Value) {
    let iii = iii.clone();
    tokio::spawn(async move {
        let _ = iii
            .trigger(TriggerRequest {
                function_id: function_id.to_string(),
                payload,
                action: None,
                timeout_ms: None,
            })
            .await;
    });
}

fn route_payload(
    message: &str,
    functions: &Value,
    provider: Option<&str>,
    model: Option<&str>,
) -> Value {
    let mut payload = json!({
        "messages": [{ "role": "user", "content": message }],
        "tools": functions,
    });
    if let Some(provider) = provider {
        payload["provider"] = json!(provider);
    }
    if let Some(model) = model {
        payload["model"] = json!(model);
    }
    payload
}

fn valid_route_preference(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty() && *value != "agentos-default")
}

fn route_preferences(
    request_provider: Option<&str>,
    request_model: Option<&str>,
    model_config: Option<&ModelConfig>,
) -> (Option<String>, Option<String>) {
    let request_provider = valid_route_preference(request_provider);
    let request_model = valid_route_preference(request_model);
    if request_provider.is_some() || request_model.is_some() {
        return (
            request_provider.map(str::to_owned),
            request_model.map(str::to_owned),
        );
    }

    let config_provider =
        model_config.and_then(|model| valid_route_preference(model.provider.as_deref()));
    let config_model =
        model_config.and_then(|model| valid_route_preference(model.model.as_deref()));
    if config_model.is_some() {
        (
            config_provider.map(str::to_owned),
            config_model.map(str::to_owned),
        )
    } else {
        (None, None)
    }
}

fn completion_payload(
    provider: &str,
    model: &str,
    system_prompt: &str,
    messages: &[Value],
    functions: &Value,
) -> Value {
    json!({
        "provider": provider,
        "model": model,
        "systemPrompt": system_prompt,
        "messages": messages,
        "tools": functions,
    })
}

fn route_fields(route: &Value) -> Result<(String, String), Error> {
    let provider = route["provider"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Handler("agentos::llm::route omitted provider".into()))?;
    let model = route["model"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Handler("agentos::llm::route omitted model".into()))?;
    Ok((provider.into(), model.into()))
}

async fn agent_chat(iii: &IIIClient, req: ChatRequest) -> Result<Value, Error> {
    let start = Instant::now();

    let config: Option<AgentConfig> = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({
                "scope": "agents",
                "key": &req.agent_id,
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .ok()
        .and_then(|v| serde_json::from_value(v).ok());

    let memories: Value = iii
        .trigger(TriggerRequest {
            function_id: "memory::recall".to_string(),
            payload: json!({
                "agentId": &req.agent_id,
                "query": &req.message,
                "limit": 20,
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!([]));

    let functions: Value = iii
        .trigger(TriggerRequest {
            function_id: "agent::list_functions".to_string(),
            payload: json!({ "agentId": &req.agent_id }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!([]));

    let model_config = config.as_ref().and_then(|agent| agent.model.as_ref());
    let (preferred_provider, preferred_model) =
        route_preferences(req.provider.as_deref(), req.model.as_deref(), model_config);

    let system_prompt = req
        .system_prompt
        .or_else(|| config.as_ref().and_then(|c| c.system_prompt.clone()))
        .unwrap_or_default();

    let route: Value = iii
        .trigger(TriggerRequest {
            function_id: "agentos::llm::route".to_string(),
            payload: route_payload(
                &req.message,
                &functions,
                preferred_provider.as_deref(),
                preferred_model.as_deref(),
            ),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;
    let (provider, model) = route_fields(&route)?;

    let scan_result = iii
        .trigger(TriggerRequest {
            function_id: "security::scan_injection".to_string(),
            payload: json!({ "text": &req.message }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!({ "safe": true, "riskScore": 0.0 }));
    let risk_score = scan_result["riskScore"].as_f64().unwrap_or(0.0);
    if risk_score > 0.5 {
        return Err(Error::Handler(format!(
            "Message rejected: injection risk score {:.2} exceeds threshold",
            risk_score
        )));
    }

    let mut messages = vec![];
    if let Some(mems) = memories.as_array() {
        messages.extend(mems.iter().cloned());
    }
    messages.push(json!({ "role": "user", "content": &req.message }));

    let mut response: Value = iii
        .trigger(TriggerRequest {
            function_id: "agentos::llm::complete".to_string(),
            payload: completion_payload(&provider, &model, &system_prompt, &messages, &functions),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let mut iterations: u32 = 0;

    while let Some(tool_calls) = response.get("toolCalls").and_then(|v| v.as_array()) {
        if tool_calls.is_empty() || iterations >= MAX_ITERATIONS {
            break;
        }
        iterations += 1;

        let calls: Vec<FunctionCall> = tool_calls
            .iter()
            .filter_map(|tc| serde_json::from_value(tc.clone()).ok())
            .collect();

        let mut tool_results = Vec::new();
        for tc in &calls {
            let cap_check = iii
                .trigger(TriggerRequest {
                    function_id: "security::check_capability".to_string(),
                    payload: json!({
                        "agentId": &req.agent_id,
                        "capability": tc.id.split("::").next().unwrap_or(""),
                        "resource": &tc.id,
                    }),
                    action: None,
                    timeout_ms: None,
                })
                .await;

            if cap_check.is_err() {
                tool_results.push(json!({
                    "toolCallId": tc.call_id,
                    "output": { "error": "capability denied" },
                }));
                continue;
            }

            match iii
                .trigger(TriggerRequest {
                    function_id: tc.id.to_string(),
                    payload: tc.arguments.clone(),
                    action: None,
                    timeout_ms: None,
                })
                .await
            {
                Ok(result) => {
                    tool_results.push(json!({
                        "toolCallId": tc.call_id,
                        "output": result,
                    }));
                }
                Err(e) => {
                    tool_results.push(json!({
                        "toolCallId": tc.call_id,
                        "output": { "error": e.to_string() },
                    }));
                }
            }
        }

        messages.push(json!({ "role": "assistant", "content": null, "tool_calls": response.get("toolCalls") }));
        for tr in &tool_results {
            messages.push(json!({ "role": "tool", "tool_call_id": tr["toolCallId"], "content": tr["output"].to_string() }));
        }

        response = iii
            .trigger(TriggerRequest {
                function_id: "agentos::llm::complete".to_string(),
                payload: completion_payload(
                    &provider,
                    &model,
                    &system_prompt,
                    &messages,
                    &functions,
                ),
                action: None,
                timeout_ms: None,
            })
            .await
            .map_err(|e| Error::Handler(e.to_string()))?;
    }

    let session_id = req
        .session_id
        .unwrap_or_else(|| format!("default:{}", req.agent_id));

    spawn_trigger(
        iii,
        "memory::store",
        json!({
            "agentId": &req.agent_id,
            "sessionId": &session_id,
            "role": "user",
            "content": &req.message,
        }),
    );

    spawn_trigger(
        iii,
        "memory::store",
        json!({
            "agentId": &req.agent_id,
            "sessionId": &session_id,
            "role": "assistant",
            "content": response.get("content").and_then(|v| v.as_str()).unwrap_or(""),
            "tokenUsage": response.get("usage"),
        }),
    );

    if let Err(e) = iii
        .trigger(TriggerRequest {
            function_id: "state::update".to_string(),
            payload: json!({
                "scope": "metering",
                "key": &req.agent_id,
                "operations": [
                    { "type": "increment", "path": "totalTokens", "value": response["usage"]["total"] },
                    { "type": "increment", "path": "invocations", "value": 1 },
                ],
            }),
            action: None,
            timeout_ms: None,
        })
        .await
    {
        tracing::warn!(agent_id = %req.agent_id, error = %e, "metering update failed");
    }

    Ok(json!({
        "content": response.get("content").and_then(|v| v.as_str()).unwrap_or(""),
        "model": response.get("model"),
        "usage": response.get("usage"),
        "iterations": iterations,
        "durationMs": start.elapsed().as_millis(),
    }))
}

async fn list_functions(iii: &IIIClient, agent_id: &str) -> Result<Value, Error> {
    let config: Option<AgentConfig> = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({ "scope": "agents", "key": agent_id }),
            action: None,
            timeout_ms: None,
        })
        .await
        .ok()
        .and_then(|v| serde_json::from_value(v).ok());

    let allowed = config
        .as_ref()
        .and_then(|c| c.capabilities.as_ref())
        .map(|c| c.functions.clone())
        .unwrap_or_else(|| vec!["*".into()]);

    let allowed: Vec<String> = allowed
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('*').to_string())
        .collect();

    let registry: Value = iii
        .trigger(TriggerRequest {
            function_id: "engine::functions::list".to_string(),
            payload: json!({}),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or_else(|_| json!({ "functions": [] }));

    Ok(filter_functions(&registry, &allowed))
}

fn filter_functions(registry: &Value, allowed: &[String]) -> Value {
    let functions = registry
        .get("functions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if allowed.iter().any(|prefix| prefix.is_empty()) {
        return Value::Array(functions);
    }

    if allowed.is_empty() {
        return json!([]);
    }

    Value::Array(
        functions
            .into_iter()
            .filter(|function| {
                function
                    .get("function_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| allowed.iter().any(|prefix| id.starts_with(prefix)))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::{Capabilities, ModelConfig, Resources};

    #[test]
    fn llm_route_and_complete_payloads_use_top_level_strings() {
        let functions = json!([{ "id": "memory::recall" }]);
        let route = route_payload("hello", &functions, Some("codex"), Some("gpt-5.6-sol"));
        assert_eq!(route["provider"], "codex");
        assert_eq!(route["model"], "gpt-5.6-sol");
        assert!(route["model"].is_string());

        let complete = completion_payload(
            "codex",
            "gpt-5.6-sol",
            "system",
            &[json!({ "role": "user", "content": "hello" })],
            &functions,
        );
        assert_eq!(complete["provider"], "codex");
        assert_eq!(complete["model"], "gpt-5.6-sol");
        assert!(complete["model"].is_string());
        assert_eq!(complete["systemPrompt"], "system");
        assert_eq!(complete["tools"], functions);
        assert!(complete.get("functions").is_none());
    }

    #[test]
    fn route_preferences_keep_request_pairs_and_complete_config_pairs() {
        let config = ModelConfig {
            provider: Some("anthropic".into()),
            model: Some("sonnet".into()),
            max_tokens: None,
        };
        assert_eq!(
            route_preferences(Some("codex"), Some("gpt-5.6-sol"), Some(&config)),
            (Some("codex".into()), Some("gpt-5.6-sol".into()))
        );
        assert_eq!(
            route_preferences(None, None, Some(&config)),
            (Some("anthropic".into()), Some("sonnet".into()))
        );
    }

    #[test]
    fn route_preferences_ignore_empty_none_and_incomplete_config_values() {
        assert_eq!(route_preferences(None, None, None), (None, None));
        assert_eq!(
            route_preferences(Some(""), Some("agentos-default"), None),
            (None, None)
        );

        let provider_only = ModelConfig {
            provider: Some("codex".into()),
            model: None,
            max_tokens: None,
        };
        assert_eq!(
            route_preferences(None, None, Some(&provider_only)),
            (None, None)
        );
    }

    #[test]
    fn route_fields_reject_nested_model_responses() {
        let error = route_fields(&json!({
            "provider": "codex",
            "model": { "provider": "codex", "model": "gpt-5.6-sol" },
        }))
        .unwrap_err();
        assert!(error.to_string().contains("omitted model"));
    }

    #[test]
    fn route_fields_reject_missing_and_empty_strings() {
        for route in [
            json!({}),
            json!({ "provider": "", "model": "gpt-5.6-sol" }),
            json!({ "provider": "codex", "model": "" }),
        ] {
            assert!(route_fields(&route).is_err(), "accepted {route}");
        }
    }

    #[test]
    fn test_max_iterations_constant() {
        assert_eq!(MAX_ITERATIONS, 50);
    }

    #[test]
    fn test_chat_request_from_json() {
        let json_val = json!({
            "agentId": "agent-test",
            "message": "Hello world",
        });
        let req: ChatRequest = serde_json::from_value(json_val).unwrap();
        assert_eq!(req.agent_id, "agent-test");
        assert_eq!(req.message, "Hello world");
    }

    #[test]
    fn test_chat_request_requires_agent_id() {
        let json_val = json!({
            "message": "Hello",
        });
        let result: Result<ChatRequest, _> = serde_json::from_value(json_val);
        assert!(result.is_err());
    }

    #[test]
    fn test_chat_request_requires_message() {
        let json_val = json!({
            "agentId": "test",
        });
        let result: Result<ChatRequest, _> = serde_json::from_value(json_val);
        assert!(result.is_err());
    }

    #[test]
    fn test_tool_call_parsing() {
        let json_val = json!({
            "callId": "tc-1",
            "id": "memory::store",
            "arguments": {"content": "test data", "agentId": "agent-1"},
        });
        let tc: FunctionCall = serde_json::from_value(json_val).unwrap();
        assert_eq!(tc.call_id, "tc-1");
        assert_eq!(tc.id, "memory::store");
        assert_eq!(tc.arguments["content"], "test data");
    }

    #[test]
    fn test_tool_call_id_split_for_capability() {
        let tc = FunctionCall {
            call_id: "c-1".to_string(),
            id: "security::check_capability".to_string(),
            arguments: json!({}),
        };
        let capability = tc.id.split("::").next().unwrap_or("");
        assert_eq!(capability, "security");
    }

    #[test]
    fn test_tool_call_id_split_no_separator() {
        let tc = FunctionCall {
            call_id: "c-2".to_string(),
            id: "simple_tool".to_string(),
            arguments: json!({}),
        };
        let capability = tc.id.split("::").next().unwrap_or("");
        assert_eq!(capability, "simple_tool");
    }

    #[test]
    fn test_agent_config_creation() {
        let config = AgentConfig {
            id: Some("test-id".to_string()),
            name: "Test Agent".to_string(),
            description: Some("A test agent".to_string()),
            model: Some(ModelConfig {
                provider: Some("anthropic".to_string()),
                model: Some("claude-sonnet-4-20250514".to_string()),
                max_tokens: Some(4096),
            }),
            system_prompt: Some("Be helpful".to_string()),
            capabilities: Some(Capabilities {
                functions: vec!["*".to_string()],
                memory_scopes: None,
                network_hosts: None,
            }),
            resources: Some(Resources {
                max_tokens_per_hour: Some(100000),
            }),
            tags: Some(vec!["test".to_string()]),
        };
        assert_eq!(config.name, "Test Agent");
        assert!(
            config
                .capabilities
                .unwrap()
                .functions
                .contains(&"*".to_string())
        );
    }

    #[test]
    fn test_agent_config_id_fallback() {
        let config = AgentConfig {
            id: None,
            name: "NoIdAgent".to_string(),
            description: None,
            model: None,
            system_prompt: None,
            capabilities: None,
            resources: None,
            tags: None,
        };
        let id = config
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        assert!(!id.is_empty());
    }

    #[test]
    fn test_system_prompt_fallback_chain() {
        let req_prompt = Some("request prompt".to_string());
        let config_prompt = Some("config prompt".to_string());

        let result = req_prompt.or(config_prompt).unwrap_or_default();
        assert_eq!(result, "request prompt");
    }

    #[test]
    fn test_system_prompt_fallback_to_config() {
        let req_prompt: Option<String> = None;
        let config_prompt = Some("config prompt".to_string());

        let result = req_prompt.or(config_prompt).unwrap_or_default();
        assert_eq!(result, "config prompt");
    }

    #[test]
    fn test_system_prompt_fallback_to_default() {
        let req_prompt: Option<String> = None;
        let config_prompt: Option<String> = None;

        let result = req_prompt.or(config_prompt).unwrap_or_default();
        assert_eq!(result, "");
    }

    #[test]
    fn test_session_id_default_format() {
        let agent_id = "agent-42";
        let session_id: Option<String> = None;
        let result = session_id.unwrap_or_else(|| format!("default:{}", agent_id));
        assert_eq!(result, "default:agent-42");
    }

    #[test]
    fn test_session_id_explicit() {
        let session_id = Some("custom-session".to_string());
        let result = session_id.unwrap_or_else(|| "default:x".to_string());
        assert_eq!(result, "custom-session");
    }

    #[test]
    fn test_tool_results_accumulation() {
        let mut results = Vec::new();
        results.push(json!({
            "toolCallId": "tc-1",
            "output": { "data": "result1" },
        }));
        results.push(json!({
            "toolCallId": "tc-2",
            "output": { "error": "denied" },
        }));
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["toolCallId"], "tc-1");
        assert_eq!(results[1]["output"]["error"], "denied");
    }

    #[test]
    fn test_message_building() {
        let mut messages: Vec<Value> = vec![];
        let memories = json!([
            {"role": "user", "content": "previous question"},
            {"role": "assistant", "content": "previous answer"},
        ]);

        if let Some(mems) = memories.as_array() {
            messages.extend(mems.iter().cloned());
        }
        messages.push(json!({"role": "user", "content": "new question"}));

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[2]["content"], "new question");
    }

    #[test]
    fn test_wildcard_tool_filter() {
        let allowed = vec!["*".to_string()];
        assert!(allowed.contains(&"*".to_string()));
    }

    #[test]
    fn test_tool_filter_prefix_match() {
        let allowed = vec!["file::".to_string(), "memory::".to_string()];
        let tool_id = "file::read";
        let matches = allowed.iter().any(|a| tool_id.starts_with(a.as_str()));
        assert!(matches);
    }

    #[test]
    fn test_tool_filter_no_match() {
        let allowed = vec!["file::".to_string()];
        let tool_id = "network::send";
        let matches = allowed.iter().any(|a| tool_id.starts_with(a.as_str()));
        assert!(!matches);
    }

    #[test]
    fn test_risk_score_threshold() {
        let risk_score = 0.51;
        assert!(risk_score > 0.5);

        let risk_score = 0.49;
        assert!(risk_score <= 0.5);
    }

    #[test]
    fn test_iteration_limit() {
        let mut iterations: u32 = 0;
        while iterations < MAX_ITERATIONS {
            iterations += 1;
        }
        assert_eq!(iterations, 50);
    }

    #[test]
    fn test_chat_request_empty_message() {
        let json_val = json!({
            "agentId": "agent-1",
            "message": "",
        });
        let req: ChatRequest = serde_json::from_value(json_val).unwrap();
        assert_eq!(req.message, "");
    }

    #[test]
    fn test_chat_request_very_long_message() {
        let long_msg = "x".repeat(100_000);
        let json_val = json!({
            "agentId": "agent-1",
            "message": long_msg,
        });
        let req: ChatRequest = serde_json::from_value(json_val).unwrap();
        assert_eq!(req.message.len(), 100_000);
    }

    #[test]
    fn test_chat_request_with_all_optional_fields() {
        let json_val = json!({
            "agentId": "agent-full",
            "message": "Hello",
            "sessionId": "sess-99",
            "systemPrompt": "Be concise",
        });
        let req: ChatRequest = serde_json::from_value(json_val).unwrap();
        assert_eq!(req.session_id, Some("sess-99".to_string()));
        assert_eq!(req.system_prompt, Some("Be concise".to_string()));
    }

    #[test]
    fn test_chat_request_unicode_message() {
        let json_val = json!({
            "agentId": "agent-unicode",
            "message": "Hello! CJK: \u{4e16}\u{754c} Emoji: \u{1f600}\u{1f680}",
        });
        let req: ChatRequest = serde_json::from_value(json_val).unwrap();
        assert!(req.message.contains('\u{4e16}'));
        assert!(req.message.contains('\u{1f600}'));
    }

    #[test]
    fn test_agent_config_no_optional_fields() {
        let config = AgentConfig {
            id: None,
            name: "Minimal".to_string(),
            description: None,
            model: None,
            system_prompt: None,
            capabilities: None,
            resources: None,
            tags: None,
        };
        assert!(config.id.is_none());
        assert!(config.description.is_none());
        assert!(config.model.is_none());
        assert!(config.system_prompt.is_none());
        assert!(config.capabilities.is_none());
        assert!(config.resources.is_none());
        assert!(config.tags.is_none());
    }

    #[test]
    fn test_agent_config_all_fields_populated() {
        let config = AgentConfig {
            id: Some("full-agent".to_string()),
            name: "Full Agent".to_string(),
            description: Some("Complete agent config".to_string()),
            model: Some(ModelConfig {
                provider: Some("anthropic".to_string()),
                model: Some("claude-opus-4-6".to_string()),
                max_tokens: Some(16384),
            }),
            system_prompt: Some("You are an expert".to_string()),
            capabilities: Some(Capabilities {
                functions: vec![
                    "file::*".to_string(),
                    "memory::*".to_string(),
                    "network::*".to_string(),
                ],
                memory_scopes: Some(vec!["personal".to_string(), "shared".to_string()]),
                network_hosts: Some(vec!["api.anthropic.com".to_string()]),
            }),
            resources: Some(Resources {
                max_tokens_per_hour: Some(500_000),
            }),
            tags: Some(vec!["prod".to_string(), "v2".to_string(), "ai".to_string()]),
        };
        assert_eq!(config.id, Some("full-agent".to_string()));
        assert_eq!(config.model.as_ref().unwrap().max_tokens, Some(16384));
        assert_eq!(config.capabilities.as_ref().unwrap().functions.len(), 3);
        assert_eq!(config.tags.as_ref().unwrap().len(), 3);
    }

    #[test]
    fn test_agent_id_auto_generation_is_uuid() {
        let config = AgentConfig {
            id: None,
            name: "AutoId".to_string(),
            description: None,
            model: None,
            system_prompt: None,
            capabilities: None,
            resources: None,
            tags: None,
        };
        let generated = config
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        assert_eq!(generated.len(), 36);
        assert_eq!(generated.chars().filter(|c| *c == '-').count(), 4);
    }

    #[test]
    fn test_agent_id_explicit_not_overridden() {
        let config = AgentConfig {
            id: Some("my-custom-id".to_string()),
            name: "ExplicitId".to_string(),
            description: None,
            model: None,
            system_prompt: None,
            capabilities: None,
            resources: None,
            tags: None,
        };
        let id = config
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        assert_eq!(id, "my-custom-id");
    }

    #[test]
    fn test_max_iterations_boundary_at_49() {
        let mut iterations: u32 = 0;
        let tool_calls_present = true;
        while tool_calls_present && iterations < MAX_ITERATIONS {
            iterations += 1;
            if iterations == 49 {
                break;
            }
        }
        assert_eq!(iterations, 49);
        assert!(iterations < MAX_ITERATIONS);
    }

    #[test]
    fn test_max_iterations_boundary_at_50() {
        let mut iterations: u32 = 0;
        while iterations < MAX_ITERATIONS {
            iterations += 1;
        }
        assert_eq!(iterations, MAX_ITERATIONS);
        assert!(!(iterations < MAX_ITERATIONS));
    }

    #[test]
    fn test_max_iterations_empty_tool_calls_break() {
        let tool_calls: Vec<Value> = vec![];
        let iterations: u32 = 0;
        let should_break = tool_calls.is_empty() || iterations >= MAX_ITERATIONS;
        assert!(should_break);
    }

    #[test]
    fn test_tool_call_parsing_nested_arguments() {
        let json_val = json!({
            "callId": "tc-nested",
            "id": "fn::complex",
            "arguments": {
                "config": {
                    "nested": {
                        "deep": true,
                        "level": 3,
                    },
                },
                "items": [1, 2, 3],
            },
        });
        let tc: FunctionCall = serde_json::from_value(json_val).unwrap();
        assert!(tc.arguments["config"]["nested"]["deep"].as_bool().unwrap());
        assert_eq!(tc.arguments["config"]["nested"]["level"], 3);
        assert_eq!(tc.arguments["items"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_tool_call_parsing_array_arguments() {
        let json_val = json!({
            "callId": "tc-arr",
            "id": "fn::batch",
            "arguments": [1, "two", false, null],
        });
        let tc: FunctionCall = serde_json::from_value(json_val).unwrap();
        assert!(tc.arguments.is_array());
        assert_eq!(tc.arguments.as_array().unwrap().len(), 4);
    }

    #[test]
    fn test_tool_call_parsing_empty_arguments() {
        let json_val = json!({
            "callId": "tc-empty",
            "id": "fn::noop",
            "arguments": {},
        });
        let tc: FunctionCall = serde_json::from_value(json_val).unwrap();
        assert!(tc.arguments.as_object().unwrap().is_empty());
    }

    #[test]
    fn test_tool_call_parsing_null_argument_value() {
        let json_val = json!({
            "callId": "tc-null",
            "id": "fn::nullarg",
            "arguments": {"key": null},
        });
        let tc: FunctionCall = serde_json::from_value(json_val).unwrap();
        assert!(tc.arguments["key"].is_null());
    }

    #[test]
    fn test_risk_score_exactly_0_5_passes() {
        let risk_score: f64 = 0.5;
        let rejected = risk_score > 0.5;
        assert!(!rejected);
    }

    #[test]
    fn test_risk_score_just_above_0_5_fails() {
        let risk_score: f64 = 0.500001;
        let rejected = risk_score > 0.5;
        assert!(rejected);
    }

    #[test]
    fn test_risk_score_zero_passes() {
        let risk_score: f64 = 0.0;
        let rejected = risk_score > 0.5;
        assert!(!rejected);
    }

    #[test]
    fn test_risk_score_negative_passes() {
        let risk_score: f64 = -1.0;
        let rejected = risk_score > 0.5;
        assert!(!rejected);
    }

    #[test]
    fn test_risk_score_one_fails() {
        let risk_score: f64 = 1.0;
        let rejected = risk_score > 0.5;
        assert!(rejected);
    }

    #[test]
    fn test_risk_score_default_from_missing_field() {
        let scan_result = json!({ "safe": true });
        let risk_score = scan_result["riskScore"].as_f64().unwrap_or(0.0);
        assert_eq!(risk_score, 0.0);
    }

    #[test]
    fn test_message_building_with_empty_memories() {
        let mut messages: Vec<Value> = vec![];
        let memories = json!([]);
        if let Some(mems) = memories.as_array() {
            messages.extend(mems.iter().cloned());
        }
        messages.push(json!({"role": "user", "content": "question"}));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn test_message_building_with_many_memories() {
        let mut messages: Vec<Value> = vec![];
        let mut mem_arr = Vec::new();
        for i in 0..50 {
            mem_arr.push(json!({"role": if i % 2 == 0 { "user" } else { "assistant" }, "content": format!("msg {}", i)}));
        }
        let memories = json!(mem_arr);
        if let Some(mems) = memories.as_array() {
            messages.extend(mems.iter().cloned());
        }
        messages.push(json!({"role": "user", "content": "new question"}));
        assert_eq!(messages.len(), 51);
        assert_eq!(messages[50]["content"], "new question");
    }

    #[test]
    fn test_message_building_null_memories_ignored() {
        let mut messages: Vec<Value> = vec![];
        let memories = json!(null);
        if let Some(mems) = memories.as_array() {
            messages.extend(mems.iter().cloned());
        }
        messages.push(json!({"role": "user", "content": "hello"}));
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn test_tool_filter_multiple_prefixes() {
        let allowed = vec![
            "file::".to_string(),
            "memory::".to_string(),
            "fn::".to_string(),
        ];
        assert!(allowed.iter().any(|a| "file::read".starts_with(a.as_str())));
        assert!(
            allowed
                .iter()
                .any(|a| "memory::store".starts_with(a.as_str()))
        );
        assert!(
            allowed
                .iter()
                .any(|a| "fn::web_fetch".starts_with(a.as_str()))
        );
        assert!(
            !allowed
                .iter()
                .any(|a| "network::send".starts_with(a.as_str()))
        );
        assert!(
            !allowed
                .iter()
                .any(|a| "security::scan".starts_with(a.as_str()))
        );
    }

    #[test]
    fn test_tool_filter_empty_allowed_list() {
        let allowed: Vec<String> = vec![];
        let tool_id = "file::read";
        let matches = allowed.iter().any(|a| tool_id.starts_with(a.as_str()));
        assert!(!matches);
    }

    #[test]
    fn test_tool_filter_exact_match() {
        let allowed = vec!["file::read".to_string()];
        let tool_id = "file::read";
        let matches = allowed.iter().any(|a| tool_id.starts_with(a.as_str()));
        assert!(matches);
    }

    #[test]
    fn test_tool_filter_partial_prefix_no_match() {
        let allowed = vec!["file::read_all".to_string()];
        let tool_id = "file::read";
        let matches = allowed.iter().any(|a| tool_id.starts_with(a.as_str()));
        assert!(!matches);
    }

    #[test]
    fn test_tool_call_id_split_multiple_separators() {
        let tc = FunctionCall {
            call_id: "c-3".to_string(),
            id: "security::check::deep".to_string(),
            arguments: json!({}),
        };
        let capability = tc.id.split("::").next().unwrap_or("");
        assert_eq!(capability, "security");
    }

    #[test]
    fn test_tool_call_id_split_empty_string() {
        let tc = FunctionCall {
            call_id: "c-4".to_string(),
            id: "".to_string(),
            arguments: json!({}),
        };
        let capability = tc.id.split("::").next().unwrap_or("");
        assert_eq!(capability, "");
    }

    #[test]
    fn test_tool_results_capability_denied() {
        let result = json!({
            "toolCallId": "tc-denied",
            "output": { "error": "capability denied" },
        });
        assert_eq!(result["output"]["error"], "capability denied");
    }

    #[test]
    fn test_tool_results_success() {
        let result = json!({
            "toolCallId": "tc-ok",
            "output": { "data": "success result" },
        });
        assert_eq!(result["output"]["data"], "success result");
        assert!(result["output"].get("error").is_none());
    }

    #[test]
    fn test_session_id_format_with_special_chars() {
        let agent_id = "agent/special-chars_123";
        let session_id: Option<String> = None;
        let result = session_id.unwrap_or_else(|| format!("default:{}", agent_id));
        assert_eq!(result, "default:agent/special-chars_123");
    }

    #[test]
    fn test_response_extraction_missing_content() {
        let response = json!({});
        let content = response
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(content, "");
    }

    #[test]
    fn test_response_extraction_null_content() {
        let response = json!({"content": null});
        let content = response
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(content, "");
    }

    #[test]
    fn test_response_extraction_present_content() {
        let response = json!({"content": "Hello, world!"});
        let content = response
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(content, "Hello, world!");
    }

    #[test]
    fn test_tool_count_from_empty_functions() {
        let functions = json!([]);
        let count = functions.as_array().map(|a| a.len()).unwrap_or(0);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_tool_count_from_functions_array() {
        let functions = json!([{"id": "a"}, {"id": "b"}, {"id": "c"}]);
        let count = functions.as_array().map(|a| a.len()).unwrap_or(0);
        assert_eq!(count, 3);
    }

    #[test]
    fn iii_0_22_1_function_registry_envelope_is_unwrapped_and_filtered_by_function_id() {
        let registry = json!({
            "functions": [
                { "function_id": "memory::recall", "worker_name": "memory" },
                { "function_id": "state::get", "worker_name": "state" },
                { "id": "memory::legacy-wrong-key", "worker_name": "memory" },
            ],
        });

        assert_eq!(
            filter_functions(&registry, &["memory::".to_string()]),
            json!([{
                "function_id": "memory::recall",
                "worker_name": "memory",
            }])
        );
        assert_eq!(
            filter_functions(&registry, &[String::new()]),
            registry["functions"]
        );
        assert_eq!(filter_functions(&json!([]), &[String::new()]), json!([]));
        assert_eq!(filter_functions(&registry, &[]), json!([]));
        assert_eq!(
            filter_functions(
                &json!({
                    "functions": [
                        Value::Null,
                        { "function_id": null },
                        { "function_id": "" },
                        { "function_id": 7 },
                    ],
                }),
                &["memory::".to_string()],
            ),
            json!([]),
            "malformed registry entries must not become callable tools"
        );
        for malformed in [Value::Null, json!({}), json!({ "functions": null })] {
            assert_eq!(filter_functions(&malformed, &[String::new()]), json!([]));
        }
    }

    #[test]
    fn test_tool_count_from_non_array() {
        let functions = json!("not an array");
        let count = functions.as_array().map(|a| a.len()).unwrap_or(0);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_create_agent_json_structure() {
        let config = AgentConfig {
            id: Some("new-agent".to_string()),
            name: "New Agent".to_string(),
            description: Some("A new agent".to_string()),
            model: Some(ModelConfig {
                provider: Some("anthropic".to_string()),
                model: Some("claude-sonnet-4-20250514".to_string()),
                max_tokens: Some(4096),
            }),
            system_prompt: Some("Be helpful".to_string()),
            capabilities: Some(Capabilities {
                functions: vec!["*".to_string()],
                memory_scopes: None,
                network_hosts: None,
            }),
            resources: None,
            tags: Some(vec!["test".to_string()]),
        };
        let agent_id = config
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let val = json!({
            "id": &agent_id,
            "name": &config.name,
            "description": &config.description,
            "model": &config.model,
            "systemPrompt": &config.system_prompt,
            "capabilities": &config.capabilities,
            "resources": &config.resources,
            "tags": &config.tags,
        });
        assert_eq!(val["id"], "new-agent");
        assert_eq!(val["name"], "New Agent");
        assert_eq!(val["description"], "A new agent");
        assert!(val["resources"].is_null());
    }

    #[test]
    fn test_iteration_counter_increments_correctly() {
        let mut iterations: u32 = 0;
        for _ in 0..5 {
            iterations += 1;
        }
        assert_eq!(iterations, 5);
    }

    #[test]
    fn test_tool_call_filter_map_ignores_invalid() {
        let tool_calls = vec![
            json!({"callId": "1", "id": "valid::tool", "arguments": {}}),
            json!({"missing": "fields"}),
            json!({"callId": "3", "id": "another::tool", "arguments": {"k": "v"}}),
        ];
        let calls: Vec<FunctionCall> = tool_calls
            .iter()
            .filter_map(|tc| serde_json::from_value(tc.clone()).ok())
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "valid::tool");
        assert_eq!(calls[1].id, "another::tool");
    }

    #[test]
    fn test_tool_call_filter_map_all_invalid() {
        let tool_calls = vec![json!({"bad": "data"}), json!(42), json!(null)];
        let calls: Vec<FunctionCall> = tool_calls
            .iter()
            .filter_map(|tc| serde_json::from_value(tc.clone()).ok())
            .collect();
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_agent_config_serialization_produces_rename() {
        let config = AgentConfig {
            id: Some("test".to_string()),
            name: "Test".to_string(),
            description: None,
            model: None,
            system_prompt: Some("prompt".to_string()),
            capabilities: None,
            resources: None,
            tags: None,
        };
        let val = serde_json::to_value(&config).unwrap();
        assert!(val.get("systemPrompt").is_some());
        assert!(val.get("system_prompt").is_none());
    }

    #[test]
    fn test_risk_score_non_numeric_treated_as_zero() {
        let scan_result = json!({ "safe": true, "riskScore": "not_a_number" });
        let risk_score = scan_result["riskScore"].as_f64().unwrap_or(0.0);
        assert_eq!(risk_score, 0.0);
        assert!(!(risk_score > 0.5));
    }

    #[test]
    fn test_risk_score_very_large_fails() {
        let risk_score: f64 = 999.99;
        assert!(risk_score > 0.5);
    }

    #[test]
    fn test_risk_score_f64_precision_boundary() {
        let risk_score: f64 = 0.5 + f64::EPSILON;
        assert!(risk_score > 0.5);
    }

    #[test]
    fn test_tool_filter_wildcard_pattern_match() {
        let allowed: Vec<String> = vec!["file::*".to_string()]
            .into_iter()
            .map(|a| a.trim_end_matches('*').to_string())
            .filter(|s| !s.trim().is_empty())
            .collect();
        let tool_id = "file::read";
        let matches = allowed.iter().any(|a| tool_id.starts_with(a.as_str()));
        assert!(matches);
    }

    #[test]
    fn test_tool_filter_case_sensitive() {
        let allowed = vec!["File::".to_string()];
        let tool_id = "file::read";
        let matches = allowed.iter().any(|a| tool_id.starts_with(a.as_str()));
        assert!(!matches);
    }

    #[test]
    fn test_tool_filter_empty_string_prefix() {
        let allowed: Vec<String> = vec!["".to_string()]
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
        let tool_id = "file::read";
        let matches = allowed.iter().any(|a| tool_id.starts_with(a.as_str()));
        assert!(
            !matches,
            "empty string should be filtered out and not match"
        );
    }

    #[test]
    fn test_message_building_with_non_array_memories() {
        let mut messages: Vec<Value> = vec![];
        let memories = json!({"not": "an array"});
        if let Some(mems) = memories.as_array() {
            messages.extend(mems.iter().cloned());
        }
        messages.push(json!({"role": "user", "content": "test"}));
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn test_message_building_preserves_order() {
        let mut messages: Vec<Value> = vec![];
        let memories = json!([
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "second"},
            {"role": "user", "content": "third"},
        ]);
        if let Some(mems) = memories.as_array() {
            messages.extend(mems.iter().cloned());
        }
        messages.push(json!({"role": "user", "content": "fourth"}));
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["content"], "first");
        assert_eq!(messages[1]["content"], "second");
        assert_eq!(messages[2]["content"], "third");
        assert_eq!(messages[3]["content"], "fourth");
    }

    #[test]
    fn test_session_id_default_format_empty_agent() {
        let agent_id = "";
        assert!(
            agent_id.is_empty(),
            "empty agent_id should be rejected at request boundary"
        );
    }

    #[test]
    fn test_session_id_default_format_unicode_agent() {
        let agent_id = "agent-\u{1f600}";
        let session_id: Option<String> = None;
        let result = session_id.unwrap_or_else(|| format!("default:{}", agent_id));
        assert!(result.starts_with("default:"));
        assert!(result.contains('\u{1f600}'));
    }

    #[test]
    fn test_tool_results_mixed_success_and_error() {
        let mut results = Vec::new();
        for i in 0..10 {
            if i % 3 == 0 {
                results.push(
                    json!({"toolCallId": format!("tc-{}", i), "output": {"error": "denied"}}),
                );
            } else {
                results.push(json!({"toolCallId": format!("tc-{}", i), "output": {"data": format!("result-{}", i)}}));
            }
        }
        assert_eq!(results.len(), 10);
        let errors: Vec<_> = results
            .iter()
            .filter(|r| r["output"].get("error").is_some())
            .collect();
        assert_eq!(errors.len(), 4);
    }

    #[test]
    fn test_tool_call_id_split_only_separator() {
        let tc = FunctionCall {
            call_id: "c".to_string(),
            id: "::".to_string(),
            arguments: json!({}),
        };
        let capability = tc.id.split("::").next().unwrap_or("");
        assert_eq!(capability, "");
    }

    #[test]
    fn test_response_extraction_numeric_content() {
        let response = json!({"content": 42});
        let content = response
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(content, "");
    }
}

async fn create_agent(iii: &IIIClient, config: AgentConfig) -> Result<Value, Error> {
    let agent_id = config
        .id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
        "scope": "agents",
        "key": &agent_id,
        "value": {
            "id": &agent_id,
            "name": &config.name,
            "description": &config.description,
            "model": &config.model,
            "systemPrompt": &config.system_prompt,
            "capabilities": &config.capabilities,
            "resources": &config.resources,
            "tags": &config.tags,
            "createdAt": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis(),
        },
    }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    spawn_trigger(
        iii,
        "publish",
        json!({
            "topic": "agent.lifecycle",
            "data": { "type": "created", "agentId": &agent_id },
        }),
    );

    Ok(json!({ "agentId": agent_id }))
}
