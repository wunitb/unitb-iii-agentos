use agentos_http_adapter::{CHAT_TIMEOUT_MS, TriggerBus, principal};
use iii_sdk::errors::Error;
use iii_sdk::{IIIClient, RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};

mod types;

use types::{ContextMode, InvokeRequest, PulseConfig, PulseRun, PulseStatus, RegisterPulseRequest};

fn config_scope(realm_id: &str) -> String {
    format!("realm:{realm_id}:pulse:config")
}

fn runs_scope(realm_id: &str) -> String {
    format!("realm:{realm_id}:pulse:runs")
}

fn payload_body(input: &Value) -> Value {
    input.get("body").cloned().unwrap_or_else(|| input.clone())
}

async fn authorize_named_agent(
    iii: &dyn TriggerBus,
    input: &Value,
    named: &str,
    expected_bearer: Option<&str>,
) -> Result<String, Error> {
    let caller = principal::resolve(input, expected_bearer)?;
    principal::acting_agent(iii, &caller, &json!({ "agentId": named }), named).await
}

fn expected_bearer() -> Option<String> {
    agentos_bus_auth::policy::expected_api_key()
}

async fn build_context(
    iii: &dyn TriggerBus,
    agent_id: &str,
    realm_id: &str,
    mode: &ContextMode,
) -> Value {
    match mode {
        ContextMode::Thin => {
            json!({
                "agentId": agent_id,
                "realmId": realm_id,
                "mode": "thin",
            })
        }
        ContextMode::Full => {
            let missions = iii
                .trigger(TriggerRequest {
                    function_id: "mission::list".to_string(),
                    payload: json!({
                        "realmId": realm_id,
                        "assigneeId": agent_id,
                        "status": "active",
                    }),
                    action: None,
                    timeout_ms: None,
                })
                .await
                .unwrap_or(json!({ "missions": [] }));

            let budget = iii
                .trigger(TriggerRequest {
                    function_id: "ledger::check".to_string(),
                    payload: json!({
                        "realmId": realm_id,
                        "agentId": agent_id,
                    }),
                    action: None,
                    timeout_ms: None,
                })
                .await
                .unwrap_or(json!({ "allowed": true }));

            let hierarchy = iii
                .trigger(TriggerRequest {
                    function_id: "hierarchy::chain".to_string(),
                    payload: json!({
                        "realmId": realm_id,
                        "agentId": agent_id,
                    }),
                    action: None,
                    timeout_ms: None,
                })
                .await
                .unwrap_or(json!({ "chain": [] }));

            let directives = iii
                .trigger(TriggerRequest {
                    function_id: "directive::list".to_string(),
                    payload: json!({
                        "realmId": realm_id,
                        "status": "active",
                    }),
                    action: None,
                    timeout_ms: None,
                })
                .await
                .unwrap_or(json!({ "directives": [] }));

            json!({
                "agentId": agent_id,
                "realmId": realm_id,
                "mode": "full",
                "missions": missions["missions"],
                "budget": budget,
                "chain": hierarchy["chain"],
                "directives": directives["directives"],
            })
        }
    }
}

async fn register_pulse(iii: &IIIClient, req: RegisterPulseRequest) -> Result<Value, Error> {
    let config = PulseConfig {
        agent_id: req.agent_id.clone(),
        realm_id: req.realm_id.clone(),
        cron: req.cron.clone(),
        enabled: true,
        context_mode: req.context_mode.unwrap_or(ContextMode::Thin),
        timeout_secs: req.timeout_secs,
        max_retries: req.max_retries,
    };

    let value = serde_json::to_value(&config).map_err(|e| Error::Handler(e.to_string()))?;

    agentos_http_adapter::register_cron_trigger_with_metadata(
        iii,
        "pulse::tick",
        &req.cron,
        Some(json!({
            "agentId": &req.agent_id,
            "realmId": &req.realm_id,
        })),
    )
    .map_err(|e| Error::Handler(format!("failed to register cron trigger: {e}")))?;

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": config_scope(&req.realm_id),
            "key": &req.agent_id,
            "value": value,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    Ok(serde_json::to_value(&config).unwrap())
}

async fn invoke_pulse(iii: &dyn TriggerBus, req: InvokeRequest) -> Result<Value, Error> {
    let realm_id = &req.realm_id;
    let mode = req.context_mode.unwrap_or(ContextMode::Thin);
    let context = build_context(iii, &req.agent_id, realm_id, &mode).await;

    let run_id = format!("run-{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now().to_rfc3339();

    let run = PulseRun {
        id: run_id.clone(),
        agent_id: req.agent_id.clone(),
        realm_id: realm_id.clone(),
        status: PulseStatus::Running,
        source: "manual".into(),
        context_snapshot: Some(context.clone()),
        started_at: now.clone(),
        finished_at: None,
        error: None,
    };

    let run_val = serde_json::to_value(&run).map_err(|e| Error::Handler(e.to_string()))?;
    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": runs_scope(realm_id),
            "key": &run_id,
            "value": run_val,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    let result = iii
        .trigger(TriggerRequest {
            function_id: "agent::chat".to_string(),
            payload: json!({
                "agentId": &req.agent_id,
                "principal": { "agentId": &req.agent_id },
                "message": "You have been invoked via pulse. Review your current context and take appropriate action.",
                "context": context,
            }),
            action: None,
            timeout_ms: Some(CHAT_TIMEOUT_MS),
        })
        .await;

    let (final_status, error) = match result {
        Ok(_) => (PulseStatus::Completed, None),
        Err(e) => (PulseStatus::Failed, Some(e.to_string())),
    };

    let finished_run = PulseRun {
        status: final_status,
        finished_at: Some(chrono::Utc::now().to_rfc3339()),
        error,
        ..run
    };

    let run_val = serde_json::to_value(&finished_run).map_err(|e| Error::Handler(e.to_string()))?;
    let _ = iii
        .trigger(TriggerRequest {
            function_id: "state::set".to_string(),
            payload: json!({
                "scope": runs_scope(realm_id),
                "key": &run_id,
                "value": run_val,
            }),
            action: None,
            timeout_ms: None,
        })
        .await;

    Ok(serde_json::to_value(&finished_run).unwrap())
}

async fn tick(iii: &dyn TriggerBus, input: Value) -> Result<Value, Error> {
    let agent_id = input["agentId"]
        .as_str()
        .ok_or_else(|| Error::Handler("missing agentId in tick".into()))?;
    let realm_id = input["realmId"]
        .as_str()
        .ok_or_else(|| Error::Handler("missing realmId in tick".into()))?;

    let config_val = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({
                "scope": config_scope(realm_id),
                "key": agent_id,
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let config: PulseConfig =
        serde_json::from_value(config_val).map_err(|e| Error::Handler(e.to_string()))?;

    if !config.enabled {
        return Ok(json!({ "skipped": true, "reason": "disabled" }));
    }

    let budget_check = iii
        .trigger(TriggerRequest {
            function_id: "ledger::check".to_string(),
            payload: json!({
                "realmId": realm_id,
                "agentId": agent_id,
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!({ "allowed": true }));

    if budget_check["allowed"] == false {
        return Ok(json!({ "skipped": true, "reason": "budget_exceeded" }));
    }

    invoke_pulse(
        iii,
        InvokeRequest {
            agent_id: agent_id.to_string(),
            realm_id: realm_id.to_string(),
            context_mode: Some(config.context_mode),
        },
    )
    .await
}

async fn tick_from_trigger(
    iii: &dyn TriggerBus,
    input: Value,
    metadata: Option<Value>,
) -> Result<Value, Error> {
    let metadata = metadata.ok_or_else(|| {
        Error::Handler("pulse::tick accepts only a registered cron trigger".to_string())
    })?;
    principal::refuse_agent_principal(&input, expected_bearer().as_deref(), "pulse::tick")?;
    // The registered metadata fixes agentId/realmId. The config row must still
    // exist and be enabled, but unauthenticated state mutation remains a known
    // residual until the bus policy protects the state mutation functions.
    tick(iii, metadata).await
}

async fn get_pulse_status(
    iii: &dyn TriggerBus,
    realm_id: &str,
    agent_id: &str,
) -> Result<Value, Error> {
    let config = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({
                "scope": config_scope(realm_id),
                "key": agent_id,
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .ok();

    let runs = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": runs_scope(realm_id) }),
            action: None,
            timeout_ms: None,
        })
        .await
        .ok();

    let recent_runs: Vec<Value> = runs
        .and_then(|v| v.as_array().cloned())
        .map(|arr| {
            arr.into_iter()
                .filter(|r| r["agentId"].as_str() == Some(agent_id))
                .rev()
                .take(10)
                .collect()
        })
        .unwrap_or_default();

    Ok(json!({
        "config": config,
        "recentRuns": recent_runs,
    }))
}

async fn toggle_pulse(
    iii: &dyn TriggerBus,
    realm_id: &str,
    agent_id: &str,
    enabled: bool,
) -> Result<Value, Error> {
    let config_val = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({
                "scope": config_scope(realm_id),
                "key": agent_id,
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let mut config: PulseConfig =
        serde_json::from_value(config_val).map_err(|e| Error::Handler(e.to_string()))?;

    config.enabled = enabled;

    let value = serde_json::to_value(&config).map_err(|e| Error::Handler(e.to_string()))?;

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": config_scope(realm_id),
            "key": agent_id,
            "value": value,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    Ok(json!({ "enabled": enabled, "agentId": agent_id }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());

    let iii_clone = iii.clone();
    iii.register_function(
        "pulse::register",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let mut req: RegisterPulseRequest = serde_json::from_value(payload_body(&input))
                    .map_err(|e| Error::Handler(e.to_string()))?;
                req.agent_id = authorize_named_agent(
                    &iii,
                    &input,
                    &req.agent_id,
                    expected_bearer().as_deref(),
                )
                .await?;
                register_pulse(&iii, req).await
            }
        })
        .description("Register scheduled pulse for an agent"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "pulse::invoke",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let mut req: InvokeRequest = serde_json::from_value(payload_body(&input))
                    .map_err(|e| Error::Handler(e.to_string()))?;
                req.agent_id = authorize_named_agent(
                    &iii,
                    &input,
                    &req.agent_id,
                    expected_bearer().as_deref(),
                )
                .await?;
                invoke_pulse(&iii, req).await
            }
        })
        .description("Manually invoke agent pulse"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "pulse::tick",
        RegisterFunction::new_async(move |input: Value, metadata: Option<Value>| {
            let iii = iii_clone.clone();
            async move { tick_from_trigger(&iii, input, metadata).await }
        })
        .description("Internal: cron-triggered pulse execution"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "pulse::status",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let body = payload_body(&input);
                let realm_id = body["realmId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing realmId".into()))?;
                let named = body["agentId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing agentId".into()))?;
                let agent_id =
                    authorize_named_agent(&iii, &input, named, expected_bearer().as_deref())
                        .await?;
                get_pulse_status(&iii, realm_id, &agent_id).await
            }
        })
        .description("Get pulse config and recent runs"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "pulse::toggle",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let body = payload_body(&input);
                let realm_id = body["realmId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing realmId".into()))?;
                let named = body["agentId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing agentId".into()))?;
                let enabled = body["enabled"].as_bool().unwrap_or(true);
                let agent_id =
                    authorize_named_agent(&iii, &input, named, expected_bearer().as_deref())
                        .await?;
                toggle_pulse(&iii, realm_id, &agent_id, enabled).await
            }
        })
        .description("Enable or disable agent pulse"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "pulse::register".to_string(),
        json!({ "http_method": "POST", "api_path": "api/pulse/register" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "pulse::invoke".to_string(),
        json!({ "http_method": "POST", "api_path": "api/pulse/invoke" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "pulse::status".to_string(),
        json!({ "http_method": "GET", "api_path": "api/pulse/:realmId/:agentId" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "pulse::toggle".to_string(),
        json!({ "http_method": "PATCH", "api_path": "api/pulse/:realmId/:agentId" }),
        None,
    )?;

    tracing::info!("pulse worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_http_adapter::{fake::FakeBus, principal};

    #[tokio::test]
    async fn tick_accepts_only_bare_registered_metadata_and_keeps_the_cron_seam() {
        let direct = FakeBus::new();
        assert!(
            tick_from_trigger(&direct, json!({ "agentId": "victim" }), None)
                .await
                .is_err()
        );
        assert!(direct.calls().is_empty());

        let labelled = FakeBus::new();
        assert!(
            tick_from_trigger(
                &labelled,
                json!({ "principal": principal::as_agent("caller") }),
                Some(json!({ "agentId": "caller", "realmId": "realm" })),
            )
            .await
            .is_err()
        );
        assert!(labelled.calls().is_empty());

        let cron = FakeBus::new();
        cron.on("state::get", |_| {
            Ok(json!({
                "agentId": "scheduled",
                "realmId": "realm",
                "cron": "0 * * * *",
                "enabled": false,
                "contextMode": "thin",
            }))
        });
        let result = tick_from_trigger(
            &cron,
            json!({}),
            Some(json!({ "agentId": "scheduled", "realmId": "realm" })),
        )
        .await
        .unwrap();
        assert_eq!(result, json!({ "skipped": true, "reason": "disabled" }));
        assert_eq!(cron.call_count("state::get"), 1);
        assert_eq!(cron.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn external_pulse_targets_are_bound_to_the_caller_before_side_effects() {
        let missing = FakeBus::new();
        assert!(
            authorize_named_agent(
                &missing,
                &json!({ "agentId": "victim" }),
                "victim",
                Some("key")
            )
            .await
            .is_err()
        );
        assert!(missing.calls().is_empty());

        let malformed = FakeBus::new();
        assert!(
            authorize_named_agent(
                &malformed,
                &json!({ "principal": {}, "agentId": "victim" }),
                "victim",
                Some("key")
            )
            .await
            .is_err()
        );
        assert!(malformed.calls().is_empty());

        let same = FakeBus::new();
        assert_eq!(
            authorize_named_agent(
                &same,
                &json!({ "principal": principal::as_agent("caller") }),
                "caller",
                None
            )
            .await
            .unwrap(),
            "caller"
        );
        assert!(same.calls().is_empty());

        let denied = FakeBus::new();
        assert!(
            authorize_named_agent(
                &denied,
                &json!({ "principal": principal::as_agent("caller") }),
                "victim",
                None
            )
            .await
            .is_err()
        );
        assert_eq!(denied.call_count("security::check_capability"), 1);
        assert_eq!(denied.call_count("state::set"), 0);
        assert_eq!(denied.call_count("agent::chat"), 0);

        let granted = FakeBus::new();
        granted.on("security::check_capability", |input| Ok(json!({
            "allowed": input["agentId"] == "caller" && input["resource"] == "grant::act_as::victim"
        })));
        assert_eq!(
            authorize_named_agent(
                &granted,
                &json!({ "principal": principal::as_agent("caller") }),
                "victim",
                None
            )
            .await
            .unwrap(),
            "victim"
        );

        let operator = FakeBus::new();
        assert_eq!(
            authorize_named_agent(
                &operator,
                &json!({ "headers": { "Authorization": "Bearer key" } }),
                "victim",
                Some("key")
            )
            .await
            .unwrap(),
            "victim"
        );

        let forged = principal::attach_agent(
            "pulse::invoke",
            json!({ "agentId": "victim", "principal": principal::as_agent("victim"), "headers": { "Authorization": "Bearer key" } }),
            "caller",
        );
        let forged_bus = FakeBus::new();
        assert!(
            authorize_named_agent(&forged_bus, &forged, "victim", Some("key"))
                .await
                .is_err()
        );
        assert_eq!(forged_bus.call_count("security::check_capability"), 1);
    }
}
