use agentos_http_adapter::{CHAT_TIMEOUT_MS, TriggerBus, principal};
use chrono::{DateTime, Timelike, Utc};
use cron::Schedule;
use iii_sdk::errors::Error;
use iii_sdk::{
    IIIClient, RegisterFunction, protocol::TriggerRequest, register_worker, trigger::Trigger,
};
use serde_json::{Value, json};
use std::{collections::HashMap, str::FromStr, sync::Arc};
use tokio::sync::Mutex;

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

/// A credential-less engine trigger is not proof of scheduler origin. The
/// table below is the authority for scheduled work in this process. Handles
/// and the boot token are deliberately treated as public replay material.
///
/// A claim is admitted only for the authorized schedule's most recent UTC due
/// slot and only within this five-second late-delivery window. Older due slots are not
/// caught up. A known pair can shift one run inside the window, but cannot
/// choose work, overlap a run, replay a claimed slot, or create an off-schedule
/// run. This remains bounded replay control, not scheduler-origin proof.
const SCHEDULER_JITTER_MILLIS: i64 = 5_000;

fn normalize_cron_expression(expression: &str) -> Result<String, Error> {
    let fields = expression.split_whitespace().collect::<Vec<_>>();
    match fields.len() {
        5 => Ok(format!("0 {}", fields.join(" "))),
        6 => Ok(fields.join(" ")),
        count => Err(Error::Handler(format!(
            "cron expression requires 5 or 6 fields, received {count}"
        ))),
    }
}

fn parse_authorized_schedule(expression: &str) -> Result<Schedule, Error> {
    let normalized = normalize_cron_expression(expression)?;
    Schedule::from_str(&normalized)
        .map_err(|error| Error::Handler(format!("invalid UTC cron expression: {error}")))
}

fn most_recent_due_slot(schedule: &Schedule, now: DateTime<Utc>) -> Option<i64> {
    let floor = now.with_nanosecond(0)?;
    let jitter = chrono::Duration::milliseconds(SCHEDULER_JITTER_MILLIS);
    for seconds_back in 0..=(SCHEDULER_JITTER_MILLIS / 1_000 + 1) {
        let candidate = floor - chrono::Duration::seconds(seconds_back);
        let lateness = now.signed_duration_since(candidate);
        if lateness > jitter {
            continue;
        }
        if schedule.includes(candidate) {
            return Some(candidate.timestamp());
        }
    }
    None
}

type ScheduledJobs = Arc<Mutex<HashMap<String, ScheduledJob>>>;

struct ScheduledJob {
    config: PulseConfig,
    schedule: Schedule,
    boot_token: String,
    in_flight: bool,
    last_claimed_slot: Option<i64>,
    trigger: Option<Trigger>,
}

fn tick_metadata(job_handle: &str, boot_token: &str) -> Value {
    json!({ "jobHandle": job_handle, "bootToken": boot_token })
}

fn metadata_handle_and_token(metadata: Option<&Value>) -> Result<(&str, &str), Error> {
    let object = metadata
        .and_then(Value::as_object)
        .ok_or_else(|| Error::Handler("pulse::tick requires an opaque job handle".into()))?;
    if object.len() != 2 {
        return Err(Error::Handler(
            "pulse::tick metadata must contain only jobHandle and bootToken".into(),
        ));
    }
    let handle = object
        .get("jobHandle")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Handler("pulse::tick requires an opaque job handle".into()))?;
    let token = object
        .get("bootToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Handler("pulse::tick requires a process boot token".into()))?;
    Ok((handle, token))
}

async fn claim_scheduled_job(
    jobs: &ScheduledJobs,
    handle: &str,
    token: &str,
    now: DateTime<Utc>,
) -> Result<Option<PulseConfig>, Error> {
    let mut jobs = jobs.lock().await;
    let job = jobs.get_mut(handle).ok_or_else(|| {
        Error::Handler("pulse::tick job handle is unknown in this process".into())
    })?;
    if job.boot_token != token {
        return Err(Error::Handler(
            "pulse::tick process boot token is stale".into(),
        ));
    }
    let due_slot = most_recent_due_slot(&job.schedule, now).ok_or_else(|| {
        Error::Handler("pulse::tick arrived outside the configured due-slot window".into())
    })?;
    if !job.config.enabled {
        return Ok(None);
    }
    if job
        .last_claimed_slot
        .is_some_and(|claimed| claimed >= due_slot)
    {
        return Err(Error::Handler(
            "pulse::tick due slot was already claimed".into(),
        ));
    }
    if job.in_flight {
        return Err(Error::Handler(
            "pulse::tick job is already in flight".into(),
        ));
    }
    job.in_flight = true;
    job.last_claimed_slot = Some(due_slot);
    Ok(Some(job.config.clone()))
}

async fn finish_scheduled_job(jobs: &ScheduledJobs, handle: &str) {
    if let Some(job) = jobs.lock().await.get_mut(handle) {
        job.in_flight = false;
    }
}

#[cfg(test)]
async fn install_test_job(jobs: &ScheduledJobs, handle: &str, token: &str, config: PulseConfig) {
    let schedule = parse_authorized_schedule(&config.cron).expect("test schedule");
    jobs.lock().await.insert(
        handle.to_string(),
        ScheduledJob {
            config,
            schedule,
            boot_token: token.to_string(),
            in_flight: false,
            last_claimed_slot: None,
            trigger: None,
        },
    );
}

#[cfg(test)]
async fn mark_test_job_in_flight(jobs: &ScheduledJobs, handle: &str) {
    jobs.lock()
        .await
        .get_mut(handle)
        .expect("test job")
        .in_flight = true;
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

async fn register_pulse(
    iii: &IIIClient,
    req: RegisterPulseRequest,
    jobs: &ScheduledJobs,
    boot_token: &str,
) -> Result<Value, Error> {
    let schedule = parse_authorized_schedule(&req.cron)?;
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

    let handle = uuid::Uuid::new_v4().to_string();
    let trigger = agentos_http_adapter::register_cron_trigger_with_metadata(
        iii,
        "pulse::tick",
        &req.cron,
        Some(tick_metadata(&handle, boot_token)),
    )
    .map_err(|e| Error::Handler(format!("failed to register cron trigger: {e}")))?;

    let mut table = jobs.lock().await;
    let stale = table
        .iter()
        .filter_map(|(key, job)| {
            (job.config.agent_id == config.agent_id && job.config.realm_id == config.realm_id)
                .then_some(key.clone())
        })
        .collect::<Vec<_>>();
    for key in stale {
        if let Some(old) = table.remove(&key)
            && let Some(trigger) = old.trigger
        {
            trigger.unregister();
        }
    }
    table.insert(
        handle,
        ScheduledJob {
            config: config.clone(),
            schedule,
            boot_token: boot_token.to_string(),
            in_flight: false,
            last_claimed_slot: None,
            trigger: Some(trigger),
        },
    );

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

async fn tick(iii: &dyn TriggerBus, config: PulseConfig) -> Result<Value, Error> {
    let agent_id = &config.agent_id;
    let realm_id = &config.realm_id;

    // Stored state can only disable an in-memory authorized job. It is never
    // allowed to name the target or enable work the table did not authorize.
    let stored = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({
                "scope": config_scope(realm_id),
                "key": agent_id,
            }),
            action: None,
            timeout_ms: None,
        })
        .await;
    if !matches!(stored, Ok(ref value) if value.get("enabled").and_then(Value::as_bool) == Some(true))
    {
        return Ok(json!({ "skipped": true, "reason": "disabled_in_state" }));
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
        .await;

    if !matches!(budget_check, Ok(ref value) if value.get("allowed").and_then(Value::as_bool) == Some(true))
    {
        return Ok(json!({ "skipped": true, "reason": "budget_unavailable" }));
    }

    invoke_pulse(
        iii,
        InvokeRequest {
            agent_id: agent_id.clone(),
            realm_id: realm_id.clone(),
            context_mode: Some(config.context_mode),
        },
    )
    .await
}

/// Execute only a job installed by an authenticated register call in THIS
/// worker process. iii 0.22.1 forwards caller-supplied invocation metadata
/// unchanged, so metadata and `_caller_worker_id` are not scheduler identity.
///
/// The handle/token may be visible. Due-slot claims and the in-flight flag
/// bound replay; a holder can shift one authorized run within the configured
/// five-second jitter window. This is not cron-origin proof, not durable across worker
/// restart, and not at-most-once across crashes. The engine triggers and this
/// table are process-local; the operator must re-register schedules after
/// restart. Schedule evaluation is UTC, matching pinned cron worker 0.21.10.
async fn tick_from_trigger(
    iii: &dyn TriggerBus,
    input: Value,
    metadata: Option<Value>,
    jobs: &ScheduledJobs,
    now: DateTime<Utc>,
) -> Result<Value, Error> {
    principal::refuse_agent_principal(&input, expected_bearer().as_deref(), "pulse::tick")?;
    let (handle, token) = metadata_handle_and_token(metadata.as_ref())?;
    let Some(config) = claim_scheduled_job(jobs, handle, token, now).await? else {
        return Ok(json!({ "skipped": true, "reason": "disabled" }));
    };

    let result = tick(iii, config).await;
    finish_scheduled_job(jobs, handle).await;
    result
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
    jobs: &ScheduledJobs,
) -> Result<Value, Error> {
    // The process-local table is authoritative. Persist only a view of the
    // authenticated job; never pull target/config fields back out of mutable
    // state and bless them into the table.
    let mut jobs = jobs.lock().await;
    let job = jobs
        .values_mut()
        .find(|job| job.config.agent_id == agent_id && job.config.realm_id == realm_id)
        .ok_or_else(|| {
            Error::Handler(
                "pulse schedule is not active in this process; register it again after restart"
                    .into(),
            )
        })?;
    let mut config = job.config.clone();
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

    job.config = config;
    Ok(json!({ "enabled": enabled, "agentId": agent_id }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());
    let scheduled_jobs = ScheduledJobs::default();
    let boot_token = Arc::<str>::from(uuid::Uuid::new_v4().to_string());
    tracing::warn!(
        jitter_millis = SCHEDULER_JITTER_MILLIS,
        "pulse schedules use UTC due-slot claims with bounded late jitter and no catch-up; schedules are process-local and must be re-registered after worker restart"
    );

    let iii_clone = iii.clone();
    let jobs_for_register = Arc::clone(&scheduled_jobs);
    let token_for_register = Arc::clone(&boot_token);
    iii.register_function(
        "pulse::register",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let jobs = Arc::clone(&jobs_for_register);
            let boot_token = Arc::clone(&token_for_register);
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
                register_pulse(&iii, req, &jobs, &boot_token).await
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
    let jobs_for_tick = Arc::clone(&scheduled_jobs);
    iii.register_function(
        "pulse::tick",
        RegisterFunction::new_async(move |input: Value, metadata: Option<Value>| {
            let iii = iii_clone.clone();
            let jobs = Arc::clone(&jobs_for_tick);
            async move { tick_from_trigger(&iii, input, metadata, &jobs, Utc::now()).await }
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
    let jobs_for_toggle = Arc::clone(&scheduled_jobs);
    iii.register_function(
        "pulse::toggle",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let jobs = Arc::clone(&jobs_for_toggle);
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
                toggle_pulse(&iii, realm_id, &agent_id, enabled, &jobs).await
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
    async fn caller_controlled_tick_metadata_is_not_cron_authority() {
        let forged = FakeBus::new();
        forged.on("state::get", |_| {
            Ok(json!({
                "agentId": "victim",
                "realmId": "forged-realm",
                "cron": "0 * * * *",
                "enabled": false,
                "contextMode": "thin",
            }))
        });

        let jobs = ScheduledJobs::default();
        let error = tick_from_trigger(
            &forged,
            json!({}),
            Some(json!({ "agentId": "victim", "realmId": "forged-realm" })),
            &jobs,
            test_now(),
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(error.contains("job handle"), "got: {error}");
        assert!(
            forged.calls().is_empty(),
            "caller metadata reached side effects: {:?}",
            forged.calls()
        );
    }

    fn test_now() -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 9, 6, 12, 0, 0)
            .single()
            .expect("fixed UTC test time")
    }

    fn scheduled_config(agent: &str) -> PulseConfig {
        PulseConfig {
            agent_id: agent.to_string(),
            realm_id: "realm".to_string(),
            cron: "* * * * * *".to_string(),
            enabled: true,
            context_mode: ContextMode::Thin,
            timeout_secs: None,
            max_retries: None,
        }
    }

    async fn scheduled_jobs(handle: &str, token: &str, agent: &str) -> ScheduledJobs {
        let jobs = ScheduledJobs::default();
        install_test_job(&jobs, handle, token, scheduled_config(agent)).await;
        jobs
    }

    fn scheduled_metadata(handle: &str, token: &str) -> Value {
        json!({ "jobHandle": handle, "bootToken": token })
    }

    #[test]
    fn pulse_parser_keeps_public_five_six_field_utc_grammar() {
        let five = parse_authorized_schedule("*/5 * * * *").unwrap();
        let six = parse_authorized_schedule("*/2 * * * * *").unwrap();
        let base = test_now();
        assert!(five.includes(base));
        assert!(!five.includes(base + chrono::Duration::seconds(2)));
        assert!(six.includes(base + chrono::Duration::seconds(2)));
        assert!(parse_authorized_schedule("0 0 0 * * * 2026").is_err());
        assert!(parse_authorized_schedule("TZ=Asia/Bangkok 0 0 * * * *").is_err());
    }

    #[test]
    fn due_slot_is_late_only_bounded_and_drops_catch_up_backlog() {
        let minutely = parse_authorized_schedule("0 * * * * *").unwrap();
        let base = test_now();
        assert_eq!(
            most_recent_due_slot(
                &minutely,
                base + chrono::Duration::milliseconds(SCHEDULER_JITTER_MILLIS)
            ),
            Some(base.timestamp())
        );
        assert_eq!(
            most_recent_due_slot(
                &minutely,
                base + chrono::Duration::milliseconds(SCHEDULER_JITTER_MILLIS + 1)
            ),
            None
        );
        assert_eq!(
            most_recent_due_slot(&minutely, base - chrono::Duration::milliseconds(1)),
            None,
            "an early call must not claim the upcoming slot"
        );

        let every_two_seconds = parse_authorized_schedule("*/2 * * * * *").unwrap();
        assert_eq!(
            most_recent_due_slot(
                &every_two_seconds,
                base + chrono::Duration::milliseconds(4_900)
            ),
            Some((base + chrono::Duration::seconds(4)).timestamp()),
            "only the newest due slot is claimable; no catch-up backlog"
        );
    }

    #[tokio::test]
    async fn configured_sub_minute_slots_are_not_coalesced_to_one_minute() {
        let jobs = ScheduledJobs::default();
        let mut config = scheduled_config("scheduled");
        config.cron = "*/2 * * * * *".to_string();
        install_test_job(&jobs, "known", "boot", config).await;
        let bus = FakeBus::new();
        bus.on_value("state::get", json!({ "enabled": false }));
        let now = test_now();

        tick_from_trigger(
            &bus,
            json!({}),
            Some(scheduled_metadata("known", "boot")),
            &jobs,
            now,
        )
        .await
        .unwrap();
        let second = tick_from_trigger(
            &bus,
            json!({}),
            Some(scheduled_metadata("known", "boot")),
            &jobs,
            now + chrono::Duration::seconds(2),
        )
        .await;

        assert!(
            second.is_ok(),
            "configured two-second slot was coalesced: {second:?}"
        );
        assert_eq!(bus.call_count("state::get"), 2);
    }

    #[tokio::test]
    async fn a_known_pair_cannot_amplify_a_slower_schedule() {
        let jobs = ScheduledJobs::default();
        let mut config = scheduled_config("scheduled");
        config.cron = "0 0 * * * *".to_string();
        install_test_job(&jobs, "known", "boot", config).await;
        let bus = FakeBus::new();
        bus.on_value("state::get", json!({ "enabled": false }));
        let now = test_now();

        tick_from_trigger(
            &bus,
            json!({}),
            Some(scheduled_metadata("known", "boot")),
            &jobs,
            now,
        )
        .await
        .unwrap();
        let replay = tick_from_trigger(
            &bus,
            json!({}),
            Some(scheduled_metadata("known", "boot")),
            &jobs,
            now + chrono::Duration::seconds(61),
        )
        .await;

        assert!(replay.is_err(), "known pair amplified an hourly schedule");
        assert_eq!(
            bus.call_count("state::get"),
            1,
            "off-slot replay reached state"
        );
    }

    #[tokio::test]
    async fn unknown_missing_and_stale_tick_handles_stop_before_the_bus() {
        let empty = ScheduledJobs::default();
        for metadata in [
            None,
            Some(json!({ "jobHandle": "unknown" })),
            Some(scheduled_metadata("unknown", "boot")),
        ] {
            let bus = FakeBus::new();
            assert!(
                tick_from_trigger(&bus, json!({}), metadata, &empty, test_now())
                    .await
                    .is_err()
            );
            assert!(bus.calls().is_empty());
        }

        let jobs = scheduled_jobs("known", "current", "scheduled").await;
        let stale = FakeBus::new();
        assert!(
            tick_from_trigger(
                &stale,
                json!({}),
                Some(scheduled_metadata("known", "old")),
                &jobs,
                test_now(),
            )
            .await
            .is_err()
        );
        assert!(stale.calls().is_empty());
    }

    #[tokio::test]
    async fn target_fields_beside_a_valid_handle_are_rejected_not_ignored() {
        let jobs = scheduled_jobs("known", "boot", "scheduled").await;
        let bus = FakeBus::new();
        let metadata = json!({
            "jobHandle": "known",
            "bootToken": "boot",
            "agentId": "victim",
            "realmId": "forged",
        });

        assert!(
            tick_from_trigger(
                &bus,
                json!({ "agentId": "victim", "realmId": "forged" }),
                Some(metadata),
                &jobs,
                test_now(),
            )
            .await
            .is_err()
        );
        assert!(bus.calls().is_empty());
    }

    #[tokio::test]
    async fn duplicate_claim_in_the_same_due_slot_stops_before_the_bus() {
        let jobs = scheduled_jobs("known", "boot", "scheduled").await;
        let bus = FakeBus::new();
        bus.on_value("state::get", json!({ "enabled": false }));
        let now = test_now();

        let first = tick_from_trigger(
            &bus,
            json!({}),
            Some(scheduled_metadata("known", "boot")),
            &jobs,
            now,
        )
        .await
        .unwrap();
        assert_eq!(
            first,
            json!({ "skipped": true, "reason": "disabled_in_state" })
        );
        assert_eq!(bus.call_count("state::get"), 1);

        assert!(
            tick_from_trigger(
                &bus,
                json!({}),
                Some(scheduled_metadata("known", "boot")),
                &jobs,
                now + chrono::Duration::milliseconds(500),
            )
            .await
            .is_err()
        );
        assert_eq!(bus.call_count("state::get"), 1, "replay reached state");
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn registered_schedule_derives_its_target_only_from_the_job_table() {
        let jobs = scheduled_jobs("known", "boot", "scheduled").await;
        let bus = FakeBus::new();
        bus.on_value("state::get", json!({ "enabled": true }));
        bus.on_value("ledger::check", json!({ "allowed": true }));
        bus.on_value("state::set", json!({ "stored": true }));
        bus.on_value("agent::chat", json!({ "content": "ok" }));

        let result = tick_from_trigger(
            &bus,
            json!({ "agentId": "victim", "realmId": "forged" }),
            Some(scheduled_metadata("known", "boot")),
            &jobs,
            test_now(),
        )
        .await
        .unwrap();

        assert_eq!(result["agentId"], "scheduled");
        assert_eq!(bus.call_count("agent::chat"), 1);
        assert_eq!(
            bus.calls_to("agent::chat")[0].payload["agentId"],
            "scheduled"
        );
    }

    #[tokio::test]
    async fn malformed_budget_is_a_denial_not_an_allow() {
        let jobs = scheduled_jobs("known", "boot", "scheduled").await;
        let bus = FakeBus::new();
        bus.on_value("state::get", json!({ "enabled": true }));
        bus.on_value("ledger::check", json!({}));

        let result = tick_from_trigger(
            &bus,
            json!({}),
            Some(scheduled_metadata("known", "boot")),
            &jobs,
            test_now(),
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            json!({ "skipped": true, "reason": "budget_unavailable" })
        );
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    struct PausingBus {
        calls: std::sync::atomic::AtomicUsize,
        entered: tokio::sync::Semaphore,
        release: tokio::sync::Semaphore,
    }

    impl Default for PausingBus {
        fn default() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                entered: tokio::sync::Semaphore::new(0),
                release: tokio::sync::Semaphore::new(0),
            }
        }
    }

    impl TriggerBus for PausingBus {
        fn trigger(&self, request: TriggerRequest) -> agentos_http_adapter::BusFuture<'_> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if request.function_id != "state::get" {
                    return Err(Error::Handler(format!(
                        "unexpected call: {}",
                        request.function_id
                    )));
                }
                self.entered.add_permits(1);
                self.release
                    .acquire()
                    .await
                    .expect("release semaphore closed")
                    .forget();
                Ok(json!({ "enabled": false }))
            })
        }
    }

    #[tokio::test]
    async fn concurrent_duplicate_uses_the_real_tick_claim_and_never_reaches_the_bus() {
        let jobs = scheduled_jobs("known", "boot", "scheduled").await;
        let bus = Arc::new(PausingBus::default());
        let now = test_now();
        let first = tokio::spawn({
            let jobs = Arc::clone(&jobs);
            let bus = Arc::clone(&bus);
            async move {
                tick_from_trigger(
                    bus.as_ref(),
                    json!({}),
                    Some(scheduled_metadata("known", "boot")),
                    &jobs,
                    now,
                )
                .await
            }
        });

        bus.entered
            .acquire()
            .await
            .expect("entered semaphore closed")
            .forget();
        let duplicate = tick_from_trigger(
            bus.as_ref(),
            json!({}),
            Some(scheduled_metadata("known", "boot")),
            &jobs,
            now + chrono::Duration::seconds(1),
        )
        .await;
        assert!(duplicate.is_err());
        assert_eq!(
            bus.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the concurrent duplicate reached the bus"
        );

        bus.release.add_permits(1);
        assert_eq!(
            first.await.expect("first task panicked").unwrap(),
            json!({ "skipped": true, "reason": "disabled_in_state" })
        );
    }

    #[tokio::test]
    async fn in_flight_duplicate_stops_before_the_bus() {
        let jobs = scheduled_jobs("known", "boot", "scheduled").await;
        mark_test_job_in_flight(&jobs, "known").await;
        let bus = FakeBus::new();

        assert!(
            tick_from_trigger(
                &bus,
                json!({}),
                Some(scheduled_metadata("known", "boot")),
                &jobs,
                test_now(),
            )
            .await
            .is_err()
        );
        assert!(bus.calls().is_empty());
    }

    #[tokio::test]
    async fn toggle_after_restart_reports_inactive_before_reading_state() {
        let bus = FakeBus::new();
        let jobs = ScheduledJobs::default();

        let error = toggle_pulse(&bus, "realm", "scheduled", false, &jobs)
            .await
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("register it again after restart"),
            "got: {error}"
        );
        assert!(bus.calls().is_empty());
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
