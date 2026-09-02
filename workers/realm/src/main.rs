use iii_sdk::errors::Error;
use iii_sdk::{IIIClient, RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};

mod types;

use types::{
    CreateRealmRequest, ExportRequest, ImportRequest, Realm, RealmStatus, UpdateRealmRequest,
};

async fn create_realm(iii: &IIIClient, req: CreateRealmRequest) -> Result<Value, Error> {
    let id = format!("realm-{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now().to_rfc3339();

    let realm = Realm {
        id: id.clone(),
        name: req.name,
        description: req.description,
        status: RealmStatus::Active,
        owner: req.owner,
        default_model: req.default_model,
        max_agents: req.max_agents,
        metadata: req.metadata,
        created_at: now.clone(),
        updated_at: now,
    };

    let value = serde_json::to_value(&realm).map_err(|e| Error::Handler(e.to_string()))?;

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": "realms",
            "key": id,
            "value": value,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    {
        let _iii = iii.clone();
        let _payload = json!({
            "topic": "realm.lifecycle",
            "data": { "type": "created", "realmId": realm.id },
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

    Ok(serde_json::to_value(&realm).unwrap())
}

async fn get_realm(iii: &IIIClient, id: &str) -> Result<Value, Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::get".to_string(),
        payload: json!({
            "scope": "realms",
            "key": id,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))
}

async fn list_realms(iii: &IIIClient) -> Result<Value, Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::list".to_string(),
        payload: json!({ "scope": "realms" }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))
}

async fn update_realm(iii: &IIIClient, req: UpdateRealmRequest) -> Result<Value, Error> {
    let existing = get_realm(iii, &req.id).await?;
    let mut realm: Realm =
        serde_json::from_value(existing).map_err(|e| Error::Handler(e.to_string()))?;

    if let Some(name) = req.name {
        realm.name = name;
    }
    if let Some(desc) = req.description {
        realm.description = Some(desc);
    }
    if let Some(status) = req.status {
        realm.status = status;
    }
    if let Some(model) = req.default_model {
        realm.default_model = Some(model);
    }
    if let Some(max) = req.max_agents {
        realm.max_agents = Some(max);
    }
    if let Some(meta) = req.metadata {
        realm.metadata = Some(meta);
    }
    realm.updated_at = chrono::Utc::now().to_rfc3339();

    let value = serde_json::to_value(&realm).map_err(|e| Error::Handler(e.to_string()))?;

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": "realms",
            "key": realm.id,
            "value": value,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    {
        let _iii = iii.clone();
        let _payload = json!({
            "topic": "realm.lifecycle",
            "data": { "type": "updated", "realmId": realm.id },
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

    Ok(serde_json::to_value(&realm).unwrap())
}

async fn delete_realm(iii: &IIIClient, id: &str) -> Result<Value, Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::delete".to_string(),
        payload: json!({
            "scope": "realms",
            "key": id,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    {
        let _iii = iii.clone();
        let _payload = json!({
            "topic": "realm.lifecycle",
            "data": { "type": "deleted", "realmId": id },
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

    Ok(json!({ "deleted": true }))
}

async fn export_realm(iii: &IIIClient, req: ExportRequest) -> Result<Value, Error> {
    let realm = get_realm(iii, &req.id).await?;
    let scrub = req.scrub_secrets.unwrap_or(true);

    let agents = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": format!("realm:{}:agents", req.id) }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!([]));

    let directives = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": format!("realm:{}:directives", req.id) }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!([]));

    let hierarchy = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": format!("realm:{}:hierarchy", req.id) }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!([]));

    let mut export = json!({
        "version": "1.0",
        "realm": realm,
        "agents": agents,
        "directives": directives,
        "hierarchy": hierarchy,
    });

    if scrub && let Some(obj) = export.as_object_mut() {
        obj.remove("secrets");
    }

    Ok(export)
}

async fn import_realm(iii: &IIIClient, req: ImportRequest) -> Result<Value, Error> {
    let version = req.data["version"].as_str().unwrap_or("1.0");
    if version != "1.0" {
        return Err(Error::Handler(format!(
            "unsupported export version: {version}"
        )));
    }

    let create_req = CreateRealmRequest {
        name: req.data["realm"]["name"]
            .as_str()
            .unwrap_or("Imported Realm")
            .to_string(),
        description: req.data["realm"]["description"].as_str().map(String::from),
        owner: req.new_owner.unwrap_or_else(|| {
            req.data["realm"]["owner"]
                .as_str()
                .unwrap_or("system")
                .to_string()
        }),
        default_model: req.data["realm"]["defaultModel"].as_str().map(String::from),
        max_agents: req.data["realm"]["maxAgents"].as_u64().map(|v| v as u32),
        metadata: None,
    };

    let realm = create_realm(iii, create_req).await?;
    let realm_id = realm["id"].as_str().unwrap_or("");

    if let Some(agents) = req.data["agents"].as_array() {
        for agent in agents {
            let mut agent = agent.clone();
            agent["realmId"] = json!(realm_id);
            let _ = iii
                .trigger(TriggerRequest {
                    function_id: "state::set".to_string(),
                    payload: json!({
                        "scope": format!("realm:{realm_id}:agents"),
                        "key": agent["id"].as_str().unwrap_or("unknown"),
                        "value": agent,
                    }),
                    action: None,
                    timeout_ms: None,
                })
                .await;
        }
    }

    Ok(json!({
        "imported": true,
        "realmId": realm_id,
    }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());

    let iii_clone = iii.clone();
    iii.register_function(
        "realm::create",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: CreateRealmRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                create_realm(&iii, req).await
            }
        })
        .description("Create a new isolated realm"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "realm::get",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let id = input["id"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing id".into()))?;
                get_realm(&iii, id).await
            }
        })
        .description("Get realm by ID"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "realm::list",
        RegisterFunction::new_async(move |_: Value| {
            let iii = iii_clone.clone();
            async move { list_realms(&iii).await }
        })
        .description("List all realms"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "realm::update",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: UpdateRealmRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                update_realm(&iii, req).await
            }
        })
        .description("Update realm configuration"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "realm::delete",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let id = input["id"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing id".into()))?;
                delete_realm(&iii, id).await
            }
        })
        .description("Delete a realm and all its data"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "realm::export",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: ExportRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                export_realm(&iii, req).await
            }
        })
        .description("Export realm as portable template"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "realm::import",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: ImportRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                import_realm(&iii, req).await
            }
        })
        .description("Import realm from template"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "realm::create".to_string(),
        json!({ "http_method": "POST", "api_path": "api/realms" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "realm::list".to_string(),
        json!({ "http_method": "GET", "api_path": "api/realms" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "realm::get".to_string(),
        json!({ "http_method": "GET", "api_path": "api/realms/:id" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "realm::update".to_string(),
        json!({ "http_method": "PATCH", "api_path": "api/realms/:id" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "realm::delete".to_string(),
        json!({ "http_method": "DELETE", "api_path": "api/realms/:id" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "realm::export".to_string(),
        json!({ "http_method": "POST", "api_path": "api/realms/:id/export" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "realm::import".to_string(),
        json!({ "http_method": "POST", "api_path": "api/realms/import" }),
        None,
    )?;

    tracing::info!("realm worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}
