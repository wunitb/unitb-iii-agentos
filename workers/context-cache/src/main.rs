use dashmap::DashMap;
use iii_sdk::errors::Error;
use iii_sdk::{
    IIIClient, InitOptions, RegisterFunction, protocol::TriggerRequest, register_worker,
};
use serde_json::{Value, json};
use std::sync::Arc;

mod types;

use types::{CacheEntry, CacheStats, cacheable, sanitize_id};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn payload_body(input: &Value) -> Value {
    if input.get("body").is_some() {
        input["body"].clone()
    } else {
        input.clone()
    }
}

#[derive(Clone, Default)]
struct State {
    stats: Arc<DashMap<String, CacheStats>>,
}

impl State {
    fn record_hit(&self, agent_id: &str) {
        let mut entry = self.stats.entry(agent_id.to_string()).or_default();
        entry.hits += 1;
    }
    fn record_miss(&self, agent_id: &str) {
        let mut entry = self.stats.entry(agent_id.to_string()).or_default();
        entry.misses += 1;
    }
    fn get(&self, agent_id: &str) -> CacheStats {
        self.stats
            .get(agent_id)
            .map(|s| *s.value())
            .unwrap_or_default()
    }
    fn snapshot(&self) -> Value {
        let mut map = serde_json::Map::new();
        for kv in self.stats.iter() {
            map.insert(
                kv.key().clone(),
                serde_json::to_value(*kv.value()).unwrap_or(json!({})),
            );
        }
        Value::Object(map)
    }
}

async fn get_or_fetch(iii: &IIIClient, state: &State, input: Value) -> Result<Value, Error> {
    let body = payload_body(&input);

    let fetch_fn = body["fetchFunctionId"]
        .as_str()
        .ok_or_else(|| Error::Handler("fetchFunctionId required".into()))?
        .to_string();
    if !cacheable(&fetch_fn) {
        return Err(Error::Handler(format!(
            "Function {fetch_fn} is not cacheable"
        )));
    }

    let agent_id_raw = body["agentId"]
        .as_str()
        .ok_or_else(|| Error::Handler("agentId required".into()))?;
    let agent_id = sanitize_id(agent_id_raw);
    let key = body["key"]
        .as_str()
        .ok_or_else(|| Error::Handler("key required".into()))?
        .to_string();
    let ttl_ms = body["ttlMs"]
        .as_i64()
        .ok_or_else(|| Error::Handler("ttlMs required".into()))?;
    let fetch_payload = body.get("fetchPayload").cloned().unwrap_or(Value::Null);

    let scope = format!("cache:{agent_id}");

    let cached_raw = iii
        .trigger(TriggerRequest {
            function_id: "state::get".into(),
            payload: json!({ "scope": &scope, "key": &key }),
            action: None,
            timeout_ms: None,
        })
        .await
        .ok();

    if let Some(cached_value) = cached_raw.as_ref().filter(|v| !v.is_null())
        && let Ok(entry) = serde_json::from_value::<CacheEntry>(cached_value.clone())
        && now_ms() - entry.cached_at < entry.ttl_ms
    {
        state.record_hit(&agent_id);
        return Ok(entry.value);
    }

    state.record_miss(&agent_id);

    let value = iii
        .trigger(TriggerRequest {
            function_id: fetch_fn,
            payload: fetch_payload,
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let entry = CacheEntry {
        value: value.clone(),
        cached_at: now_ms(),
        ttl_ms,
    };

    iii.trigger(TriggerRequest {
        function_id: "state::set".into(),
        payload: json!({
            "scope": &scope,
            "key": &key,
            "value": serde_json::to_value(&entry).map_err(|e| Error::Handler(e.to_string()))?,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    Ok(value)
}

async fn invalidate(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let body = payload_body(&input);
    let agent_id_raw = body["agentId"]
        .as_str()
        .ok_or_else(|| Error::Handler("agentId required".into()))?;
    let agent_id = sanitize_id(agent_id_raw);
    let scope = format!("cache:{agent_id}");

    if let Some(key) = body["key"].as_str() {
        iii.trigger(TriggerRequest {
            function_id: "state::delete".into(),
            payload: json!({ "scope": &scope, "key": key }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;
        return Ok(json!({ "cleared": 1 }));
    }

    let entries = iii
        .trigger(TriggerRequest {
            function_id: "state::list".into(),
            payload: json!({ "scope": &scope }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or_else(|_| json!([]));

    let mut cleared = 0;
    if let Some(arr) = entries.as_array() {
        for entry in arr {
            if let Some(k) = entry["key"].as_str()
                && iii
                    .trigger(TriggerRequest {
                        function_id: "state::delete".into(),
                        payload: json!({ "scope": &scope, "key": k }),
                        action: None,
                        timeout_ms: None,
                    })
                    .await
                    .is_ok()
            {
                cleared += 1;
            }
        }
    }

    Ok(json!({ "cleared": cleared }))
}

async fn stats_fn(state: &State, input: Value) -> Result<Value, Error> {
    let body = payload_body(&input);
    if let Some(agent_id_raw) = body["agentId"].as_str() {
        // Stats are recorded under the sanitized id (see record_hit/miss in
        // get_or_fetch); normalize here so a lookup with the raw id matches.
        let agent_id = sanitize_id(agent_id_raw);
        let stats = state.get(&agent_id);
        return Ok(serde_json::to_value(stats).unwrap_or(json!({})));
    }
    Ok(state.snapshot())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, InitOptions::default());
    let state = State::default();

    let iii_ref = iii.clone();
    let state_ref = state.clone();
    iii.register_function(
        "context_cache::get_or_fetch",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            let state = state_ref.clone();
            async move { get_or_fetch(&iii, &state, input).await }
        })
        .description("Memoized context fetch with TTL-based expiry"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "context_cache::invalidate",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { invalidate(&iii, input).await }
        })
        .description("Clear cache entry or all entries for an agent"),
    );

    let state_ref = state.clone();
    iii.register_function(
        "context_cache::stats",
        RegisterFunction::new_async(move |input: Value| {
            let state = state_ref.clone();
            async move { stats_fn(&state, input).await }
        })
        .description("Cache hit/miss stats per agent"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "context_cache::get_or_fetch",
        json!({ "http_method": "POST", "api_path": "api/context-cache/fetch" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "context_cache::invalidate",
        json!({ "http_method": "POST", "api_path": "api/context-cache/invalidate" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "context_cache::stats",
        json!({ "http_method": "POST", "api_path": "api/context-cache/stats" }),
        None,
    )?;

    tracing::info!("context-cache worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_records_hits_and_misses() {
        let s = State::default();
        s.record_hit("a1");
        s.record_hit("a1");
        s.record_miss("a1");
        let stats = s.get("a1");
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 1);
    }

    #[test]
    fn state_snapshot_includes_all() {
        let s = State::default();
        s.record_hit("a");
        s.record_miss("b");
        let snap = s.snapshot();
        assert!(snap["a"]["hits"].as_u64().unwrap() == 1);
        assert!(snap["b"]["misses"].as_u64().unwrap() == 1);
    }

    #[test]
    fn unknown_agent_returns_zero_stats() {
        let s = State::default();
        let stats = s.get("missing");
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 0);
    }
}
