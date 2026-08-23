use iii_sdk::errors::Error;
use iii_sdk::{
    IIIClient, InitOptions, RegisterFunction,
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

async fn cleanup_stale_sessions(iii: &IIIClient) -> Result<Value, Error> {
    let agents = list_scope(iii, "agents").await;
    let cutoff = now_ms() - 24 * 60 * 60 * 1000;
    let mut cleaned = 0u64;

    for agent in agents {
        let agent_id = agent
            .get("key")
            .and_then(|v| v.as_str())
            .or_else(|| agent.get("id").and_then(|v| v.as_str()))
            .map(String::from);

        let agent_id = match agent_id {
            Some(id) if !id.is_empty() => id,
            _ => continue,
        };

        let sessions = list_scope(iii, &format!("sessions:{agent_id}")).await;

        for session in sessions {
            let value = session.get("value").cloned().unwrap_or(json!({}));
            let last_active = value
                .get("lastActiveAt")
                .and_then(|v| v.as_i64())
                .or_else(|| value.get("createdAt").and_then(|v| v.as_i64()))
                .unwrap_or(0);

            if last_active != 0 && last_active < cutoff {
                let key = match session.get("key").and_then(|v| v.as_str()) {
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
            total_tokens += entry
                .get("value")
                .and_then(|v| v.get("totalTokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        }

        let _ = iii
            .trigger(TriggerRequest {
                function_id: "state::update".to_string(),
                payload: json!({
                    "scope": "costs",
                    "key": today,
                    "operations": [
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
        let value = rate.get("value").cloned().unwrap_or(json!({}));
        let window_end = value.get("windowEnd").and_then(|v| v.as_i64()).unwrap_or(0);

        if window_end != 0 && window_end < now {
            let key = match rate.get("key").and_then(|v| v.as_str()) {
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
        .filter_map(|entry| entry.get("value").cloned())
        .collect()
}

async fn list_control_records(iii: &IIIClient, scope: &str) -> Result<Value, Error> {
    let entries = call_state(iii, "state::list", json!({ "scope": scope })).await?;
    Ok(json!(state_values(entries)))
}

fn register_cron_job(iii: &IIIClient, record: &Value) -> Result<Trigger, Error> {
    let expression = required_string(record, "expression")?;
    let function_id = required_string(record, "functionId")?;
    agentos_http_adapter::register_cron_trigger(iii, function_id.to_string(), expression)
}

fn register_managed_trigger(iii: &IIIClient, record: &Value) -> Result<Trigger, Error> {
    let trigger_type = required_string(record, "type")?;
    let function_id = required_string(record, "functionId")?;
    iii.register_trigger(RegisterTriggerInput {
        trigger_type: trigger_type.to_string(),
        function_id: function_id.to_string(),
        config: record.get("config").cloned().unwrap_or_else(|| json!({})),
        metadata: record.get("metadata").cloned(),
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
            Err(error) => errors.push(format!("cron {id}: {error}")),
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
            Err(error) => errors.push(format!("trigger {id}: {error}")),
        }
    }

    Ok(json!({ "restored": restored, "errors": errors }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, InitOptions::default());

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
