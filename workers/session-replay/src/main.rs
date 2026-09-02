use iii_sdk::errors::Error;
use iii_sdk::{IIIClient, RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};

mod types;

use types::ReplayEntry;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn record(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let session_id = input["sessionId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    let agent_id = input["agentId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    let action = input["action"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);

    let (Some(session_id), Some(agent_id), Some(action)) = (session_id, agent_id, action) else {
        return Ok(json!({ "error": "sessionId, agentId, and action required" }));
    };

    let data = input.get("data").cloned().unwrap_or_else(|| json!({}));
    let duration_ms = input["durationMs"].as_i64().unwrap_or(0);
    let iteration = input["iteration"].as_i64().unwrap_or(0);

    let counter_key = format!("{session_id}:counter");
    // state::update is the only mechanism that gives us atomic ordering for
    // replay sequence numbers. Falling back to now_ms() here breaks that
    // guarantee under load and lets two concurrent records collide on the
    // same key.
    let counter_resp = iii
        .trigger(TriggerRequest {
            function_id: "state::update".into(),
            payload: counter_payload(&counter_key),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(format!("failed to increment replay counter: {e}")))?;

    if let Some(rejection) = update_rejection(&counter_resp) {
        return Err(Error::Handler(format!(
            "replay counter update rejected for {counter_key}: {rejection}"
        )));
    }

    let sequence = sequence_from_update(&counter_resp)
        .ok_or_else(|| Error::Handler("invalid counter response from state::update".into()))?;

    let entry = ReplayEntry {
        session_id: session_id.clone(),
        agent_id,
        action,
        data,
        duration_ms,
        timestamp: now_ms(),
        iteration,
        sequence,
    };

    let entry_value = serde_json::to_value(&entry).map_err(|e| Error::Handler(e.to_string()))?;
    let key = format!("{session_id}:{:0>8}", sequence);

    iii.trigger(TriggerRequest {
        function_id: "state::set".into(),
        payload: json!({
            "scope": "replay",
            "key": key,
            "value": entry_value,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    Ok(json!({ "recorded": true, "sequence": sequence }))
}

/// `state::update` payload for the per-session sequence counter.
///
/// The engine names the operation list `ops` (an `operations` key fails the
/// whole invocation with "missing field `ops`") and an `increment` carries the
/// step in `by`, not `value`. A missing key increments from zero, so no
/// separate upsert is needed.
fn counter_payload(counter_key: &str) -> Value {
    json!({
        "scope": "replay",
        "key": counter_key,
        "ops": [
            { "type": "increment", "path": "value", "by": 1 }
        ],
    })
}

/// A rejected operation still answers success at the transport level: the
/// engine reports it inside an `errors` array of an otherwise normal result.
fn update_rejection(result: &Value) -> Option<String> {
    let errors = result.get("errors")?.as_array()?;
    if errors.is_empty() {
        return None;
    }
    Some(Value::Array(errors.clone()).to_string())
}

/// The post-increment sequence number. `state::update` answers
/// `{"new_value": ..., "old_value": ...}`, and the counter lives at
/// `new_value.value` because the increment targets the `value` path.
fn sequence_from_update(result: &Value) -> Option<i64> {
    result.get("new_value")?.get("value")?.as_i64()
}

/// `state::list` answers a bare array of stored values: there is no `key` to
/// filter on and no `value` envelope to unwrap. The sequence counters share
/// the `replay` scope, so they are excluded by shape instead.
fn parse_entries(list: &Value) -> Vec<ReplayEntry> {
    let Some(arr) = list.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for value in arr {
        if !value.is_object() {
            continue;
        }
        // A counter document is `{"value": n}`: it carries neither field.
        if value["sessionId"].as_str().is_none() || value["action"].as_str().is_none() {
            continue;
        }
        if let Ok(entry) = serde_json::from_value::<ReplayEntry>(value.clone()) {
            out.push(entry);
        }
    }
    out
}

async fn list_replays(iii: &IIIClient) -> Value {
    iii.trigger(TriggerRequest {
        function_id: "state::list".into(),
        payload: json!({ "scope": "replay" }),
        action: None,
        timeout_ms: None,
    })
    .await
    .unwrap_or_else(|_| json!([]))
}

async fn get_session(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let session_id = input["sessionId"].as_str().unwrap_or("");
    if session_id.is_empty() {
        return Ok(json!({ "error": "sessionId required" }));
    }

    let raw = list_replays(iii).await;
    let mut entries: Vec<ReplayEntry> = parse_entries(&raw)
        .into_iter()
        .filter(|e| e.session_id == session_id)
        .collect();
    entries.sort_by_key(|e| e.sequence);
    serde_json::to_value(entries).map_err(|e| Error::Handler(e.to_string()))
}

async fn search(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let agent_filter = input["agentId"].as_str().map(String::from);
    let tool_filter = input["toolUsed"].as_str().map(String::from);
    let raw_limit = input["limit"].as_i64().unwrap_or(50);
    let limit = raw_limit.clamp(1, 200) as usize;

    let from = input["timeRange"]["from"].as_i64();
    let to = input["timeRange"]["to"].as_i64();
    let has_time_range = from.is_some() && to.is_some();

    let raw = list_replays(iii).await;
    let entries = parse_entries(&raw);

    let mut session_map: BTreeMap<String, Vec<ReplayEntry>> = BTreeMap::new();
    for entry in entries {
        if let Some(ref a) = agent_filter
            && entry.agent_id != *a
        {
            continue;
        }
        if has_time_range {
            let f = from.unwrap();
            let t = to.unwrap();
            if entry.timestamp < f || entry.timestamp > t {
                continue;
            }
        }
        if let Some(ref tool) = tool_filter {
            let matches =
                entry.action == "tool_call" && entry.data["toolId"].as_str() == Some(tool.as_str());
            if !matches {
                continue;
            }
        }
        session_map
            .entry(entry.session_id.clone())
            .or_default()
            .push(entry);
    }

    let mut summaries: Vec<Value> = session_map
        .into_iter()
        .map(|(sid, actions)| {
            let agent = actions
                .first()
                .map(|a| a.agent_id.clone())
                .unwrap_or_default();
            let action_count = actions.len();
            let start = actions.iter().map(|a| a.timestamp).min().unwrap_or(0);
            let end = actions.iter().map(|a| a.timestamp).max().unwrap_or(0);
            json!({
                "sessionId": sid,
                "agentId": agent,
                "actionCount": action_count,
                "startTime": start,
                "endTime": end,
            })
        })
        .collect();

    summaries.sort_by(|a, b| {
        b["startTime"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&a["startTime"].as_i64().unwrap_or(0))
    });
    summaries.truncate(limit);

    Ok(json!(summaries))
}

async fn summary(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let session_id = input["sessionId"].as_str().unwrap_or("");
    if session_id.is_empty() {
        return Ok(json!({ "error": "sessionId required" }));
    }

    let entries_value = iii
        .trigger(TriggerRequest {
            function_id: "replay::get".into(),
            payload: json!({ "sessionId": session_id }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let entries: Vec<ReplayEntry> = serde_json::from_value(entries_value).unwrap_or_default();
    if entries.is_empty() {
        return Ok(json!({ "error": "Session not found" }));
    }

    let mut total_duration: i64 = 0;
    let mut tokens_used: i64 = 0;
    let mut cost: f64 = 0.0;
    let mut tool_set: HashSet<String> = HashSet::new();
    let mut max_iter: i64 = 0;
    let mut tool_calls: i64 = 0;

    for entry in &entries {
        total_duration += entry.duration_ms;
        if entry.iteration > max_iter {
            max_iter = entry.iteration;
        }
        if entry.action == "tool_call" {
            tool_calls += 1;
            if let Some(tool_id) = entry.data["toolId"].as_str() {
                tool_set.insert(tool_id.to_string());
            }
        }
        if entry.action == "llm_call" {
            if let Some(total) = entry.data["usage"]["total"].as_i64() {
                tokens_used += total;
            }
            if let Some(c) = entry.data["usage"]["cost"].as_f64() {
                cost += c;
            }
        }
    }

    let agent_id = entries[0].agent_id.clone();
    let tools: Vec<String> = tool_set.into_iter().collect();

    Ok(json!({
        "sessionId": session_id,
        "agentId": agent_id,
        "totalDuration": total_duration,
        "iterations": max_iter,
        "toolCalls": tool_calls,
        "tokensUsed": tokens_used,
        "cost": cost,
        "tools": tools,
        "actionCount": entries.len(),
    }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());

    let iii_ref = iii.clone();
    iii.register_function(
        "replay::record",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { record(&iii, input).await }
        })
        .description("Record an action in the session replay log"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "replay::get",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { get_session(&iii, input).await }
        })
        .description("Get full session replay"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "replay::search",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { search(&iii, input).await }
        })
        .description("Search replay sessions by criteria"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "replay::summary",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { summary(&iii, input).await }
        })
        .description("Get session replay summary with stats"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "replay::record",
        json!({ "http_method": "POST", "api_path": "api/replay" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "replay::get",
        json!({ "http_method": "GET", "api_path": "api/replay/:sessionId" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "replay::search",
        json!({ "http_method": "GET", "api_path": "api/replay/search" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "replay::summary",
        json!({ "http_method": "GET", "api_path": "api/replay/:sessionId/summary" }),
        None,
    )?;

    tracing::info!("session-replay worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // `state::list` answers a bare array of stored values (verified against
    // iii 0.22.1: `iii trigger state::list scope=replay`). The fixtures below
    // use that shape; the previous `{key, value}` fixtures described an
    // envelope the engine has never sent.

    #[test]
    fn parse_entries_filters_counter() {
        let raw = json!([
            {
                "sessionId": "s1", "agentId": "a1", "action": "tool_call",
                "data": {}, "durationMs": 50, "timestamp": 1000, "iteration": 1, "sequence": 1
            },
            { "value": 1 },
            {
                "sessionId": "s1", "agentId": "a1", "action": "llm_call",
                "data": {}, "durationMs": 0, "timestamp": 1100, "iteration": 1, "sequence": 2
            }
        ]);
        let entries = parse_entries(&raw);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.session_id == "s1"));
    }

    #[test]
    fn parse_entries_skips_incomplete() {
        let raw = json!([
            { "sessionId": "s1" },
            "string-not-object",
            null,
            {
                "sessionId": "s2", "agentId": "a", "action": "x",
                "data": {}, "durationMs": 0, "timestamp": 0, "iteration": 0, "sequence": 0
            }
        ]);
        let entries = parse_entries(&raw);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "s2");
    }

    #[test]
    fn parse_entries_reads_nothing_from_the_envelope_this_worker_used_to_expect() {
        // Every replay read returned an empty list while the worker looked for
        // `entry["value"]` in a bare-array response.
        let enveloped = json!([
            { "key": "s1:00000001", "value": {
                "sessionId": "s1", "agentId": "a1", "action": "tool_call",
                "data": {}, "durationMs": 50, "timestamp": 1000, "iteration": 1, "sequence": 1
            }}
        ]);
        assert!(parse_entries(&enveloped).is_empty());
    }

    // --- state::update protocol (verified against iii 0.22.1) ---

    #[test]
    fn counter_payload_uses_ops_not_operations() {
        let payload = counter_payload("s1:counter");
        assert!(
            payload.get("operations").is_none(),
            "`operations` fails the whole invocation with `missing field ops`"
        );
        assert!(
            payload.get("upsert").is_none(),
            "state::update has no upsert parameter; a missing key increments from zero"
        );
        assert_eq!(payload["scope"], "replay");
        assert_eq!(payload["key"], "s1:counter");
    }

    #[test]
    fn counter_payload_carries_the_step_in_by() {
        let op = counter_payload("s1:counter")["ops"][0].clone();
        assert_eq!(op["type"], "increment");
        assert_eq!(op["path"], "value");
        assert_eq!(op["by"], json!(1));
        assert!(
            op.get("value").is_none(),
            "`value` fails the whole invocation with `missing field by`"
        );
    }

    #[test]
    fn sequence_is_read_from_new_value() {
        let engine_result = json!({
            "new_value": { "value": 3 },
            "old_value": { "value": 2 },
        });
        assert_eq!(sequence_from_update(&engine_result), Some(3));
    }

    #[test]
    fn sequence_rejects_the_shape_this_worker_used_to_assume() {
        assert_eq!(sequence_from_update(&json!({ "value": 3 })), None);
    }

    #[test]
    fn a_rejected_increment_is_detected_inside_a_successful_response() {
        let engine_result = json!({
            "errors": [{ "code": "increment.not_number", "op_index": 0 }],
            "new_value": { "value": "x" },
            "old_value": { "value": "x" },
        });
        assert!(
            update_rejection(&engine_result)
                .expect("rejection must be reported")
                .contains("increment.not_number")
        );
        assert_eq!(
            update_rejection(&json!({ "new_value": { "value": 1 } })),
            None
        );
        assert_eq!(update_rejection(&json!({ "errors": [] })), None);
    }
}
