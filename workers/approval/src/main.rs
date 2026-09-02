use agentos_http_adapter::TriggerBus;
use agentos_http_adapter::policy;
use agentos_http_adapter::state::{self, set_op};
use iii_sdk::errors::Error;
use iii_sdk::{RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};

mod types;

use types::sanitize_id;

const MAX_PENDING_PER_AGENT: usize = 5;
const DEFAULT_TIMEOUT_MS: u64 = 300_000;

/// Verdicts returned by `approval::check` (contract I2).
const DECISION_APPROVED: &str = "approved";
const DECISION_DENIED: &str = "denied";
const DECISION_REQUIRED: &str = "required";

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Emits a side-effect trigger whose failure is logged rather than dropped.
async fn emit(iii: &dyn TriggerBus, function_id: &str, payload: Value) {
    if let Err(error) = iii
        .trigger(TriggerRequest {
            function_id: function_id.to_string(),
            payload,
            action: None,
            timeout_ms: None,
        })
        .await
    {
        tracing::warn!(function_id, %error, "approval side-effect trigger failed");
    }
}

/// Reads the function id under either the contract name or the legacy one.
fn requested_function_id(input: &Value) -> Option<&str> {
    input
        .get("functionId")
        .or_else(|| input.get("toolName"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

/// True when the configured policy asks for approval of `function_id`.
fn policy_requires_approval(policy: &Value, function_id: &str) -> bool {
    policy
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|pattern| match pattern.as_str() {
            Some("*") => true,
            // A family pattern covers everything beneath it, deliberately wider
            // than capability matching: over-asking is the safe direction here.
            Some(pattern) if pattern.ends_with("::*") => {
                function_id.starts_with(&pattern[..pattern.len() - 1])
            }
            Some(pattern) => function_id == pattern,
            None => false,
        })
}

/// A previous request for exactly this agent, function and payload.
fn matching_request<'a>(
    requests: &'a [&'a Value],
    function_id: &str,
    payload_digest: &str,
) -> Option<&'a Value> {
    requests
        .iter()
        .copied()
        .filter(|request| {
            request
                .get("functionId")
                .or_else(|| request.get("toolName"))
                .and_then(Value::as_str)
                == Some(function_id)
                && request.get("payloadDigest").and_then(Value::as_str) == Some(payload_digest)
        })
        .max_by_key(|request| {
            request
                .get("createdAt")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        })
}

/// Decides whether a tool call may run (contract I2).
///
/// Returns `decision` ∈ {approved, denied, required}. The deny-by-default
/// families always need a decision, with or without a configured policy, so a
/// missing policy cannot silently authorise `shell::exec`.
async fn check(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let agent_id = input
        .get("agentId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::Handler("agentId is required".into()))?;
    let function_id = requested_function_id(&input)
        .ok_or_else(|| Error::Handler("functionId is required".into()))?
        .to_string();
    let safe_agent_id = sanitize_id(agent_id).map_err(Error::Handler)?;
    let payload_digest = input
        .get("payloadDigest")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let caller_reason = input
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let params = input.get("params").cloned().unwrap_or(json!({}));

    let policy = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({ "scope": "approval_policy", "key": "default" }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let deny_by_default = policy::is_deny_by_default(&function_id);
    let requires_approval = deny_by_default || policy_requires_approval(&policy, &function_id);

    if !requires_approval {
        return Ok(decision_result(
            DECISION_APPROVED,
            "no approval policy matches this function",
            "",
        ));
    }

    let pending = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": format!("approvals:{safe_agent_id}") }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!([]));
    let requests = state::values(&pending);

    if let Some(existing) = matching_request(&requests, &function_id, &payload_digest) {
        let request_id = existing
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match existing.get("status").and_then(Value::as_str) {
            Some("approved") => {
                // Single use: an approval authorises this call, not every future
                // call with the same payload.
                let consumed = iii
                    .trigger(TriggerRequest {
                        function_id: "state::update".to_string(),
                        payload: state::update_payload(
                            format!("approvals:{safe_agent_id}"),
                            &request_id,
                            vec![
                                set_op("status", json!("consumed")),
                                set_op("consumedAt", json!(now_ms() as u64)),
                            ],
                        ),
                        action: None,
                        timeout_ms: None,
                    })
                    .await;
                return match consumed {
                    Ok(response) => match state::update_errors(&response) {
                        None => Ok(decision_result(
                            DECISION_APPROVED,
                            "an operator approved this exact request",
                            &request_id,
                        )),
                        // Could not mark it used, so it cannot be honoured.
                        Some(codes) => Ok(decision_result(
                            DECISION_DENIED,
                            &format!("approval could not be consumed: {codes}"),
                            &request_id,
                        )),
                    },
                    Err(error) => Ok(decision_result(
                        DECISION_DENIED,
                        &format!("approval could not be consumed: {error}"),
                        &request_id,
                    )),
                };
            }
            Some("denied") => {
                return Ok(decision_result(
                    DECISION_DENIED,
                    "an operator denied this exact request",
                    &request_id,
                ));
            }
            Some("pending") => {
                return Ok(decision_result(
                    DECISION_REQUIRED,
                    "an operator has not decided yet",
                    &request_id,
                ));
            }
            // "consumed" or anything else: a fresh request is needed.
            _ => {}
        }
    }

    let pending_count = requests
        .iter()
        .filter(|request| {
            request.get("status").and_then(Value::as_str) == Some(DECISION_REQUIRED)
                || request.get("status").and_then(Value::as_str) == Some("pending")
        })
        .count();
    if pending_count >= MAX_PENDING_PER_AGENT {
        return Err(Error::Handler(format!(
            "Agent {safe_agent_id} has {pending_count} pending approvals (max {MAX_PENDING_PER_AGENT})"
        )));
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let timeout_ms = policy
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let reason = if caller_reason.is_empty() {
        format!("Agent {safe_agent_id} wants to execute {function_id}")
    } else {
        caller_reason
    };

    let request = json!({
        "id": request_id,
        "agentId": safe_agent_id,
        "functionId": function_id,
        // Kept for existing operator surfaces that read `toolName`.
        "toolName": function_id,
        "payloadDigest": payload_digest,
        "params": params,
        "reason": reason,
        "denyByDefault": deny_by_default,
        "createdAt": now_ms() as u64,
        "timeoutMs": timeout_ms,
        "status": "pending",
    });

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": format!("approvals:{safe_agent_id}"),
            "key": request_id,
            "value": request,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    emit(
        iii,
        "publish",
        json!({
            "topic": "approval.requested",
            "data": {
                "requestId": request_id,
                "agentId": safe_agent_id,
                "toolName": function_id,
                "functionId": function_id,
            },
        }),
    )
    .await;

    Ok(decision_result(
        DECISION_REQUIRED,
        "an operator decision is needed",
        &request_id,
    ))
}

/// The contract I2 response, with the legacy fields kept beside it.
fn decision_result(decision: &str, reason: &str, request_id: &str) -> Value {
    json!({
        "decision": decision,
        "reason": reason,
        "requestId": request_id,
        // Legacy shape, so existing callers keep working.
        "required": decision != DECISION_APPROVED,
        "approved": decision == DECISION_APPROVED,
        "status": decision,
    })
}

fn state_group_names(response: &Value) -> Vec<&str> {
    state::groups(response)
}

async fn resolve_approval_agent(iii: &dyn TriggerBus, request_id: &str) -> Result<String, Error> {
    let scopes = iii
        .trigger(TriggerRequest {
            function_id: "state::list_groups".to_string(),
            payload: json!({}),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let mut found = None;
    for scope in state_group_names(&scopes) {
        let Some(agent_id) = scope.strip_prefix("approvals:") else {
            continue;
        };
        let current = iii
            .trigger(TriggerRequest {
                function_id: "state::get".to_string(),
                payload: json!({ "scope": scope, "key": request_id }),
                action: None,
                timeout_ms: None,
            })
            .await
            .map_err(|e| Error::Handler(e.to_string()))?;
        if current.is_null() {
            continue;
        }
        if found.is_some() {
            return Err(Error::Handler(format!(
                "approval request {request_id} exists for multiple agents"
            )));
        }
        found = Some(sanitize_id(agent_id).map_err(Error::Handler)?);
    }

    found.ok_or_else(|| Error::Handler(format!("approval request {request_id} not found")))
}

async fn decide(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let request_id = input
        .get("requestId")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("requestId is required".into()))?;
    let safe_request_id = sanitize_id(request_id).map_err(Error::Handler)?;
    let decision = input
        .get("decision")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("decision is required".into()))?;
    let safe_agent_id = match input.get("agentId").and_then(Value::as_str) {
        Some(agent_id) => sanitize_id(agent_id).map_err(Error::Handler)?,
        None => resolve_approval_agent(iii, &safe_request_id).await?,
    };
    let status = match decision {
        "approve" => "approved",
        "deny" => "denied",
        other => {
            return Err(Error::Handler(format!(
                "Invalid decision: {other} (expected approve|deny)"
            )));
        }
    };
    let decided_by = input
        .get("decidedBy")
        .and_then(Value::as_str)
        .unwrap_or("system");

    iii.trigger(TriggerRequest {
        function_id: "state::update".to_string(),
        payload: json!({
            "scope": format!("approvals:{safe_agent_id}"),
            "key": safe_request_id,
            "ops": [
                { "type": "set", "path": "status", "value": status },
                { "type": "set", "path": "decidedBy", "value": decided_by },
                { "type": "set", "path": "decidedAt", "value": now_ms() },
            ],
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    emit(
        iii,
        "publish",
        json!({
            "topic": "approval.decided",
            "data": {
                "requestId": safe_request_id,
                "agentId": safe_agent_id,
                "decision": status,
                "decidedBy": decided_by,
            },
        }),
    )
    .await;

    Ok::<Value, Error>(json!({
        "requestId": safe_request_id,
        "status": status,
    }))
}

async fn list(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let filter_status = input
        .get("status")
        .and_then(Value::as_str)
        .map(String::from);

    if let Some(agent_id) = input.get("agentId").and_then(Value::as_str) {
        let safe_agent_id = sanitize_id(agent_id).map_err(Error::Handler)?;
        let items = iii
            .trigger(TriggerRequest {
                function_id: "state::list".to_string(),
                payload: json!({ "scope": format!("approvals:{safe_agent_id}") }),
                action: None,
                timeout_ms: None,
            })
            .await
            .unwrap_or(json!([]));
        let out: Vec<Value> = state::values(&items)
            .into_iter()
            .filter(|value| match &filter_status {
                None => true,
                Some(status) => value.get("status").and_then(Value::as_str) == Some(status),
            })
            .cloned()
            .collect();
        return Ok::<Value, Error>(Value::Array(out));
    }

    let scopes = iii
        .trigger(TriggerRequest {
            function_id: "state::list_groups".to_string(),
            payload: json!({}),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!([]));
    let scopes_arr: Vec<String> = state_group_names(&scopes)
        .into_iter()
        .filter(|scope| scope.starts_with("approvals:"))
        .map(String::from)
        .collect();

    let mut all: Vec<Value> = Vec::new();
    for scope in scopes_arr {
        let items = iii
            .trigger(TriggerRequest {
                function_id: "state::list".to_string(),
                payload: json!({ "scope": scope }),
                action: None,
                timeout_ms: None,
            })
            .await
            .unwrap_or(json!([]));
        for value in state::values(&items) {
            let pass = match &filter_status {
                None => true,
                Some(status) => value.get("status").and_then(Value::as_str) == Some(status),
            };
            if pass {
                all.push(value.clone());
            }
        }
    }

    all.sort_by(|a, b| {
        let a_t = a.get("createdAt").and_then(Value::as_u64).unwrap_or(0);
        let b_t = b.get("createdAt").and_then(Value::as_u64).unwrap_or(0);
        b_t.cmp(&a_t)
    });

    Ok::<Value, Error>(Value::Array(all))
}

async fn wait(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let request_id = input
        .get("requestId")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("requestId is required".into()))?;
    let agent_id = input
        .get("agentId")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("agentId is required".into()))?;
    let safe_request_id = sanitize_id(request_id).map_err(Error::Handler)?;
    let safe_agent_id = sanitize_id(agent_id).map_err(Error::Handler)?;

    let current = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({
                "scope": format!("approvals:{safe_agent_id}"),
                "key": safe_request_id,
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(Value::Null);

    if current.is_null() {
        return Ok::<Value, Error>(json!({ "status": "not_found" }));
    }

    let status = current
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("pending")
        .to_string();

    if status == "approved" || status == "denied" {
        let audit_type = if status == "approved" {
            "approval_granted"
        } else {
            "approval_denied"
        };
        let tool_name = current
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("");
        let decided_by = current
            .get("decidedBy")
            .and_then(Value::as_str)
            .unwrap_or("");
        emit(
            iii,
            "security::audit",
            json!({
                "type": audit_type,
                "agentId": safe_agent_id,
                "detail": {
                    "requestId": safe_request_id,
                    "toolName": tool_name,
                    "decidedBy": decided_by,
                },
            }),
        )
        .await;
    }

    let mut response = json!({ "status": status, "requestId": safe_request_id });
    if status != "pending" {
        response["decision"] = current;
    }
    Ok::<Value, Error>(response)
}

async fn set_policy(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let tools_val = input.get("tools").cloned().unwrap_or(json!([]));
    let tools_arr = tools_val
        .as_array()
        .ok_or_else(|| Error::Handler("tools must be an array".into()))?;
    if !tools_arr.iter().all(|t| t.as_str().is_some()) {
        return Err(Error::Handler(
            "tools must contain only string patterns".into(),
        ));
    }
    let tools = Value::Array(tools_arr.clone());
    let timeout_ms = input
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS);

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": "approval_policy",
            "key": "default",
            "value": { "tools": tools, "timeoutMs": timeout_ms },
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    Ok::<Value, Error>(json!({ "updated": true }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());

    let iii_clone = iii.clone();
    iii.register_function(
        "approval::check",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move { check(&iii, input).await }
        })
        .description("Check if tool requires approval and gate execution"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "approval::decide",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move { decide(&iii, input).await }
        })
        .description("Approve or deny a pending request"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "approval::list",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move { list(&iii, input).await }
        })
        .description("List pending approvals"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "approval::wait",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move { wait(&iii, input).await }
        })
        .description("Poll approval status (non-blocking)"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "approval::set_policy",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move { set_policy(&iii, input).await }
        })
        .description("Set approval policy"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "approval::list".to_string(),
        json!({ "http_method": "GET", "api_path": "api/approvals" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "approval::decide".to_string(),
        json!({ "http_method": "POST", "api_path": "api/approvals/decide" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "approval::wait".to_string(),
        json!({ "http_method": "POST", "api_path": "api/approvals/wait" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "approval::set_policy".to_string(),
        json!({ "http_method": "POST", "api_path": "api/approvals/policy" }),
        None,
    )?;

    tracing::info!("approval worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_group_names_accepts_engine_envelope() {
        let response = json!({ "groups": ["approvals:one", "workflows"] });
        assert_eq!(
            state_group_names(&response),
            vec!["approvals:one", "workflows"]
        );
    }

    #[test]
    fn state_group_names_accepts_legacy_array() {
        let response = json!(["approvals:one", 42, "workflow_runs"]);
        assert_eq!(
            state_group_names(&response),
            vec!["approvals:one", "workflow_runs"]
        );
    }

    // ---- contract I2: approval::check ---------------------------------------

    use agentos_http_adapter::fake::FakeBus;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    /// The `state::*` subset `check` uses, with the engine's real shapes:
    /// `list` answers a bare array of values, `update` takes `ops`.
    #[derive(Default)]
    struct States {
        scopes: Mutex<BTreeMap<String, BTreeMap<String, Value>>>,
    }

    impl States {
        fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, BTreeMap<String, Value>>> {
            self.scopes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
        }

        fn put(&self, scope: &str, key: &str, value: Value) {
            self.lock()
                .entry(scope.to_string())
                .or_default()
                .insert(key.to_string(), value);
        }

        fn get_value(&self, scope: &str, key: &str) -> Value {
            self.lock()
                .get(scope)
                .and_then(|scope| scope.get(key))
                .cloned()
                .unwrap_or(Value::Null)
        }
    }

    fn approval_bus() -> (FakeBus, Arc<States>) {
        let states = Arc::new(States::default());
        let bus = FakeBus::new();

        let state = states.clone();
        bus.on("state::get", move |input| {
            Ok(state.get_value(
                input["scope"].as_str().unwrap_or_default(),
                input["key"].as_str().unwrap_or_default(),
            ))
        });
        let state = states.clone();
        bus.on("state::set", move |input| {
            state.put(
                input["scope"].as_str().unwrap_or_default(),
                input["key"].as_str().unwrap_or_default(),
                input["value"].clone(),
            );
            Ok(json!({ "stored": true }))
        });
        let state = states.clone();
        bus.on("state::list", move |input| {
            let scope = input["scope"].as_str().unwrap_or_default().to_string();
            let values = state
                .lock()
                .get(&scope)
                .map(|scope| scope.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            Ok(Value::Array(values))
        });
        let state = states.clone();
        bus.on("state::update", move |input| {
            let Some(ops) = input.get("ops").and_then(Value::as_array) else {
                return Err(Error::Handler("missing field `ops`".to_string()));
            };
            let scope = input["scope"].as_str().unwrap_or_default().to_string();
            let key = input["key"].as_str().unwrap_or_default().to_string();
            let mut value = state.get_value(&scope, &key);
            if !value.is_object() {
                value = json!({});
            }
            for op in ops {
                if op["type"] == "set"
                    && let Some(object) = value.as_object_mut()
                {
                    object.insert(
                        op["path"].as_str().unwrap_or_default().to_string(),
                        op["value"].clone(),
                    );
                }
            }
            state.put(&scope, &key, value);
            Ok(json!({ "errors": [] }))
        });
        bus.on_value("publish", json!({ "published": true }));

        (bus, states)
    }

    #[tokio::test]
    async fn a_deny_by_default_function_needs_approval_even_without_a_policy() {
        let (bus, _states) = approval_bus();

        let result = check(
            &bus,
            json!({ "agentId": "a-1", "functionId": "shell::exec", "payloadDigest": "d1" }),
        )
        .await
        .expect("check failed");

        assert_eq!(result["decision"], "required");
        assert!(
            result["requestId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert_eq!(result["approved"], false, "legacy field stays consistent");
    }

    #[tokio::test]
    async fn an_ordinary_function_without_a_policy_is_approved() {
        let (bus, _states) = approval_bus();

        let result = check(
            &bus,
            json!({ "agentId": "a-1", "functionId": "memory::recall" }),
        )
        .await
        .expect("check failed");

        assert_eq!(result["decision"], "approved");
        assert_eq!(result["required"], false);
    }

    #[tokio::test]
    async fn a_policy_match_requires_approval_for_an_ordinary_function() {
        let (bus, states) = approval_bus();
        states.put(
            "approval_policy",
            "default",
            json!({ "tools": ["workflow::*"], "timeoutMs": 1000 }),
        );

        let result = check(
            &bus,
            json!({ "agentId": "a-1", "functionId": "workflow::run" }),
        )
        .await
        .expect("check failed");

        assert_eq!(result["decision"], "required");
    }

    #[tokio::test]
    async fn an_operator_decision_is_honoured_once_and_only_for_the_same_payload() {
        let (bus, _states) = approval_bus();
        let first = check(
            &bus,
            json!({ "agentId": "a-1", "functionId": "shell::exec", "payloadDigest": "d1" }),
        )
        .await
        .expect("check failed");
        let request_id = first["requestId"].as_str().expect("requestId").to_string();

        decide(
            &bus,
            json!({ "agentId": "a-1", "requestId": &request_id, "decision": "approve" }),
        )
        .await
        .expect("decide failed");

        let approved = check(
            &bus,
            json!({ "agentId": "a-1", "functionId": "shell::exec", "payloadDigest": "d1" }),
        )
        .await
        .expect("check failed");
        assert_eq!(approved["decision"], "approved");
        assert_eq!(approved["requestId"], request_id);

        // Single use: the same call has to be approved again.
        let replay = check(
            &bus,
            json!({ "agentId": "a-1", "functionId": "shell::exec", "payloadDigest": "d1" }),
        )
        .await
        .expect("check failed");
        assert_eq!(replay["decision"], "required");
        assert_ne!(replay["requestId"], request_id);

        // A different payload never inherits the decision.
        let other = check(
            &bus,
            json!({ "agentId": "a-1", "functionId": "shell::exec", "payloadDigest": "d2" }),
        )
        .await
        .expect("check failed");
        assert_eq!(other["decision"], "required");
    }

    #[tokio::test]
    async fn a_denied_request_stays_denied_for_the_same_payload() {
        let (bus, _states) = approval_bus();
        let first = check(
            &bus,
            json!({ "agentId": "a-1", "functionId": "vault::get", "payloadDigest": "d1" }),
        )
        .await
        .expect("check failed");
        let request_id = first["requestId"].as_str().expect("requestId").to_string();

        decide(
            &bus,
            json!({ "agentId": "a-1", "requestId": request_id, "decision": "deny" }),
        )
        .await
        .expect("decide failed");

        let denied = check(
            &bus,
            json!({ "agentId": "a-1", "functionId": "vault::get", "payloadDigest": "d1" }),
        )
        .await
        .expect("check failed");
        assert_eq!(denied["decision"], "denied");
    }

    #[tokio::test]
    async fn a_pending_request_is_not_duplicated() {
        let (bus, _states) = approval_bus();
        let payload =
            json!({ "agentId": "a-1", "functionId": "shell::exec", "payloadDigest": "d1" });

        let first = check(&bus, payload.clone()).await.expect("check failed");
        let second = check(&bus, payload).await.expect("check failed");

        assert_eq!(second["decision"], "required");
        assert_eq!(
            second["requestId"], first["requestId"],
            "a repeated check must not pile up requests"
        );
    }

    #[tokio::test]
    async fn check_still_accepts_the_legacy_tool_name_field() {
        let (bus, _states) = approval_bus();
        let result = check(&bus, json!({ "agentId": "a-1", "toolName": "shell::exec" }))
            .await
            .expect("check failed");
        assert_eq!(result["decision"], "required");
    }

    #[tokio::test]
    async fn check_requires_an_agent_and_a_function() {
        let (bus, _states) = approval_bus();
        assert!(
            check(&bus, json!({ "functionId": "shell::exec" }))
                .await
                .unwrap_err()
                .to_string()
                .contains("agentId is required")
        );
        assert!(
            check(&bus, json!({ "agentId": "a-1" }))
                .await
                .unwrap_err()
                .to_string()
                .contains("functionId is required")
        );
    }

    #[tokio::test]
    async fn check_fails_when_the_state_store_is_unreachable() {
        let bus = FakeBus::new();
        bus.on_error("state::get", "state worker offline");

        let error = check(
            &bus,
            json!({ "agentId": "a-1", "functionId": "shell::exec" }),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("state worker offline"));
    }

    #[test]
    fn policy_patterns_match_families_and_exact_ids() {
        let policy = json!({ "tools": ["workflow::*", "memory::store"] });
        assert!(policy_requires_approval(&policy, "workflow::run"));
        assert!(policy_requires_approval(&policy, "memory::store"));
        assert!(!policy_requires_approval(&policy, "memory::recall"));
        assert!(policy_requires_approval(
            &json!({ "tools": ["*"] }),
            "any::id"
        ));
        assert!(!policy_requires_approval(&Value::Null, "any::id"));
    }
}
