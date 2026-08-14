use iii_sdk::errors::Error;
use iii_sdk::{
    IIIClient, InitOptions, RegisterFunction, protocol::TriggerRequest, register_worker,
};
use serde_json::{Value, json};

mod types;

use types::{
    CheckoutRequest, Comment, CommentRequest, CreateMissionRequest, ListMissionsRequest, Mission,
    MissionPriority, MissionStatus, TransitionRequest,
};

fn scope(realm_id: &str) -> String {
    format!("realm:{realm_id}:missions")
}

fn comments_scope(realm_id: &str, mission_id: &str) -> String {
    format!("realm:{realm_id}:missions:{mission_id}:comments")
}

async fn create_mission(iii: &IIIClient, req: CreateMissionRequest) -> Result<Value, Error> {
    let id = format!("msn-{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now().to_rfc3339();

    let mission = Mission {
        id: id.clone(),
        realm_id: req.realm_id.clone(),
        directive_id: req.directive_id,
        parent_id: req.parent_id,
        title: req.title,
        description: req.description,
        status: MissionStatus::Backlog,
        priority: req.priority.unwrap_or(MissionPriority::Normal),
        assignee_id: None,
        created_by: req.created_by,
        billing_code: req.billing_code,
        version: 1,
        started_at: None,
        completed_at: None,
        created_at: now.clone(),
        updated_at: now,
    };

    let value = serde_json::to_value(&mission).map_err(|e| Error::Handler(e.to_string()))?;

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": scope(&req.realm_id),
            "key": id,
            "value": value,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    let _ = {
        let _iii = iii.clone();
        let _payload = json!({
            "topic": "mission.lifecycle",
            "data": { "type": "created", "missionId": mission.id, "realmId": mission.realm_id },
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

    Ok(serde_json::to_value(&mission).unwrap())
}

async fn load_mission(iii: &IIIClient, realm_id: &str, id: &str) -> Result<Mission, Error> {
    let val = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({
                "scope": scope(realm_id),
                "key": id,
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    serde_json::from_value(val).map_err(|e| Error::Handler(format!("mission {id} not found: {e}")))
}

async fn save_mission(iii: &IIIClient, mission: &Mission) -> Result<(), Error> {
    let value = serde_json::to_value(mission).map_err(|e| Error::Handler(e.to_string()))?;
    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": scope(&mission.realm_id),
            "key": mission.id,
            "value": value,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;
    Ok(())
}

async fn checkout_mission(iii: &IIIClient, req: CheckoutRequest) -> Result<Value, Error> {
    let realm_id = &req.realm_id;
    let mut mission = load_mission(iii, realm_id, &req.id).await?;

    if mission.assignee_id.is_some() && mission.assignee_id.as_deref() != Some(&req.agent_id) {
        return Err(Error::Handler(format!(
            "mission {} already checked out by {}",
            req.id,
            mission.assignee_id.as_deref().unwrap_or("unknown")
        )));
    }

    if !matches!(
        mission.status,
        MissionStatus::Backlog | MissionStatus::Queued | MissionStatus::Blocked
    ) {
        return Err(Error::Handler(format!(
            "cannot checkout mission in {:?} status",
            mission.status
        )));
    }

    let expected_version = mission.version;

    mission.assignee_id = Some(req.agent_id.clone());
    mission.status = MissionStatus::Active;
    mission.started_at = Some(chrono::Utc::now().to_rfc3339());
    mission.version = expected_version + 1;
    mission.updated_at = chrono::Utc::now().to_rfc3339();

    save_mission(iii, &mission).await?;

    let _ = {
        let _iii = iii.clone();
        let _payload = json!({
            "topic": "mission.lifecycle",
            "data": {
                "type": "checked_out",
                "missionId": mission.id,
                "agentId": req.agent_id,
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

    Ok(serde_json::to_value(&mission).unwrap())
}

async fn release_mission(
    iii: &IIIClient,
    realm_id: &str,
    id: &str,
    agent_id: &str,
) -> Result<Value, Error> {
    let mut mission = load_mission(iii, realm_id, id).await?;

    if mission.assignee_id.as_deref() != Some(agent_id) {
        return Err(Error::Handler(
            "only the assignee can release a mission".into(),
        ));
    }

    mission.assignee_id = None;
    mission.status = MissionStatus::Queued;
    mission.version += 1;
    mission.updated_at = chrono::Utc::now().to_rfc3339();

    save_mission(iii, &mission).await?;

    Ok(json!({ "released": true, "missionId": id }))
}

async fn transition_mission(iii: &IIIClient, req: TransitionRequest) -> Result<Value, Error> {
    let realm_id = &req.realm_id;
    let mut mission = load_mission(iii, realm_id, &req.id).await?;

    if !mission.status.can_transition_to(&req.status) {
        return Err(Error::Handler(format!(
            "invalid transition: {:?} -> {:?}",
            mission.status, req.status
        )));
    }

    let prev_status = mission.status;
    mission.status = req.status;
    mission.version += 1;
    mission.updated_at = chrono::Utc::now().to_rfc3339();

    if matches!(
        req.status,
        MissionStatus::Complete | MissionStatus::Cancelled
    ) {
        mission.completed_at = Some(chrono::Utc::now().to_rfc3339());
    }

    save_mission(iii, &mission).await?;

    let _ = {
        let _iii = iii.clone();
        let _payload = json!({
            "topic": "mission.lifecycle",
            "data": {
                "type": "transitioned",
                "missionId": mission.id,
                "from": format!("{prev_status:?}").to_lowercase(),
                "to": format!("{:?}", req.status).to_lowercase(),
                "agentId": req.agent_id,
                "reason": req.reason,
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

    Ok(serde_json::to_value(&mission).unwrap())
}

async fn add_comment(iii: &IIIClient, req: CommentRequest) -> Result<Value, Error> {
    let realm_id = &req.realm_id;
    let id = format!("cmt-{}", uuid::Uuid::new_v4());

    let comment = Comment {
        id: id.clone(),
        mission_id: req.mission_id.clone(),
        author_id: req.author_id,
        body: req.body,
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    let value = serde_json::to_value(&comment).map_err(|e| Error::Handler(e.to_string()))?;

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": comments_scope(&realm_id, &req.mission_id),
            "key": id,
            "value": value,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    Ok(serde_json::to_value(&comment).unwrap())
}

async fn list_comments(iii: &IIIClient, realm_id: &str, mission_id: &str) -> Result<Value, Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::list".to_string(),
        payload: json!({
            "scope": comments_scope(realm_id, mission_id),
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))
}

async fn list_missions(iii: &IIIClient, req: ListMissionsRequest) -> Result<Value, Error> {
    let all = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": scope(&req.realm_id) }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let missions: Vec<Mission> = all
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value::<Mission>(v.clone()).ok())
                .filter(|m| {
                    let status_ok = req.status.as_ref().map_or(true, |s| &m.status == s);
                    let assignee_ok = req
                        .assignee_id
                        .as_ref()
                        .map_or(true, |a| m.assignee_id.as_ref() == Some(a));
                    let dir_ok = req
                        .directive_id
                        .as_ref()
                        .map_or(true, |d| m.directive_id.as_ref() == Some(d));
                    status_ok && assignee_ok && dir_ok
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(json!({
        "missions": missions,
        "count": missions.len(),
    }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, InitOptions::default());

    let iii_clone = iii.clone();
    iii.register_function(
        "mission::create",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: CreateMissionRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                create_mission(&iii, req).await
            }
        })
        .description("Create a new mission"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "mission::checkout",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: CheckoutRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                checkout_mission(&iii, req).await
            }
        })
        .description("Atomically claim a mission for an agent"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "mission::release",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let realm_id = input["realmId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing realmId".into()))?;
                let id = input["id"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing id".into()))?;
                let agent_id = input["agentId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing agentId".into()))?;
                release_mission(&iii, realm_id, id, agent_id).await
            }
        })
        .description("Release a mission back to the queue"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "mission::transition",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: TransitionRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                transition_mission(&iii, req).await
            }
        })
        .description("Transition mission to a new status"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "mission::list",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: ListMissionsRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                list_missions(&iii, req).await
            }
        })
        .description("List missions with filtering"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "mission::comment",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: CommentRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                add_comment(&iii, req).await
            }
        })
        .description("Add a comment to a mission"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "mission::comments",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let realm_id = input["realmId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing realmId".into()))?;
                let mission_id = input["missionId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing missionId".into()))?;
                list_comments(&iii, realm_id, mission_id).await
            }
        })
        .description("List comments on a mission"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "mission::create".to_string(),
        json!({ "http_method": "POST", "api_path": "api/missions" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "mission::checkout".to_string(),
        json!({ "http_method": "POST", "api_path": "api/missions/:id/checkout" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "mission::release".to_string(),
        json!({ "http_method": "POST", "api_path": "api/missions/:id/release" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "mission::transition".to_string(),
        json!({ "http_method": "PATCH", "api_path": "api/missions/:id/status" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "mission::list".to_string(),
        json!({ "http_method": "GET", "api_path": "api/missions/:realmId" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "mission::comment".to_string(),
        json!({ "http_method": "POST", "api_path": "api/missions/:id/comments" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "mission::comments".to_string(),
        json!({ "http_method": "GET", "api_path": "api/missions/:realmId/:missionId/comments" }),
        None,
    )?;

    tracing::info!("mission worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}
