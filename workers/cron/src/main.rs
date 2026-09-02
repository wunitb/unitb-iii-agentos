use iii_sdk::errors::Error;
use iii_sdk::{
    IIIClient, RegisterFunction,
    protocol::{RegisterTriggerInput, TriggerRequest},
    register_worker,
    trigger::Trigger,
};
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

async fn list_scope(iii: &IIIClient, scope: &str) -> Vec<Value> {
    iii.trigger(TriggerRequest {
        function_id: "state::list".to_string(),
        payload: json!({ "scope": scope }),
        action: None,
        timeout_ms: None,
    })
    .await
    .ok()
    .and_then(|v| v.as_array().cloned())
    .unwrap_or_default()
}

/// `state::list` on iii 0.22.1 answers with a BARE ARRAY OF VALUES: no `key`,
/// no `{key, value}` envelope (verified against the pinned engine:
/// `iii trigger state::list scope=<s>` -> `[{...}, {...}]`). These two readers
/// accept the bare value and still tolerate an envelope, so a backend that
/// re-introduces one does not silently empty every list again.
fn entry_value(entry: &Value) -> &Value {
    match entry.get("value") {
        Some(value) if value.is_object() || value.is_array() => value,
        _ => entry,
    }
}

/// The record id, which after the bare-array change must come from inside the
/// value itself. Falls back to the envelope `key` when one is present.
fn entry_key(entry: &Value) -> Option<&str> {
    entry
        .get("key")
        .and_then(Value::as_str)
        .or_else(|| entry_value(entry).get("id").and_then(Value::as_str))
        .or_else(|| entry_value(entry).get("key").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
}

async fn cleanup_stale_sessions(iii: &IIIClient) -> Result<Value, Error> {
    let agents = list_scope(iii, "agents").await;
    let cutoff = now_ms() - 24 * 60 * 60 * 1000;
    let mut cleaned = 0u64;

    for agent in agents {
        let agent_id = match entry_key(&agent) {
            Some(id) => id.to_string(),
            None => continue,
        };

        let sessions = list_scope(iii, &format!("sessions:{agent_id}")).await;

        for session in sessions {
            let value = entry_value(&session);
            let last_active = value
                .get("lastActiveAt")
                .and_then(|v| v.as_i64())
                .or_else(|| value.get("createdAt").and_then(|v| v.as_i64()))
                .unwrap_or(0);

            if last_active != 0 && last_active < cutoff {
                let key = match entry_key(&session) {
                    Some(k) => k.to_string(),
                    None => continue,
                };
                let _ = iii
                    .trigger(TriggerRequest {
                        function_id: "state::delete".to_string(),
                        payload: json!({
                            "scope": format!("sessions:{agent_id}"),
                            "key": key,
                        }),
                        action: None,
                        timeout_ms: None,
                    })
                    .await;
                cleaned += 1;
            }
        }
    }

    Ok(json!({
        "cleaned": cleaned,
        "checkedAt": now_iso(),
    }))
}

async fn aggregate_daily_costs(iii: &IIIClient) -> Result<Value, Error> {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

    let costs = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({ "scope": "costs", "key": today }),
            action: None,
            timeout_ms: None,
        })
        .await
        .ok();

    if costs.is_some() {
        let metering = list_scope(iii, "metering").await;
        let mut total_tokens: u64 = 0;
        for entry in &metering {
            total_tokens += entry_value(entry)
                .get("totalTokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        }

        let _ = iii
            .trigger(TriggerRequest {
                function_id: "state::update".to_string(),
                payload: json!({
                    "scope": "costs",
                    "key": today,
                    // iii 0.22.1 names this field `ops`; `operations` failed the
                    // whole invocation with "missing field `ops`".
                    "ops": [
                        { "type": "set", "path": "totalTokens", "value": total_tokens },
                        { "type": "set", "path": "aggregatedAt", "value": now_iso() },
                    ],
                }),
                action: None,
                timeout_ms: None,
            })
            .await;
    }

    Ok(json!({
        "date": today,
        "aggregated": true,
    }))
}

async fn reset_rate_limits(iii: &IIIClient) -> Result<Value, Error> {
    let rates = list_scope(iii, "rates").await;
    let now = now_ms();
    let mut reset = 0u64;

    for rate in rates {
        let window_end = entry_value(&rate)
            .get("windowEnd")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        if window_end != 0 && window_end < now {
            let key = match entry_key(&rate) {
                Some(k) => k.to_string(),
                None => continue,
            };
            let _ = iii
                .trigger(TriggerRequest {
                    function_id: "state::delete".to_string(),
                    payload: json!({ "scope": "rates", "key": key }),
                    action: None,
                    timeout_ms: None,
                })
                .await;
            reset += 1;
        }
    }

    Ok(json!({
        "reset": reset,
        "checkedAt": now_iso(),
    }))
}

type TriggerRegistry = Arc<Mutex<HashMap<String, Trigger>>>;

const CRON_SCOPE: &str = "control:cron";
const TRIGGER_SCOPE: &str = "control:triggers";

/// Operator allowlist for everything `cron::create` and `trigger::create` may
/// mint, and for everything `control::rehydrate` may restore at boot.
///
/// `POST /api/cron` and `POST /api/triggers` used to accept ANY function id and
/// (for triggers) registered it with `iii.register_trigger` directly, i.e.
/// outside `agentos_http_adapter`, so one authenticated call minted a permanent
/// UNAUTHENTICATED route to any function on the bus — and `rehydrate_registry`
/// restored it after every restart.
///
/// Override with `AGENTOS_TRIGGER_ALLOWLIST` (comma-separated exact ids, or
/// `namespace::*` globs). `RESERVED_*` below is not overridable.
const DEFAULT_MINTABLE_FUNCTIONS: &[&str] = &[
    "cron::cleanup_stale_sessions",
    "cron::aggregate_daily_costs",
    "cron::reset_rate_limits",
    "workflow::run",
];

const TRIGGER_ALLOWLIST_ENV: &str = "AGENTOS_TRIGGER_ALLOWLIST";

/// Namespaces a minted job or trigger may never reach, whatever the operator
/// allowlist says. The first twelve are the deny-by-default set from the
/// remediation contract; the rest close self-amplification (a trigger that
/// mints triggers) and the engine's own control surface.
const RESERVED_NAMESPACES: &[&str] = &[
    "shell",
    "bridge",
    "mcp",
    "hook",
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

/// `cron::*` as a whole cannot be reserved — the worker's own maintenance jobs
/// live there — so the control functions are reserved by exact id instead.
const RESERVED_FUNCTIONS: &[&str] = &["cron::create", "cron::patch", "cron::delete"];

/// Trigger types a minted trigger may use. `http` is accepted but is always
/// routed through `agentos_http_adapter::register_http_trigger`, which attaches
/// the bearer check; `stream*` is refused because a minted join trigger sits on
/// the stream authorization path.
const MINTABLE_TRIGGER_TYPES: &[&str] = &["cron", "queue", "subscribe", "state", "log", "http"];

/// One glob definition for the whole tree: `agentos_http_adapter::policy`
/// (contract I1). `*` stands for one segment, a trailing `*` also covers any
/// further segments, and an empty pattern or id matches nothing. Callers here
/// reject malformed ids first (`ensure_mintable_function`), which is the only
/// thing the shared matcher deliberately leaves open.
fn allowlist_pattern_matches(pattern: &str, function_id: &str) -> bool {
    agentos_http_adapter::policy::capability_matches(pattern, function_id)
}

fn mintable_allowlist() -> Vec<String> {
    match std::env::var(TRIGGER_ALLOWLIST_ENV) {
        Ok(raw) if !raw.trim().is_empty() => raw
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_string)
            .collect(),
        _ => DEFAULT_MINTABLE_FUNCTIONS
            .iter()
            .map(|entry| (*entry).to_string())
            .collect(),
    }
}

fn is_reserved_function(function_id: &str) -> bool {
    if RESERVED_FUNCTIONS.contains(&function_id) {
        return true;
    }
    match function_id.split("::").next() {
        Some(namespace) if !namespace.is_empty() => RESERVED_NAMESPACES.contains(&namespace),
        _ => true,
    }
}

/// Gate for every function id a cron job or managed trigger can point at.
/// Applied inside `register_cron_job` / `register_managed_trigger`, so it covers
/// creation AND the boot-time rehydrate of anything already persisted.
fn ensure_mintable_function(function_id: &str) -> Result<(), Error> {
    let segments: Vec<&str> = function_id.split("::").collect();
    if segments.len() < 2 || segments.iter().any(|segment| segment.is_empty()) {
        return Err(Error::Handler(format!(
            "functionId must be a namespaced function id, got {function_id:?}"
        )));
    }
    if is_reserved_function(function_id) {
        return Err(Error::Handler(format!(
            "functionId {function_id} is reserved and can never be scheduled or triggered"
        )));
    }
    let allowlist = mintable_allowlist();
    if allowlist
        .iter()
        .any(|pattern| allowlist_pattern_matches(pattern, function_id))
    {
        return Ok(());
    }
    Err(Error::Handler(format!(
        "functionId {function_id} is not in the operator allowlist (set {TRIGGER_ALLOWLIST_ENV} to widen it)"
    )))
}

fn ensure_mintable_trigger_type(trigger_type: &str) -> Result<(), Error> {
    if MINTABLE_TRIGGER_TYPES.contains(&trigger_type) {
        Ok(())
    } else {
        Err(Error::Handler(format!(
            "trigger type {trigger_type} cannot be minted through this API"
        )))
    }
}

/// Strip a caller-supplied `auth` flag so `register_http_trigger` falls back to
/// its default (`auth: true`), which also refuses to register when
/// `AGENTOS_API_KEY` is unset.
fn forced_auth_http_config(config: Value) -> Value {
    let mut config = config;
    if let Some(object) = config.as_object_mut() {
        object.remove("auth");
    }
    config
}

fn required_string<'a>(input: &'a Value, key: &str) -> Result<&'a str, Error> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Handler(format!("{key} is required")))
}

fn lock_registry(
    registry: &TriggerRegistry,
) -> Result<std::sync::MutexGuard<'_, HashMap<String, Trigger>>, Error> {
    registry
        .lock()
        .map_err(|_| Error::Handler("trigger registry lock poisoned".to_string()))
}

async fn call_state(iii: &IIIClient, function_id: &str, payload: Value) -> Result<Value, Error> {
    iii.trigger(TriggerRequest {
        function_id: function_id.to_string(),
        payload,
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|error| Error::Handler(error.to_string()))
}

async fn state_record(iii: &IIIClient, scope: &str, id: &str) -> Result<Value, Error> {
    let value = call_state(iii, "state::get", json!({ "scope": scope, "key": id })).await?;
    if value.is_null() {
        return Err(Error::Handler(format!("record not found: {id}")));
    }
    Ok(value)
}

async fn set_state_record(
    iii: &IIIClient,
    scope: &str,
    id: &str,
    value: &Value,
) -> Result<(), Error> {
    call_state(
        iii,
        "state::set",
        json!({ "scope": scope, "key": id, "value": value }),
    )
    .await?;
    Ok(())
}

async fn delete_state_record(iii: &IIIClient, scope: &str, id: &str) -> Result<(), Error> {
    call_state(iii, "state::delete", json!({ "scope": scope, "key": id })).await?;
    Ok(())
}

fn state_values(entries: Value) -> Vec<Value> {
    entries
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| entry_value(entry).clone())
        .collect()
}

async fn list_control_records(iii: &IIIClient, scope: &str) -> Result<Value, Error> {
    let entries = call_state(iii, "state::list", json!({ "scope": scope })).await?;
    Ok(json!(state_values(entries)))
}

fn register_cron_job(iii: &IIIClient, record: &Value) -> Result<Trigger, Error> {
    let expression = required_string(record, "expression")?;
    let function_id = required_string(record, "functionId")?;
    ensure_mintable_function(function_id)?;
    agentos_http_adapter::register_cron_trigger(iii, function_id.to_string(), expression)
}

fn register_managed_trigger(iii: &IIIClient, record: &Value) -> Result<Trigger, Error> {
    let trigger_type = required_string(record, "type")?;
    let function_id = required_string(record, "functionId")?;
    ensure_mintable_trigger_type(trigger_type)?;
    ensure_mintable_function(function_id)?;
    let config = record.get("config").cloned().unwrap_or_else(|| json!({}));
    let metadata = record.get("metadata").cloned();

    if trigger_type == "http" {
        // Never `iii.register_trigger` an HTTP route directly: the AgentOS
        // bearer check lives inside the adapter closure, so a route registered
        // here would have no auth wrapper at all.
        return agentos_http_adapter::register_http_trigger(
            iii,
            function_id.to_string(),
            forced_auth_http_config(config),
            metadata,
        );
    }

    iii.register_trigger(RegisterTriggerInput {
        trigger_type: trigger_type.to_string(),
        function_id: function_id.to_string(),
        config,
        metadata,
    })
}

async fn create_cron_job(
    iii: &IIIClient,
    registry: &TriggerRegistry,
    input: Value,
) -> Result<Value, Error> {
    let id = uuid::Uuid::new_v4().to_string();
    let record = json!({
        "id": id,
        "expression": required_string(&input, "expression")?,
        "functionId": required_string(&input, "functionId")?,
        "enabled": true,
        "createdAt": now_iso(),
    });
    let handle = register_cron_job(iii, &record)?;
    if let Err(error) = set_state_record(iii, CRON_SCOPE, &id, &record).await {
        handle.unregister();
        return Err(error);
    }
    lock_registry(registry)?.insert(id, handle);
    Ok(record)
}

async fn patch_cron_job(
    iii: &IIIClient,
    registry: &TriggerRegistry,
    input: Value,
) -> Result<Value, Error> {
    let id = required_string(&input, "id")?;
    let enabled = input
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| Error::Handler("enabled is required".to_string()))?;
    let mut record = state_record(iii, CRON_SCOPE, id).await?;
    let current = record
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if current == enabled {
        return Ok(record);
    }

    let new_handle = if enabled {
        Some(register_cron_job(iii, &record)?)
    } else {
        None
    };
    record["enabled"] = json!(enabled);
    record["updatedAt"] = json!(now_iso());
    if let Err(error) = set_state_record(iii, CRON_SCOPE, id, &record).await {
        if let Some(handle) = new_handle {
            handle.unregister();
        }
        return Err(error);
    }

    let previous = if enabled {
        lock_registry(registry)?.insert(id.to_string(), new_handle.expect("enabled handle"))
    } else {
        lock_registry(registry)?.remove(id)
    };
    if let Some(handle) = previous {
        handle.unregister();
    }
    Ok(record)
}

async fn delete_control_record(
    iii: &IIIClient,
    registry: &TriggerRegistry,
    scope: &str,
    input: Value,
) -> Result<Value, Error> {
    let id = required_string(&input, "id")?;
    state_record(iii, scope, id).await?;
    delete_state_record(iii, scope, id).await?;
    if let Some(handle) = lock_registry(registry)?.remove(id) {
        handle.unregister();
    }
    Ok(json!({ "deleted": true, "id": id }))
}

async fn create_managed_trigger(
    iii: &IIIClient,
    registry: &TriggerRegistry,
    input: Value,
) -> Result<Value, Error> {
    let id = uuid::Uuid::new_v4().to_string();
    let mut record = Map::new();
    record.insert("id".to_string(), json!(id));
    record.insert("type".to_string(), json!(required_string(&input, "type")?));
    record.insert(
        "functionId".to_string(),
        json!(required_string(&input, "functionId")?),
    );
    record.insert(
        "config".to_string(),
        input.get("config").cloned().unwrap_or_else(|| json!({})),
    );
    if let Some(metadata) = input.get("metadata") {
        record.insert("metadata".to_string(), metadata.clone());
    }
    record.insert("createdAt".to_string(), json!(now_iso()));
    let record = Value::Object(record);
    let handle = register_managed_trigger(iii, &record)?;
    if let Err(error) = set_state_record(iii, TRIGGER_SCOPE, &id, &record).await {
        handle.unregister();
        return Err(error);
    }
    lock_registry(registry)?.insert(id, handle);
    Ok(record)
}

async fn rehydrate_registry(
    iii: &IIIClient,
    cron_registry: &TriggerRegistry,
    trigger_registry: &TriggerRegistry,
) -> Result<Value, Error> {
    let cron_entries = call_state(iii, "state::list", json!({ "scope": CRON_SCOPE })).await?;
    let trigger_entries = call_state(iii, "state::list", json!({ "scope": TRIGGER_SCOPE })).await?;
    let mut restored = 0u64;
    let mut errors = Vec::new();

    for record in state_values(cron_entries) {
        let id = record.get("id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty()
            || !record
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            continue;
        }
        if lock_registry(cron_registry)?.contains_key(id) {
            continue;
        }
        match register_cron_job(iii, &record) {
            Ok(handle) => {
                lock_registry(cron_registry)?.insert(id.to_string(), handle);
                restored += 1;
            }
            Err(error) => {
                // A persisted job whose target is no longer mintable is a
                // security event, not noise: it is exactly the record an
                // attacker would have planted before the allowlist landed.
                tracing::warn!(id, error = %error, "refused to rehydrate cron job");
                errors.push(format!("cron {id}: {error}"));
            }
        }
    }

    for record in state_values(trigger_entries) {
        let id = record.get("id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() || lock_registry(trigger_registry)?.contains_key(id) {
            continue;
        }
        match register_managed_trigger(iii, &record) {
            Ok(handle) => {
                lock_registry(trigger_registry)?.insert(id.to_string(), handle);
                restored += 1;
            }
            Err(error) => {
                tracing::warn!(id, error = %error, "refused to rehydrate managed trigger");
                errors.push(format!("trigger {id}: {error}"));
            }
        }
    }

    Ok(json!({ "restored": restored, "errors": errors }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());

    let cron_registry: TriggerRegistry = Arc::new(Mutex::new(HashMap::new()));
    let trigger_registry: TriggerRegistry = Arc::new(Mutex::new(HashMap::new()));

    let iii_ref = iii.clone();
    iii.register_function(
        "cron::list",
        RegisterFunction::new_async(move |_input: Value| {
            let iii = iii_ref.clone();
            async move { list_control_records(&iii, CRON_SCOPE).await }
        })
        .description("List managed cron jobs"),
    );

    let iii_ref = iii.clone();
    let registry_ref = cron_registry.clone();
    iii.register_function(
        "cron::create",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            let registry = registry_ref.clone();
            async move { create_cron_job(&iii, &registry, input).await }
        })
        .description("Create and enable a managed cron job"),
    );

    let iii_ref = iii.clone();
    let registry_ref = cron_registry.clone();
    iii.register_function(
        "cron::patch",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            let registry = registry_ref.clone();
            async move { patch_cron_job(&iii, &registry, input).await }
        })
        .description("Enable or disable a managed cron job"),
    );

    let iii_ref = iii.clone();
    let registry_ref = cron_registry.clone();
    iii.register_function(
        "cron::delete",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            let registry = registry_ref.clone();
            async move { delete_control_record(&iii, &registry, CRON_SCOPE, input).await }
        })
        .description("Delete a managed cron job"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "trigger::list",
        RegisterFunction::new_async(move |_input: Value| {
            let iii = iii_ref.clone();
            async move { list_control_records(&iii, TRIGGER_SCOPE).await }
        })
        .description("List managed iii triggers"),
    );

    let iii_ref = iii.clone();
    let registry_ref = trigger_registry.clone();
    iii.register_function(
        "trigger::create",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            let registry = registry_ref.clone();
            async move { create_managed_trigger(&iii, &registry, input).await }
        })
        .description("Create a managed iii trigger"),
    );

    let iii_ref = iii.clone();
    let registry_ref = trigger_registry.clone();
    iii.register_function(
        "trigger::delete",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            let registry = registry_ref.clone();
            async move { delete_control_record(&iii, &registry, TRIGGER_SCOPE, input).await }
        })
        .description("Delete a managed iii trigger"),
    );

    let iii_ref = iii.clone();
    let cron_registry_ref = cron_registry.clone();
    let trigger_registry_ref = trigger_registry.clone();
    iii.register_function(
        "control::rehydrate",
        RegisterFunction::new_async(move |_input: Value| {
            let iii = iii_ref.clone();
            let cron_registry = cron_registry_ref.clone();
            let trigger_registry = trigger_registry_ref.clone();
            async move { rehydrate_registry(&iii, &cron_registry, &trigger_registry).await }
        })
        .description("Restore persisted cron jobs and triggers"),
    );

    for (function_id, method, path) in [
        ("cron::list", "GET", "api/cron"),
        ("cron::create", "POST", "api/cron"),
        ("cron::patch", "PATCH", "api/cron/:id"),
        ("cron::delete", "DELETE", "api/cron/:id"),
        ("trigger::list", "GET", "api/triggers"),
        ("trigger::create", "POST", "api/triggers"),
        ("trigger::delete", "DELETE", "api/triggers/:id"),
    ] {
        agentos_http_adapter::register_http_trigger(
            &iii,
            function_id.to_string(),
            json!({ "http_method": method, "api_path": path }),
            None,
        )?;
    }

    let iii_boot = iii.clone();
    let cron_registry_boot = cron_registry.clone();
    let trigger_registry_boot = trigger_registry.clone();
    tokio::spawn(async move {
        for attempt in 1..=5 {
            tokio::time::sleep(std::time::Duration::from_millis(250 * attempt)).await;
            match rehydrate_registry(&iii_boot, &cron_registry_boot, &trigger_registry_boot).await {
                Ok(result) => {
                    tracing::info!(result = %result, "control registries rehydrated");
                    return;
                }
                Err(error) if attempt < 5 => {
                    tracing::debug!(attempt, error = %error, "control rehydration deferred");
                }
                Err(error) => {
                    tracing::error!(error = %error, "control rehydration failed");
                }
            }
        }
    });

    let iii_ref = iii.clone();
    iii.register_function(
        "cron::cleanup_stale_sessions",
        RegisterFunction::new_async(move |_input: Value| {
            let iii = iii_ref.clone();
            async move { cleanup_stale_sessions(&iii).await }
        })
        .description("Clean up sessions inactive for more than 24 hours"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "cron::aggregate_daily_costs",
        RegisterFunction::new_async(move |_input: Value| {
            let iii = iii_ref.clone();
            async move { aggregate_daily_costs(&iii).await }
        })
        .description("Aggregate and summarize daily cost data"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "cron::reset_rate_limits",
        RegisterFunction::new_async(move |_input: Value| {
            let iii = iii_ref.clone();
            async move { reset_rate_limits(&iii).await }
        })
        .description("Reset expired rate limit windows"),
    );

    agentos_http_adapter::register_cron_trigger(
        &iii,
        "cron::cleanup_stale_sessions".to_string(),
        "0 */6 * * *",
    )?;
    agentos_http_adapter::register_cron_trigger(
        &iii,
        "cron::aggregate_daily_costs".to_string(),
        "0 * * * *",
    )?;
    agentos_http_adapter::register_cron_trigger(
        &iii,
        "cron::reset_rate_limits".to_string(),
        "*/5 * * * *",
    )?;

    tracing::info!("cron worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- route-factory closure (plan item 12)

    fn with_allowlist<T>(value: Option<&str>, test: impl FnOnce() -> T) -> T {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os(TRIGGER_ALLOWLIST_ENV);
        unsafe {
            match value {
                Some(value) => std::env::set_var(TRIGGER_ALLOWLIST_ENV, value),
                None => std::env::remove_var(TRIGGER_ALLOWLIST_ENV),
            }
        }
        let result = test();
        unsafe {
            match previous {
                Some(value) => std::env::set_var(TRIGGER_ALLOWLIST_ENV, value),
                None => std::env::remove_var(TRIGGER_ALLOWLIST_ENV),
            }
        }
        result
    }

    /// The mint deny set is deliberately WIDER than the capability deny set:
    /// `agentos_http_adapter::policy::DENY_BY_DEFAULT_FAMILIES` says what an
    /// agent may never be granted by a wildcard, while this says what may never
    /// be scheduled or triggered at all — which additionally covers the
    /// control-plane families (`trigger`, `control`, `configuration`,
    /// `security`, `approval`, `agentos`) and `coder`. Two different questions,
    /// so two different sets; this asserts the containment that must hold, so a
    /// family added to the shared contract cannot quietly become mintable.
    #[test]
    fn reserved_namespaces_are_a_superset_of_the_shared_contract_families() {
        for family in agentos_http_adapter::policy::DENY_BY_DEFAULT_FAMILIES {
            if family == "cron" {
                // `cron::*` cannot be reserved wholesale — the worker's own
                // maintenance jobs are the canonical schedulable targets — so
                // the control functions are reserved by exact id instead.
                assert!(RESERVED_FUNCTIONS.contains(&"cron::create"));
                assert!(RESERVED_FUNCTIONS.contains(&"cron::patch"));
                assert!(RESERVED_FUNCTIONS.contains(&"cron::delete"));
                continue;
            }
            assert!(
                RESERVED_NAMESPACES.contains(&family),
                "{family} is deny-by-default for capabilities but mintable as a trigger"
            );
        }
    }

    #[test]
    fn minting_refuses_the_privileged_namespaces() {
        with_allowlist(None, || {
            for function_id in [
                "vault::get",
                "shell::exec",
                "bridge::invoke",
                "mcp::connect",
                "state::set",
                "hook::register",
                "engine::functions::list",
                "harness::spawn",
                "browser::navigate",
                "wasm::run",
                "coder::apply",
                "security::set_capabilities",
                "trigger::create",
                "control::rehydrate",
                "cron::create",
                "cron::delete",
                "cron::patch",
            ] {
                let error = ensure_mintable_function(function_id)
                    .expect_err(&format!("{function_id} must not be mintable"))
                    .to_string();
                assert!(error.contains("reserved"), "{function_id}: {error}");
            }
        });
    }

    #[test]
    fn minting_refuses_ids_outside_the_allowlist() {
        with_allowlist(None, || {
            let error = ensure_mintable_function("memory::store")
                .expect_err("an unlisted id must be refused")
                .to_string();
            assert!(error.contains("not in the operator allowlist"), "{error}");
        });
    }

    #[test]
    fn minting_accepts_the_default_maintenance_jobs() {
        with_allowlist(None, || {
            for function_id in DEFAULT_MINTABLE_FUNCTIONS {
                ensure_mintable_function(function_id)
                    .unwrap_or_else(|error| panic!("{function_id} must be mintable: {error}"));
            }
        });
    }

    #[test]
    fn operator_allowlist_widens_but_never_reaches_a_reserved_id() {
        with_allowlist(Some("memory::*, workflow::run"), || {
            assert!(ensure_mintable_function("memory::store").is_ok());
            assert!(ensure_mintable_function("workflow::run").is_ok());
            assert!(ensure_mintable_function("memory::session::list").is_ok());
            assert!(ensure_mintable_function("agent::chat").is_err());
        });
        with_allowlist(Some("*"), || {
            assert!(
                ensure_mintable_function("vault::get").is_err(),
                "a wildcard allowlist must still not reach a reserved namespace"
            );
            assert!(
                ensure_mintable_function("shell::exec").is_err(),
                "a wildcard allowlist must still not reach a reserved namespace"
            );
            assert!(ensure_mintable_function("memory::store").is_ok());
        });
    }

    #[test]
    fn minting_refuses_ids_that_are_not_namespaced() {
        with_allowlist(Some("*"), || {
            for function_id in ["", "bare", "::", "::store", "memory::"] {
                assert!(
                    ensure_mintable_function(function_id).is_err(),
                    "{function_id:?} must be refused"
                );
            }
        });
    }

    #[test]
    fn only_known_trigger_types_can_be_minted() {
        for trigger_type in MINTABLE_TRIGGER_TYPES {
            assert!(ensure_mintable_trigger_type(trigger_type).is_ok());
        }
        for trigger_type in ["stream", "stream:join", "stream:leave", "made-up", ""] {
            assert!(
                ensure_mintable_trigger_type(trigger_type).is_err(),
                "{trigger_type} must not be mintable"
            );
        }
    }

    #[test]
    fn http_config_never_carries_a_caller_supplied_auth_flag() {
        let config = forced_auth_http_config(json!({
            "api_path": "/pwn",
            "http_method": "POST",
            "auth": false,
        }));
        assert!(
            config.get("auth").is_none(),
            "auth must fall back to the adapter default (true)"
        );
        assert_eq!(config["api_path"], "/pwn");
    }

    #[test]
    fn allowlist_glob_has_no_prefix_semantics() {
        assert!(!allowlist_pattern_matches("", "memory::store"));
        assert!(!allowlist_pattern_matches("memory", "memory::store"));
        assert!(!allowlist_pattern_matches("memory::st", "memory::store"));
        assert!(allowlist_pattern_matches("memory::*", "memory::store"));
        assert!(allowlist_pattern_matches("*", "memory::store"));
        assert!(!allowlist_pattern_matches("*", ""));
    }

    #[test]
    fn entry_readers_accept_the_bare_state_list_shape() {
        // iii 0.22.1 `state::list` returns bare values.
        let bare = json!({ "id": "job-1", "expression": "0 * * * * *" });
        assert_eq!(entry_value(&bare), &bare);
        assert_eq!(entry_key(&bare), Some("job-1"));

        // an envelope, if a backend ever supplies one, still resolves
        let enveloped = json!({ "key": "job-2", "value": { "id": "job-2" } });
        assert_eq!(entry_value(&enveloped), &json!({ "id": "job-2" }));
        assert_eq!(entry_key(&enveloped), Some("job-2"));

        // and a record with neither is skipped rather than mis-keyed
        assert_eq!(entry_key(&json!({ "expression": "* * * * * *" })), None);
        assert_eq!(entry_key(&json!({ "id": "" })), None);
    }

    #[test]
    fn state_values_passes_bare_records_through() {
        let bare = json!([{ "id": "a", "type": "cron" }, { "id": "b", "type": "cron" }]);
        let values = state_values(bare);
        assert_eq!(values.len(), 2, "bare values must not be dropped");
        assert_eq!(values[0]["id"], "a");

        let enveloped = json!([{ "key": "a", "value": { "id": "a" } }]);
        assert_eq!(state_values(enveloped), vec![json!({ "id": "a" })]);
    }

    #[test]
    fn metering_total_reads_bare_records() {
        let metering = [json!({ "totalTokens": 500 }), json!({ "totalTokens": 300 })];
        let total: u64 = metering
            .iter()
            .map(|entry| {
                entry_value(entry)
                    .get("totalTokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
            })
            .sum();
        assert_eq!(total, 800);
    }

    #[test]
    fn test_now_ms_positive() {
        assert!(now_ms() > 0);
    }

    #[test]
    fn test_now_iso_format() {
        let s = now_iso();
        assert!(s.contains('T'));
        assert!(s.len() >= 19);
    }

    #[test]
    fn test_today_format() {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        assert_eq!(today.len(), 10);
        assert_eq!(&today[4..5], "-");
        assert_eq!(&today[7..8], "-");
    }

    #[test]
    fn test_cleanup_cutoff_24h() {
        let cutoff = now_ms() - 24 * 60 * 60 * 1000;
        let now = now_ms();
        assert!(cutoff < now);
        assert!((now - cutoff) >= 24 * 60 * 60 * 1000);
    }

    #[test]
    fn test_session_value_extraction() {
        let session = json!({
            "key": "session-1",
            "value": { "lastActiveAt": 12345, "createdAt": 67890 },
        });
        let value = session.get("value").cloned().unwrap_or(json!({}));
        let last_active = value
            .get("lastActiveAt")
            .and_then(|v| v.as_i64())
            .or_else(|| value.get("createdAt").and_then(|v| v.as_i64()))
            .unwrap_or(0);
        assert_eq!(last_active, 12345);
    }

    #[test]
    fn test_session_value_falls_back_to_created_at() {
        let session = json!({
            "key": "session-1",
            "value": { "createdAt": 67890 },
        });
        let value = session.get("value").cloned().unwrap_or(json!({}));
        let last_active = value
            .get("lastActiveAt")
            .and_then(|v| v.as_i64())
            .or_else(|| value.get("createdAt").and_then(|v| v.as_i64()))
            .unwrap_or(0);
        assert_eq!(last_active, 67890);
    }

    #[test]
    fn test_session_value_missing_returns_zero() {
        let session = json!({ "key": "s", "value": {} });
        let value = session.get("value").cloned().unwrap_or(json!({}));
        let last_active = value
            .get("lastActiveAt")
            .and_then(|v| v.as_i64())
            .or_else(|| value.get("createdAt").and_then(|v| v.as_i64()))
            .unwrap_or(0);
        assert_eq!(last_active, 0);
    }

    #[test]
    fn test_metering_total_tokens_sum() {
        let metering = vec![
            json!({ "key": "e1", "value": { "totalTokens": 500 } }),
            json!({ "key": "e2", "value": { "totalTokens": 300 } }),
            json!({ "key": "e3", "value": {} }),
        ];
        let mut total: u64 = 0;
        for entry in &metering {
            total += entry
                .get("value")
                .and_then(|v| v.get("totalTokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        }
        assert_eq!(total, 800);
    }

    #[test]
    fn test_rate_window_expired_detection() {
        let rate = json!({ "key": "r1", "value": { "windowEnd": 1000 } });
        let value = rate.get("value").cloned().unwrap_or(json!({}));
        let window_end = value.get("windowEnd").and_then(|v| v.as_i64()).unwrap_or(0);
        let now = now_ms();
        assert!(window_end < now);
    }

    #[test]
    fn test_rate_window_future_active() {
        let future = now_ms() + 60_000;
        let rate = json!({ "key": "r1", "value": { "windowEnd": future } });
        let value = rate.get("value").cloned().unwrap_or(json!({}));
        let window_end = value.get("windowEnd").and_then(|v| v.as_i64()).unwrap_or(0);
        let now = now_ms();
        assert!(window_end >= now);
    }

    #[test]
    fn test_agent_id_from_key_field() {
        let agent = json!({ "key": "agent-1", "value": {} });
        let id = agent
            .get("key")
            .and_then(|v| v.as_str())
            .or_else(|| agent.get("id").and_then(|v| v.as_str()))
            .map(String::from);
        assert_eq!(id.as_deref(), Some("agent-1"));
    }

    #[test]
    fn test_agent_id_from_id_field_fallback() {
        let agent = json!({ "id": "agent-2" });
        let id = agent
            .get("key")
            .and_then(|v| v.as_str())
            .or_else(|| agent.get("id").and_then(|v| v.as_str()))
            .map(String::from);
        assert_eq!(id.as_deref(), Some("agent-2"));
    }

    #[test]
    fn test_agent_id_missing_returns_none() {
        let agent = json!({ "value": {} });
        let id = agent
            .get("key")
            .and_then(|v| v.as_str())
            .or_else(|| agent.get("id").and_then(|v| v.as_str()))
            .map(String::from);
        assert_eq!(id, None);
    }

    #[test]
    fn test_cleanup_result_shape() {
        let result = json!({
            "cleaned": 5u64,
            "checkedAt": now_iso(),
        });
        assert_eq!(result["cleaned"], 5);
        assert!(result["checkedAt"].is_string());
    }

    #[test]
    fn test_aggregate_result_shape() {
        let result = json!({
            "date": "2026-01-01",
            "aggregated": true,
        });
        assert_eq!(result["date"], "2026-01-01");
        assert_eq!(result["aggregated"], true);
    }

    #[test]
    fn test_reset_result_shape() {
        let result = json!({
            "reset": 3u64,
            "checkedAt": now_iso(),
        });
        assert_eq!(result["reset"], 3);
        assert!(result["checkedAt"].is_string());
    }
}
