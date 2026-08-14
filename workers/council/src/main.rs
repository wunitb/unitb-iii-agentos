use iii_sdk::errors::Error;
use iii_sdk::{
    IIIClient, InitOptions, RegisterFunction,
    protocol::{RegisterTriggerInput, TriggerRequest},
    register_worker,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

mod types;

use types::{
    ActivityEntry, ActorKind, DecideProposalRequest, LogActivityRequest, OverrideRequest, Proposal,
    ProposalStatus, SubmitProposalRequest,
};

fn proposals_scope(realm_id: &str) -> String {
    format!("realm:{realm_id}:proposals")
}

fn activity_scope(realm_id: &str) -> String {
    format!("realm:{realm_id}:activity")
}

async fn get_prev_hash(iii: &IIIClient, realm_id: &str) -> String {
    let entries = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": activity_scope(realm_id) }),
            action: None,
            timeout_ms: None,
        })
        .await
        .ok();

    entries
        .and_then(|v| v.as_array().cloned())
        .map(|mut arr| {
            arr.sort_by(|a, b| {
                let ta = a["timestamp"].as_str().unwrap_or("");
                let tb = b["timestamp"].as_str().unwrap_or("");
                ta.cmp(tb)
            });
            arr
        })
        .and_then(|arr| arr.last().cloned())
        .and_then(|entry| entry["hash"].as_str().map(String::from))
        .unwrap_or_else(|| "0".repeat(64))
}

fn compute_hash(prev_hash: &str, action: &str, entity_id: &str, timestamp: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev_hash.as_bytes());
    hasher.update(action.as_bytes());
    hasher.update(entity_id.as_bytes());
    hasher.update(timestamp.as_bytes());
    hex::encode(hasher.finalize())
}

async fn log_activity(iii: &IIIClient, req: LogActivityRequest) -> Result<Value, Error> {
    let now = chrono::Utc::now().to_rfc3339();
    let prev_hash = get_prev_hash(iii, &req.realm_id).await;
    let hash = compute_hash(&prev_hash, &req.action, &req.entity_id, &now);
    let id = format!("act-{}", uuid::Uuid::new_v4());

    let entry = ActivityEntry {
        id: id.clone(),
        realm_id: req.realm_id.clone(),
        actor_kind: req.actor_kind,
        actor_id: req.actor_id,
        action: req.action,
        entity_type: req.entity_type,
        entity_id: req.entity_id,
        details: req.details,
        hash,
        prev_hash,
        timestamp: now,
    };

    let value = serde_json::to_value(&entry).map_err(|e| Error::Handler(e.to_string()))?;

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": activity_scope(&req.realm_id),
            "key": id,
            "value": value,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    Ok(serde_json::to_value(&entry).unwrap())
}

async fn submit_proposal(iii: &IIIClient, req: SubmitProposalRequest) -> Result<Value, Error> {
    let id = format!("prop-{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now().to_rfc3339();

    let proposal = Proposal {
        id: id.clone(),
        realm_id: req.realm_id.clone(),
        kind: req.kind,
        status: ProposalStatus::Pending,
        title: req.title,
        payload: req.payload,
        requested_by: req.requested_by.clone(),
        decided_by: None,
        decision_note: None,
        decided_at: None,
        created_at: now,
    };

    let value = serde_json::to_value(&proposal).map_err(|e| Error::Handler(e.to_string()))?;

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": proposals_scope(&req.realm_id),
            "key": id,
            "value": value,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    let _ = log_activity(
        iii,
        LogActivityRequest {
            realm_id: req.realm_id.clone(),
            actor_kind: ActorKind::Agent,
            actor_id: req.requested_by,
            action: "proposal_submitted".into(),
            entity_type: "proposal".into(),
            entity_id: proposal.id.clone(),
            details: Some(json!({ "kind": proposal.kind, "title": proposal.title })),
        },
    )
    .await;

    let _ = {
        let _iii = iii.clone();
        let _payload = json!({
            "topic": "council.proposal",
            "data": { "type": "submitted", "proposalId": proposal.id, "realmId": req.realm_id },
        });
        tokio::spawn(async move {
            let _ = _iii
                .trigger(TriggerRequest {
                    function_id: "publish".to_string(),
                    payload: _payload,
                    action: None,
                    timeout_ms: None,
                })
                .await;
        });
    };

    Ok(serde_json::to_value(&proposal).unwrap())
}

async fn decide_proposal(iii: &IIIClient, req: DecideProposalRequest) -> Result<Value, Error> {
    let realm_id = &req.realm_id;

    let val = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({ "scope": proposals_scope(realm_id), "key": &req.id }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let mut proposal: Proposal = serde_json::from_value(val)
        .map_err(|e| Error::Handler(format!("proposal {} not found: {e}", req.id)))?;

    if proposal.status != ProposalStatus::Pending {
        return Err(Error::Handler(format!(
            "proposal {} already {:?}",
            req.id, proposal.status
        )));
    }

    proposal.status = if req.approved {
        ProposalStatus::Approved
    } else {
        ProposalStatus::Rejected
    };
    proposal.decided_by = Some(req.decided_by.clone());
    proposal.decision_note = req.note.clone();
    proposal.decided_at = Some(chrono::Utc::now().to_rfc3339());

    let value = serde_json::to_value(&proposal).map_err(|e| Error::Handler(e.to_string()))?;

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": proposals_scope(realm_id),
            "key": proposal.id,
            "value": value,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    let _ = log_activity(
        iii,
        LogActivityRequest {
            realm_id: realm_id.to_string(),
            actor_kind: ActorKind::Human,
            actor_id: req.decided_by,
            action: if req.approved {
                "proposal_approved"
            } else {
                "proposal_rejected"
            }
            .into(),
            entity_type: "proposal".into(),
            entity_id: proposal.id.clone(),
            details: req.note.map(|n| json!({ "note": n })),
        },
    )
    .await;

    let _ = {
        let _iii = iii.clone();
        let _payload = json!({
            "topic": "council.proposal",
            "data": {
                "type": if req.approved { "approved" } else { "rejected" },
                "proposalId": proposal.id,
                "realmId": realm_id,
            },
        });
        tokio::spawn(async move {
            let _ = _iii
                .trigger(TriggerRequest {
                    function_id: "publish".to_string(),
                    payload: _payload,
                    action: None,
                    timeout_ms: None,
                })
                .await;
        });
    };

    Ok(serde_json::to_value(&proposal).unwrap())
}

async fn list_proposals(
    iii: &IIIClient,
    realm_id: &str,
    status_filter: Option<ProposalStatus>,
) -> Result<Value, Error> {
    let all = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": proposals_scope(realm_id) }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let proposals: Vec<Proposal> = all
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value::<Proposal>(v.clone()).ok())
                .filter(|p| status_filter.map_or(true, |s| p.status == s))
                .collect()
        })
        .unwrap_or_default();

    Ok(json!({
        "proposals": proposals,
        "count": proposals.len(),
    }))
}

async fn override_agent(iii: &IIIClient, req: OverrideRequest) -> Result<Value, Error> {
    let new_status = match req.action.as_str() {
        "pause" => "paused",
        "resume" => "active",
        "terminate" => "terminated",
        other => return Err(Error::Handler(format!("unknown override action: {other}"))),
    };

    iii.trigger(TriggerRequest {
        function_id: "state::update".to_string(),
        payload: json!({
            "scope": "agents",
            "key": &req.target_agent_id,
            "path": "status",
            "value": new_status,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(format!("failed to update agent state: {e}")))?;

    let _ = log_activity(
        iii,
        LogActivityRequest {
            realm_id: req.realm_id.clone(),
            actor_kind: ActorKind::Human,
            actor_id: req.operator_id.clone(),
            action: format!("override_{}", req.action),
            entity_type: "agent".into(),
            entity_id: req.target_agent_id.clone(),
            details: Some(json!({ "reason": req.reason })),
        },
    )
    .await;

    let _ = {
        let _iii = iii.clone();
        let _payload = json!({
            "topic": "council.override",
            "data": {
                "action": req.action,
                "agentId": req.target_agent_id,
                "operatorId": req.operator_id,
                "realmId": req.realm_id,
            },
        });
        tokio::spawn(async move {
            let _ = _iii
                .trigger(TriggerRequest {
                    function_id: "publish".to_string(),
                    payload: _payload,
                    action: None,
                    timeout_ms: None,
                })
                .await;
        });
    };

    Ok(json!({
        "overridden": true,
        "action": req.action,
        "agentId": req.target_agent_id,
    }))
}

async fn get_activity_log(iii: &IIIClient, realm_id: &str, limit: usize) -> Result<Value, Error> {
    let all = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": activity_scope(realm_id) }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let mut entries: Vec<ActivityEntry> = all
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default();

    entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    entries.truncate(limit);

    Ok(json!({
        "entries": entries,
        "count": entries.len(),
    }))
}

async fn verify_activity_chain(iii: &IIIClient, realm_id: &str) -> Result<Value, Error> {
    let all = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": activity_scope(realm_id) }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let mut entries: Vec<ActivityEntry> = all
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default();

    entries.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

    let mut valid = true;
    let mut expected_prev = "0".repeat(64);

    for entry in &entries {
        if entry.prev_hash != expected_prev {
            valid = false;
            break;
        }
        let computed = compute_hash(
            &entry.prev_hash,
            &entry.action,
            &entry.entity_id,
            &entry.timestamp,
        );
        if computed != entry.hash {
            valid = false;
            break;
        }
        expected_prev = entry.hash.clone();
    }

    Ok(json!({
        "valid": valid,
        "entryCount": entries.len(),
    }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, InitOptions::default());

    let iii_clone = iii.clone();
    iii.register_function(
        "council::submit",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: SubmitProposalRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                submit_proposal(&iii, req).await
            }
        })
        .description("Submit a proposal for council review"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "council::decide",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: DecideProposalRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                decide_proposal(&iii, req).await
            }
        })
        .description("Approve or reject a proposal"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "council::proposals",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let realm_id = input["realmId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing realmId".into()))?;
                let status: Option<ProposalStatus> = input
                    .get("status")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                list_proposals(&iii, realm_id, status).await
            }
        })
        .description("List proposals for a realm"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "council::override",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: OverrideRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                override_agent(&iii, req).await
            }
        })
        .description("Override agent state (pause/resume/terminate)"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "council::activity",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: LogActivityRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                log_activity(&iii, req).await
            }
        })
        .description("Log an activity entry"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "council::activity_log",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let realm_id = input["realmId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing realmId".into()))?;
                let limit = input["limit"].as_u64().unwrap_or(50) as usize;
                get_activity_log(&iii, realm_id, limit).await
            }
        })
        .description("Get recent activity log entries"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "council::verify",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let realm_id = input["realmId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing realmId".into()))?;
                verify_activity_chain(&iii, realm_id).await
            }
        })
        .description("Verify integrity of activity chain"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "council::submit".to_string(),
        json!({ "http_method": "POST", "api_path": "api/council/proposals" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "council::decide".to_string(),
        json!({ "http_method": "POST", "api_path": "api/council/proposals/:id/decide" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "council::proposals".to_string(),
        json!({ "http_method": "GET", "api_path": "api/council/proposals/:realmId" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "council::override".to_string(),
        json!({ "http_method": "POST", "api_path": "api/council/override" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "council::activity_log".to_string(),
        json!({ "http_method": "GET", "api_path": "api/council/activity/:realmId" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "council::verify".to_string(),
        json!({ "http_method": "GET", "api_path": "api/council/verify/:realmId" }),
        None,
    )?;

    iii.register_trigger(RegisterTriggerInput {
        trigger_type: "subscribe".to_string(),
        function_id: "council::activity".to_string(),
        config: json!({ "topic": "council.audit" }),
        metadata: None,
    })?;

    tracing::info!("council worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}
