use iii_sdk::errors::Error;
use iii_sdk::{
    IIIClient, InitOptions, RegisterFunction, protocol::TriggerRequest, register_worker,
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};
use tokio::task::JoinSet;

mod types;

use types::{
    BroadcastRequest, CollectRequest, ConsensusRequest, CreateSwarmRequest, DissolveRequest,
    MessageType, SwarmConfig, SwarmMessage, SwarmStatus, sanitize_id,
};

const DEFAULT_MAX_DURATION_MS: u64 = 600_000;
const DEFAULT_CONSENSUS_THRESHOLD: f64 = 0.66;
const MAX_AGENTS_PER_SWARM: usize = 20;
const MAX_MESSAGES_PER_SWARM: usize = 500;
const MAX_SWARM_DURATION_MS: u64 = 3_600_000;
const MIN_SWARM_DURATION_MS: u64 = 1_000;
const AGENT_ATTEMPTS: usize = 2;
const STATE_STARTUP_ATTEMPTS: usize = 5;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn fire_and_forget(iii: &IIIClient, function_id: &str, payload: Value) {
    let iii = iii.clone();
    let function_id = function_id.to_string();
    tokio::spawn(async move {
        let _ = iii
            .trigger(TriggerRequest {
                function_id,
                payload,
                action: None,
                timeout_ms: None,
            })
            .await;
    });
}

async fn state_get(iii: &IIIClient, scope: &str, key: &str) -> Result<Option<Value>, Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::get".to_string(),
        payload: json!({ "scope": scope, "key": key }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map(|value| (!value.is_null()).then_some(value))
    .map_err(|error| Error::Handler(error.to_string()))
}

async fn state_set(iii: &IIIClient, scope: &str, key: &str, value: Value) -> Result<(), Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({ "scope": scope, "key": key, "value": value }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map(|_| ())
    .map_err(|e| Error::Handler(e.to_string()))
}

async fn state_list(iii: &IIIClient, scope: &str) -> Result<Vec<Value>, Error> {
    let value = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": scope }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|error| Error::Handler(error.to_string()))?;
    value
        .as_array()
        .cloned()
        .ok_or_else(|| Error::Handler("state::list returned a non-array response".to_string()))
}

/// Decode one `state::list` entry.
///
/// The engine answers a bare array of the stored values themselves: there is
/// no `{key, value}` envelope. Unwrapping a `value` field would replace any
/// document that carries one with just that field.
fn decode_entry<T: serde::de::DeserializeOwned>(entry: &Value) -> Option<T> {
    serde_json::from_value::<T>(entry.clone()).ok()
}

/// Swarm messages from a bare `state::list` response, oldest first.
fn messages_from_list(raw: &[Value]) -> Vec<SwarmMessage> {
    let mut items: Vec<SwarmMessage> = raw.iter().filter_map(decode_entry).collect();
    items.sort_by_key(|m| m.timestamp);
    items
}

fn normalize_swarm_input(input: Value) -> Value {
    let mut body = input.get("body").cloned().unwrap_or_else(|| input.clone());
    let route_id = input
        .get("id")
        .or_else(|| body.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if body.get("swarmId").is_none()
        && let Some(id) = route_id
        && let Some(object) = body.as_object_mut()
    {
        object.insert("swarmId".to_string(), json!(id));
    }
    body
}

fn swarm_prompt(swarm: &SwarmConfig, agent_id: &str) -> String {
    format!(
        "You are member {agent_id} of swarm {}. The shared goal is:\n{}\n\nInvestigate independently. Return one concrete, actionable proposal with supporting evidence, assumptions, and risks. Do not claim evidence you did not observe.",
        swarm.id, swarm.goal
    )
}

async fn call_swarm_agent(
    iii: &IIIClient,
    swarm: &SwarmConfig,
    agent_id: &str,
    message: String,
    budget_ms: u64,
) -> Result<String, String> {
    let started = Instant::now();
    let mut last_error = "agent invocation failed".to_string();
    for attempt in 0..AGENT_ATTEMPTS {
        let elapsed = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let remaining = budget_ms.saturating_sub(elapsed);
        if remaining == 0 {
            break;
        }
        let attempts_left = (AGENT_ATTEMPTS - attempt) as u64;
        let timeout_ms = (remaining / attempts_left).max(1);
        let invocation = iii.trigger(TriggerRequest {
            function_id: "agent::chat".to_string(),
            payload: json!({
                "agentId": agent_id,
                "message": message,
                "sessionId": format!("swarm:{}:{agent_id}", swarm.id),
            }),
            action: None,
            timeout_ms: Some(timeout_ms),
        });
        match tokio::time::timeout(Duration::from_millis(timeout_ms), invocation).await {
            Ok(Ok(response)) => {
                let content = response
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim();
                if !content.is_empty() {
                    return Ok(content.to_string());
                }
                last_error = "agent returned an empty response".to_string();
            }
            Ok(Err(error)) => last_error = error.to_string(),
            Err(_) => last_error = format!("agent invocation timed out after {timeout_ms}ms"),
        }
        if attempt + 1 < AGENT_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    Err(last_error)
}

async fn finish_swarm(
    iii: &IIIClient,
    swarm_id: &str,
    completed: usize,
    errors: &[String],
    synthesis: Option<&str>,
) -> Result<(), Error> {
    let Some(value) = state_get(iii, "swarms", swarm_id).await? else {
        return Ok(());
    };
    let mut swarm: SwarmConfig =
        serde_json::from_value(value).map_err(|error| Error::Handler(error.to_string()))?;
    if swarm.status != SwarmStatus::Active {
        return Ok(());
    }

    let completed_at = now_ms();
    let failed_agents = swarm.agent_ids.len().saturating_sub(completed);
    swarm.status = if errors.is_empty() {
        SwarmStatus::Completed
    } else {
        SwarmStatus::Failed
    };
    swarm.completed_at = Some(completed_at);
    state_set(
        iii,
        "swarms",
        swarm_id,
        serde_json::to_value(&swarm).map_err(|error| Error::Handler(error.to_string()))?,
    )
    .await?;
    state_set(
        iii,
        "swarm_runs",
        swarm_id,
        json!({
            "status": swarm.status,
            "completedAgents": completed,
            "failedAgents": failed_agents,
            "errorCount": errors.len(),
            "errors": errors,
            "synthesis": synthesis,
            "completedAt": completed_at,
        }),
    )
    .await?;
    fire_and_forget(
        iii,
        "publish",
        json!({
            "topic": format!("swarm:{swarm_id}"),
            "data": {
                "type": "swarm_completed",
                "swarmId": swarm_id,
                "status": swarm.status,
                "completedAgents": completed,
                "failedAgents": failed_agents,
                "errorCount": errors.len(),
            },
        }),
    );
    Ok(())
}

async fn run_swarm(iii: IIIClient, swarm: SwarmConfig) -> Result<(), Error> {
    let started = Instant::now();
    let initial_budget = (swarm.max_duration_ms.saturating_mul(2) / 3)
        .max(MIN_SWARM_DURATION_MS)
        .min(swarm.max_duration_ms);
    let mut tasks = JoinSet::new();
    for agent_id in &swarm.agent_ids {
        let iii = iii.clone();
        let swarm = swarm.clone();
        let agent_id = agent_id.clone();
        tasks.spawn(async move {
            let message = swarm_prompt(&swarm, &agent_id);
            let result = call_swarm_agent(&iii, &swarm, &agent_id, message, initial_budget).await;
            (agent_id, result)
        });
    }

    let mut proposals = Vec::new();
    let mut errors = Vec::new();
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok((agent_id, Ok(content))) => {
                broadcast(
                    &iii,
                    BroadcastRequest {
                        swarm_id: swarm.id.clone(),
                        agent_id: agent_id.clone(),
                        message: content.clone(),
                        kind: MessageType::Proposal,
                        vote: None,
                    },
                )
                .await?;
                proposals.push((agent_id, content));
            }
            Ok((agent_id, Err(error))) => {
                let detail = format!("{agent_id}: {error}");
                errors.push(detail.clone());
                broadcast(
                    &iii,
                    BroadcastRequest {
                        swarm_id: swarm.id.clone(),
                        agent_id,
                        message: detail,
                        kind: MessageType::Error,
                        vote: None,
                    },
                )
                .await?;
            }
            Err(error) => errors.push(format!("swarm task failed: {error}")),
        }
    }

    let mut synthesis = None;
    if proposals.len() > 1 {
        let elapsed = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let remaining = swarm.max_duration_ms.saturating_sub(elapsed);
        if remaining > 0 {
            let evidence = proposals
                .iter()
                .map(|(agent, proposal)| {
                    let bounded = proposal.chars().take(4_000).collect::<String>();
                    format!("## {agent}\n{bounded}")
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            let coordinator = &proposals[0].0;
            let message = format!(
                "Synthesize the swarm findings below into one final proposal for this goal: {}\n\n{}\n\nResolve conflicts explicitly, retain evidence, and state remaining uncertainty.",
                swarm.goal, evidence
            );
            match call_swarm_agent(&iii, &swarm, coordinator, message, remaining).await {
                Ok(content) => {
                    broadcast(
                        &iii,
                        BroadcastRequest {
                            swarm_id: swarm.id.clone(),
                            agent_id: coordinator.clone(),
                            message: content.clone(),
                            kind: MessageType::Proposal,
                            vote: None,
                        },
                    )
                    .await?;
                    synthesis = Some(content);
                }
                Err(error) => errors.push(format!("synthesis: {error}")),
            }
        } else {
            errors.push("synthesis: swarm duration exhausted".to_string());
        }
    }

    finish_swarm(
        &iii,
        &swarm.id,
        proposals.len(),
        &errors,
        synthesis.as_deref(),
    )
    .await
}

async fn rehydrate_swarms(iii: &IIIClient) -> Result<(), Error> {
    let mut last_error = None;
    let mut entries = None;
    for attempt in 0..STATE_STARTUP_ATTEMPTS {
        match state_list(iii, "swarms").await {
            Ok(values) => {
                entries = Some(values);
                break;
            }
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < STATE_STARTUP_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    let entries = entries.ok_or_else(|| {
        last_error.unwrap_or_else(|| Error::Handler("swarm state unavailable".to_string()))
    })?;

    for entry in entries {
        let Some(mut swarm) = decode_entry::<SwarmConfig>(&entry) else {
            continue;
        };
        if swarm.status != SwarmStatus::Active {
            continue;
        }

        let elapsed_ms = now_ms()
            .saturating_sub(swarm.created_at)
            .try_into()
            .unwrap_or(u64::MAX);
        if elapsed_ms >= swarm.max_duration_ms {
            if let Err(error) = finish_swarm(
                iii,
                &swarm.id,
                0,
                &["swarm interrupted and exceeded its maximum duration".to_string()],
                None,
            )
            .await
            {
                tracing::error!(swarm_id = %swarm.id, error = %error, "failed to close expired swarm");
            }
            continue;
        }

        swarm.max_duration_ms -= elapsed_ms;
        let run_client = iii.clone();
        tokio::spawn(async move {
            if let Err(error) = run_swarm(run_client, swarm).await {
                tracing::error!(error = %error, "rehydrated swarm execution failed");
            }
        });
    }
    Ok(())
}

async fn create_swarm(iii: &IIIClient, req: CreateSwarmRequest) -> Result<Value, Error> {
    let goal = req
        .goal
        .map(|goal| goal.trim().to_string())
        .filter(|goal| !goal.is_empty())
        .ok_or_else(|| Error::Handler("goal and agentIds are required".into()))?;
    let agent_ids = req
        .agent_ids
        .filter(|ids| !ids.is_empty())
        .ok_or_else(|| Error::Handler("goal and agentIds are required".into()))?;

    if agent_ids.len() > MAX_AGENTS_PER_SWARM {
        return Err(Error::Handler(format!(
            "Maximum {MAX_AGENTS_PER_SWARM} agents per swarm"
        )));
    }

    let agent_ids: Vec<String> = agent_ids
        .into_iter()
        .map(|id| sanitize_id(&id))
        .collect::<Result<Vec<_>, _>>()
        .map_err(Error::Handler)?;
    let unique_agents = agent_ids.iter().collect::<HashSet<_>>();
    if unique_agents.len() != agent_ids.len() {
        return Err(Error::Handler("agentIds must be unique".to_string()));
    }
    let mut missing_agents = Vec::new();
    for agent_id in &agent_ids {
        if state_get(iii, "agents", agent_id).await?.is_none() {
            missing_agents.push(agent_id.clone());
        }
    }
    if !missing_agents.is_empty() {
        return Err(Error::Handler(format!(
            "unknown swarm agents: {}",
            missing_agents.join(", ")
        )));
    }

    let max_duration_ms = req.max_duration_ms.unwrap_or(DEFAULT_MAX_DURATION_MS);
    if !(MIN_SWARM_DURATION_MS..=MAX_SWARM_DURATION_MS).contains(&max_duration_ms) {
        return Err(Error::Handler(format!(
            "maxDurationMs must be between {MIN_SWARM_DURATION_MS} and {MAX_SWARM_DURATION_MS}"
        )));
    }
    let consensus_threshold = req
        .consensus_threshold
        .unwrap_or(DEFAULT_CONSENSUS_THRESHOLD);
    if !(0.0 < consensus_threshold && consensus_threshold <= 1.0) {
        return Err(Error::Handler(
            "consensusThreshold must be greater than 0 and at most 1".to_string(),
        ));
    }

    let swarm_id = uuid::Uuid::new_v4().to_string();
    let swarm = SwarmConfig {
        id: swarm_id.clone(),
        goal: goal.clone(),
        agent_ids: agent_ids.clone(),
        max_duration_ms,
        consensus_threshold,
        created_at: now_ms(),
        status: SwarmStatus::Active,
        completed_at: None,
        dissolved_at: None,
    };

    let value = serde_json::to_value(&swarm).map_err(|e| Error::Handler(e.to_string()))?;
    state_set(iii, "swarms", &swarm_id, value).await?;

    fire_and_forget(
        iii,
        "publish",
        json!({
            "topic": format!("swarm:{swarm_id}"),
            "data": {
                "type": "swarm_created",
                "swarmId": swarm_id,
                "goal": goal,
                "agents": agent_ids,
            }
        }),
    );

    fire_and_forget(
        iii,
        "security::audit",
        json!({
            "type": "swarm_created",
            "detail": { "swarmId": swarm_id, "goal": goal, "agentCount": agent_ids.len() },
        }),
    );

    let run_client = iii.clone();
    let run_config = swarm.clone();
    tokio::spawn(async move {
        if let Err(error) = run_swarm(run_client, run_config).await {
            tracing::error!(error = %error, "swarm execution failed");
        }
    });

    Ok::<Value, Error>(json!({
        "swarmId": swarm_id,
        "agents": agent_ids,
        "createdAt": swarm.created_at,
        "status": swarm.status,
    }))
}

async fn broadcast(iii: &IIIClient, req: BroadcastRequest) -> Result<Value, Error> {
    let safe_swarm_id = sanitize_id(&req.swarm_id).map_err(Error::Handler)?;
    let safe_agent_id = sanitize_id(&req.agent_id).map_err(Error::Handler)?;

    let swarm_val = state_get(iii, "swarms", &safe_swarm_id)
        .await?
        .ok_or_else(|| Error::Handler(format!("Swarm {safe_swarm_id} not found or not active")))?;
    let swarm: SwarmConfig =
        serde_json::from_value(swarm_val).map_err(|e| Error::Handler(e.to_string()))?;

    if swarm.status != SwarmStatus::Active {
        return Err(Error::Handler(format!(
            "Swarm {safe_swarm_id} not found or not active"
        )));
    }

    if !swarm.agent_ids.iter().any(|id| id == &safe_agent_id) {
        return Err(Error::Handler(format!(
            "Agent {safe_agent_id} is not a member of swarm {safe_swarm_id}"
        )));
    }

    let scope = format!("swarm_messages:{safe_swarm_id}");
    let existing = state_list(iii, &scope).await?;
    if existing.len() >= MAX_MESSAGES_PER_SWARM {
        return Err(Error::Handler(format!(
            "Swarm {safe_swarm_id} has reached the message limit"
        )));
    }

    let msg_id = uuid::Uuid::new_v4().to_string();
    let swarm_message = SwarmMessage {
        id: msg_id.clone(),
        swarm_id: safe_swarm_id.clone(),
        agent_id: safe_agent_id,
        message: req.message,
        kind: req.kind,
        vote: if req.kind == MessageType::Vote {
            req.vote
        } else {
            None
        },
        timestamp: now_ms(),
    };

    let value = serde_json::to_value(&swarm_message).map_err(|e| Error::Handler(e.to_string()))?;
    state_set(iii, &scope, &msg_id, value.clone()).await?;

    fire_and_forget(
        iii,
        "publish",
        json!({
            "topic": format!("swarm:{safe_swarm_id}"),
            "data": value,
        }),
    );

    Ok::<Value, Error>(json!({
        "messageId": msg_id,
        "swarmId": safe_swarm_id,
    }))
}

async fn collect(iii: &IIIClient, req: CollectRequest) -> Result<Value, Error> {
    let safe_swarm_id = sanitize_id(&req.swarm_id).map_err(Error::Handler)?;
    let swarm_value = state_get(iii, "swarms", &safe_swarm_id)
        .await?
        .ok_or_else(|| Error::Handler(format!("Swarm {safe_swarm_id} not found")))?;
    let swarm: SwarmConfig =
        serde_json::from_value(swarm_value).map_err(|error| Error::Handler(error.to_string()))?;
    let run = state_get(iii, "swarm_runs", &safe_swarm_id).await?;
    let scope = format!("swarm_messages:{safe_swarm_id}");
    let raw = state_list(iii, &scope).await?;

    let items = messages_from_list(&raw);

    let mut by_agent: std::collections::BTreeMap<String, Vec<&SwarmMessage>> =
        std::collections::BTreeMap::new();
    for msg in &items {
        by_agent.entry(msg.agent_id.clone()).or_default().push(msg);
    }

    let observations: Vec<&SwarmMessage> = items
        .iter()
        .filter(|m| m.kind == MessageType::Observation)
        .collect();
    let proposals: Vec<&SwarmMessage> = items
        .iter()
        .filter(|m| m.kind == MessageType::Proposal)
        .collect();
    let votes: Vec<&SwarmMessage> = items
        .iter()
        .filter(|m| m.kind == MessageType::Vote)
        .collect();
    let errors: Vec<&SwarmMessage> = items
        .iter()
        .filter(|message| message.kind == MessageType::Error)
        .collect();

    Ok::<Value, Error>(json!({
        "swarmId": safe_swarm_id,
        "status": swarm.status,
        "goal": swarm.goal,
        "agentIds": swarm.agent_ids,
        "createdAt": swarm.created_at,
        "completedAt": swarm.completed_at,
        "dissolvedAt": swarm.dissolved_at,
        "run": run,
        "totalMessages": items.len(),
        "agents": by_agent,
        "observations": observations,
        "proposals": proposals,
        "votes": votes,
        "errors": errors,
    }))
}

async fn consensus(iii: &IIIClient, req: ConsensusRequest) -> Result<Value, Error> {
    let safe_swarm_id = sanitize_id(&req.swarm_id).map_err(Error::Handler)?;

    let swarm_val = state_get(iii, "swarms", &safe_swarm_id)
        .await?
        .ok_or_else(|| Error::Handler(format!("Swarm {safe_swarm_id} not found")))?;
    let swarm: SwarmConfig =
        serde_json::from_value(swarm_val).map_err(|e| Error::Handler(e.to_string()))?;

    let scope = format!("swarm_messages:{safe_swarm_id}");
    let raw = state_list(iii, &scope).await?;

    let votes: Vec<SwarmMessage> = messages_from_list(&raw)
        .into_iter()
        .filter(|m| m.kind == MessageType::Vote && m.message == req.proposal)
        .collect();

    let mut latest: std::collections::HashMap<String, SwarmMessage> =
        std::collections::HashMap::new();
    for v in votes {
        let entry = latest.get(&v.agent_id);
        if entry.is_none_or(|e| v.timestamp > e.timestamp) {
            latest.insert(v.agent_id.clone(), v);
        }
    }

    let mut votes_for = 0u32;
    let mut votes_against = 0u32;
    for v in latest.values() {
        match v.vote {
            Some(types::VoteValue::For) => votes_for += 1,
            Some(types::VoteValue::Against) => votes_against += 1,
            None => {}
        }
    }

    let total_voters = swarm.agent_ids.len();
    let ratio = if total_voters == 0 {
        0.0
    } else {
        votes_for as f64 / total_voters as f64
    };
    let has_consensus = ratio >= swarm.consensus_threshold;

    Ok::<Value, Error>(json!({
        "hasConsensus": has_consensus,
        "votesFor": votes_for,
        "votesAgainst": votes_against,
        "threshold": swarm.consensus_threshold,
        "agents": swarm.agent_ids,
        "totalVoters": total_voters,
    }))
}

async fn dissolve(iii: &IIIClient, req: DissolveRequest) -> Result<Value, Error> {
    let safe_swarm_id = sanitize_id(&req.swarm_id).map_err(Error::Handler)?;

    let swarm_val = state_get(iii, "swarms", &safe_swarm_id)
        .await?
        .ok_or_else(|| Error::Handler(format!("Swarm {safe_swarm_id} not found")))?;
    let mut swarm: SwarmConfig =
        serde_json::from_value(swarm_val).map_err(|e| Error::Handler(e.to_string()))?;

    let findings = collect(
        iii,
        CollectRequest {
            swarm_id: safe_swarm_id.clone(),
        },
    )
    .await?;
    let total_messages = findings
        .get("totalMessages")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    if let Some(agents_obj) = findings.get("agents").and_then(|v| v.as_object()) {
        for agent_id in &swarm.agent_ids {
            if let Some(agent_findings) = agents_obj.get(agent_id).and_then(|v| v.as_array())
                && !agent_findings.is_empty()
            {
                let preview: Vec<&Value> = agent_findings.iter().take(10).collect();
                let summary = serde_json::to_string(&preview).unwrap_or_else(|_| "[]".into());
                fire_and_forget(
                    iii,
                    "memory::store",
                    json!({
                        "agentId": agent_id,
                        "sessionId": format!("swarm:{safe_swarm_id}"),
                        "role": "system",
                        "content": format!("Swarm {safe_swarm_id} findings: {summary}"),
                    }),
                );
            }
        }
    }

    swarm.status = SwarmStatus::Dissolved;
    swarm.dissolved_at = Some(now_ms());
    let value = serde_json::to_value(&swarm).map_err(|e| Error::Handler(e.to_string()))?;
    state_set(iii, "swarms", &safe_swarm_id, value).await?;

    fire_and_forget(
        iii,
        "publish",
        json!({
            "topic": format!("swarm:{safe_swarm_id}"),
            "data": { "type": "swarm_dissolved", "swarmId": safe_swarm_id },
        }),
    );

    fire_and_forget(
        iii,
        "security::audit",
        json!({
            "type": "swarm_dissolved",
            "detail": { "swarmId": safe_swarm_id, "messageCount": total_messages },
        }),
    );

    Ok::<Value, Error>(json!({
        "dissolved": true,
        "swarmId": safe_swarm_id,
    }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, InitOptions::default());

    let iii_clone = iii.clone();
    iii.register_function(
        "swarm::create",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: CreateSwarmRequest = serde_json::from_value(normalize_swarm_input(input))
                    .map_err(|error| Error::Handler(error.to_string()))?;
                create_swarm(&iii, req).await
            }
        })
        .description("Create a new decentralized agent swarm"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "swarm::broadcast",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: BroadcastRequest = serde_json::from_value(normalize_swarm_input(input))
                    .map_err(|error| Error::Handler(error.to_string()))?;
                broadcast(&iii, req).await
            }
        })
        .description("Broadcast a message to all agents in a swarm"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "swarm::collect",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: CollectRequest = serde_json::from_value(normalize_swarm_input(input))
                    .map_err(|error| Error::Handler(error.to_string()))?;
                collect(&iii, req).await
            }
        })
        .description("Gather all findings from a swarm"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "swarm::consensus",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: ConsensusRequest = serde_json::from_value(normalize_swarm_input(input))
                    .map_err(|error| Error::Handler(error.to_string()))?;
                consensus(&iii, req).await
            }
        })
        .description("Check if a swarm has reached consensus on a proposal"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "swarm::dissolve",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: DissolveRequest = serde_json::from_value(normalize_swarm_input(input))
                    .map_err(|error| Error::Handler(error.to_string()))?;
                dissolve(&iii, req).await
            }
        })
        .description("Dissolve a swarm and archive its findings"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "swarm::create".to_string(),
        json!({ "api_path": "api/swarm/create", "http_method": "POST" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "swarm::broadcast".to_string(),
        json!({ "api_path": "api/swarm/:id/broadcast", "http_method": "POST" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "swarm::collect".to_string(),
        json!({ "api_path": "api/swarm/:id/status", "http_method": "GET" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "swarm::consensus".to_string(),
        json!({ "api_path": "api/swarm/:id/consensus", "http_method": "POST" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "swarm::dissolve".to_string(),
        json!({ "api_path": "api/swarm/:id/dissolve", "http_method": "POST" }),
        None,
    )?;

    rehydrate_swarms(&iii).await?;

    tracing::info!("swarm worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_http_route_id_into_body() {
        let input = json!({
            "id": "swarm-1",
            "body": { "agentId": "agent-1", "message": "finding", "type": "proposal" }
        });

        let body = normalize_swarm_input(input);

        assert_eq!(body["swarmId"], "swarm-1");
        assert_eq!(body["agentId"], "agent-1");
    }

    #[test]
    fn explicit_swarm_id_takes_precedence_over_route_id() {
        let input = json!({
            "id": "route-swarm",
            "body": { "swarmId": "explicit-swarm" }
        });

        assert_eq!(normalize_swarm_input(input)["swarmId"], "explicit-swarm");
    }

    #[test]
    fn swarm_prompt_binds_goal_and_member() {
        let swarm = SwarmConfig {
            id: "swarm-1".to_string(),
            goal: "find the root cause".to_string(),
            agent_ids: vec!["agent-1".to_string()],
            max_duration_ms: DEFAULT_MAX_DURATION_MS,
            consensus_threshold: DEFAULT_CONSENSUS_THRESHOLD,
            created_at: 1,
            status: SwarmStatus::Active,
            completed_at: None,
            dissolved_at: None,
        };

        let prompt = swarm_prompt(&swarm, "agent-1");

        assert!(prompt.contains("agent-1"));
        assert!(prompt.contains("find the root cause"));
    }

    // --- state::list protocol (verified against iii 0.22.1) ---

    fn stored_message(id: &str, timestamp: i64) -> Value {
        json!({
            "id": id,
            "swarmId": "s1",
            "agentId": "agent-1",
            "message": "observation",
            "type": "observation",
            "timestamp": timestamp,
        })
    }

    #[test]
    fn messages_are_decoded_from_a_bare_list_oldest_first() {
        let raw = vec![stored_message("m2", 20), stored_message("m1", 10)];
        let items = messages_from_list(&raw);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "m1");
        assert_eq!(items[1].id, "m2");
    }

    #[test]
    fn the_envelope_this_worker_used_to_expect_is_never_sent() {
        // The old reader took `entry["value"]` when present. state::list has
        // no such field, so an enveloped fixture decodes to nothing.
        let enveloped = vec![json!({ "key": "m1", "value": stored_message("m1", 10) })];
        assert!(messages_from_list(&enveloped).is_empty());
    }

    #[test]
    fn deleted_entries_and_foreign_documents_are_skipped() {
        // `state::set value=null` leaves a null entry in the scope.
        let raw = vec![
            Value::Null,
            json!({ "unrelated": true }),
            stored_message("m1", 1),
        ];
        let items = messages_from_list(&raw);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "m1");
    }
}
