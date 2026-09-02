use dashmap::DashMap;
use iii_sdk::errors::Error;
use iii_sdk::{IIIClient, RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};
use std::sync::Arc;

mod types;

use types::{
    CancelRequest, InvokeRuntimeRequest, RegisterRuntimeRequest, RunStatus, RuntimeConfig,
    RuntimeKind, RuntimeRun,
};

fn runtimes_scope() -> &'static str {
    "bridge:runtimes"
}

fn runs_scope() -> &'static str {
    "bridge:runs"
}

async fn register_runtime(iii: &IIIClient, req: RegisterRuntimeRequest) -> Result<Value, Error> {
    let id = format!("rt-{}", uuid::Uuid::new_v4());

    let config = RuntimeConfig {
        id: id.clone(),
        kind: req.kind,
        name: req.name,
        command: req.command,
        args: req.args,
        url: req.url,
        headers: req.headers,
        env_vars: req.env_vars,
        work_dir: req.work_dir,
        timeout_secs: req.timeout_secs,
    };

    match config.kind {
        RuntimeKind::Process
        | RuntimeKind::ClaudeCode
        | RuntimeKind::Codex
        | RuntimeKind::Cursor
        | RuntimeKind::OpenCode => {
            if config.command.is_none() {
                return Err(Error::Handler(
                    "process-based runtimes require 'command'".into(),
                ));
            }
        }
        RuntimeKind::Http => {
            if config.url.is_none() {
                return Err(Error::Handler("http runtime requires 'url'".into()));
            }
        }
        RuntimeKind::Custom => {}
    }

    let value = serde_json::to_value(&config).map_err(|e| Error::Handler(e.to_string()))?;

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": runtimes_scope(),
            "key": &id,
            "value": value,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    Ok(serde_json::to_value(&config).unwrap())
}

async fn invoke_runtime(
    iii: &IIIClient,
    req: InvokeRuntimeRequest,
    active_runs: &Arc<DashMap<String, tokio::task::JoinHandle<()>>>,
) -> Result<Value, Error> {
    let config_val = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({
                "scope": runtimes_scope(),
                "key": &req.runtime_id,
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let config: RuntimeConfig = serde_json::from_value(config_val)
        .map_err(|e| Error::Handler(format!("runtime {} not found: {e}", req.runtime_id)))?;

    let run_id = format!("brun-{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now().to_rfc3339();

    let run = RuntimeRun {
        id: run_id.clone(),
        runtime_id: req.runtime_id.clone(),
        agent_id: req.agent_id.clone(),
        status: RunStatus::Running,
        output: None,
        error: None,
        exit_code: None,
        started_at: now,
        finished_at: None,
    };

    let run_val = serde_json::to_value(&run).map_err(|e| Error::Handler(e.to_string()))?;
    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": runs_scope(),
            "key": &run_id,
            "value": run_val,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    let iii_bg = iii.clone();
    let run_id_bg = run_id.clone();
    let timeout = req.timeout_secs.or(config.timeout_secs).unwrap_or(300);

    let handle = tokio::spawn(async move {
        let result = execute_runtime(&iii_bg, &config, &req.context, timeout).await;

        let (status, output, error, exit_code) = match result {
            Ok(out) => (RunStatus::Completed, Some(out), None, Some(0)),
            Err(e) => (RunStatus::Failed, None, Some(e.to_string()), Some(1)),
        };

        let finished_run = RuntimeRun {
            id: run_id_bg.clone(),
            runtime_id: config.id.clone(),
            agent_id: req.agent_id.clone(),
            status,
            output,
            error,
            exit_code,
            started_at: run.started_at.clone(),
            finished_at: Some(chrono::Utc::now().to_rfc3339()),
        };

        let val = serde_json::to_value(&finished_run).unwrap();
        if let Err(e) = iii_bg
            .trigger(TriggerRequest {
                function_id: "state::set".to_string(),
                payload: json!({
                    "scope": runs_scope(),
                    "key": &run_id_bg,
                    "value": val,
                }),
                action: None,
                timeout_ms: None,
            })
            .await
        {
            tracing::error!(run_id = %run_id_bg, error = %e, "failed to persist terminal run state");
        }

        {
            let _iii = iii_bg.clone();
            let _payload = json!({
                "topic": "bridge.run.completed",
                "data": { "runId": run_id_bg, "status": format!("{:?}", finished_run.status).to_lowercase() },
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
    });

    active_runs.insert(run_id.clone(), handle);

    Ok(json!({
        "runId": run_id,
        "status": "running",
    }))
}

async fn execute_runtime(
    iii: &IIIClient,
    config: &RuntimeConfig,
    context: &Value,
    timeout_secs: u64,
) -> Result<String, Error> {
    let timeout = std::time::Duration::from_secs(timeout_secs);

    match config.kind {
        RuntimeKind::Http => {
            let url = config
                .url
                .as_deref()
                .ok_or_else(|| Error::Handler("missing url".into()))?;

            let result = iii
                .trigger(TriggerRequest {
                    function_id: "http::post".to_string(),
                    payload: json!({
                        "url": url,
                        "body": context,
                        "headers": config.headers,
                        "timeoutMs": timeout_secs * 1000,
                    }),
                    action: None,
                    timeout_ms: None,
                })
                .await
                .map_err(|e| Error::Handler(format!("http invoke failed: {e}")))?;

            Ok(result.to_string())
        }

        RuntimeKind::Process
        | RuntimeKind::ClaudeCode
        | RuntimeKind::Codex
        | RuntimeKind::Cursor
        | RuntimeKind::OpenCode
        | RuntimeKind::Custom => {
            let cmd = config
                .command
                .as_deref()
                .ok_or_else(|| Error::Handler("missing command".into()))?;
            let args = config.args.as_deref().unwrap_or(&[]);
            let context_str = serde_json::to_string(context).unwrap_or_default();

            let work_dir = if let Some(ref dir) = config.work_dir {
                let canonical = std::path::Path::new(dir)
                    .canonicalize()
                    .map_err(|e| Error::Handler(format!("invalid work_dir: {e}")))?;
                if !canonical.starts_with(std::env::current_dir().unwrap_or_default())
                    && !canonical.starts_with("/tmp")
                {
                    return Err(Error::Handler("work_dir must be under cwd or /tmp".into()));
                }
                canonical
            } else {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            };

            let mut command_args = args.to_vec();
            command_args.push(context_str);

            tokio::time::timeout(timeout, async {
                let mut cmd_builder = tokio::process::Command::new(cmd);
                cmd_builder.args(&command_args).current_dir(&work_dir);

                if let Some(ref env_vars) = config.env_vars
                    && let Some(obj) = env_vars.as_object()
                {
                    for (k, v) in obj {
                        if let Some(val) = v.as_str() {
                            cmd_builder.env(k, val);
                        }
                    }
                }

                let output = cmd_builder
                    .output()
                    .await
                    .map_err(|e| Error::Handler(format!("spawn failed: {e}")))?;

                if output.status.success() {
                    Ok(String::from_utf8_lossy(&output.stdout).to_string())
                } else {
                    Err(Error::Handler(
                        String::from_utf8_lossy(&output.stderr).to_string(),
                    ))
                }
            })
            .await
            .map_err(|_| Error::Handler("runtime execution timed out".into()))?
        }
    }
}

async fn cancel_run(
    active_runs: &Arc<DashMap<String, tokio::task::JoinHandle<()>>>,
    iii: &IIIClient,
    req: CancelRequest,
) -> Result<Value, Error> {
    if let Some((_, handle)) = active_runs.remove(&req.run_id) {
        handle.abort();

        iii.trigger(TriggerRequest {
            function_id: "state::update".to_string(),
            payload: json!({
                "scope": runs_scope(),
                "key": &req.run_id,
                "path": "status",
                "value": "cancelled",
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| {
            Error::Handler(format!(
                "failed to mark run {} as cancelled: {e}",
                req.run_id
            ))
        })?;

        Ok(json!({ "cancelled": true, "runId": req.run_id }))
    } else {
        Err(Error::Handler(format!(
            "run {} not found or already completed",
            req.run_id
        )))
    }
}

async fn list_runtimes(iii: &IIIClient) -> Result<Value, Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::list".to_string(),
        payload: json!({ "scope": runtimes_scope() }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))
}

async fn get_run(iii: &IIIClient, run_id: &str) -> Result<Value, Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::get".to_string(),
        payload: json!({
            "scope": runs_scope(),
            "key": run_id,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());
    let active_runs: Arc<DashMap<String, tokio::task::JoinHandle<()>>> = Arc::new(DashMap::new());

    let iii_clone = iii.clone();
    iii.register_function(
        "bridge::register",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: RegisterRuntimeRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                register_runtime(&iii, req).await
            }
        })
        .description("Register an external agent runtime"),
    );

    let iii_clone = iii.clone();
    let runs_clone = active_runs.clone();
    iii.register_function(
        "bridge::invoke",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let runs = runs_clone.clone();
            async move {
                let req: InvokeRuntimeRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                invoke_runtime(&iii, req, &runs).await
            }
        })
        .description("Invoke an agent through its runtime bridge"),
    );

    let iii_clone = iii.clone();
    let runs_clone = active_runs.clone();
    iii.register_function(
        "bridge::cancel",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let runs = runs_clone.clone();
            async move {
                let req: CancelRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                cancel_run(&runs, &iii, req).await
            }
        })
        .description("Cancel a running bridge invocation"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "bridge::list",
        RegisterFunction::new_async(move |_: Value| {
            let iii = iii_clone.clone();
            async move { list_runtimes(&iii).await }
        })
        .description("List registered runtimes"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "bridge::run",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let run_id = input["runId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing runId".into()))?;
                get_run(&iii, run_id).await
            }
        })
        .description("Get status of a bridge run"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "bridge::register".to_string(),
        json!({ "http_method": "POST", "api_path": "api/bridge/runtimes" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "bridge::invoke".to_string(),
        json!({ "http_method": "POST", "api_path": "api/bridge/invoke" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "bridge::cancel".to_string(),
        json!({ "http_method": "POST", "api_path": "api/bridge/cancel" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "bridge::list".to_string(),
        json!({ "http_method": "GET", "api_path": "api/bridge/runtimes" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "bridge::run".to_string(),
        json!({ "http_method": "GET", "api_path": "api/bridge/runs/:runId" }),
        None,
    )?;

    tracing::info!("bridge worker started");
    tokio::signal::ctrl_c().await?;

    for entry in active_runs.iter() {
        entry.value().abort();
    }
    active_runs.clear();

    iii.shutdown_async().await;
    Ok(())
}
