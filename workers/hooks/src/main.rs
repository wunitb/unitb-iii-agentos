use iii_sdk::errors::Error;
use iii_sdk::{
    IIIClient, InitOptions, RegisterFunction,
    protocol::{RegisterTriggerInput, TriggerRequest},
    register_worker,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const VALID_HOOK_TYPES: &[&str] = &[
    "BeforeToolCall",
    "AfterToolCall",
    "BeforePromptBuild",
    "AgentLoopEnd",
    "SessionStart",
    "SessionEnd",
    "RequestStart",
    "RequestEnd",
    "BeforeCompact",
    "AfterCompact",
];

fn is_valid_hook_type(s: &str) -> bool {
    VALID_HOOK_TYPES.contains(&s)
}

/// Namespaces a hook may never target.
///
/// `hook::register` used to accept ANY `functionId` and `hook::fire` later
/// invoked it, so a `BeforeToolCall` hook pointing at `bridge::invoke` or
/// `vault::get` was accepted and fired on every tool call. The first twelve
/// entries are the deny-by-default set from the remediation contract; the rest
/// close self-amplification and the engine control surface.
const RESERVED_HOOK_NAMESPACES: &[&str] = &[
    "shell",
    "bridge",
    "mcp",
    "hook",
    "cron",
    "vault",
    "state",
    "engine",
    "code",
    "harness",
    "browser",
    "wasm",
    "coder",
    "security",
    "approval",
    "approval-tiers",
    "agentos",
    "trigger",
    "control",
    "configuration",
];

/// Optional operator allowlist. When set, a hook target must ALSO match one of
/// these patterns (exact ids or `namespace::*` globs). When unset, any
/// well-formed non-reserved id is accepted, because hook targets are ordinary
/// user-supplied functions.
const HOOK_ALLOWLIST_ENV: &str = "AGENTOS_HOOK_ALLOWLIST";

/// Same segment glob as the capability reader in `workers/security` and the
/// mint allowlist in `workers/cron`: `*` stands for one segment, a trailing `*`
/// also covers any further segments, and an empty pattern matches nothing.
fn hook_pattern_matches(pattern: &str, function_id: &str) -> bool {
    if pattern.is_empty() || function_id.is_empty() {
        return false;
    }
    let pattern_segments: Vec<&str> = pattern.split("::").collect();
    let function_segments: Vec<&str> = function_id.split("::").collect();
    if pattern_segments.iter().any(|segment| segment.is_empty())
        || function_segments.iter().any(|segment| segment.is_empty())
    {
        return false;
    }
    if pattern == function_id {
        return true;
    }
    let trailing_wildcard = pattern_segments
        .last()
        .is_some_and(|segment| *segment == "*");
    let compared = if trailing_wildcard {
        if function_segments.len() < pattern_segments.len() {
            return false;
        }
        &pattern_segments[..pattern_segments.len() - 1]
    } else {
        if pattern_segments.len() != function_segments.len() {
            return false;
        }
        &pattern_segments[..]
    };
    compared
        .iter()
        .zip(function_segments.iter())
        .all(|(pattern_segment, function_segment)| {
            *pattern_segment == "*" || pattern_segment == function_segment
        })
}

fn is_reserved_hook_target(function_id: &str) -> bool {
    match function_id.split("::").next() {
        Some(namespace) if !namespace.is_empty() => RESERVED_HOOK_NAMESPACES.contains(&namespace),
        _ => true,
    }
}

/// Gate for every function id a hook can point at. Enforced at registration AND
/// again at fire time, so a hook persisted before this landed cannot still run.
fn ensure_hookable_function(function_id: &str) -> Result<(), Error> {
    if function_id.is_empty() {
        return Err(Error::Handler("functionId is required".into()));
    }
    let segments: Vec<&str> = function_id.split("::").collect();
    if segments.len() < 2 || segments.iter().any(|segment| segment.is_empty()) {
        return Err(Error::Handler(format!(
            "functionId must be a namespaced function id, got {function_id:?}"
        )));
    }
    if is_reserved_hook_target(function_id) {
        return Err(Error::Handler(format!(
            "functionId {function_id} is reserved and cannot be used as a hook target"
        )));
    }
    if let Ok(raw) = std::env::var(HOOK_ALLOWLIST_ENV)
        && !raw.trim().is_empty()
    {
        let allowed = raw
            .split(',')
            .map(str::trim)
            .any(|pattern| hook_pattern_matches(pattern, function_id));
        if !allowed {
            return Err(Error::Handler(format!(
                "functionId {function_id} is not in the operator allowlist ({HOOK_ALLOWLIST_ENV})"
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HookDefinition {
    id: String,
    name: String,
    #[serde(rename = "type")]
    hook_type: String,
    priority: i64,
    #[serde(rename = "functionId")]
    function_id: String,
    enabled: bool,
    #[serde(rename = "agentId", skip_serializing_if = "Option::is_none")]
    agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    filter: Option<Value>,
    #[serde(rename = "createdAt")]
    created_at: i64,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn state_get(iii: &IIIClient, scope: &str, key: &str) -> Result<Option<Value>, Error> {
    let res = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({ "scope": scope, "key": key }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;
    Ok(if res.is_null() { None } else { Some(res) })
}

async fn state_list(iii: &IIIClient, scope: &str) -> Result<Vec<Value>, Error> {
    let res = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": scope }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;
    Ok(res.as_array().cloned().unwrap_or_default())
}

async fn state_set(iii: &IIIClient, scope: &str, key: &str, value: Value) -> Result<(), Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({ "scope": scope, "key": key, "value": value }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;
    Ok(())
}

async fn state_delete(iii: &IIIClient, scope: &str, key: &str) -> Result<(), Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::delete".to_string(),
        payload: json!({ "scope": scope, "key": key }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;
    Ok(())
}

fn fire_and_forget(iii: &IIIClient, function_id: &str, payload: Value) {
    let iii_clone = iii.clone();
    let id = function_id.to_string();
    tokio::spawn(async move {
        let _ = iii_clone
            .trigger(TriggerRequest {
                function_id: id,
                payload,
                action: None,
                timeout_ms: None,
            })
            .await;
    });
}

async fn load_hooks(iii: &IIIClient) -> Result<Vec<HookDefinition>, Error> {
    let entries = state_list(iii, "hooks").await?;
    let mut out = Vec::new();
    for e in entries {
        // entries can be either {key, value} or just the value, depending on engine
        let v = e.get("value").cloned().unwrap_or(e);
        if let Ok(h) = serde_json::from_value::<HookDefinition>(v)
            && !h.id.is_empty()
        {
            out.push(h);
        }
    }
    Ok(out)
}

async fn save_hook(iii: &IIIClient, hook: &HookDefinition) -> Result<(), Error> {
    let value = serde_json::to_value(hook).map_err(|e| Error::Handler(e.to_string()))?;
    state_set(iii, "hooks", &hook.id, value).await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, InitOptions::default());

    let iii_ref = iii.clone();
    iii.register_function(
        "hook::register",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move {
                let hook_type = input["type"].as_str().unwrap_or("").to_string();
                if !is_valid_hook_type(&hook_type) {
                    return Err(Error::Handler(format!(
                        "Invalid hook type: {hook_type}. Valid: {}",
                        VALID_HOOK_TYPES.join(", ")
                    )));
                }
                let function_id = input["functionId"].as_str().unwrap_or("").to_string();
                ensure_hookable_function(&function_id)?;

                let hook_id = uuid::Uuid::new_v4().to_string();
                let provided_name = input["name"].as_str().unwrap_or("").to_string();
                let name = if provided_name.is_empty() {
                    let short = hook_id.chars().take(8).collect::<String>();
                    format!("{hook_type}-{short}")
                } else {
                    provided_name
                };
                let priority = input["priority"].as_i64().unwrap_or(100);
                let agent_id = input["agentId"].as_str().map(String::from);
                let filter = input.get("filter").cloned().filter(|v| !v.is_null());

                // Validate tool-call filter shape up front so hook::fire can
                // trust filter.toolIds is an array of strings (or absent).
                let is_tool_call = matches!(hook_type.as_str(), "BeforeToolCall" | "AfterToolCall");
                if let (true, Some(tool_ids)) =
                    (is_tool_call, filter.as_ref().and_then(|f| f.get("toolIds")))
                {
                    let arr = tool_ids.as_array().ok_or_else(|| {
                        Error::Handler("filter.toolIds must be an array of strings".into())
                    })?;
                    if !arr.iter().all(|v| v.is_string()) {
                        return Err(Error::Handler(
                            "filter.toolIds must contain only strings".into(),
                        ));
                    }
                }

                let hook = HookDefinition {
                    id: hook_id.clone(),
                    name: name.clone(),
                    hook_type: hook_type.clone(),
                    priority,
                    function_id: function_id.clone(),
                    enabled: true,
                    agent_id,
                    filter,
                    created_at: now_ms(),
                };

                save_hook(&iii, &hook).await?;

                fire_and_forget(
                    &iii,
                    "security::audit",
                    json!({
                        "type": "hook_registered",
                        "detail": {
                            "hookId": hook_id,
                            "name": name,
                            "hookType": hook_type,
                            "functionId": function_id,
                        }
                    }),
                );

                Ok::<Value, Error>(json!({
                    "registered": true,
                    "id": hook.id,
                    "name": hook.name,
                    "type": hook.hook_type,
                }))
            }
        })
        .description("Register a hook")
        .metadata(json!({ "category": "hooks" })),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "hook::unregister",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move {
                let hook_id = input["hookId"].as_str().unwrap_or("").to_string();
                let raw = state_get(&iii, "hooks", &hook_id).await?;
                let hook: HookDefinition = match raw.and_then(|v| serde_json::from_value(v).ok()) {
                    Some(h) => h,
                    None => {
                        return Err(Error::Handler(format!("Hook not found: {hook_id}")));
                    }
                };

                state_delete(&iii, "hooks", &hook_id).await?;

                fire_and_forget(
                    &iii,
                    "security::audit",
                    json!({
                        "type": "hook_unregistered",
                        "detail": { "hookId": hook_id, "name": hook.name }
                    }),
                );

                Ok::<Value, Error>(json!({ "unregistered": true, "id": hook_id }))
            }
        })
        .description("Remove a hook")
        .metadata(json!({ "category": "hooks" })),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "hook::fire",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move {
                let hook_type = input["type"].as_str().unwrap_or("").to_string();
                if !is_valid_hook_type(&hook_type) {
                    return Err(Error::Handler(format!("Invalid hook type: {hook_type}")));
                }
                let payload = input.get("payload").cloned().unwrap_or(json!({}));
                let agent_id = input["agentId"].as_str().map(String::from);

                let all_hooks = load_hooks(&iii).await?;

                let payload_agent_id = payload
                    .get("agentId")
                    .and_then(|v| v.as_str())
                    .map(String::from);

                let mut applicable: Vec<HookDefinition> = all_hooks
                    .into_iter()
                    .filter(|h| {
                        // Re-check the target at fire time: a hook stored before
                        // the registration gate existed would otherwise still be
                        // invoked on every matching event.
                        if let Err(error) = ensure_hookable_function(&h.function_id) {
                            tracing::warn!(
                                hook_id = %h.id,
                                function_id = %h.function_id,
                                error = %error,
                                "refusing to fire a hook with a reserved target"
                            );
                            return false;
                        }
                        h.hook_type == hook_type
                            && h.enabled
                            && (h.agent_id.is_none()
                                || h.agent_id == agent_id
                                || h.agent_id == payload_agent_id)
                    })
                    .collect();

                if hook_type == "BeforeToolCall" || hook_type == "AfterToolCall" {
                    let tool_id = payload.get("toolId").and_then(|v| v.as_str()).unwrap_or("");
                    applicable.retain(|h| match &h.filter {
                        Some(filter) => match filter.get("toolIds").and_then(|v| v.as_array()) {
                            Some(allowed) => allowed.iter().any(|x| x.as_str() == Some(tool_id)),
                            None => true,
                        },
                        None => true,
                    });
                }

                applicable.sort_by(|a, b| a.priority.cmp(&b.priority));

                let mut results: Vec<Value> = Vec::new();
                let mut blocked = false;
                let mut block_reason = String::new();
                let mut modified_payload = payload.clone();

                for hook in &applicable {
                    let start = now_ms();
                    let trigger_result = iii
                        .trigger(TriggerRequest {
                            function_id: hook.function_id.clone(),
                            payload: json!({
                                "hookType": hook_type,
                                "hookId": hook.id,
                                "hookName": hook.name,
                                "payload": modified_payload,
                            }),
                            action: None,
                            timeout_ms: None,
                        })
                        .await;

                    let duration_ms = (now_ms() - start).max(0);

                    match trigger_result {
                        Ok(res) => {
                            let mut hook_result = Map::new();
                            hook_result.insert("hookId".into(), json!(hook.id));
                            hook_result.insert("hookName".into(), json!(hook.name));
                            hook_result.insert("result".into(), res.clone());
                            hook_result.insert("durationMs".into(), json!(duration_ms));

                            if hook_type == "BeforeToolCall"
                                && res.get("block").and_then(|v| v.as_bool()).unwrap_or(false)
                            {
                                let reason = res
                                    .get("reason")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("Blocked by hook")
                                    .to_string();
                                hook_result.insert("blocked".into(), json!(true));
                                hook_result.insert("reason".into(), json!(reason.clone()));
                                blocked = true;
                                block_reason = reason;
                                results.push(Value::Object(hook_result));
                                break;
                            }

                            if hook_type == "BeforePromptBuild"
                                && let Some(mp) = res.get("modifiedPayload")
                                && !mp.is_null()
                            {
                                modified_payload = mp.clone();
                            }

                            results.push(Value::Object(hook_result));
                        }
                        Err(err) => {
                            let mut hook_result = Map::new();
                            hook_result.insert("hookId".into(), json!(hook.id));
                            hook_result.insert("hookName".into(), json!(hook.name));
                            hook_result.insert("result".into(), Value::Null);
                            hook_result.insert("durationMs".into(), json!(duration_ms));
                            hook_result.insert("error".into(), json!(err.to_string()));
                            results.push(Value::Object(hook_result));
                        }
                    }
                }

                let mut response = Map::new();
                response.insert("type".into(), json!(hook_type));
                response.insert("hooksFired".into(), json!(results.len()));
                response.insert("results".into(), Value::Array(results));

                if hook_type == "BeforeToolCall" {
                    response.insert("blocked".into(), json!(blocked));
                    response.insert("blockReason".into(), json!(block_reason));
                }
                if hook_type == "BeforePromptBuild" {
                    response.insert("modifiedPayload".into(), modified_payload);
                }

                Ok::<Value, Error>(Value::Object(response))
            }
        })
        .description("Fire hooks of a given type and return results")
        .metadata(json!({ "category": "hooks" })),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "hook::list",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move {
                let type_filter = input.get("type").and_then(|v| v.as_str()).map(String::from);
                let agent_id_filter = input
                    .get("agentId")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let enabled_only = input
                    .get("enabledOnly")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                let mut hooks = load_hooks(&iii).await?;
                if let Some(t) = &type_filter {
                    hooks.retain(|h| h.hook_type == *t);
                }
                if let Some(_a) = &agent_id_filter {
                    hooks.retain(|h| h.agent_id.is_none() || h.agent_id == agent_id_filter);
                }
                if enabled_only {
                    hooks.retain(|h| h.enabled);
                }

                hooks.sort_by(|a, b| {
                    if a.hook_type != b.hook_type {
                        a.hook_type.cmp(&b.hook_type)
                    } else {
                        a.priority.cmp(&b.priority)
                    }
                });

                let count = hooks.len();
                let mut grouped: Map<String, Value> = Map::new();
                for h in &hooks {
                    let entry = grouped
                        .entry(h.hook_type.clone())
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if let Some(arr) = entry.as_array_mut() {
                        arr.push(serde_json::to_value(h).unwrap_or(Value::Null));
                    }
                }

                let hooks_json: Vec<Value> = hooks
                    .into_iter()
                    .map(|h| serde_json::to_value(&h).unwrap_or(Value::Null))
                    .collect();

                Ok::<Value, Error>(json!({
                    "hooks": hooks_json,
                    "count": count,
                    "grouped": Value::Object(grouped),
                }))
            }
        })
        .description("List registered hooks")
        .metadata(json!({ "category": "hooks" })),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "hook::toggle",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move {
                let hook_id = input["hookId"].as_str().unwrap_or("").to_string();
                let enabled = input
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| Error::Handler("enabled must be a boolean".into()))?;

                let raw = state_get(&iii, "hooks", &hook_id).await?;
                let mut hook: HookDefinition =
                    match raw.and_then(|v| serde_json::from_value(v).ok()) {
                        Some(h) => h,
                        None => {
                            return Err(Error::Handler(format!("Hook not found: {hook_id}")));
                        }
                    };

                hook.enabled = enabled;
                save_hook(&iii, &hook).await?;

                Ok::<Value, Error>(json!({
                    "toggled": true,
                    "id": hook_id,
                    "enabled": enabled,
                }))
            }
        })
        .description("Enable or disable a hook")
        .metadata(json!({ "category": "hooks" })),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "hook::update_priority",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move {
                let hook_id = input["hookId"].as_str().unwrap_or("").to_string();
                let priority = input
                    .get("priority")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| Error::Handler("priority must be an integer".into()))?;

                let raw = state_get(&iii, "hooks", &hook_id).await?;
                let mut hook: HookDefinition =
                    match raw.and_then(|v| serde_json::from_value(v).ok()) {
                        Some(h) => h,
                        None => {
                            return Err(Error::Handler(format!("Hook not found: {hook_id}")));
                        }
                    };

                hook.priority = priority;
                save_hook(&iii, &hook).await?;

                Ok::<Value, Error>(json!({
                    "updated": true,
                    "id": hook_id,
                    "priority": priority,
                }))
            }
        })
        .description("Update hook priority")
        .metadata(json!({ "category": "hooks" })),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "hook::register".to_string(),
        json!({ "api_path": "api/hooks", "http_method": "POST" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "hook::unregister".to_string(),
        json!({ "api_path": "api/hooks/:hookId", "http_method": "DELETE" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "hook::list".to_string(),
        json!({ "api_path": "api/hooks", "http_method": "GET" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "hook::fire".to_string(),
        json!({ "api_path": "api/hooks/fire", "http_method": "POST" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "hook::toggle".to_string(),
        json!({ "api_path": "api/hooks/toggle", "http_method": "POST" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "hook::update_priority".to_string(),
        json!({ "api_path": "api/hooks/priority", "http_method": "POST" }),
        None,
    )?;

    iii.register_trigger(RegisterTriggerInput {
        trigger_type: "subscribe".to_string(),
        function_id: "hook::fire".to_string(),
        config: json!({ "topic": "hooks.fire" }),
        metadata: None,
    })?;

    tracing::info!("hooks worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- hook target policy

    fn with_hook_allowlist<T>(value: Option<&str>, test: impl FnOnce() -> T) -> T {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os(HOOK_ALLOWLIST_ENV);
        unsafe {
            match value {
                Some(value) => std::env::set_var(HOOK_ALLOWLIST_ENV, value),
                None => std::env::remove_var(HOOK_ALLOWLIST_ENV),
            }
        }
        let result = test();
        unsafe {
            match previous {
                Some(value) => std::env::set_var(HOOK_ALLOWLIST_ENV, value),
                None => std::env::remove_var(HOOK_ALLOWLIST_ENV),
            }
        }
        result
    }

    #[test]
    fn hook_targets_refuse_the_privileged_namespaces() {
        with_hook_allowlist(None, || {
            for function_id in [
                "bridge::invoke",
                "shell::exec",
                "vault::get",
                "mcp::connect",
                "state::set",
                "hook::register",
                "hook::fire",
                "cron::create",
                "engine::functions::list",
                "harness::spawn",
                "browser::navigate",
                "wasm::run",
                "coder::apply",
                "security::set_capabilities",
                "trigger::create",
                "configuration::set",
            ] {
                let error = ensure_hookable_function(function_id)
                    .expect_err(&format!("{function_id} must be refused"))
                    .to_string();
                assert!(error.contains("reserved"), "{function_id}: {error}");
            }
        });
    }

    #[test]
    fn hook_targets_accept_ordinary_functions_by_default() {
        with_hook_allowlist(None, || {
            for function_id in ["memory::store", "workflow::run", "myworker::on_tool_call"] {
                ensure_hookable_function(function_id)
                    .unwrap_or_else(|error| panic!("{function_id} must be allowed: {error}"));
            }
        });
    }

    #[test]
    fn hook_targets_require_a_namespaced_id() {
        with_hook_allowlist(None, || {
            for function_id in ["", "bare", "::", "::fire", "memory::"] {
                assert!(
                    ensure_hookable_function(function_id).is_err(),
                    "{function_id:?} must be refused"
                );
            }
        });
    }

    #[test]
    fn operator_allowlist_narrows_but_never_widens_past_reserved() {
        with_hook_allowlist(Some("myworker::*"), || {
            assert!(ensure_hookable_function("myworker::on_tool_call").is_ok());
            assert!(
                ensure_hookable_function("memory::store").is_err(),
                "an id outside the allowlist must be refused"
            );
        });
        with_hook_allowlist(Some("*"), || {
            assert!(ensure_hookable_function("memory::store").is_ok());
            assert!(
                ensure_hookable_function("bridge::invoke").is_err(),
                "a wildcard allowlist must not reach a reserved namespace"
            );
        });
    }

    #[test]
    fn hook_pattern_has_no_prefix_semantics() {
        assert!(!hook_pattern_matches("", "memory::store"));
        assert!(!hook_pattern_matches("memory", "memory::store"));
        assert!(!hook_pattern_matches("memory::st", "memory::store"));
        assert!(hook_pattern_matches("memory::*", "memory::store"));
        assert!(hook_pattern_matches("*", "memory::store"));
    }

    #[test]
    fn valid_hook_types_recognised() {
        assert!(is_valid_hook_type("BeforeToolCall"));
        assert!(is_valid_hook_type("AfterToolCall"));
        assert!(is_valid_hook_type("BeforePromptBuild"));
        assert!(is_valid_hook_type("AgentLoopEnd"));
        assert!(is_valid_hook_type("SessionStart"));
        assert!(is_valid_hook_type("SessionEnd"));
        assert!(is_valid_hook_type("RequestStart"));
        assert!(is_valid_hook_type("RequestEnd"));
        assert!(is_valid_hook_type("BeforeCompact"));
        assert!(is_valid_hook_type("AfterCompact"));
    }

    #[test]
    fn invalid_hook_type_rejected() {
        assert!(!is_valid_hook_type("InvalidType"));
        assert!(!is_valid_hook_type(""));
    }

    #[test]
    fn hook_definition_round_trips() {
        let h = HookDefinition {
            id: "abc".into(),
            name: "n".into(),
            hook_type: "BeforeToolCall".into(),
            priority: 50,
            function_id: "fn::x".into(),
            enabled: true,
            agent_id: Some("agent-1".into()),
            filter: Some(json!({ "toolIds": ["fn::a"] })),
            created_at: 12345,
        };
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["type"], "BeforeToolCall");
        assert_eq!(v["functionId"], "fn::x");
        assert_eq!(v["agentId"], "agent-1");
        let parsed: HookDefinition = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.id, "abc");
        assert_eq!(parsed.priority, 50);
    }

    #[test]
    fn hook_priority_sort_lowest_first() {
        let mut hooks = vec![
            HookDefinition {
                id: "a".into(),
                name: "Low".into(),
                hook_type: "BeforeToolCall".into(),
                priority: 200,
                function_id: "fn".into(),
                enabled: true,
                agent_id: None,
                filter: None,
                created_at: 0,
            },
            HookDefinition {
                id: "b".into(),
                name: "High".into(),
                hook_type: "BeforeToolCall".into(),
                priority: 10,
                function_id: "fn".into(),
                enabled: true,
                agent_id: None,
                filter: None,
                created_at: 0,
            },
        ];
        hooks.sort_by(|a, b| a.priority.cmp(&b.priority));
        assert_eq!(hooks[0].name, "High");
        assert_eq!(hooks[1].name, "Low");
    }

    #[test]
    fn list_sort_groups_by_type_then_priority() {
        let mut hooks = vec![
            HookDefinition {
                id: "1".into(),
                name: "AfterLow".into(),
                hook_type: "AfterToolCall".into(),
                priority: 50,
                function_id: "fn".into(),
                enabled: true,
                agent_id: None,
                filter: None,
                created_at: 0,
            },
            HookDefinition {
                id: "2".into(),
                name: "BeforeHigh".into(),
                hook_type: "BeforeToolCall".into(),
                priority: 10,
                function_id: "fn".into(),
                enabled: true,
                agent_id: None,
                filter: None,
                created_at: 0,
            },
            HookDefinition {
                id: "3".into(),
                name: "AfterHigh".into(),
                hook_type: "AfterToolCall".into(),
                priority: 5,
                function_id: "fn".into(),
                enabled: true,
                agent_id: None,
                filter: None,
                created_at: 0,
            },
        ];
        hooks.sort_by(|a, b| {
            if a.hook_type != b.hook_type {
                a.hook_type.cmp(&b.hook_type)
            } else {
                a.priority.cmp(&b.priority)
            }
        });
        assert_eq!(hooks[0].name, "AfterHigh");
        assert_eq!(hooks[1].name, "AfterLow");
        assert_eq!(hooks[2].name, "BeforeHigh");
    }

    #[test]
    fn filter_tool_ids_when_present() {
        let hook = HookDefinition {
            id: "f".into(),
            name: "filter".into(),
            hook_type: "BeforeToolCall".into(),
            priority: 100,
            function_id: "fn".into(),
            enabled: true,
            agent_id: None,
            filter: Some(json!({ "toolIds": ["fn::a", "fn::b"] })),
            created_at: 0,
        };
        let allowed = match &hook.filter {
            Some(f) => match f.get("toolIds").and_then(|v| v.as_array()) {
                Some(ids) => ids.iter().any(|x| x.as_str() == Some("fn::a")),
                None => true,
            },
            None => true,
        };
        assert!(allowed);

        let not_allowed = match &hook.filter {
            Some(f) => match f.get("toolIds").and_then(|v| v.as_array()) {
                Some(ids) => ids.iter().any(|x| x.as_str() == Some("fn::z")),
                None => true,
            },
            None => true,
        };
        assert!(!not_allowed);
    }
}
