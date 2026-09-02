use iii_sdk::errors::Error;
use iii_sdk::{
    IIIClient, RegisterFunction,
    protocol::{TriggerAction, TriggerRequest},
    register_worker,
};
use serde_json::{Value, json};

mod types;

use types::{LifecycleState, Reaction};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn fire_void(iii: &IIIClient, function_id: &str, payload: Value) {
    let iii = iii.clone();
    let id = function_id.to_string();
    tokio::spawn(async move {
        let _ = iii
            .trigger(TriggerRequest {
                function_id: id,
                payload,
                action: Some(TriggerAction::Void),
                timeout_ms: None,
            })
            .await;
    });
}

async fn safe_state_get(iii: &IIIClient, scope: &str, key: &str) -> Option<Value> {
    iii.trigger(TriggerRequest {
        function_id: "state::get".into(),
        payload: json!({ "scope": scope, "key": key }),
        action: None,
        timeout_ms: None,
    })
    .await
    .ok()
    .filter(|v| !v.is_null())
}

async fn safe_state_list(iii: &IIIClient, scope: &str) -> Vec<Value> {
    iii.trigger(TriggerRequest {
        function_id: "state::list".into(),
        payload: json!({ "scope": scope }),
        action: None,
        timeout_ms: None,
    })
    .await
    .ok()
    .and_then(|v| v.as_array().cloned())
    .unwrap_or_default()
}

/// `state::list` answers a bare array of stored values: there is no `{key,
/// value}` envelope to unwrap, so a reaction is decoded straight from the
/// entry.
fn reaction_from_entry(entry: &Value) -> Option<Reaction> {
    serde_json::from_value::<Reaction>(entry.clone()).ok()
}

/// The non-deleted documents of a `state::list` response.
///
/// `state::set value=null` leaves a null entry in the scope until the key is
/// deleted, so the nulls are dropped here.
fn stored_values(entries: Vec<Value>) -> Vec<Value> {
    entries.into_iter().filter(|v| !v.is_null()).collect()
}

/// Agent ids from a `state::list` over the `agents` scope.
///
/// The storage key is not part of the response, so the id is read from the
/// agent document itself, which `agent::create` writes as `id`.
fn agent_ids_from_entries(entries: &[Value]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|agent| agent.get("id")?.as_str().map(String::from))
        .filter(|id| !id.is_empty())
        .collect()
}

/// `state::update` payload that appends one transition to the history list.
///
/// The engine names the operation list `ops` (an `operations` key fails the
/// whole invocation with "missing field `ops`"), and a list grows with
/// `append` carrying the element itself. `merge` only accepts an object and
/// answers `merge.value.not_an_object` inside an otherwise successful result.
fn transition_history_payload(scope: &str, entry: &Value) -> Value {
    json!({
        "scope": scope,
        "key": "history",
        "ops": [
            { "type": "append", "path": "transitions", "value": entry },
        ],
    })
}

/// `state::update` payload that records one reaction firing. An `increment`
/// carries its step in `by`, not `value`.
fn reaction_attempt_payload(scope: &str, reaction_id: &str, fired_at: i64) -> Value {
    json!({
        "scope": scope,
        "key": reaction_id,
        "ops": [
            { "type": "increment", "path": "attempts", "by": 1 },
            { "type": "set", "path": "lastFiredAt", "value": fired_at },
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

async fn transition(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let agent_id = input["agentId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Handler("agentId required".into()))?;
    let new_state_str = input["newState"]
        .as_str()
        .ok_or_else(|| Error::Handler("newState required".into()))?;
    let new_state = LifecycleState::from_str(new_state_str)
        .ok_or_else(|| Error::Handler(format!("invalid newState: {new_state_str}")))?;
    let reason = input["reason"].as_str().unwrap_or("").to_string();

    let scope = format!("lifecycle:{agent_id}");
    let current = safe_state_get(iii, &scope, "state").await;
    let current_state_str = current
        .as_ref()
        .and_then(|v| v["state"].as_str())
        .unwrap_or("spawning");
    let current_state =
        LifecycleState::from_str(current_state_str).unwrap_or(LifecycleState::Spawning);

    if current_state.is_terminal() {
        return Ok(json!({
            "transitioned": false,
            "reason": format!("Cannot transition from terminal state: {}", current_state.as_str()),
        }));
    }

    if !current_state.allows(new_state) {
        return Ok(json!({
            "transitioned": false,
            "reason": format!("Invalid transition: {} → {}", current_state.as_str(), new_state.as_str()),
        }));
    }

    let entry = json!({
        "state": new_state.as_str(),
        "previousState": current_state.as_str(),
        "reason": reason,
        "transitionedAt": now_ms(),
    });

    iii.trigger(TriggerRequest {
        function_id: "state::set".into(),
        payload: json!({ "scope": &scope, "key": "state", "value": entry }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    let history_update = iii
        .trigger(TriggerRequest {
            function_id: "state::update".into(),
            payload: transition_history_payload(&scope, &entry),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;
    if let Some(rejection) = update_rejection(&history_update) {
        return Err(Error::Handler(format!(
            "transition history append rejected for {scope}: {rejection}"
        )));
    }

    fire_void(
        iii,
        "hook::fire",
        json!({
            "type": "SessionStateChange",
            "agentId": agent_id,
            "from": current_state.as_str(),
            "to": new_state.as_str(),
            "reason": reason,
        }),
    );

    let agent_scope = format!("lifecycle_reactions:{agent_id}");
    let agent_reactions = safe_state_list(iii, &agent_scope).await;
    let global_reactions = safe_state_list(iii, "lifecycle_reactions").await;

    let mut combined: Vec<(String, Value)> = Vec::new();
    for r in &agent_reactions {
        combined.push((agent_scope.clone(), r.clone()));
    }
    for r in &global_reactions {
        combined.push(("lifecycle_reactions".into(), r.clone()));
    }

    for (scope_name, raw) in combined {
        let Some(reaction) = reaction_from_entry(&raw) else {
            continue;
        };
        if reaction.from != current_state || reaction.to != new_state {
            continue;
        }

        if reaction.attempts >= reaction.escalate_after {
            fire_void(
                iii,
                "hook::fire",
                json!({
                    "type": "LifecycleEscalation",
                    "agentId": agent_id,
                    "reaction": reaction.id,
                    "attempts": reaction.attempts,
                }),
            );
            continue;
        }

        match reaction.action.as_str() {
            "send_to_agent" => {
                let message = reaction.payload["message"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| {
                        format!(
                            "State changed: {} → {}",
                            current_state.as_str(),
                            new_state.as_str()
                        )
                    });
                fire_void(
                    iii,
                    "fn::agent_send",
                    json!({ "targetAgentId": agent_id, "message": message }),
                );
            }
            "auto_recover" => {
                // recovery::recover requires a `classification` to pick the
                // correct recovery path; default to "wake-up" when the
                // reaction payload doesn't carry one.
                let classification = reaction.payload["classification"]
                    .as_str()
                    .unwrap_or("wake-up");
                fire_void(
                    iii,
                    "recovery::recover",
                    json!({ "agentId": agent_id, "classification": classification }),
                );
            }
            "notify" => {
                fire_void(
                    iii,
                    "hook::fire",
                    json!({
                        "type": "LifecycleNotification",
                        "agentId": agent_id,
                        "from": current_state.as_str(),
                        "to": new_state.as_str(),
                        "payload": reaction.payload,
                    }),
                );
            }
            "escalate" => {
                fire_void(
                    iii,
                    "hook::fire",
                    json!({
                        "type": "LifecycleEscalation",
                        "agentId": agent_id,
                        "reaction": reaction.id,
                        "attempts": reaction.attempts,
                        "immediate": true,
                    }),
                );
            }
            _ => {}
        }

        // The attempt counter drives escalation, so a lost write is not
        // cosmetic: report it instead of discarding the result.
        match iii
            .trigger(TriggerRequest {
                function_id: "state::update".into(),
                payload: reaction_attempt_payload(&scope_name, &reaction.id, now_ms()),
                action: None,
                timeout_ms: None,
            })
            .await
        {
            Ok(result) => {
                if let Some(rejection) = update_rejection(&result) {
                    tracing::warn!(
                        reaction = %reaction.id,
                        rejection = %rejection,
                        "reaction attempt counter update was rejected"
                    );
                }
            }
            Err(e) => tracing::warn!(
                reaction = %reaction.id,
                error = %e,
                "reaction attempt counter update failed"
            ),
        }
    }

    tracing::info!(
        agent_id = agent_id,
        from = current_state.as_str(),
        to = new_state.as_str(),
        "Lifecycle transition"
    );

    Ok(json!({
        "transitioned": true,
        "from": current_state.as_str(),
        "to": new_state.as_str(),
    }))
}

async fn get_state(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let agent_id = input["agentId"]
        .as_str()
        .ok_or_else(|| Error::Handler("agentId required".into()))?;
    let scope = format!("lifecycle:{agent_id}");
    let state = safe_state_get(iii, &scope, "state").await;
    Ok(state.unwrap_or_else(|| json!({ "state": "spawning", "transitionedAt": now_ms() })))
}

async fn add_reaction(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    // agentId is optional: an empty/missing id stores the rule under the
    // global "lifecycle_reactions" scope, which `transition` already
    // evaluates alongside the per-agent scope (see lines 133-136).
    let agent_id = input["agentId"].as_str().filter(|s| !s.is_empty());
    let from = input["from"]
        .as_str()
        .and_then(LifecycleState::from_str)
        .ok_or_else(|| Error::Handler("invalid from state".into()))?;
    let to = input["to"]
        .as_str()
        .and_then(LifecycleState::from_str)
        .ok_or_else(|| Error::Handler("invalid to state".into()))?;
    let action = input["action"]
        .as_str()
        .ok_or_else(|| Error::Handler("action required".into()))?
        .to_string();
    let payload = input.get("payload").cloned().unwrap_or_else(|| json!({}));
    let raw_escalate = input["escalateAfter"].as_i64().unwrap_or(3);
    let escalate_after = raw_escalate.max(1) as u32;

    let id = format!(
        "rxn_{}_{}",
        now_ms(),
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    let reaction = Reaction {
        id: id.clone(),
        from,
        to,
        action,
        payload,
        escalate_after,
        attempts: 0,
    };

    let scope = match agent_id {
        Some(id) => format!("lifecycle_reactions:{id}"),
        None => "lifecycle_reactions".to_string(),
    };
    iii.trigger(TriggerRequest {
        function_id: "state::set".into(),
        payload: json!({
            "scope": scope,
            "key": &id,
            "value": serde_json::to_value(&reaction).map_err(|e| Error::Handler(e.to_string()))?
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    Ok(json!({ "id": id, "registered": true }))
}

async fn list_reactions(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let scope = match input["agentId"].as_str() {
        Some(id) if !id.is_empty() => format!("lifecycle_reactions:{id}"),
        _ => "lifecycle_reactions".to_string(),
    };
    let entries = safe_state_list(iii, &scope).await;
    Ok(json!(stored_values(entries)))
}

async fn check_all(iii: &IIIClient) -> Result<Value, Error> {
    let agents = safe_state_list(iii, "agents").await;
    let valid_agents = agent_ids_from_entries(&agents);

    let mut active: Vec<(String, Value)> = Vec::new();
    for agent_id in &valid_agents {
        let scope = format!("lifecycle:{agent_id}");
        if let Some(state) = safe_state_get(iii, &scope, "state").await {
            let is_terminal = state["state"]
                .as_str()
                .and_then(LifecycleState::from_str)
                .map(|s| s.is_terminal())
                .unwrap_or(false);
            if !is_terminal {
                active.push((agent_id.clone(), state));
            }
        }
    }

    let mut transitioned = 0;
    let two_hours_ms: i64 = 2 * 60 * 60 * 1000;
    let now = now_ms();

    for (agent_id, state) in active {
        let guard_stats = iii
            .trigger(TriggerRequest {
                function_id: "guard::stats".into(),
                payload: json!({ "agentId": &agent_id }),
                action: None,
                timeout_ms: None,
            })
            .await
            .ok();

        let circuit_broken = guard_stats
            .as_ref()
            .and_then(|s| s["circuitBroken"].as_bool())
            .unwrap_or(false);

        let state_name = state["state"].as_str().unwrap_or("");

        if circuit_broken && state_name == "working" {
            let _ = iii
                .trigger(TriggerRequest {
                    function_id: "lifecycle::transition".into(),
                    payload: json!({
                        "agentId": &agent_id,
                        "newState": "blocked",
                        "reason": "Circuit breaker tripped"
                    }),
                    action: None,
                    timeout_ms: None,
                })
                .await;
            transitioned += 1;
            continue;
        }

        if state_name == "working"
            && let Some(transitioned_at) = state["transitionedAt"].as_i64()
            && now - transitioned_at > two_hours_ms
        {
            let _ = iii
                .trigger(TriggerRequest {
                    function_id: "lifecycle::transition".into(),
                    payload: json!({
                        "agentId": &agent_id,
                        "newState": "blocked",
                        "reason": "Inactive for 2+ hours"
                    }),
                    action: None,
                    timeout_ms: None,
                })
                .await;
            transitioned += 1;
        }
    }

    Ok(json!({ "checked": valid_agents.len(), "transitioned": transitioned }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());

    let iii_ref = iii.clone();
    iii.register_function(
        "lifecycle::transition",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { transition(&iii, input).await }
        })
        .description("Move session to new state, validate transition, fire reactions"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "lifecycle::get_state",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { get_state(&iii, input).await }
        })
        .description("Get current lifecycle state for a session"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "lifecycle::add_reaction",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { add_reaction(&iii, input).await }
        })
        .description("Register a declarative reaction rule for state transitions"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "lifecycle::list_reactions",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { list_reactions(&iii, input).await }
        })
        .description("List configured reaction rules"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "lifecycle::check_all",
        RegisterFunction::new_async(move |_input: Value| {
            let iii = iii_ref.clone();
            async move { check_all(&iii).await }
        })
        .description("Scan all sessions, detect state changes, auto-transition"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "lifecycle::transition",
        json!({ "http_method": "POST", "api_path": "api/lifecycle/transition" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "lifecycle::get_state",
        json!({ "http_method": "GET", "api_path": "api/lifecycle/state/:agentId" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "lifecycle::add_reaction",
        json!({ "http_method": "POST", "api_path": "api/lifecycle/reactions" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "lifecycle::list_reactions",
        json!({ "http_method": "GET", "api_path": "api/lifecycle/reactions" }),
        None,
    )?;
    agentos_http_adapter::register_cron_trigger(&iii, "lifecycle::check_all", "*/2 * * * *")?;

    tracing::info!("session-lifecycle worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored_reaction() -> Value {
        json!({
            "id": "rxn_1",
            "from": "working",
            "to": "blocked",
            "action": "notify",
            "payload": { "message": "stuck" },
            "escalateAfter": 3,
            "attempts": 1,
        })
    }

    // --- state::list protocol (verified against iii 0.22.1) ---

    #[test]
    fn a_reaction_is_decoded_from_a_bare_list_entry() {
        let reaction = reaction_from_entry(&stored_reaction()).expect("bare entry must decode");
        assert_eq!(reaction.id, "rxn_1");
        assert_eq!(reaction.attempts, 1);
        assert_eq!(reaction.escalate_after, 3);
    }

    #[test]
    fn the_envelope_this_worker_used_to_expect_is_never_sent() {
        // The old reader took `entry["value"]`; state::list has no such field,
        // so every reaction was skipped and no reaction ever fired.
        let entry = stored_reaction();
        assert!(entry.get("value").is_none());
        assert!(reaction_from_entry(&entry["value"]).is_none());
    }

    // --- state::update protocol (verified against iii 0.22.1) ---

    #[test]
    fn transition_history_payload_uses_ops_not_operations() {
        let payload = transition_history_payload("lifecycle:a1", &json!({ "state": "working" }));
        assert!(
            payload.get("operations").is_none(),
            "`operations` fails the whole invocation with `missing field ops`"
        );
        assert_eq!(payload["scope"], "lifecycle:a1");
        assert_eq!(payload["key"], "history");
    }

    #[test]
    fn transition_history_appends_the_element_instead_of_merging_a_list() {
        let entry = json!({ "state": "working", "previousState": "spawning" });
        let payload = transition_history_payload("lifecycle:a1", &entry);
        let op = &payload["ops"][0];
        assert_eq!(
            op["type"], "append",
            "merge answers merge.value.not_an_object for a list value"
        );
        assert_eq!(op["path"], "transitions");
        assert_eq!(op["value"], entry, "append carries the element, not a list");
    }

    #[test]
    fn reaction_attempt_payload_carries_the_step_in_by() {
        let payload = reaction_attempt_payload("lifecycle_reactions", "rxn_1", 1_700_000_000_000);
        assert!(payload.get("operations").is_none());
        let increment = &payload["ops"][0];
        assert_eq!(increment["type"], "increment");
        assert_eq!(increment["path"], "attempts");
        assert_eq!(increment["by"], json!(1));
        assert!(
            increment.get("value").is_none(),
            "`value` fails the whole invocation with `missing field by`"
        );
        let set = &payload["ops"][1];
        assert_eq!(set["type"], "set");
        assert_eq!(set["path"], "lastFiredAt");
        assert_eq!(set["value"], json!(1_700_000_000_000i64));
    }

    #[test]
    fn reactions_are_listed_from_the_bare_values() {
        // `lifecycle::list_reactions` used to read `entry["value"]` and so
        // returned an empty array for every scope.
        let entries = vec![stored_reaction(), Value::Null];
        let values = stored_values(entries);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["id"], "rxn_1");
    }

    #[test]
    fn agent_ids_come_from_the_document_because_the_key_is_not_returned() {
        // `check_all` used to read `entry["key"]`, which state::list never
        // sends, so it never found a single agent to check.
        let agents = vec![
            json!({ "id": "agent-1", "name": "One" }),
            json!({ "name": "no id" }),
            json!({ "id": "" }),
            Value::Null,
            json!({ "id": "agent-2" }),
        ];
        assert_eq!(
            agent_ids_from_entries(&agents),
            vec!["agent-1".to_string(), "agent-2".to_string()]
        );
        assert!(agent_ids_from_entries(&[json!({ "key": "agent-1" })]).is_empty());
    }

    #[test]
    fn a_rejected_operation_is_detected_inside_a_successful_response() {
        let engine_result = json!({
            "errors": [{ "code": "merge.value.not_an_object", "op_index": 0 }],
            "new_value": { "transitions": [] },
            "old_value": { "transitions": [] },
        });
        assert!(
            update_rejection(&engine_result)
                .expect("rejection must be reported")
                .contains("merge.value.not_an_object")
        );
        assert_eq!(
            update_rejection(&json!({ "new_value": { "transitions": [] } })),
            None
        );
        assert_eq!(update_rejection(&json!({ "errors": [] })), None);
    }
}
