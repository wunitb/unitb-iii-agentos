use agentos_http_adapter::TriggerBus;
use agentos_http_adapter::principal::{self, Principal};
use agentos_http_adapter::state::{
    append_op, groups as state_groups, increment_op, set_op, update_errors,
    update_payload as state_update_payload, value_of as state_value, values as state_values,
};
use iii_sdk::errors::Error;
use iii_sdk::{RegisterFunction, protocol::TriggerRequest, register_worker};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Messages loaded per session by `memory::session::history` when the caller
/// does not ask for a specific number.
const DEFAULT_HISTORY_LIMIT: usize = 50;

/// One stored memory.
///
/// The wire shape is camelCase because that is what `store_memory` writes; the
/// rename is what makes `recall` and `evict` able to read their own store back
/// (contract I5).
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct MemoryEntry {
    id: String,
    agent_id: String,
    content: String,
    role: String,
    embedding: Option<Vec<f64>>,
    timestamp: u64,
    session_id: Option<String>,
    importance: f64,
    hash: String,
    confidence: f64,
    access_count: u64,
    last_accessed: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::store",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { store_memory(&iii, input).await }
        })
        .description("Store a memory entry with dedup"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::recall",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { recall_memory(&iii, input).await }
        })
        .description("Hybrid semantic + keyword + recency search"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::kg::add",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { kg_add(&iii, input).await }
        })
        .description("Add knowledge graph entity with bidirectional relations"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::kg::query",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { kg_query(&iii, input).await }
        })
        .description("Traverse knowledge graph from entity"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::evict",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { evict_memories(&iii, input).await }
        })
        .description("Evict stale and low-importance memories"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::consolidate",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { consolidate(&iii, input).await }
        })
        .description("Decay confidence on unaccessed memories"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::session::list",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { list_sessions(&iii, input).await }
        })
        .description("List sessions for an agent"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::session::history",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { session_history(&iii, input).await }
        })
        .description("Load a session's conversation in time order"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::session::compact",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { compact_session(&iii, input).await }
        })
        .description("Compact session via LLM summarization"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::session::repair",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { repair_session(&iii, input).await }
        })
        .description("7-phase session validation and repair"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::kv::get",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { memory_kv_get(&iii, input).await }
        })
        .description("Get an agent-scoped memory value"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::kv::set",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { memory_kv_set(&iii, input).await }
        })
        .description("Set an agent-scoped memory value"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::kv::delete",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { memory_kv_delete(&iii, input).await }
        })
        .description("Delete an agent-scoped memory value"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::kv::list",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { memory_kv_list(&iii, input).await }
        })
        .description("List agent-scoped memory values"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::list",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { list_memories(&iii, input).await }
        })
        .description("List semantic memories for an agent"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "memory::session::delete",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { delete_session(&iii, input).await }
        })
        .description("Delete a persisted agent session"),
    );

    for (function_id, method, path) in [
        ("memory::kv::get", "GET", "api/memory/:key"),
        ("memory::kv::set", "POST", "api/memory"),
        ("memory::kv::delete", "DELETE", "api/memory/:key"),
        ("memory::kv::list", "GET", "api/memory"),
        ("memory::list", "GET", "agentmemory/memories"),
        ("memory::recall", "POST", "agentmemory/search"),
        ("memory::store", "POST", "agentmemory/remember"),
        ("memory::session::list", "GET", "/api/sessions"),
        (
            "memory::session::history",
            "GET",
            "/api/sessions/:sessionId/messages",
        ),
        ("memory::session::delete", "DELETE", "/api/sessions/:id"),
    ] {
        agentos_http_adapter::register_http_trigger(
            &iii,
            function_id.to_string(),
            json!({ "http_method": method, "api_path": path }),
            None,
        )?;
    }

    agentos_http_adapter::register_cron_trigger(
        &iii,
        "memory::consolidate".to_string(),
        "0 */6 * * *",
    )?;

    agentos_http_adapter::register_cron_trigger(&iii, "memory::evict".to_string(), "0 3 * * *")?;

    tracing::info!("memory worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

/// The agent an operation falls back to when the OPERATOR names none. An agent
/// principal never falls back: it acts on itself.
const DEFAULT_AGENT: &str = "default";

/// Who this call is from (contract T1). The bearer the operator has to present
/// is the one `agentos_bus_auth` owns; nothing here reads a second one.
fn principal_of(input: &Value) -> Result<Principal, Error> {
    let expected = agentos_bus_auth::policy::expected_api_key();
    Ok(principal::resolve(input, expected.as_deref())?)
}

/// Which agent this call may act on: the principal's own agent, or the agent it
/// names when it is the operator or holds the exact `grant::act_as::<target>`
/// grant. Every per-agent read and write goes through here; `agentId` in the
/// payload is what the call is ABOUT, never who it is FROM.
async fn acting_agent(iii: &dyn TriggerBus, input: &Value) -> Result<String, Error> {
    let principal = principal_of(input)?;
    principal::acting_agent(iii, &principal, input, DEFAULT_AGENT).await
}

/// Cron-fired maintenance has no principal by design (the engine's cron worker
/// cannot present one); an agent principal is refused because the job touches
/// every agent's scope. See `principal::refuse_agent_principal`.
fn refuse_agent_maintenance(input: &Value, operation: &str) -> Result<(), Error> {
    let expected = agentos_bus_auth::policy::expected_api_key();
    principal::refuse_agent_principal(input, expected.as_deref(), operation)
}

fn memory_key(input: &Value) -> Result<&str, Error> {
    input
        .get("key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| Error::Handler("key is required".to_string()))
}

async fn call_state(
    iii: &dyn TriggerBus,
    function_id: &str,
    payload: Value,
) -> Result<Value, Error> {
    iii.trigger(TriggerRequest {
        function_id: function_id.to_string(),
        payload,
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|error| Error::Handler(error.to_string()))
}

async fn memory_kv_get(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let agent = acting_agent(iii, &input).await?;
    call_state(
        iii,
        "state::get",
        json!({ "scope": format!("agent-memory:{agent}"), "key": memory_key(&input)? }),
    )
    .await
}

async fn memory_kv_set(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let agent = acting_agent(iii, &input).await?;
    let value = input
        .get("value")
        .cloned()
        .ok_or_else(|| Error::Handler("value is required".to_string()))?;
    let key = memory_key(&input)?;
    call_state(
        iii,
        "state::set",
        json!({ "scope": format!("agent-memory:{agent}"), "key": key, "value": value }),
    )
    .await?;
    Ok(json!({ "stored": true, "key": key }))
}

async fn memory_kv_delete(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let agent = acting_agent(iii, &input).await?;
    let key = memory_key(&input)?;
    call_state(
        iii,
        "state::delete",
        json!({ "scope": format!("agent-memory:{agent}"), "key": key }),
    )
    .await?;
    Ok(json!({ "deleted": true, "key": key }))
}

async fn memory_kv_list(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let agent = acting_agent(iii, &input).await?;
    call_state(
        iii,
        "state::list",
        json!({ "scope": format!("agent-memory:{agent}") }),
    )
    .await
}

async fn list_memories(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let agent = acting_agent(iii, &input).await?;
    let entries = call_state(
        iii,
        "state::list",
        json!({ "scope": format!("memory:{agent}") }),
    )
    .await?;
    let memories = state_values(&entries)
        .into_iter()
        .filter(|value| value.get("content").is_some())
        .cloned()
        .collect::<Vec<_>>();
    Ok(json!({ "memories": memories }))
}

/// The agents whose session scopes a call may walk.
///
/// The operator naming nobody gets every agent (the TUI's sessions screen); an
/// agent principal gets itself, or the one agent it is granted, never the list.
async fn session_agents(iii: &dyn TriggerBus, input: &Value) -> Result<Vec<String>, Error> {
    let principal = principal_of(input)?;
    if !principal.is_operator() || principal::requested_agent(input).is_some() {
        let agent = principal::acting_agent(iii, &principal, input, DEFAULT_AGENT).await?;
        return Ok(vec![agent]);
    }

    let agents = call_state(iii, "state::list", json!({ "scope": "agents" })).await?;
    let mut agent_ids = state_values(&agents)
        .into_iter()
        .filter_map(|value| value.get("id").and_then(Value::as_str))
        .filter(|agent| !agent.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    agent_ids.sort();
    agent_ids.dedup();
    if agent_ids.is_empty() {
        agent_ids.push("default".to_string());
    }
    Ok(agent_ids)
}

async fn list_sessions(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let mut sessions = Vec::new();
    for agent in session_agents(iii, &input).await? {
        let entries = call_state(
            iii,
            "state::list",
            json!({ "scope": format!("sessions:{agent}") }),
        )
        .await?;
        for entry in entries.as_array().into_iter().flatten() {
            let value = state_value(entry);
            // `state::list` returns no keys, so the id has to come from the
            // session document that `append_session_message` maintains.
            let Some(id) = entry
                .get("key")
                .or_else(|| value.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            let mut session = value.clone();
            if !session.is_object() {
                session = json!({ "value": session });
            }
            if let Some(object) = session.as_object_mut() {
                object.entry("id".to_string()).or_insert_with(|| json!(&id));
                object
                    .entry("agent".to_string())
                    .or_insert_with(|| json!(agent));
                object
                    .entry("status".to_string())
                    .or_insert_with(|| json!("active"));
                if !object.contains_key("startedAt") {
                    let started_at = object
                        .get("createdAt")
                        .or_else(|| object.get("updatedAt"))
                        .map(|value| {
                            value
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| value.to_string())
                        })
                        .unwrap_or_default();
                    object.insert("startedAt".to_string(), json!(started_at));
                }
            }
            sessions.push(session);
        }
    }
    Ok(json!(sessions))
}

async fn delete_session(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let id = input
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::Handler("id is required".to_string()))?;
    let mut deleted = false;
    for agent in session_agents(iii, &input).await? {
        // `state::list` returns no keys, so existence is checked by reading the
        // key directly; a miss reads back as null.
        let existing = call_state(
            iii,
            "state::get",
            json!({ "scope": format!("sessions:{agent}"), "key": id }),
        )
        .await
        .unwrap_or(Value::Null);
        if !existing.is_null() {
            call_state(
                iii,
                "state::delete",
                json!({ "scope": format!("sessions:{agent}"), "key": id }),
            )
            .await?;
            deleted = true;
        }
    }
    if !deleted {
        return Err(Error::Handler(format!("session not found: {id}")));
    }
    Ok(json!({ "deleted": true, "id": id }))
}

async fn store_memory(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let agent_id = acting_agent(iii, &input).await?;
    let agent_id = agent_id.as_str();
    let content = input["content"].as_str().unwrap_or("");
    let role = input["role"].as_str().unwrap_or("user");
    let session_id = input["sessionId"].as_str().map(String::from);

    let hash = {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        format!("{:x}", hasher.finalize())
    };

    // A missing key is reported as `null` by some state backends and as an error
    // by others; both mean "not stored yet". Without the null filter every store
    // would dedup against nothing and silently drop the message.
    let existing: Option<Value> = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({
                "scope": format!("memory:{}", agent_id),
                "key": &hash,
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .ok()
        .filter(|value| !value.is_null());

    if let Some(existing) = existing {
        // The entry itself is already stored, but the turn still happened: the
        // session index has to record it, or a repeated message ("ok", "yes")
        // disappears from the conversation history.
        let existing_id = existing
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let indexed = match (&session_id, existing_id.is_empty()) {
            (Some(sid), false) => {
                append_session_message(iii, agent_id, sid, &existing_id, role, now_ms()).await
            }
            _ => false,
        };
        return Ok(json!({
            "deduplicated": true,
            "id": existing_id,
            "sessionIndexed": indexed,
        }));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let now = now_ms();

    let embedding: Option<Vec<f64>> = iii
        .trigger(TriggerRequest {
            function_id: "embedding::generate".to_string(),
            payload: json!({ "text": content }),
            action: None,
            timeout_ms: None,
        })
        .await
        .ok()
        .and_then(|v| {
            v.get("embedding")
                .and_then(|e| serde_json::from_value(e.clone()).ok())
        });

    let importance = estimate_importance(content, role);

    let entry = json!({
        "id": &id,
        "agentId": agent_id,
        "content": content,
        "role": role,
        "embedding": embedding,
        "timestamp": now,
        "sessionId": session_id,
        "importance": importance,
        "hash": &hash,
        "confidence": 1.0,
        "accessCount": 0_u64,
        "lastAccessed": now,
    });

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": format!("memory:{}", agent_id),
            "key": &id,
            "value": &entry,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": format!("memory:{}", agent_id),
            "key": &hash,
            "value": { "id": &id, "timestamp": now },
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    let indexed = match &session_id {
        Some(sid) => append_session_message(iii, agent_id, sid, &id, role, now).await,
        None => false,
    };

    Ok(json!({ "id": id, "stored": true, "sessionIndexed": indexed }))
}

/// Appends one message reference to the `sessions:{agent}` index.
///
/// The index is what `session_history` reads, so a failure here is the
/// difference between a conversation and amnesia: it is logged, never silently
/// dropped. Returns whether the index now contains this message.
async fn append_session_message(
    iii: &dyn TriggerBus,
    agent_id: &str,
    session_id: &str,
    message_id: &str,
    role: &str,
    now: u64,
) -> bool {
    let result = iii
        .trigger(TriggerRequest {
            function_id: "state::update".to_string(),
            payload: state_update_payload(
                format!("sessions:{agent_id}"),
                session_id,
                vec![
                    append_op(
                        "messages",
                        json!({ "id": message_id, "role": role, "timestamp": now }),
                    ),
                    set_op("updatedAt", json!(now)),
                    // The engine's `state::list` does not return keys, so the
                    // session has to carry its own identity for `session::list`.
                    set_op("id", json!(session_id)),
                    set_op("agent", json!(agent_id)),
                ],
            ),
            action: None,
            timeout_ms: None,
        })
        .await;

    match result {
        Ok(response) => match update_errors(&response) {
            None => true,
            Some(codes) => {
                tracing::warn!(
                    agent_id,
                    session_id,
                    message_id,
                    codes,
                    "session index update was rejected; this turn will be missing from history"
                );
                false
            }
        },
        Err(error) => {
            tracing::warn!(
                agent_id,
                session_id,
                message_id,
                %error,
                "session index update failed; this turn will be missing from history"
            );
            false
        }
    }
}

/// Loads one session's conversation in time order (contract I5).
///
/// The session index has been written since the worker was created and never
/// read; this is the reader. Ordering is by the index timestamp, which is
/// assigned per append, so a deduplicated repeat still lands in the right place.
async fn session_history(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let agent_id = acting_agent(iii, &input).await?;
    let session_id = input
        .get("sessionId")
        .or_else(|| input.get("session"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::Handler("sessionId is required".to_string()))?
        .to_string();
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_HISTORY_LIMIT, |limit| limit as usize);

    let session = call_state(
        iii,
        "state::get",
        json!({ "scope": format!("sessions:{agent_id}"), "key": &session_id }),
    )
    .await
    .unwrap_or(Value::Null);

    let mut refs: Vec<(u64, &str)> = session
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|message| {
            let id = message.get("id").and_then(Value::as_str)?;
            if id.is_empty() {
                return None;
            }
            Some((
                message
                    .get("timestamp")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                id,
            ))
        })
        .collect();
    // Stable: equal timestamps keep the order they were appended in.
    refs.sort_by_key(|(timestamp, _)| *timestamp);
    if refs.len() > limit {
        refs.drain(..refs.len() - limit);
    }

    if refs.is_empty() {
        return Ok(json!({ "sessionId": session_id, "agentId": agent_id, "messages": [] }));
    }

    let entries = call_state(
        iii,
        "state::list",
        json!({ "scope": format!("memory:{agent_id}") }),
    )
    .await
    .unwrap_or(json!([]));
    // Only entries with content: the dedup records stored under the content
    // hash carry the same `id` and would otherwise shadow the real entry.
    let by_id: HashMap<&str, &Value> = state_values(&entries)
        .into_iter()
        .filter_map(|value| {
            value.get("content")?;
            Some((value.get("id")?.as_str()?, value))
        })
        .collect();

    let messages = refs
        .into_iter()
        .filter_map(|(timestamp, id)| {
            let entry = by_id.get(id)?;
            let content = entry.get("content").and_then(Value::as_str)?;
            let role = entry.get("role").and_then(Value::as_str)?;
            Some(json!({
                "id": id,
                "role": role,
                "content": content,
                "timestamp": timestamp,
            }))
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "sessionId": session_id,
        "agentId": agent_id,
        "messages": messages,
    }))
}

async fn recall_memory(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let agent_id = acting_agent(iii, &input).await?;
    let query = input["query"].as_str().unwrap_or("");
    let limit = input["limit"].as_u64().unwrap_or(10) as usize;

    let entries: Value = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": format!("memory:{}", agent_id) }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!([]));

    let memories: Vec<MemoryEntry> = state_values(&entries)
        .into_iter()
        .filter_map(|val| {
            if val.get("content").is_none() || val.get("role").is_none() {
                return None;
            }
            serde_json::from_value(val.clone()).ok()
        })
        .collect();

    if memories.is_empty() {
        return Ok(json!([]));
    }

    let query_embedding: Option<Vec<f64>> = iii
        .trigger(TriggerRequest {
            function_id: "embedding::generate".to_string(),
            payload: json!({ "text": query }),
            action: None,
            timeout_ms: None,
        })
        .await
        .ok()
        .and_then(|v| {
            v.get("embedding")
                .and_then(|e| serde_json::from_value(e.clone()).ok())
        });

    let keywords: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();

    let now = now_ms();
    let mut scored: Vec<(f64, &MemoryEntry)> = memories
        .iter()
        .map(|m| {
            let mut score = 0.0_f64;

            if let (Some(qe), Some(me)) = (&query_embedding, &m.embedding) {
                score += cosine_similarity(qe, me) * 0.5;
            }

            let content_lower = m.content.to_lowercase();
            let hits = keywords
                .iter()
                .filter(|k| content_lower.contains(k.as_str()))
                .count();
            let keyword_score = hits as f64 / keywords.len().max(1) as f64;
            score += keyword_score * 0.25;

            let age_hours = (now.saturating_sub(m.timestamp)) as f64 / 3_600_000.0;
            let recency = (-age_hours / 168.0_f64).exp();
            score += recency * 0.1;

            score += m.importance * 0.1;

            score += m.confidence * 0.05;

            (score, m)
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut results: Vec<Value> = Vec::new();
    for (score, m) in scored.into_iter().take(limit) {
        // Awaited rather than spawned: a dropped access-count update used to be
        // invisible, and a spawned task cannot be observed by a test.
        if let Err(error) = iii
            .trigger(TriggerRequest {
                function_id: "state::update".to_string(),
                payload: state_update_payload(
                    format!("memory:{}", m.agent_id),
                    &m.id,
                    vec![
                        increment_op("accessCount", 1),
                        set_op("lastAccessed", json!(now_ms())),
                    ],
                ),
                action: None,
                timeout_ms: None,
            })
            .await
        {
            tracing::warn!(id = %m.id, %error, "recall access-count update failed");
        }

        results.push(json!({
            "role": m.role,
            "content": m.content,
            "score": score,
            "timestamp": m.timestamp,
            "id": m.id,
        }));
    }

    Ok(json!(results))
}

async fn kg_add(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let agent_id = acting_agent(iii, &input).await?;
    let entity = &input["entity"];
    let entity_id = entity["id"].as_str().unwrap_or("");

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": format!("kg:{}", agent_id),
            "key": entity_id,
            "value": entity,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    if let Some(relations) = entity["relations"].as_array() {
        for rel in relations {
            let target_id = rel["target"].as_str().unwrap_or("");
            let rel_type = rel["type"].as_str().unwrap_or("");

            if let Ok(target) = iii
                .trigger(TriggerRequest {
                    function_id: "state::get".to_string(),
                    payload: json!({
                        "scope": format!("kg:{}", agent_id),
                        "key": target_id,
                    }),
                    action: None,
                    timeout_ms: None,
                })
                .await
            {
                let mut back_refs: Vec<Value> =
                    target["relations"].as_array().cloned().unwrap_or_default();

                let already = back_refs.iter().any(|r| {
                    r["target"].as_str() == Some(entity_id)
                        && r["type"].as_str() == Some(&format!("inverse:{}", rel_type))
                });

                if !already {
                    back_refs.push(
                        json!({ "target": entity_id, "type": format!("inverse:{}", rel_type) }),
                    );
                    if let Err(error) = iii
                        .trigger(TriggerRequest {
                            function_id: "state::update".to_string(),
                            payload: state_update_payload(
                                format!("kg:{agent_id}"),
                                target_id,
                                vec![set_op("relations", json!(back_refs))],
                            ),
                            action: None,
                            timeout_ms: None,
                        })
                        .await
                    {
                        tracing::warn!(target_id, %error, "back-reference update failed");
                    }
                }
            }
        }
    }

    Ok(json!({ "stored": true, "id": entity_id }))
}

async fn kg_query(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let agent_id = acting_agent(iii, &input).await?;
    let entity_id = input["entityId"].as_str().unwrap_or("");
    let depth = input["depth"].as_u64().unwrap_or(2);

    let mut visited = std::collections::HashSet::new();
    let mut results = Vec::new();
    let mut queue: Vec<(String, u64)> = vec![(entity_id.to_string(), 0)];

    while let Some((id, d)) = queue.pop() {
        if d >= depth || visited.contains(&id) {
            continue;
        }
        visited.insert(id.clone());

        if let Ok(entity) = iii
            .trigger(TriggerRequest {
                function_id: "state::get".to_string(),
                payload: json!({
                    "scope": format!("kg:{}", agent_id),
                    "key": &id,
                }),
                action: None,
                timeout_ms: None,
            })
            .await
        {
            if let Some(relations) = entity["relations"].as_array() {
                for rel in relations {
                    if let Some(target) = rel["target"].as_str() {
                        queue.push((target.to_string(), d + 1));
                    }
                }
            }
            results.push(entity);
        }
    }

    Ok(json!(results))
}

/// System-wide by design: fired by the engine's cron worker, which has no
/// principal to present, so a bare payload is accepted. It walks EVERY
/// `memory:*` scope and never reads `agentId`; an agent principal is refused
/// because "evict my memories" is not what this does (contract T1).
async fn evict_memories(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    refuse_agent_maintenance(&input, "memory::evict")?;
    let max_age_ms = input["maxAge"].as_u64().unwrap_or(30 * 86_400_000);
    let min_importance = input["minImportance"].as_f64().unwrap_or(0.2);
    let cap = input["cap"].as_u64().unwrap_or(10_000) as usize;

    let scopes: Value = iii
        .trigger(TriggerRequest {
            function_id: "state::list_groups".to_string(),
            payload: json!({}),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!([]));

    let memory_scopes: Vec<String> = state_groups(&scopes)
        .into_iter()
        .filter(|scope| scope.starts_with("memory:"))
        .map(String::from)
        .collect();

    let now = now_ms();
    let mut total_evicted = 0_u64;

    for scope in &memory_scopes {
        let entries: Value = iii
            .trigger(TriggerRequest {
                function_id: "state::list".to_string(),
                payload: json!({ "scope": scope }),
                action: None,
                timeout_ms: None,
            })
            .await
            .unwrap_or(json!([]));

        let memories: Vec<MemoryEntry> = state_values(&entries)
            .into_iter()
            .filter_map(|val| serde_json::from_value(val.clone()).ok())
            .collect();

        let mut scope_evicted = 0_u64;

        for m in &memories {
            let age = now.saturating_sub(m.timestamp);
            let is_stale = age > max_age_ms;
            let is_low_value = m.importance < min_importance;
            let is_low_confidence = m.confidence < 0.1;

            if (is_stale && is_low_value) || is_low_confidence {
                let _ = iii
                    .trigger(TriggerRequest {
                        function_id: "state::delete".to_string(),
                        payload: json!({ "scope": scope, "key": &m.id }),
                        action: None,
                        timeout_ms: None,
                    })
                    .await;
                let _ = iii
                    .trigger(TriggerRequest {
                        function_id: "state::delete".to_string(),
                        payload: json!({ "scope": scope, "key": &m.hash }),
                        action: None,
                        timeout_ms: None,
                    })
                    .await;
                scope_evicted += 1;
            }
        }

        let remaining = (memories.len() as u64).saturating_sub(scope_evicted);
        if remaining > cap as u64 {
            let mut sorted: Vec<&MemoryEntry> = memories.iter().collect();
            sorted.sort_by(|a, b| {
                a.importance
                    .partial_cmp(&b.importance)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            let overflow = (remaining - cap as u64) as usize;
            for m in sorted.into_iter().take(overflow) {
                let _ = iii
                    .trigger(TriggerRequest {
                        function_id: "state::delete".to_string(),
                        payload: json!({ "scope": scope, "key": &m.id }),
                        action: None,
                        timeout_ms: None,
                    })
                    .await;
                let _ = iii
                    .trigger(TriggerRequest {
                        function_id: "state::delete".to_string(),
                        payload: json!({ "scope": scope, "key": &m.hash }),
                        action: None,
                        timeout_ms: None,
                    })
                    .await;
                scope_evicted += 1;
            }
        }

        total_evicted += scope_evicted;
    }

    Ok(json!({ "evicted": total_evicted }))
}

/// System-wide by design, exactly like `evict_memories`.
async fn consolidate(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    refuse_agent_maintenance(&input, "memory::consolidate")?;
    let decay_rate = input["decayRate"].as_f64().unwrap_or(0.05);
    let start = std::time::Instant::now();

    let scopes: Value = iii
        .trigger(TriggerRequest {
            function_id: "state::list_groups".to_string(),
            payload: json!({}),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!([]));

    let memory_scopes: Vec<String> = state_groups(&scopes)
        .into_iter()
        .filter(|scope| scope.starts_with("memory:"))
        .map(String::from)
        .collect();

    let now = now_ms();
    let seven_days_ms = 7 * 86_400_000_u64;
    let mut decayed = 0_u64;

    for scope in &memory_scopes {
        let entries: Value = iii
            .trigger(TriggerRequest {
                function_id: "state::list".to_string(),
                payload: json!({ "scope": scope }),
                action: None,
                timeout_ms: None,
            })
            .await
            .unwrap_or(json!([]));

        for val in state_values(&entries) {
            let last_accessed = val["lastAccessed"].as_u64().unwrap_or(0);
            let confidence = val["confidence"].as_f64().unwrap_or(1.0);
            let id = match val["id"].as_str() {
                Some(id) => id,
                None => continue,
            };

            if now.saturating_sub(last_accessed) > seven_days_ms && confidence > 0.1 {
                let new_confidence = (confidence * (1.0 - decay_rate)).max(0.1);
                let _ = iii
                    .trigger(TriggerRequest {
                        function_id: "state::update".to_string(),
                        payload: state_update_payload(
                            scope.clone(),
                            id,
                            vec![set_op("confidence", json!(new_confidence))],
                        ),
                        action: None,
                        timeout_ms: None,
                    })
                    .await;
                decayed += 1;
            }
        }
    }

    Ok(json!({
        "memoriesDecayed": decayed,
        "memoriesMerged": 0,
        "durationMs": start.elapsed().as_millis() as u64,
    }))
}

fn memory_summary_payload(chunk: &str) -> Value {
    json!({
        "provider": "anthropic",
        "model": "claude-haiku-4-5-20251001",
        "systemPrompt": "Summarize this conversation concisely. Preserve key facts, decisions, and context. Be brief.",
        "messages": [{ "role": "user", "content": chunk }],
    })
}

async fn compact_session(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let agent_id = acting_agent(iii, &input).await?;
    let session_id = input["sessionId"].as_str().unwrap_or("default");
    let threshold = input["threshold"].as_u64().unwrap_or(30) as usize;
    let keep_recent = input["keepRecent"].as_u64().unwrap_or(10) as usize;

    let session: Value = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({
                "scope": format!("sessions:{}", agent_id),
                "key": session_id,
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!({}));

    let messages = session["messages"].as_array().cloned().unwrap_or_default();

    if messages.len() < threshold {
        return Ok(json!({ "compacted": false, "reason": "below_threshold" }));
    }

    let to_summarize = &messages[..messages.len().saturating_sub(keep_recent)];
    let to_keep = &messages[messages.len().saturating_sub(keep_recent)..];

    let mut full_messages = Vec::new();
    for msg_ref in to_summarize {
        let msg_id = msg_ref["id"].as_str().unwrap_or("");
        if let Ok(entry) = iii
            .trigger(TriggerRequest {
                function_id: "state::get".to_string(),
                payload: json!({
                    "scope": format!("memory:{}", agent_id),
                    "key": msg_id,
                }),
                action: None,
                timeout_ms: None,
            })
            .await
        {
            full_messages.push(format!(
                "{}: {}",
                entry["role"].as_str().unwrap_or("unknown"),
                entry["content"].as_str().unwrap_or("")
            ));
        }
    }

    let conversation_text = full_messages.join("\n\n");
    let chunks = chunk_text(&conversation_text, 80_000);
    let mut summaries = Vec::new();

    for chunk in &chunks {
        let summary = iii
            .trigger(TriggerRequest {
                function_id: "agentos::llm::complete".to_string(),
                payload: memory_summary_payload(chunk),
                action: None,
                timeout_ms: None,
            })
            .await;

        if let Ok(resp) = summary {
            summaries.push(resp["content"].as_str().unwrap_or("").to_string());
        }
    }

    let final_summary = summaries.join("\n\n");

    let summary_id = uuid::Uuid::new_v4().to_string();
    let now = now_ms();

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": format!("memory:{}", agent_id),
            "key": &summary_id,
            "value": {
                "id": &summary_id,
                "agentId": agent_id,
                "content": &final_summary,
                "role": "system",
                "timestamp": now,
                "sessionId": session_id,
                "importance": 0.9,
                "hash": "",
                "confidence": 1.0,
                "accessCount": 0_u64,
                "lastAccessed": now,
            },
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    let mut new_messages = vec![json!({ "id": summary_id, "role": "system", "timestamp": now })];
    new_messages.extend(to_keep.iter().cloned());

    iii.trigger(TriggerRequest {
        function_id: "state::update".to_string(),
        payload: state_update_payload(
            format!("sessions:{agent_id}"),
            session_id,
            vec![
                set_op("messages", json!(new_messages)),
                set_op("compactedAt", json!(now)),
            ],
        ),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    Ok(json!({
        "compacted": true,
        "summarized": to_summarize.len(),
        "kept": to_keep.len(),
        "summaryId": summary_id,
    }))
}

async fn repair_session(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let agent_id = acting_agent(iii, &input).await?;
    let session_id = input["sessionId"].as_str().unwrap_or("default");

    let session: Value = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({
                "scope": format!("sessions:{}", agent_id),
                "key": session_id,
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!({}));

    let messages = session["messages"].as_array().cloned().unwrap_or_default();
    let mut repaired = messages.clone();
    let mut stats = HashMap::new();

    let before = repaired.len();
    repaired.retain(|m| {
        m.get("id")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    });
    stats.insert("emptyRemoved", (before - repaired.len()) as u64);

    let before = repaired.len();
    let mut seen = std::collections::HashSet::new();
    repaired.retain(|m| {
        let id = m["id"].as_str().unwrap_or("").to_string();
        seen.insert(id)
    });
    stats.insert("duplicatesRemoved", (before - repaired.len()) as u64);

    let mut merged = Vec::new();
    let mut merge_count = 0_u64;
    for msg in &repaired {
        let role = msg["role"].as_str().unwrap_or("");
        if let Some(last) = merged.last() {
            let last_role: &str = match last {
                Value::Object(obj) => obj.get("role").and_then(|v| v.as_str()).unwrap_or(""),
                _ => "",
            };
            if last_role == role && role != "system" {
                merge_count += 1;
                continue;
            }
        }
        merged.push(msg.clone());
    }
    repaired = merged;
    stats.insert("consecutiveMerged", merge_count);

    let mut orphaned = 0_u64;
    for msg in &repaired {
        let msg_id = msg["id"].as_str().unwrap_or("");
        let exists = iii
            .trigger(TriggerRequest {
                function_id: "state::get".to_string(),
                payload: json!({
                    "scope": format!("memory:{}", agent_id),
                    "key": msg_id,
                }),
                action: None,
                timeout_ms: None,
            })
            .await;
        if exists.is_err() {
            orphaned += 1;
        }
    }
    stats.insert("orphanedRefs", orphaned);

    let mut reordered = 0_u64;
    let mut prev_ts = 0_u64;
    for msg in &mut repaired {
        let ts = msg["timestamp"].as_u64().unwrap_or(0);
        if ts < prev_ts
            && let Some(obj) = msg.as_object_mut()
        {
            obj.insert("timestamp".into(), json!(prev_ts + 1));
            reordered += 1;
        }
        prev_ts = msg["timestamp"].as_u64().unwrap_or(prev_ts);
    }
    stats.insert("reordered", reordered);

    let mut truncated = 0_u64;
    for msg in &repaired {
        let msg_id = msg["id"].as_str().unwrap_or("");
        if let Ok(entry) = iii
            .trigger(TriggerRequest {
                function_id: "state::get".to_string(),
                payload: json!({
                    "scope": format!("memory:{}", agent_id),
                    "key": msg_id,
                }),
                action: None,
                timeout_ms: None,
            })
            .await
        {
            let content_len = entry["content"].as_str().map(|s| s.len()).unwrap_or(0);
            if content_len > 500_000 {
                truncated += 1;
            }
        }
    }
    stats.insert("oversizedDetected", truncated);

    let total_repairs: u64 = stats.values().sum();
    if total_repairs > 0 {
        iii.trigger(TriggerRequest {
            function_id: "state::update".to_string(),
            payload: state_update_payload(
                format!("sessions:{agent_id}"),
                session_id,
                vec![
                    set_op("messages", json!(repaired)),
                    set_op("repairedAt", json!(now_ms())),
                    set_op("repairStats", json!(stats)),
                ],
            ),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;
    }

    Ok(json!({
        "repaired": total_repairs > 0,
        "totalFixes": total_repairs,
        "stats": stats,
    }))
}

fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut norm_a = 0.0;
    let mut norm_b = 0.0;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom > 0.0 { dot / denom } else { 0.0 }
}

fn estimate_importance(content: &str, role: &str) -> f64 {
    let mut score: f64 = 0.5;
    if role == "assistant" {
        score += 0.1;
    }
    if content.len() > 500 {
        score += 0.1;
    }
    if content.contains("error")
        || content.contains("bug")
        || content.contains("fix")
        || content.contains("critical")
    {
        score += 0.15;
    }
    if content.contains("```") {
        score += 0.1;
    }
    score.min(1.0)
}

fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return vec![];
    }
    if text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }
    let char_indices: Vec<(usize, char)> = text.char_indices().collect();
    let total_chars = char_indices.len();
    let mut chunks = Vec::new();
    let mut start_char = 0;
    while start_char < total_chars {
        let end_char = (start_char + max_chars).min(total_chars);
        let start_byte = char_indices[start_char].0;
        let end_byte = if end_char < total_chars {
            char_indices[end_char].0
        } else {
            text.len()
        };
        let slice = &text[start_byte..end_byte];
        let mut split_char = if end_char < total_chars {
            slice
                .rfind('\n')
                .map(|byte_pos| {
                    char_indices[start_char..end_char]
                        .iter()
                        .position(|(b, _)| *b == start_byte + byte_pos)
                        .map(|p| start_char + p + 1)
                        .unwrap_or(end_char)
                })
                .unwrap_or(end_char)
        } else {
            end_char
        };
        if split_char <= start_char {
            split_char = end_char;
        }
        let split_byte = if split_char < total_chars {
            char_indices[split_char].0
        } else {
            text.len()
        };
        chunks.push(text[start_byte..split_byte].to_string());
        start_char = split_char;
    }
    chunks
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_summary_payload_uses_top_level_route_fields() {
        let payload = memory_summary_payload("conversation");
        assert_eq!(payload["provider"], "anthropic");
        assert_eq!(payload["model"], "claude-haiku-4-5-20251001");
        assert!(payload["model"].is_string());
    }

    #[test]
    fn test_sha256_dedup_same_content_same_hash() {
        let content = "Hello, world!";
        let mut hasher1 = sha2::Digest::new();
        sha2::Digest::update(&mut hasher1, content.as_bytes());
        let h1 = format!("{:x}", <Sha256 as sha2::Digest>::finalize(hasher1));

        let mut hasher2 = sha2::Digest::new();
        sha2::Digest::update(&mut hasher2, content.as_bytes());
        let h2 = format!("{:x}", <Sha256 as sha2::Digest>::finalize(hasher2));

        assert_eq!(h1, h2);
    }

    #[test]
    fn test_sha256_dedup_different_content_different_hash() {
        let mut h1 = Sha256::new();
        h1.update(b"content A");
        let r1 = format!("{:x}", h1.finalize());

        let mut h2 = Sha256::new();
        h2.update(b"content B");
        let r2 = format!("{:x}", h2.finalize());

        assert_ne!(r1, r2);
    }

    #[test]
    fn test_sha256_hash_length() {
        let mut hasher = Sha256::new();
        hasher.update(b"test");
        let hash = format!("{:x}", hasher.finalize());
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn test_cosine_similarity_identical_vectors() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_cosine_similarity_orthogonal_vectors() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-10);
    }

    #[test]
    fn test_cosine_similarity_opposite_vectors() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - (-1.0)).abs() < 1e-10);
    }

    #[test]
    fn test_cosine_similarity_different_lengths() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0, 2.0, 3.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_cosine_similarity_empty_vectors() {
        let a: Vec<f64> = vec![];
        let b: Vec<f64> = vec![];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 2.0, 3.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_cosine_similarity_proportional_vectors() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![2.0, 4.0, 6.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_cosine_similarity_single_element() {
        let a = vec![5.0];
        let b = vec![3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_user_short() {
        let score = estimate_importance("hello", "user");
        assert_eq!(score, 0.5);
    }

    #[test]
    fn test_estimate_importance_assistant_bonus() {
        let score = estimate_importance("hello", "assistant");
        assert!((score - 0.6).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_long_content_bonus() {
        let long = "a".repeat(501);
        let score = estimate_importance(&long, "user");
        assert!((score - 0.6).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_error_keyword() {
        let score = estimate_importance("there was an error in the code", "user");
        assert!((score - 0.65).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_bug_keyword() {
        let score = estimate_importance("found a bug in the system", "user");
        assert!((score - 0.65).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_fix_keyword() {
        let score = estimate_importance("please fix this issue", "user");
        assert!((score - 0.65).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_critical_keyword() {
        let score = estimate_importance("this is critical", "user");
        assert!((score - 0.65).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_code_block_bonus() {
        let score = estimate_importance("here is code ```rust\nfn main() {}```", "user");
        assert!((score - 0.6).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_all_bonuses() {
        let long = "a".repeat(501);
        let content = format!("{} error critical ``` code block ```", long);
        let score = estimate_importance(&content, "assistant");
        assert!(score >= 0.9);
        assert!(score <= 1.0);
    }

    #[test]
    fn test_estimate_importance_capped_at_one() {
        let long = "a".repeat(1000);
        let content = format!("{} error bug fix critical ``` block ```", long);
        let score = estimate_importance(&content, "assistant");
        assert!(score <= 1.0);
    }

    #[test]
    fn test_estimate_importance_system_role() {
        let score = estimate_importance("system message", "system");
        assert_eq!(score, 0.5);
    }

    #[test]
    fn test_chunk_text_short_text() {
        let text = "short text";
        let chunks = chunk_text(text, 100);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "short text");
    }

    #[test]
    fn test_chunk_text_exact_length() {
        let text = "abcde";
        let chunks = chunk_text(text, 5);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "abcde");
    }

    #[test]
    fn test_chunk_text_splits_on_newline() {
        let text = "line one\nline two\nline three\nline four";
        let chunks = chunk_text(text, 20);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.len() <= 20 || !chunk.contains('\n'));
        }
    }

    #[test]
    fn test_chunk_text_no_newline_splits_at_max() {
        let text = "a".repeat(100);
        let chunks = chunk_text(&text, 30);
        assert!(chunks.len() >= 3);
    }

    #[test]
    fn test_chunk_text_empty() {
        let chunks = chunk_text("", 100);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "");
    }

    #[test]
    fn test_chunk_text_preserves_all_content() {
        let text = "Hello\nWorld\nFoo\nBar\nBaz\nQux";
        let chunks = chunk_text(text, 10);
        let rejoined: String = chunks.join("");
        assert_eq!(rejoined, text);
    }

    #[test]
    fn test_now_ms_returns_nonzero() {
        let ts = now_ms();
        assert!(ts > 0);
    }

    #[test]
    fn test_now_ms_increasing() {
        let t1 = now_ms();
        let t2 = now_ms();
        assert!(t2 >= t1);
    }

    #[test]
    fn test_memory_entry_serialization() {
        let entry = MemoryEntry {
            id: "m-1".to_string(),
            agent_id: "agent-1".to_string(),
            content: "test content".to_string(),
            role: "user".to_string(),
            embedding: Some(vec![0.1, 0.2, 0.3]),
            timestamp: 1000,
            session_id: Some("sess-1".to_string()),
            importance: 0.75,
            hash: "abc123".to_string(),
            confidence: 0.9,
            access_count: 5,
            last_accessed: 2000,
        };
        let val = serde_json::to_value(&entry).unwrap();
        assert_eq!(val["id"], "m-1");
        assert_eq!(val["agentId"], "agent-1");
        assert_eq!(val["content"], "test content");
        assert_eq!(val["role"], "user");
        assert_eq!(val["importance"], 0.75);
        assert_eq!(val["confidence"], 0.9);
        assert_eq!(val["accessCount"], 5);
        assert_eq!(val["sessionId"], "sess-1");
        assert_eq!(val["lastAccessed"], 2000);
        assert!(
            val.get("agent_id").is_none(),
            "snake_case must not reach the store"
        );
    }

    #[test]
    fn test_memory_entry_deserialization() {
        // Exactly the shape `store_memory` writes.
        let json_val = json!({
            "id": "m-2",
            "agentId": "agent-2",
            "content": "remembered fact",
            "role": "assistant",
            "embedding": null,
            "timestamp": 5000,
            "sessionId": null,
            "importance": 0.5,
            "hash": "def456",
            "confidence": 1.0,
            "accessCount": 0,
            "lastAccessed": 5000,
        });
        let entry: MemoryEntry = serde_json::from_value(json_val).unwrap();
        assert_eq!(entry.id, "m-2");
        assert_eq!(entry.agent_id, "agent-2");
        assert_eq!(entry.embedding, None);
        assert_eq!(entry.session_id, None);
    }

    #[test]
    fn test_memory_entry_roundtrip() {
        let entry = MemoryEntry {
            id: "rt-1".to_string(),
            agent_id: "agent-rt".to_string(),
            content: "roundtrip test".to_string(),
            role: "system".to_string(),
            embedding: Some(vec![1.0, 2.0]),
            timestamp: 42,
            session_id: Some("s-1".to_string()),
            importance: 0.8,
            hash: "h1".to_string(),
            confidence: 0.95,
            access_count: 10,
            last_accessed: 100,
        };
        let serialized = serde_json::to_string(&entry).unwrap();
        let deserialized: MemoryEntry = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.id, entry.id);
        assert_eq!(deserialized.content, entry.content);
        assert_eq!(deserialized.embedding, entry.embedding);
    }

    #[test]
    fn test_cosine_similarity_known_value() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        let expected = 1.0 / (2.0_f64).sqrt();
        assert!((sim - expected).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_exact_500_chars_no_bonus() {
        let content = "a".repeat(500);
        let score = estimate_importance(&content, "user");
        assert_eq!(score, 0.5);
    }

    #[test]
    fn test_chunk_text_single_char_max() {
        let text = "abc";
        let chunks = chunk_text(text, 1);
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn test_chunk_text_large_max() {
        let text = "Hello world";
        let chunks = chunk_text(text, 1000000);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_estimate_importance_empty_content() {
        let score = estimate_importance("", "user");
        assert_eq!(score, 0.5);
    }

    #[test]
    fn test_cosine_similarity_negative_values() {
        let a = vec![-1.0, -2.0, -3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_cosine_similarity_mixed_values() {
        let a = vec![1.0, -1.0];
        let b = vec![-1.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - (-1.0)).abs() < 1e-10);
    }

    #[test]
    fn test_cosine_similarity_large_vectors_100_elements() {
        let a: Vec<f64> = (0..100).map(|i| (i as f64) * 0.1).collect();
        let b: Vec<f64> = (0..100).map(|i| (i as f64) * 0.2).collect();
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_cosine_similarity_large_vectors_1000_elements() {
        let a: Vec<f64> = (0..1000).map(|i| ((i as f64) * 0.01).sin()).collect();
        let b: Vec<f64> = (0..1000).map(|i| ((i as f64) * 0.01).sin()).collect();
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_cosine_similarity_nan_produces_nan_or_zero() {
        let a = vec![f64::NAN, 1.0, 2.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.is_nan() || sim == 0.0);
    }

    #[test]
    fn test_cosine_similarity_very_small_values_underflow() {
        let a = vec![1e-300, 2e-300, 3e-300];
        let b = vec![1e-300, 2e-300, 3e-300];
        let sim = cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn test_cosine_similarity_moderately_small_values() {
        let a = vec![1e-100, 2e-100, 3e-100];
        let b = vec![1e-100, 2e-100, 3e-100];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_very_large_values() {
        let a = vec![1e150, 2e150, 3e150];
        let b = vec![1e150, 2e150, 3e150];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_one_zero_one_nonzero() {
        let a = vec![0.0, 0.0];
        let b = vec![0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn test_estimate_importance_error_and_bug_combined() {
        let score = estimate_importance("this error is actually a bug", "user");
        assert!((score - 0.65).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_error_critical_code() {
        let score = estimate_importance("error critical ```code```", "user");
        assert!((score - 0.75).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_all_keywords_assistant_long() {
        let long = "a".repeat(501);
        let content = format!("{} error bug fix critical ```block```", long);
        let score = estimate_importance(&content, "assistant");
        assert!((score - 0.95).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_exact_500_chars_boundary() {
        let exactly_500 = "a".repeat(500);
        let score_at = estimate_importance(&exactly_500, "user");
        assert_eq!(score_at, 0.5);

        let exactly_501 = "a".repeat(501);
        let score_above = estimate_importance(&exactly_501, "user");
        assert!((score_above - 0.6).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_case_sensitive_keywords() {
        let score_lower = estimate_importance("error", "user");
        assert!((score_lower - 0.65).abs() < 1e-10);

        let score_upper = estimate_importance("ERROR", "user");
        assert_eq!(score_upper, 0.5);
    }

    #[test]
    fn test_estimate_importance_keyword_in_larger_word() {
        let score = estimate_importance("errorhandling bugfix fixture critically", "user");
        assert!((score - 0.65).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_code_block_only() {
        let score = estimate_importance("some text with ``` code blocks ```", "user");
        assert!((score - 0.6).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_assistant_with_code_and_long() {
        let long = "a".repeat(501);
        let content = format!("{} ```some code```", long);
        let score = estimate_importance(&content, "assistant");
        assert!((score - 0.8).abs() < 1e-10);
    }

    #[test]
    fn test_chunk_text_unicode_emoji() {
        let text = "\u{1f600}\u{1f680}\u{1f4a1}\u{2764}\u{fe0f}\u{1f525}";
        let chunks = chunk_text(text, 100);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains('\u{1f600}'));
    }

    #[test]
    fn test_chunk_text_cjk_characters() {
        let text =
            "\u{4e16}\u{754c}\u{4f60}\u{597d}\u{6211}\u{4eec}\u{5b66}\u{4e60}\u{7f16}\u{7a0b}";
        let chunks = chunk_text(text, 15);
        assert!(!chunks.is_empty());
        let rejoined: String = chunks.join("");
        assert_eq!(rejoined, text);
    }

    #[test]
    fn test_chunk_text_cjk_split_at_one() {
        let text = "\u{4e16}\u{754c}\u{4f60}\u{597d}";
        let chunks = chunk_text(text, 1);
        assert_eq!(chunks.len(), 4);
        let rejoined: String = chunks.join("");
        assert_eq!(rejoined, text);
    }

    #[test]
    fn test_chunk_text_emoji_split_at_one() {
        let text = "\u{1f600}\u{1f680}\u{1f4a1}";
        let chunks = chunk_text(text, 1);
        assert_eq!(chunks.len(), 3);
        let rejoined: String = chunks.join("");
        assert_eq!(rejoined, text);
    }

    #[test]
    fn test_chunk_text_only_newlines() {
        let text = "\n\n\n\n\n";
        let chunks = chunk_text(text, 3);
        assert!(!chunks.is_empty());
        let rejoined: String = chunks.join("");
        assert_eq!(rejoined, text);
    }

    #[test]
    fn test_chunk_text_single_long_line() {
        let text = "a".repeat(200);
        let chunks = chunk_text(&text, 50);
        assert_eq!(chunks.len(), 4);
        for chunk in &chunks {
            assert!(chunk.len() <= 50);
        }
    }

    #[test]
    fn test_chunk_text_mixed_newlines_and_content() {
        let text = "line1\nline2\n\nline4\nline5\nline6";
        let chunks = chunk_text(text, 12);
        let rejoined: String = chunks.join("");
        assert_eq!(rejoined, text);
    }

    #[test]
    fn test_chunk_text_exact_boundary_at_newline() {
        let text = "12345\n67890\nabcde";
        let chunks = chunk_text(text, 6);
        assert!(chunks.len() >= 2);
        let rejoined: String = chunks.join("");
        assert_eq!(rejoined, text);
    }

    #[test]
    fn test_memory_entry_all_defaults_and_empty() {
        let entry = MemoryEntry {
            id: "".to_string(),
            agent_id: "".to_string(),
            content: "".to_string(),
            role: "".to_string(),
            embedding: None,
            timestamp: 0,
            session_id: None,
            importance: 0.0,
            hash: "".to_string(),
            confidence: 0.0,
            access_count: 0,
            last_accessed: 0,
        };
        let val = serde_json::to_value(&entry).unwrap();
        assert_eq!(val["id"], "");
        assert_eq!(val["importance"], 0.0);
        assert_eq!(val["confidence"], 0.0);
        assert_eq!(val["accessCount"], 0);
    }

    #[test]
    fn test_memory_entry_very_long_content() {
        let long_content = "x".repeat(1_000_000);
        let entry = MemoryEntry {
            id: "long-1".to_string(),
            agent_id: "agent-1".to_string(),
            content: long_content.clone(),
            role: "user".to_string(),
            embedding: None,
            timestamp: 1000,
            session_id: None,
            importance: 0.5,
            hash: "hash-long".to_string(),
            confidence: 1.0,
            access_count: 0,
            last_accessed: 1000,
        };
        assert_eq!(entry.content.len(), 1_000_000);
        let serialized = serde_json::to_string(&entry).unwrap();
        let deserialized: MemoryEntry = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.content.len(), 1_000_000);
    }

    #[test]
    fn test_memory_entry_empty_embedding_vec() {
        let entry = MemoryEntry {
            id: "emb-empty".to_string(),
            agent_id: "a".to_string(),
            content: "test".to_string(),
            role: "user".to_string(),
            embedding: Some(vec![]),
            timestamp: 100,
            session_id: None,
            importance: 0.5,
            hash: "h".to_string(),
            confidence: 1.0,
            access_count: 0,
            last_accessed: 100,
        };
        assert_eq!(entry.embedding.as_ref().unwrap().len(), 0);
        let val = serde_json::to_value(&entry).unwrap();
        assert!(val["embedding"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_memory_entry_large_embedding() {
        let embedding = vec![0.01; 1536];
        let entry = MemoryEntry {
            id: "emb-large".to_string(),
            agent_id: "a".to_string(),
            content: "test".to_string(),
            role: "user".to_string(),
            embedding: Some(embedding.clone()),
            timestamp: 100,
            session_id: None,
            importance: 0.5,
            hash: "h".to_string(),
            confidence: 1.0,
            access_count: 0,
            last_accessed: 100,
        };
        assert_eq!(entry.embedding.as_ref().unwrap().len(), 1536);
    }

    #[test]
    fn test_now_ms_monotonicity_repeated() {
        let t1 = now_ms();
        let t2 = now_ms();
        let t3 = now_ms();
        assert!(t2 >= t1);
        assert!(t3 >= t2);
    }

    #[test]
    fn test_now_ms_returns_reasonable_value() {
        let ts = now_ms();
        assert!(ts > 1_700_000_000_000);
    }

    #[test]
    fn test_sha256_empty_string_hash() {
        let mut hasher = Sha256::new();
        hasher.update(b"");
        let hash = format!("{:x}", hasher.finalize());
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_sha256_whitespace_only_hash() {
        let mut h1 = Sha256::new();
        h1.update(b" ");
        let hash_space = format!("{:x}", h1.finalize());

        let mut h2 = Sha256::new();
        h2.update(b"\t");
        let hash_tab = format!("{:x}", h2.finalize());

        let mut h3 = Sha256::new();
        h3.update(b"\n");
        let hash_newline = format!("{:x}", h3.finalize());

        assert_ne!(hash_space, hash_tab);
        assert_ne!(hash_space, hash_newline);
        assert_ne!(hash_tab, hash_newline);
        assert_eq!(hash_space.len(), 64);
    }

    #[test]
    fn test_sha256_unicode_content() {
        let mut h1 = Sha256::new();
        h1.update("\u{1f600}".as_bytes());
        let hash = format!("{:x}", h1.finalize());
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn test_session_compact_below_threshold() {
        let messages: Vec<Value> = (0..29)
            .map(|i| json!({"id": format!("m-{}", i), "role": "user"}))
            .collect();
        let threshold = 30_usize;
        let below = messages.len() < threshold;
        assert!(below);
    }

    #[test]
    fn test_session_compact_at_threshold() {
        let messages: Vec<Value> = (0..30)
            .map(|i| json!({"id": format!("m-{}", i), "role": "user"}))
            .collect();
        let threshold = 30_usize;
        let below = messages.len() < threshold;
        assert!(!below);
    }

    #[test]
    fn test_session_compact_above_threshold() {
        let messages: Vec<Value> = (0..31)
            .map(|i| json!({"id": format!("m-{}", i), "role": "user"}))
            .collect();
        let threshold = 30_usize;
        let keep_recent = 10_usize;
        let below = messages.len() < threshold;
        assert!(!below);
        let to_summarize = &messages[..messages.len().saturating_sub(keep_recent)];
        let to_keep = &messages[messages.len().saturating_sub(keep_recent)..];
        assert_eq!(to_summarize.len(), 21);
        assert_eq!(to_keep.len(), 10);
    }

    #[test]
    fn test_session_compact_keep_recent_larger_than_messages() {
        let messages: Vec<Value> = (0..5).map(|i| json!({"id": format!("m-{}", i)})).collect();
        let keep_recent = 10_usize;
        let to_keep = &messages[messages.len().saturating_sub(keep_recent)..];
        assert_eq!(to_keep.len(), 5);
    }

    #[test]
    fn test_eviction_stale_and_low_value() {
        let now = 10_000_000_000u64;
        let max_age_ms = 30 * 86_400_000u64;
        let timestamp = 0u64;
        let importance = 0.1;
        let confidence = 0.5;
        let min_importance = 0.2;

        let age = now.saturating_sub(timestamp);
        let is_stale = age > max_age_ms;
        let is_low_value = importance < min_importance;
        let is_low_confidence = confidence < 0.1;

        assert!(is_stale);
        assert!(is_low_value);
        assert!(!is_low_confidence);
        assert!((is_stale && is_low_value) || is_low_confidence);
    }

    #[test]
    fn test_eviction_low_confidence_only() {
        let confidence = 0.05;
        let is_low_confidence = confidence < 0.1;
        assert!(is_low_confidence);
    }

    #[test]
    fn test_eviction_not_stale_not_low_value() {
        let now = 1000u64;
        let max_age_ms = 30 * 86_400_000u64;
        let timestamp = 500u64;
        let importance = 0.8;
        let confidence = 0.5;

        let age = now.saturating_sub(timestamp);
        let is_stale = age > max_age_ms;
        let is_low_value = importance < 0.2;
        let is_low_confidence = confidence < 0.1;

        assert!(!is_stale);
        assert!(!is_low_value);
        assert!(!is_low_confidence);
        assert!(!((is_stale && is_low_value) || is_low_confidence));
    }

    #[test]
    fn test_consolidation_decay_rate() {
        let decay_rate: f64 = 0.05;
        let confidence: f64 = 0.8;
        let new_confidence = (confidence * (1.0 - decay_rate)).max(0.1);
        assert!((new_confidence - 0.76).abs() < 1e-10);
    }

    #[test]
    fn test_consolidation_decay_floors_at_0_1() {
        let decay_rate: f64 = 0.05;
        let confidence: f64 = 0.1;
        let new_confidence = (confidence * (1.0 - decay_rate)).max(0.1);
        assert_eq!(new_confidence, 0.1);
    }

    #[test]
    fn test_consolidation_skip_recently_accessed() {
        let now = 100_000u64;
        let seven_days_ms = 7 * 86_400_000u64;
        let last_accessed = now - 1000;
        let should_decay = now.saturating_sub(last_accessed) > seven_days_ms;
        assert!(!should_decay);
    }

    #[test]
    fn test_keyword_scoring() {
        let query = "find the error in code";
        let keywords: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        assert_eq!(keywords.len(), 5);

        let content_lower = "there was an error in the code block".to_lowercase();
        let hits = keywords
            .iter()
            .filter(|k| content_lower.contains(k.as_str()))
            .count();
        assert_eq!(hits, 4);
        let keyword_score = hits as f64 / keywords.len().max(1) as f64;
        assert!((keyword_score - 0.8).abs() < 1e-10);
    }

    #[test]
    fn test_keyword_scoring_empty_query() {
        let query = "";
        let keywords: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        assert_eq!(keywords.len(), 0);
        let hits = 0usize;
        let keyword_score = hits as f64 / keywords.len().max(1) as f64;
        assert_eq!(keyword_score, 0.0);
    }

    #[test]
    fn test_recency_score_recent() {
        let now = 1_000_000u64;
        let timestamp = 999_000u64;
        let age_hours = (now.saturating_sub(timestamp)) as f64 / 3_600_000.0;
        let recency = (-age_hours / 168.0_f64).exp();
        assert!(recency > 0.99);
    }

    #[test]
    fn test_recency_score_old() {
        let now = 100_000_000_000u64;
        let timestamp = 0u64;
        let age_hours = (now.saturating_sub(timestamp)) as f64 / 3_600_000.0;
        let recency = (-age_hours / 168.0_f64).exp();
        assert!(recency < 0.01);
    }

    #[test]
    fn test_memory_scope_format() {
        let agent_id = "agent-42";
        let scope = format!("memory:{}", agent_id);
        assert_eq!(scope, "memory:agent-42");
        assert!(scope.starts_with("memory:"));
    }

    #[test]
    fn test_session_scope_format() {
        let agent_id = "agent-42";
        let scope = format!("sessions:{}", agent_id);
        assert_eq!(scope, "sessions:agent-42");
    }

    #[test]
    fn test_repair_empty_id_removal() {
        let messages = vec![
            json!({"id": "m-1", "role": "user"}),
            json!({"id": "", "role": "user"}),
            json!({"role": "user"}),
            json!({"id": "m-4", "role": "assistant"}),
        ];
        let mut repaired = messages.clone();
        let before = repaired.len();
        repaired.retain(|m| {
            m.get("id")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        });
        assert_eq!(before - repaired.len(), 2);
        assert_eq!(repaired.len(), 2);
    }

    #[test]
    fn test_repair_dedup_by_id() {
        let messages = [
            json!({"id": "m-1", "role": "user"}),
            json!({"id": "m-1", "role": "user"}),
            json!({"id": "m-2", "role": "assistant"}),
        ];
        let mut seen = std::collections::HashSet::new();
        let repaired: Vec<&Value> = messages
            .iter()
            .filter(|m| {
                let id = m["id"].as_str().unwrap_or("").to_string();
                seen.insert(id)
            })
            .collect();
        assert_eq!(repaired.len(), 2);
    }

    #[test]
    fn test_repair_consecutive_role_merge() {
        let messages = vec![
            json!({"id": "m-1", "role": "user"}),
            json!({"id": "m-2", "role": "user"}),
            json!({"id": "m-3", "role": "assistant"}),
            json!({"id": "m-4", "role": "assistant"}),
            json!({"id": "m-5", "role": "system"}),
            json!({"id": "m-6", "role": "system"}),
        ];
        let mut merged = Vec::new();
        let mut merge_count = 0u64;
        for msg in &messages {
            let role = msg["role"].as_str().unwrap_or("");
            if let Some(last) = merged.last() {
                let last_role: &str = match last {
                    Value::Object(obj) => obj.get("role").and_then(|v| v.as_str()).unwrap_or(""),
                    _ => "",
                };
                if last_role == role && role != "system" {
                    merge_count += 1;
                    continue;
                }
            }
            merged.push(msg.clone());
        }
        assert_eq!(merge_count, 2);
        assert_eq!(merged.len(), 4);
    }

    #[test]
    fn test_repair_timestamp_reordering() {
        let mut messages = vec![
            json!({"id": "m-1", "timestamp": 100, "role": "user"}),
            json!({"id": "m-2", "timestamp": 50, "role": "assistant"}),
            json!({"id": "m-3", "timestamp": 200, "role": "user"}),
        ];
        let mut reordered = 0u64;
        let mut prev_ts = 0u64;
        for msg in &mut messages {
            let ts = msg["timestamp"].as_u64().unwrap_or(0);
            if ts < prev_ts
                && let Some(obj) = msg.as_object_mut()
            {
                obj.insert("timestamp".into(), json!(prev_ts + 1));
                reordered += 1;
            }
            prev_ts = msg["timestamp"].as_u64().unwrap_or(prev_ts);
        }
        assert_eq!(reordered, 1);
        assert_eq!(messages[1]["timestamp"], 101);
    }

    #[test]
    fn test_cosine_similarity_unit_vectors() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let c = vec![0.0, 0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-10);
        assert!(cosine_similarity(&a, &c).abs() < 1e-10);
        assert!(cosine_similarity(&b, &c).abs() < 1e-10);
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_cosine_similarity_half_pi_angle() {
        let a = vec![1.0, 1.0];
        let b = vec![1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        let expected = 1.0 / (2.0_f64).sqrt();
        assert!((sim - expected).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_multiple_keywords_only_one_bonus() {
        let score_one = estimate_importance("error happened", "user");
        let score_two = estimate_importance("error and bug happened", "user");
        assert_eq!(score_one, score_two);
    }

    #[test]
    fn test_estimate_importance_assistant_long_error_code() {
        let long = "a".repeat(501);
        let content = format!("{} error ```code```", long);
        let score = estimate_importance(&content, "assistant");
        assert!((score - 0.95).abs() < 1e-10);
    }

    #[test]
    fn test_estimate_importance_empty_role() {
        let score = estimate_importance("hello", "");
        assert_eq!(score, 0.5);
    }

    #[test]
    fn test_chunk_text_two_char_max() {
        let text = "abcdef";
        let chunks = chunk_text(text, 2);
        assert_eq!(chunks.len(), 3);
        let rejoined: String = chunks.join("");
        assert_eq!(rejoined, text);
    }

    #[test]
    fn test_chunk_text_newline_at_boundary() {
        let text = "ab\ncd\nef";
        let chunks = chunk_text(text, 3);
        assert!(chunks.len() >= 2);
        let rejoined: String = chunks.join("");
        assert_eq!(rejoined, text);
    }

    #[test]
    fn test_chunk_text_all_same_char() {
        let text = "x".repeat(50);
        let chunks = chunk_text(&text, 10);
        assert_eq!(chunks.len(), 5);
        for chunk in &chunks {
            assert_eq!(chunk.len(), 10);
        }
    }

    #[test]
    fn test_memory_entry_unicode_content() {
        let entry = MemoryEntry {
            id: "unicode-1".to_string(),
            agent_id: "agent".to_string(),
            content: "\u{4f60}\u{597d}\u{4e16}\u{754c} \u{1f600}".to_string(),
            role: "user".to_string(),
            embedding: None,
            timestamp: 100,
            session_id: None,
            importance: 0.5,
            hash: "h".to_string(),
            confidence: 1.0,
            access_count: 0,
            last_accessed: 100,
        };
        let serialized = serde_json::to_string(&entry).unwrap();
        let deserialized: MemoryEntry = serde_json::from_str(&serialized).unwrap();
        assert!(deserialized.content.contains("\u{4f60}"));
        assert!(deserialized.content.contains("\u{1f600}"));
    }

    #[test]
    fn test_memory_entry_max_importance_confidence() {
        let entry = MemoryEntry {
            id: "max-1".to_string(),
            agent_id: "agent".to_string(),
            content: "test".to_string(),
            role: "user".to_string(),
            embedding: None,
            timestamp: 100,
            session_id: None,
            importance: 1.0,
            hash: "h".to_string(),
            confidence: 1.0,
            access_count: u64::MAX,
            last_accessed: u64::MAX,
        };
        let val = serde_json::to_value(&entry).unwrap();
        assert_eq!(val["importance"], 1.0);
        assert_eq!(val["confidence"], 1.0);
    }

    #[test]
    fn test_consolidation_decay_multiple_rounds() {
        let decay_rate = 0.05_f64;
        let mut confidence = 1.0_f64;
        for _ in 0..100 {
            confidence = (confidence * (1.0 - decay_rate)).max(0.1);
        }
        assert_eq!(confidence, 0.1);
    }

    #[test]
    fn test_consolidation_decay_rate_zero() {
        let decay_rate = 0.0_f64;
        let confidence = 0.8_f64;
        let new_confidence = (confidence * (1.0 - decay_rate)).max(0.1);
        assert!((new_confidence - 0.8).abs() < 1e-10);
    }

    #[test]
    fn test_consolidation_decay_rate_one() {
        let decay_rate = 1.0_f64;
        let confidence = 0.8_f64;
        let new_confidence = (confidence * (1.0 - decay_rate)).max(0.1);
        assert_eq!(new_confidence, 0.1);
    }

    #[test]
    fn test_eviction_stale_but_high_value() {
        let now = 10_000_000_000u64;
        let max_age_ms = 30 * 86_400_000u64;
        let timestamp = 0u64;
        let importance = 0.9;
        let confidence = 0.5;

        let age = now.saturating_sub(timestamp);
        let is_stale = age > max_age_ms;
        let is_low_value = importance < 0.2;
        let is_low_confidence = confidence < 0.1;

        assert!(is_stale);
        assert!(!is_low_value);
        assert!(!((is_stale && is_low_value) || is_low_confidence));
    }

    #[test]
    fn test_eviction_not_stale_but_low_value() {
        let now = 1000u64;
        let max_age_ms = 30 * 86_400_000u64;
        let timestamp = 500u64;
        let importance = 0.1;

        let age = now.saturating_sub(timestamp);
        let is_stale = age > max_age_ms;
        let is_low_value = importance < 0.2;
        let is_low_confidence = false;

        assert!(!is_stale);
        assert!(is_low_value);
        assert!(!((is_stale && is_low_value) || is_low_confidence));
    }

    #[test]
    fn test_eviction_at_confidence_boundary() {
        let confidence_just_below = 0.09999;
        let confidence_at = 0.1;
        assert!(confidence_just_below < 0.1);
        assert!(confidence_at >= 0.1);
    }

    #[test]
    fn test_recency_score_one_week() {
        let now = 1_000_000_000u64;
        let one_week_ms = 7 * 24 * 3_600_000u64;
        let timestamp = now - one_week_ms;
        let age_hours = (now.saturating_sub(timestamp)) as f64 / 3_600_000.0;
        let recency = (-age_hours / 168.0_f64).exp();
        assert!((recency - (-1.0_f64).exp()).abs() < 1e-6);
    }

    #[test]
    fn test_keyword_scoring_all_match() {
        let query = "the code";
        let keywords: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        let content_lower = "the code is great".to_lowercase();
        let hits = keywords
            .iter()
            .filter(|k| content_lower.contains(k.as_str()))
            .count();
        let score = hits as f64 / keywords.len().max(1) as f64;
        assert!((score - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_keyword_scoring_no_match() {
        let query = "xyz abc";
        let keywords: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        let content_lower = "nothing matches here".to_lowercase();
        let hits = keywords
            .iter()
            .filter(|k| content_lower.contains(k.as_str()))
            .count();
        let score = hits as f64 / keywords.len().max(1) as f64;
        assert_eq!(score, 0.0);
    }

    #[test]
    fn test_repair_all_empty_ids() {
        let messages = vec![
            json!({"id": "", "role": "user"}),
            json!({"role": "user"}),
            json!({"id": null, "role": "user"}),
        ];
        let mut repaired = messages.clone();
        repaired.retain(|m| {
            m.get("id")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        });
        assert_eq!(repaired.len(), 0);
    }

    #[test]
    fn test_repair_no_duplicates() {
        let messages = [
            json!({"id": "m-1", "role": "user"}),
            json!({"id": "m-2", "role": "assistant"}),
            json!({"id": "m-3", "role": "user"}),
        ];
        let mut seen = std::collections::HashSet::new();
        let repaired: Vec<&Value> = messages
            .iter()
            .filter(|m| {
                let id = m["id"].as_str().unwrap_or("").to_string();
                seen.insert(id)
            })
            .collect();
        assert_eq!(repaired.len(), 3);
    }

    #[test]
    fn test_repair_consecutive_system_messages_not_merged() {
        let messages = vec![
            json!({"id": "m-1", "role": "system"}),
            json!({"id": "m-2", "role": "system"}),
            json!({"id": "m-3", "role": "system"}),
        ];
        let mut merged = Vec::new();
        let mut merge_count = 0u64;
        for msg in &messages {
            let role = msg["role"].as_str().unwrap_or("");
            if let Some(last) = merged.last() {
                let last_role: &str = match last {
                    Value::Object(obj) => obj.get("role").and_then(|v| v.as_str()).unwrap_or(""),
                    _ => "",
                };
                if last_role == role && role != "system" {
                    merge_count += 1;
                    continue;
                }
            }
            merged.push(msg.clone());
        }
        assert_eq!(merge_count, 0);
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn test_repair_timestamp_all_ascending() {
        let mut messages = vec![
            json!({"id": "m-1", "timestamp": 100}),
            json!({"id": "m-2", "timestamp": 200}),
            json!({"id": "m-3", "timestamp": 300}),
        ];
        let mut reordered = 0u64;
        let mut prev_ts = 0u64;
        for msg in &mut messages {
            let ts = msg["timestamp"].as_u64().unwrap_or(0);
            if ts < prev_ts
                && let Some(obj) = msg.as_object_mut()
            {
                obj.insert("timestamp".into(), json!(prev_ts + 1));
                reordered += 1;
            }
            prev_ts = msg["timestamp"].as_u64().unwrap_or(prev_ts);
        }
        assert_eq!(reordered, 0);
    }

    #[test]
    fn test_repair_timestamp_all_descending() {
        let mut messages = vec![
            json!({"id": "m-1", "timestamp": 300}),
            json!({"id": "m-2", "timestamp": 200}),
            json!({"id": "m-3", "timestamp": 100}),
        ];
        let mut reordered = 0u64;
        let mut prev_ts = 0u64;
        for msg in &mut messages {
            let ts = msg["timestamp"].as_u64().unwrap_or(0);
            if ts < prev_ts
                && let Some(obj) = msg.as_object_mut()
            {
                obj.insert("timestamp".into(), json!(prev_ts + 1));
                reordered += 1;
            }
            prev_ts = msg["timestamp"].as_u64().unwrap_or(prev_ts);
        }
        assert_eq!(reordered, 2);
    }

    #[test]
    fn test_kg_scope_format() {
        let agent_id = "agent-99";
        let scope = format!("kg:{}", agent_id);
        assert_eq!(scope, "kg:agent-99");
    }

    #[test]
    fn test_sha256_known_value() {
        let mut hasher = Sha256::new();
        hasher.update(b"hello");
        let hash = format!("{:x}", hasher.finalize());
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_chunk_text_preserves_content_large() {
        let text: String = (0..100).map(|i| format!("line {}\n", i)).collect();
        let chunks = chunk_text(&text, 50);
        let rejoined: String = chunks.join("");
        assert_eq!(rejoined, text);
    }

    // ---- round-trip tests through the real handlers -------------------------
    //
    // These drive `store_memory`, `recall_memory`, `evict_memories` and
    // `session_history` against an in-memory `state::*` implementation, so a
    // wire-shape mismatch between what store writes and what recall reads is a
    // test failure instead of a silent empty result.

    use agentos_http_adapter::fake::FakeBus;
    use agentos_http_adapter::policy;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    /// In-memory stand-in for the engine's `state::*` functions.
    ///
    /// The shapes below were verified against the pinned engine (iii 0.22.1)
    /// with `iii trigger state::<fn> --help` and real invocations:
    /// `state::list` answers a bare array of values with no keys,
    /// `state::list_groups` answers `{ "groups": [...] }`, `state::update`
    /// takes `ops` (not `operations`) where `increment` carries `by`, `append`
    /// takes the element itself, and a rejected operation comes back as a 200
    /// with an `errors` array.
    #[derive(Default)]
    struct StateStore {
        scopes: Mutex<BTreeMap<String, BTreeMap<String, Value>>>,
    }

    impl StateStore {
        fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, BTreeMap<String, Value>>> {
            self.scopes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
        }

        fn scope_of(input: &Value) -> String {
            input["scope"].as_str().unwrap_or_default().to_string()
        }

        fn key_of(input: &Value) -> String {
            input["key"].as_str().unwrap_or_default().to_string()
        }

        /// A missing key reads back as `null`.
        fn get(&self, input: &Value) -> Value {
            self.lock()
                .get(&Self::scope_of(input))
                .and_then(|scope| scope.get(&Self::key_of(input)))
                .cloned()
                .unwrap_or(Value::Null)
        }

        fn set(&self, input: &Value) {
            self.lock()
                .entry(Self::scope_of(input))
                .or_default()
                .insert(Self::key_of(input), input["value"].clone());
        }

        fn delete(&self, input: &Value) {
            if let Some(scope) = self.lock().get_mut(&Self::scope_of(input)) {
                scope.remove(&Self::key_of(input));
            }
        }

        /// A bare array of values: the engine does not return keys.
        fn list(&self, input: &Value) -> Value {
            let entries = self
                .lock()
                .get(&Self::scope_of(input))
                .map(|scope| scope.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            Value::Array(entries)
        }

        fn groups(&self) -> Value {
            json!({ "groups": self.lock().keys().collect::<Vec<_>>() })
        }

        /// Applies `ops`, reporting rejected operations the way the engine does.
        fn update(&self, input: &Value) -> Value {
            let mut scopes = self.lock();
            let entry = scopes
                .entry(Self::scope_of(input))
                .or_default()
                .entry(Self::key_of(input))
                .or_insert_with(|| json!({}));
            if !entry.is_object() {
                *entry = json!({});
            }
            // `operations` is not a field the engine knows: it fails the whole
            // invocation with `missing field ops`.
            let Some(ops) = input.get("ops").and_then(Value::as_array).cloned() else {
                return Value::Null;
            };
            let mut errors = Vec::new();
            for (index, op) in ops.iter().enumerate() {
                let path = op["path"].as_str().unwrap_or_default().to_string();
                let object = entry.as_object_mut().expect("entry is an object");
                match op["type"].as_str().unwrap_or_default() {
                    "set" => {
                        object.insert(path, op["value"].clone());
                    }
                    "append" => {
                        let target = object.entry(path).or_insert_with(|| json!([]));
                        match target.as_array_mut() {
                            Some(target) => target.push(op["value"].clone()),
                            None => errors.push(json!({
                                "code": "append.target_not_object",
                                "op_index": index,
                            })),
                        }
                    }
                    "merge" => {
                        if !op["value"].is_object() {
                            errors.push(json!({
                                "code": "merge.value.not_an_object",
                                "op_index": index,
                            }));
                        }
                    }
                    "increment" => match op.get("by").and_then(Value::as_i64) {
                        Some(by) => {
                            let current = object.get(&path).and_then(Value::as_i64).unwrap_or(0);
                            object.insert(path, json!(current + by));
                        }
                        None => errors.push(json!({
                            "code": "increment.missing_by",
                            "op_index": index,
                        })),
                    },
                    _ => {}
                }
            }
            json!({ "errors": errors })
        }
    }

    /// A bus whose `state::*` functions are backed by `StateStore`.
    fn state_bus() -> (FakeBus, Arc<StateStore>) {
        let store = Arc::new(StateStore::default());
        let bus = FakeBus::new();

        let state = store.clone();
        bus.on("state::get", move |input| Ok(state.get(&input)));
        let state = store.clone();
        bus.on("state::set", move |input| {
            state.set(&input);
            Ok(json!({ "stored": true }))
        });
        let state = store.clone();
        bus.on("state::delete", move |input| {
            state.delete(&input);
            Ok(json!({ "deleted": true }))
        });
        let state = store.clone();
        bus.on("state::list", move |input| Ok(state.list(&input)));
        let state = store.clone();
        bus.on("state::list_groups", move |_| Ok(state.groups()));
        let state = store.clone();
        bus.on("state::update", move |input| {
            // The engine rejects the whole invocation when `ops` is missing.
            if input.get("ops").and_then(Value::as_array).is_none() {
                return Err(Error::Handler(
                    "serialization error: missing field `ops`".to_string(),
                ));
            }
            Ok(state.update(&input))
        });
        // No embedding worker in these tests: recall must still work on keyword
        // and recency alone.
        bus.on_error("embedding::generate", "no embedding worker");

        (bus, store)
    }

    /// A payload as a trusted deputy sends it on behalf of `agent` (contract
    /// T1): the principal is `agent`, and `agentId` names the same agent, which
    /// is what agent-core sends for its own turns.
    fn as_agent(agent: &str, fields: Value) -> Value {
        let mut payload = fields;
        payload["agentId"] = json!(agent);
        payload["principal"] = principal::as_agent(agent);
        payload
    }

    async fn store(bus: &FakeBus, agent: &str, session: &str, role: &str, content: &str) -> Value {
        store_memory(
            bus,
            as_agent(
                agent,
                json!({
                    "sessionId": session,
                    "role": role,
                    "content": content,
                }),
            ),
        )
        .await
        .expect("store_memory failed")
    }

    async fn recall(bus: &FakeBus, agent: &str, query: &str) -> Value {
        recall_memory(bus, as_agent(agent, json!({ "query": query })))
            .await
            .expect("recall_memory failed")
    }

    #[tokio::test]
    async fn store_recall_evict_round_trip_through_the_real_handlers() {
        let (bus, _store) = state_bus();

        let first = store(&bus, "a-1", "s-1", "user", "the deploy pipeline is broken").await;
        let second = store(&bus, "a-1", "s-1", "assistant", "I will fix the pipeline").await;
        assert_eq!(first["stored"], true);
        assert_eq!(second["stored"], true);

        let recalled = recall(&bus, "a-1", "pipeline").await;
        let recalled = recalled.as_array().expect("recall returns an array");
        assert_eq!(
            recalled.len(),
            2,
            "recall must read back what store wrote, got {recalled:?}"
        );
        let contents: Vec<&str> = recalled
            .iter()
            .filter_map(|entry| entry["content"].as_str())
            .collect();
        assert!(contents.contains(&"the deploy pipeline is broken"));
        assert!(contents.contains(&"I will fix the pipeline"));
        assert!(recalled.iter().all(|entry| entry["role"].is_string()));

        // `cap: 0` forces every entry over the cap, independent of wall clock.
        let evicted = evict_memories(&bus, json!({ "cap": 0 }))
            .await
            .expect("evict_memories failed");
        assert_eq!(
            evicted["evicted"], 2,
            "eviction must be able to deserialize the stored entries"
        );

        let after = recall(&bus, "a-1", "pipeline").await;
        assert_eq!(after, json!([]), "evicted memories must be gone");
    }

    #[tokio::test]
    async fn recall_updates_access_counters_on_the_entries_it_returns() {
        let (bus, store_handle) = state_bus();
        let stored = store(&bus, "a-2", "s-1", "user", "remember the release date").await;
        let id = stored["id"].as_str().expect("stored id").to_string();

        recall(&bus, "a-2", "release").await;

        let entry = store_handle.get(&json!({ "scope": "memory:a-2", "key": id }));
        assert_eq!(entry["accessCount"], 1);
    }

    #[tokio::test]
    async fn session_history_returns_turns_in_time_order() {
        let (bus, _store) = state_bus();
        store(&bus, "a-3", "s-9", "user", "first question").await;
        store(&bus, "a-3", "s-9", "assistant", "first answer").await;
        store(&bus, "a-3", "s-9", "user", "second question").await;
        // Another session for the same agent must not leak in.
        store(&bus, "a-3", "other", "user", "unrelated").await;

        let history = session_history(&bus, as_agent("a-3", json!({ "sessionId": "s-9" })))
            .await
            .expect("session_history failed");

        let messages = history["messages"].as_array().expect("messages array");
        let pairs: Vec<(&str, &str)> = messages
            .iter()
            .map(|message| {
                (
                    message["role"].as_str().unwrap_or_default(),
                    message["content"].as_str().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("user", "first question"),
                ("assistant", "first answer"),
                ("user", "second question"),
            ]
        );
        assert!(
            messages
                .windows(2)
                .all(|pair| pair[0]["timestamp"].as_u64() <= pair[1]["timestamp"].as_u64()),
            "history must be ascending by time"
        );
    }

    #[tokio::test]
    async fn session_history_keeps_a_repeated_message_in_place() {
        let (bus, _store) = state_bus();
        store(&bus, "a-4", "s-1", "user", "ok").await;
        store(&bus, "a-4", "s-1", "assistant", "anything else?").await;
        let repeat = store(&bus, "a-4", "s-1", "user", "ok").await;
        assert_eq!(
            repeat["deduplicated"], true,
            "content hash dedup still runs"
        );
        assert_eq!(repeat["sessionIndexed"], true);

        let history = session_history(&bus, as_agent("a-4", json!({ "sessionId": "s-1" })))
            .await
            .expect("session_history failed");

        let contents: Vec<&str> = history["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .filter_map(|message| message["content"].as_str())
            .collect();
        assert_eq!(contents, vec!["ok", "anything else?", "ok"]);
    }

    #[tokio::test]
    async fn session_history_honours_the_limit_and_keeps_the_newest_turns() {
        let (bus, _store) = state_bus();
        for index in 0..5 {
            store(&bus, "a-5", "s-1", "user", &format!("message {index}")).await;
        }

        let history = session_history(
            &bus,
            as_agent("a-5", json!({ "sessionId": "s-1", "limit": 2 })),
        )
        .await
        .expect("session_history failed");

        let contents: Vec<&str> = history["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .filter_map(|message| message["content"].as_str())
            .collect();
        assert_eq!(contents, vec!["message 3", "message 4"]);
    }

    #[tokio::test]
    async fn session_history_requires_a_session_id() {
        let (bus, _store) = state_bus();
        let error = session_history(&bus, as_agent("a-6", json!({})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("sessionId is required"));
    }

    #[tokio::test]
    async fn session_history_is_empty_for_an_unknown_session() {
        let (bus, _store) = state_bus();
        let history = session_history(&bus, as_agent("a-7", json!({ "sessionId": "nope" })))
            .await
            .expect("session_history failed");
        assert_eq!(history["messages"], json!([]));
    }

    #[tokio::test]
    async fn store_reports_a_failed_session_index_write() {
        let (bus, _store) = state_bus();
        bus.on_error("state::update", "state worker offline");

        let stored = store(&bus, "a-8", "s-1", "user", "will not be indexed").await;

        assert_eq!(stored["stored"], true);
        assert_eq!(
            stored["sessionIndexed"], false,
            "a dropped index write must be visible, not silent"
        );
    }

    #[tokio::test]
    async fn session_index_write_uses_the_engine_update_contract() {
        let (bus, _store) = state_bus();
        store(&bus, "a-9", "s-1", "user", "contract check").await;

        let calls = bus.calls_to("state::update");
        let update = &calls
            .iter()
            .find(|call| call.payload["scope"] == "sessions:a-9")
            .expect("the session index is updated")
            .payload;

        assert!(
            update.get("operations").is_none(),
            "the engine field is `ops`; `operations` fails with missing field `ops`"
        );
        let ops = update["ops"].as_array().expect("ops array");
        let append = &ops[0];
        assert_eq!(append["type"], "append", "a list needs append, not merge");
        assert!(
            append["value"].is_object(),
            "append takes the element itself; an array would nest"
        );
        assert_eq!(append["value"]["role"], "user");
    }

    #[tokio::test]
    async fn recall_access_counter_uses_the_increment_amount_field() {
        let (bus, _store) = state_bus();
        store(&bus, "a-10", "s-1", "user", "counter contract").await;

        recall(&bus, "a-10", "counter").await;

        let increment = bus
            .calls_to("state::update")
            .into_iter()
            .find(|call| call.payload["scope"] == "memory:a-10")
            .expect("the access counter is updated")
            .payload["ops"][0]
            .clone();
        assert_eq!(increment["type"], "increment");
        assert_eq!(increment["by"], 1, "the engine field is `by`, not `value`");
    }

    #[tokio::test]
    async fn store_reports_a_rejected_session_index_operation() {
        let (bus, _store) = state_bus();
        // A rejected operation arrives as a 200 with an `errors` array.
        bus.on("state::update", |_| {
            Ok(json!({ "errors": [{ "code": "append.target_not_object" }] }))
        });

        let stored = store(&bus, "a-11", "s-1", "user", "rejected index write").await;

        assert_eq!(stored["stored"], true);
        assert_eq!(stored["sessionIndexed"], false);
    }

    // --- tenancy (contract T1): who a call is FROM vs what it is ABOUT ---

    /// `std::env::set_var` is process-global and `cargo test` runs tests in
    /// parallel threads, so operator tests serialise on this lock and restore
    /// the previous value. Same pattern as `agentos_bus_auth::client`.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_api_key<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("AGENTOS_API_KEY");
        unsafe {
            match value {
                Some(value) => std::env::set_var("AGENTOS_API_KEY", value),
                None => std::env::remove_var("AGENTOS_API_KEY"),
            }
        }
        let result = body();
        unsafe {
            match previous {
                Some(value) => std::env::set_var("AGENTOS_API_KEY", value),
                None => std::env::remove_var("AGENTOS_API_KEY"),
            }
        }
        result
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(future)
    }

    /// What the HTTP adapter forwards for an authenticated edge request.
    fn as_operator(token: &str, fields: Value) -> Value {
        let mut payload = fields;
        payload["headers"] = json!({ "authorization": format!("Bearer {token}") });
        payload
    }

    /// A capability reader whose store grants `a-1` exactly `grant::act_as::a-2`,
    /// and `a-wild` everything a wildcard can express. It answers through the
    /// shared matcher, so what this proves is the real rule, not a stub's.
    fn capability_reader(bus: &FakeBus) {
        bus.on("security::check_capability", |input| {
            let agent = input["agentId"].as_str().unwrap_or_default();
            let resource = input["resource"].as_str().unwrap_or_default();
            let tools: Vec<String> = match agent {
                "a-1" => vec!["memory::*".into(), policy::act_as_grant("a-2")],
                "a-wild" => vec!["*".into(), "memory::*".into(), "grant::*".into()],
                _ => vec![],
            };
            if policy::capabilities_grant(&tools, resource) {
                Ok(json!({ "allowed": true, "reason": "granted" }))
            } else {
                Err(Error::Handler(format!("Agent {agent} denied: {resource}")))
            }
        });
    }

    #[tokio::test]
    async fn a_missing_principal_fails_closed_before_any_state_is_read() {
        let (bus, _store) = state_bus();
        capability_reader(&bus);
        let bare = json!({ "agentId": "a-1", "sessionId": "s-1", "query": "x", "key": "k", "value": 1, "content": "c" });

        let results = vec![
            store_memory(&bus, bare.clone()).await,
            recall_memory(&bus, bare.clone()).await,
            session_history(&bus, bare.clone()).await,
            list_sessions(&bus, bare.clone()).await,
            delete_session(&bus, json!({ "id": "s-1", "agentId": "a-1" })).await,
            list_memories(&bus, bare.clone()).await,
            memory_kv_get(&bus, bare.clone()).await,
            memory_kv_set(&bus, bare.clone()).await,
            memory_kv_delete(&bus, bare.clone()).await,
            memory_kv_list(&bus, bare.clone()).await,
            kg_add(&bus, json!({ "agentId": "a-1", "entity": { "id": "e" } })).await,
            kg_query(&bus, json!({ "agentId": "a-1", "entityId": "e" })).await,
            compact_session(&bus, bare.clone()).await,
            repair_session(&bus, bare.clone()).await,
        ];
        for result in results {
            let error = result.expect_err("a payload agentId alone is not a principal");
            assert!(
                error.to_string().contains("principal required"),
                "unexpected error: {error}"
            );
        }
        assert!(
            bus.calls().is_empty(),
            "nothing may be read or written for an unidentified caller, got {:?}",
            bus.calls()
        );
    }

    #[tokio::test]
    async fn agent_a_cannot_recall_read_or_evict_agent_b() {
        let (bus, _store) = state_bus();
        capability_reader(&bus);
        store(&bus, "a-2", "s-2", "user", "agent two's private note").await;
        store(&bus, "a-3", "s-3", "user", "agent three's private note").await;
        let before = bus.calls().len();

        // a-3 holds no grant at all; a-1 holds one for a-2 but not for a-3.
        let attempts = vec![
            recall_memory(&bus, json!({ "principal": { "agentId": "a-3" }, "agentId": "a-2", "query": "note" })).await,
            recall_memory(&bus, json!({ "principal": { "agentId": "a-1" }, "agentId": "a-3", "query": "note" })).await,
            session_history(&bus, json!({ "principal": { "agentId": "a-3" }, "agentId": "a-2", "sessionId": "s-2" })).await,
            list_sessions(&bus, json!({ "principal": { "agentId": "a-3" }, "agent": "a-2" })).await,
            delete_session(&bus, json!({ "principal": { "agentId": "a-3" }, "agentId": "a-2", "id": "s-2" })).await,
            list_memories(&bus, json!({ "principal": { "agentId": "a-3" }, "agentId": "a-2" })).await,
            memory_kv_list(&bus, json!({ "principal": { "agentId": "a-3" }, "agentId": "a-2" })).await,
            store_memory(&bus, json!({ "principal": { "agentId": "a-3" }, "agentId": "a-2", "content": "planted" })).await,
        ];
        for attempt in attempts {
            let error = attempt.expect_err("cross-agent access without the grant");
            let message = error.to_string();
            assert!(
                message.contains("grant::act_as::") && message.contains("may not act on agent"),
                "unexpected error: {message}"
            );
        }

        let after: Vec<_> = bus.calls().into_iter().skip(before).collect();
        assert!(
            after
                .iter()
                .all(|call| call.function_id == "security::check_capability"),
            "only the capability reader may be consulted, got {after:?}"
        );
        assert!(
            after.iter().all(|call| call.payload["resource"]
                .as_str()
                .is_some_and(|resource| resource.starts_with("grant::act_as::"))),
            "the reader is asked for the exact act_as grant, got {after:?}"
        );

        // And a-2's data is intact and still its own.
        let own = recall(&bus, "a-2", "note").await;
        assert_eq!(own.as_array().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn a_trusted_worker_with_the_exact_grant_can_act_on_the_other_agent() {
        let (bus, _store) = state_bus();
        capability_reader(&bus);
        store(&bus, "a-2", "s-2", "user", "agent two's shared note").await;

        // a-1 holds grant::act_as::a-2.
        let recalled = recall_memory(
            &bus,
            json!({ "principal": { "agentId": "a-1" }, "agentId": "a-2", "query": "shared" }),
        )
        .await
        .expect("granted cross-agent recall");
        assert_eq!(recalled[0]["content"], "agent two's shared note");

        let history = session_history(
            &bus,
            json!({ "principal": { "agentId": "a-1" }, "agentId": "a-2", "sessionId": "s-2" }),
        )
        .await
        .expect("granted cross-agent history");
        assert_eq!(history["agentId"], "a-2");
        assert_eq!(history["messages"].as_array().map(Vec::len), Some(1));

        let asked: Vec<_> = bus
            .calls_to("security::check_capability")
            .into_iter()
            .map(|call| call.payload)
            .collect();
        assert_eq!(asked.len(), 2);
        assert!(
            asked.iter().all(|payload| payload["agentId"] == "a-1"
                && payload["resource"] == "grant::act_as::a-2")
        );
    }

    #[tokio::test]
    async fn no_wildcard_capability_reaches_another_agent() {
        let (bus, _store) = state_bus();
        capability_reader(&bus);
        store(&bus, "a-2", "s-2", "user", "not for wildcards").await;

        // `a-wild` holds `*`, `memory::*` and `grant::*` — every wildcard a
        // capability document can express — and still may not act as a-2.
        let error = recall_memory(
            &bus,
            json!({ "principal": { "agentId": "a-wild" }, "agentId": "a-2", "query": "wildcards" }),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("grant::act_as::a-2"), "{error}");
    }

    #[tokio::test]
    async fn an_agent_principal_is_confined_to_its_own_scopes() {
        let (bus, _store) = state_bus();
        capability_reader(&bus);
        store(&bus, "a-2", "s-2", "user", "in agent two's scope").await;
        let before = bus.calls().len();

        // No `agentId` at all: an agent principal acts on itself, never on
        // "default" and never on every agent.
        let sessions = list_sessions(&bus, json!({ "principal": { "agentId": "a-3" } }))
            .await
            .expect("own sessions");
        assert_eq!(sessions, json!([]));
        let missing = delete_session(
            &bus,
            json!({ "principal": { "agentId": "a-3" }, "id": "s-2" }),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(missing.contains("session not found"), "{missing}");
        let recalled = recall_memory(
            &bus,
            json!({ "principal": { "agentId": "a-3" }, "query": "scope" }),
        )
        .await
        .expect("own recall");
        assert_eq!(recalled, json!([]));

        let touched: Vec<String> = bus
            .calls()
            .into_iter()
            .skip(before)
            .filter_map(|call| call.payload["scope"].as_str().map(str::to_string))
            .collect();
        assert!(
            touched.iter().all(|scope| scope.ends_with(":a-3")),
            "an agent principal may only touch its own scopes, got {touched:?}"
        );
        assert!(!touched.is_empty());
    }

    #[tokio::test]
    async fn the_cron_path_still_evicts_and_consolidates_every_agent() {
        let (bus, _store) = state_bus();
        store(&bus, "a-1", "s-1", "user", "one").await;
        store(&bus, "a-2", "s-2", "user", "two").await;

        // Exactly what the engine's cron worker sends: a cron event, no headers,
        // no principal. `cap: 0` forces every entry over the cap.
        let cron_event = json!({ "cap": 0, "timestamp": 1_700_000_000_000_u64 });
        let evicted = evict_memories(&bus, cron_event.clone())
            .await
            .expect("cron eviction");
        assert_eq!(evicted["evicted"], 2);
        assert_eq!(recall(&bus, "a-1", "one").await, json!([]));
        assert_eq!(recall(&bus, "a-2", "two").await, json!([]));

        let consolidated = consolidate(&bus, cron_event)
            .await
            .expect("cron consolidation");
        assert_eq!(consolidated["memoriesDecayed"], 0);
    }

    #[tokio::test]
    async fn an_agent_principal_may_not_run_system_wide_maintenance() {
        let (bus, _store) = state_bus();
        store(&bus, "a-1", "s-1", "user", "keep me").await;
        store(&bus, "a-2", "s-2", "user", "keep me too").await;
        let before = bus.calls().len();

        // Before contract T1 an agent granted `memory::*` could reach this
        // through a tool call and wipe every agent's memory.
        for operation in ["memory::evict", "memory::consolidate"] {
            let payload = json!({ "principal": { "agentId": "a-1" }, "cap": 0 });
            let result = if operation == "memory::evict" {
                evict_memories(&bus, payload).await
            } else {
                consolidate(&bus, payload).await
            };
            let error = result.unwrap_err().to_string();
            assert!(error.contains("system-wide"), "{operation}: {error}");
        }
        assert_eq!(bus.calls().len(), before, "nothing may be touched");
        assert_eq!(
            recall(&bus, "a-2", "keep").await.as_array().map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn the_operator_names_any_agent_and_lists_every_agent() {
        with_api_key(Some("operator-key"), || {
            block_on(async {
                let (bus, store_handle) = {
                    // `agents` is what `agent::create` writes and what the
                    // sessions screen walks when no agent is named.
                    let (bus, store_handle) = state_bus();
                    capability_reader(&bus);
                    store_handle
                        .set(&json!({ "scope": "agents", "key": "a-1", "value": { "id": "a-1" } }));
                    store_handle
                        .set(&json!({ "scope": "agents", "key": "a-2", "value": { "id": "a-2" } }));
                    (bus, store_handle)
                };
                store(&bus, "a-1", "s-1", "user", "one").await;
                store(&bus, "a-2", "s-2", "user", "two").await;

                let recalled = recall_memory(
                    &bus,
                    as_operator("operator-key", json!({ "agentId": "a-2", "query": "two" })),
                )
                .await
                .expect("operator recall");
                assert_eq!(recalled[0]["content"], "two");

                let sessions = list_sessions(&bus, as_operator("operator-key", json!({})))
                    .await
                    .expect("operator session list");
                let ids: Vec<&str> = sessions
                    .as_array()
                    .expect("array")
                    .iter()
                    .filter_map(|session| session["id"].as_str())
                    .collect();
                assert_eq!(ids, vec!["s-1", "s-2"]);

                let default_kv = memory_kv_set(
                    &bus,
                    as_operator("operator-key", json!({ "key": "k", "value": 1 })),
                )
                .await
                .expect("operator kv set without an agent");
                assert_eq!(default_kv["stored"], true);
                assert_eq!(
                    store_handle.get(&json!({ "scope": "agent-memory:default", "key": "k" })),
                    json!(1),
                    "the operator naming nobody still means the default agent"
                );
                assert_eq!(
                    bus.call_count("security::check_capability"),
                    0,
                    "the operator needs no grant"
                );
            })
        });
    }

    #[test]
    fn a_wrong_bearer_or_an_unconfigured_key_is_refused() {
        let refused = |key: Option<&str>, token: &str| {
            with_api_key(key, || {
                block_on(async {
                    let (bus, _store) = state_bus();
                    let error = recall_memory(
                        &bus,
                        as_operator(token, json!({ "agentId": "a-1", "query": "x" })),
                    )
                    .await
                    .unwrap_err()
                    .to_string();
                    assert!(bus.calls().is_empty());
                    error
                })
            })
        };
        assert!(refused(Some("operator-key"), "not-the-key").contains("Unauthorized"));
        assert!(
            refused(None, "operator-key").contains("Unauthorized"),
            "a stack without a key has no operator"
        );
    }

    #[test]
    fn state_list_values_are_read_without_a_key_value_envelope() {
        // What the engine really answers.
        let bare = json!([{ "id": "m-1", "content": "hi" }]);
        assert_eq!(state_values(&bare)[0]["id"], "m-1");

        // And the envelope shape stays readable.
        let enveloped = json!([{ "key": "m-1", "value": { "id": "m-1", "content": "hi" } }]);
        assert_eq!(state_values(&enveloped)[0]["id"], "m-1");

        // A stored document that merely has a `value` field is not an envelope.
        let ambiguous = json!([{ "id": "m-1", "value": 7 }]);
        assert_eq!(state_values(&ambiguous)[0]["id"], "m-1");
    }

    #[test]
    fn state_groups_reads_the_engine_envelope() {
        assert_eq!(
            state_groups(&json!({ "groups": ["memory:a", "sessions:a"] })),
            vec!["memory:a", "sessions:a"]
        );
        assert_eq!(state_groups(&json!(["memory:a"])), vec!["memory:a"]);
        assert!(state_groups(&Value::Null).is_empty());
    }
}
