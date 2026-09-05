use dashmap::DashMap;
use iii_sdk::errors::Error;
use iii_sdk::{IIIClient, RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc};

mod types;

use types::{
    CancelRequest, InvokeRuntimeRequest, RegisterRuntimeRequest, RunStatus, RuntimeConfig,
    RuntimeKind, RuntimeRun,
};

const MAX_RUNTIME_TIMEOUT_SECS: u64 = 300;
const SAFE_PROCESS_ENV_KEYS: &[&str] = &["PATH", "HOME", "USER", "LANG", "TERM"];
const DENIED_ENV_PREFIXES: &[&str] = &["LD_", "DYLD_", "BASH_FUNC_", "MALLOC_", "GLIBC_"];
const DENIED_ENV_KEYS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "TERM",
    "SHELL",
    "IFS",
    "ENV",
    "BASH_ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "CDPATH",
    "GLOBIGNORE",
    "PS4",
    "GCONV_PATH",
    "LOCPATH",
    "NLSPATH",
    "HOSTALIASES",
    "RESOLV_HOST_CONF",
    "TZDIR",
    "PYTHONPATH",
    "PYTHONHOME",
    "PYTHONSTARTUP",
    "PYTHONEXECUTABLE",
    "PYTHONINSPECT",
    "PYTHONWARNINGS",
    "NODE_OPTIONS",
    "NODE_PATH",
    "NODE_REPL_EXTERNAL_MODULE",
    "PERL5LIB",
    "PERL5OPT",
    "PERL5DB",
    "PERLLIB",
    "RUBYOPT",
    "RUBYLIB",
    "LUA_PATH",
    "LUA_CPATH",
    "CLASSPATH",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "JDK_JAVA_OPTIONS",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_EXTERNAL_DIFF",
    "GIT_PAGER",
    "GIT_EDITOR",
    "EDITOR",
    "VISUAL",
    "PAGER",
    "AGENTOS_API_KEY",
    "III_URL",
];

fn process_bridge_enabled() -> bool {
    std::env::var("AGENTOS_ENABLE_PROCESS_BRIDGE").as_deref() == Ok("1")
}

fn validate_env_key(key: &str) -> Result<(), Error> {
    if key.is_empty()
        || !key
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        return Err(Error::Handler(format!("invalid environment key: {key}")));
    }
    if DENIED_ENV_KEYS.contains(&key)
        || DENIED_ENV_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
    {
        return Err(Error::Handler(format!(
            "environment key {key} is not allowed for bridge processes"
        )));
    }
    Ok(())
}

fn validated_caller_environment(value: Option<&Value>) -> Result<BTreeMap<String, String>, Error> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let object = value
        .as_object()
        .ok_or_else(|| Error::Handler("envVars must be an object of string values".into()))?;
    object
        .iter()
        .map(|(key, value)| {
            validate_env_key(key)?;
            let value = value.as_str().ok_or_else(|| {
                Error::Handler(format!("environment value for {key} must be a string"))
            })?;
            Ok((key.clone(), value.to_string()))
        })
        .collect()
}

fn inherited_safe_process_environment() -> BTreeMap<String, String> {
    SAFE_PROCESS_ENV_KEYS
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| ((*key).to_string(), value))
        })
        .collect()
}

fn child_process_environment(
    inherited: BTreeMap<String, String>,
    caller: Option<&Value>,
) -> Result<BTreeMap<String, String>, Error> {
    let mut environment = validated_caller_environment(caller)?;
    for key in SAFE_PROCESS_ENV_KEYS {
        if let Some(value) = inherited.get(*key) {
            environment.insert((*key).to_string(), value.clone());
        }
    }
    Ok(environment)
}

fn validate_http_runtime_url(raw: &str) -> Result<(), Error> {
    let parsed = url::Url::parse(raw)
        .map_err(|error| Error::Handler(format!("invalid bridge URL: {error}")))?;
    let authority = raw
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default())
        .ok_or_else(|| Error::Handler("bridge URL requires an explicit authority".into()))?;
    if authority.contains('@') || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::Handler(
            "bridge URL must not contain credentials".into(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(Error::Handler(
            "bridge URL must not contain a query or fragment".into(),
        ));
    }
    let host = parsed
        .host()
        .ok_or_else(|| Error::Handler("bridge URL requires a host".into()))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http"
            if matches!(host, url::Host::Ipv4(address) if address.is_loopback())
                || matches!(host, url::Host::Ipv6(address) if address.is_loopback()) =>
        {
            Ok(())
        }
        "http" => Err(Error::Handler(
            "HTTP bridge URLs require a loopback IP literal".into(),
        )),
        _ => Err(Error::Handler(
            "bridge URL scheme must be HTTPS, or HTTP on a loopback IP literal".into(),
        )),
    }
}

fn validate_requested_timeout(timeout_secs: Option<u64>) -> Result<u64, Error> {
    let timeout_secs = timeout_secs.unwrap_or(MAX_RUNTIME_TIMEOUT_SECS);
    if !(1..=MAX_RUNTIME_TIMEOUT_SECS).contains(&timeout_secs) {
        return Err(Error::Handler(format!(
            "timeoutSecs must be between 1 and {MAX_RUNTIME_TIMEOUT_SECS}"
        )));
    }
    Ok(timeout_secs)
}

fn expected_process_basename(kind: RuntimeKind) -> Option<&'static str> {
    match kind {
        RuntimeKind::ClaudeCode => Some("claude"),
        RuntimeKind::Codex => Some("codex"),
        RuntimeKind::Cursor => Some("cursor"),
        RuntimeKind::OpenCode => Some("opencode"),
        _ => None,
    }
}

fn validate_runtime_config(config: &RuntimeConfig, process_enabled: bool) -> Result<(), Error> {
    validate_requested_timeout(config.timeout_secs)?;
    validated_caller_environment(config.env_vars.as_ref())?;

    match config.kind {
        RuntimeKind::Http => {
            let url = config
                .url
                .as_deref()
                .ok_or_else(|| Error::Handler("http runtime requires 'url'".into()))?;
            validate_http_runtime_url(url)
        }
        RuntimeKind::Process | RuntimeKind::Custom => Err(Error::Handler(
            "open-ended process and custom bridge runtimes are not supported".into(),
        )),
        kind => {
            if !process_enabled {
                return Err(Error::Handler(
                    "process bridge runtimes are disabled; set AGENTOS_ENABLE_PROCESS_BRIDGE=1 to opt in"
                        .into(),
                ));
            }
            let command = config
                .command
                .as_deref()
                .ok_or_else(|| Error::Handler("named process runtimes require 'command'".into()))?;
            let expected = expected_process_basename(kind)
                .ok_or_else(|| Error::Handler("unsupported process runtime kind".into()))?;
            if Path::new(command)
                .file_name()
                .and_then(|name| name.to_str())
                != Some(expected)
            {
                return Err(Error::Handler(format!(
                    "{kind:?} runtime command basename must be {expected}"
                )));
            }
            Ok(())
        }
    }
}

fn validate_persisted_runtime(config: &RuntimeConfig, process_enabled: bool) -> Result<(), Error> {
    validate_runtime_config(config, process_enabled)
        .map_err(|error| Error::Handler(format!("persisted runtime failed validation: {error}")))
}

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

    validate_runtime_config(&config, process_bridge_enabled())?;

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
    validate_persisted_runtime(&config, process_bridge_enabled())?;
    let timeout = validate_requested_timeout(req.timeout_secs.or(config.timeout_secs))?;

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
    validate_persisted_runtime(config, process_bridge_enabled())?;
    let timeout_secs = validate_requested_timeout(Some(timeout_secs))?;
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
                let environment = child_process_environment(
                    inherited_safe_process_environment(),
                    config.env_vars.as_ref(),
                )?;
                let mut cmd_builder = tokio::process::Command::new(cmd);
                cmd_builder
                    .args(&command_args)
                    .current_dir(&work_dir)
                    .env_clear()
                    .envs(environment)
                    .kill_on_drop(true);

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

#[cfg(test)]
mod containment_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn runtime(kind: RuntimeKind) -> RuntimeConfig {
        RuntimeConfig {
            id: "rt-test".into(),
            kind,
            name: "test".into(),
            command: None,
            args: None,
            url: None,
            headers: None,
            env_vars: None,
            work_dir: None,
            timeout_secs: Some(30),
        }
    }

    #[test]
    fn process_runtimes_are_disabled_by_default() {
        let mut config = runtime(RuntimeKind::ClaudeCode);
        config.command = Some("claude".into());
        let error = validate_runtime_config(&config, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("AGENTOS_ENABLE_PROCESS_BRIDGE=1"), "{error}");
    }

    #[test]
    fn enabled_process_bridge_allows_only_named_matching_binaries() {
        for (kind, command) in [
            (RuntimeKind::ClaudeCode, "/opt/bin/claude"),
            (RuntimeKind::Codex, "codex"),
            (RuntimeKind::Cursor, "/usr/local/bin/cursor"),
            (RuntimeKind::OpenCode, "opencode"),
        ] {
            let mut config = runtime(kind);
            config.command = Some(command.into());
            validate_runtime_config(&config, true)
                .unwrap_or_else(|error| panic!("{kind:?}/{command} should be allowed: {error}"));
        }

        for (kind, command, args) in [
            (
                RuntimeKind::ClaudeCode,
                "/bin/sh",
                vec!["-c", "touch /tmp/must-not-run"],
            ),
            (
                RuntimeKind::Codex,
                "python",
                vec!["-c", "open('/tmp/must-not-run','w')"],
            ),
            (
                RuntimeKind::Cursor,
                "/opt/caller/arbitrary",
                vec!["--payload"],
            ),
        ] {
            let mut config = runtime(kind);
            config.command = Some(command.into());
            config.args = Some(args.into_iter().map(str::to_string).collect());
            assert!(
                validate_runtime_config(&config, true).is_err(),
                "{kind:?}/{command} must be rejected before spawn"
            );
        }
    }

    #[test]
    fn open_ended_process_and_custom_kinds_remain_rejected_when_enabled() {
        for kind in [RuntimeKind::Process, RuntimeKind::Custom] {
            let mut config = runtime(kind);
            config.command = Some("/bin/sh".into());
            config.args = Some(vec!["-c".into(), "touch /tmp/must-not-run".into()]);
            assert!(validate_runtime_config(&config, true).is_err(), "{kind:?}");
        }
    }

    #[test]
    fn process_environment_rejects_injection_keys_and_does_not_inherit_bus_secrets() {
        for key in [
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "BASH_FUNC_x%%",
            "PATH",
            "IFS",
            "BASH_ENV",
            "PYTHONPATH",
            "NODE_OPTIONS",
            "AGENTOS_API_KEY",
            "III_URL",
        ] {
            let mut config = runtime(RuntimeKind::Codex);
            config.command = Some("codex".into());
            config.env_vars = Some(json!({ (key): "attacker-controlled" }));
            assert!(
                validate_runtime_config(&config, true).is_err(),
                "{key} must be rejected at registration"
            );
            assert!(
                validate_persisted_runtime(&config, true).is_err(),
                "{key} must be rejected again during replay"
            );
        }

        let inherited = BTreeMap::from([
            ("PATH".to_string(), "/safe/bin".to_string()),
            ("HOME".to_string(), "/safe/home".to_string()),
            ("LANG".to_string(), "C.UTF-8".to_string()),
            ("AGENTOS_API_KEY".to_string(), "secret".to_string()),
            ("III_URL".to_string(), "ws://secret-bus".to_string()),
            ("UNRELATED_SECRET".to_string(), "also-secret".to_string()),
        ]);
        let caller = json!({ "ANTHROPIC_API_KEY": "provider-key" });
        let child = child_process_environment(inherited, Some(&caller)).unwrap();
        assert_eq!(child.get("PATH").map(String::as_str), Some("/safe/bin"));
        assert_eq!(
            child.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("provider-key")
        );
        assert!(!child.contains_key("AGENTOS_API_KEY"));
        assert!(!child.contains_key("III_URL"));
        assert!(!child.contains_key("UNRELATED_SECRET"));
    }

    #[test]
    fn http_runtime_accepts_https_or_literal_loopback_http_only() {
        for url in [
            "https://agent.example.com/invoke",
            "http://127.0.0.1:8080/invoke",
            "http://[::1]:8080/invoke",
        ] {
            validate_http_runtime_url(url)
                .unwrap_or_else(|error| panic!("{url} should be allowed: {error}"));
        }

        for url in [
            "http://agent.example.com/invoke",
            "http://localhost:8080/invoke",
            "https://user:pass@agent.example.com/invoke",
            "https://@agent.example.com/invoke",
            "https://agent.example.com/invoke?token=x",
            "https://agent.example.com/invoke#fragment",
            "ftp://agent.example.com/invoke",
            "file:///tmp/socket",
        ] {
            assert!(
                validate_http_runtime_url(url).is_err(),
                "{url} must be rejected"
            );
        }
    }

    #[test]
    fn poisoned_persisted_state_is_revalidated_before_execution() {
        let mut shell = runtime(RuntimeKind::ClaudeCode);
        shell.command = Some("/bin/sh".into());
        shell.args = Some(vec!["-c".into(), "touch /tmp/must-not-run".into()]);
        assert!(validate_persisted_runtime(&shell, true).is_err());

        let mut injected_env = runtime(RuntimeKind::Codex);
        injected_env.command = Some("codex".into());
        injected_env.env_vars = Some(json!({ "LD_PRELOAD": "/tmp/evil.so" }));
        assert!(validate_persisted_runtime(&injected_env, true).is_err());

        let mut unsafe_http = runtime(RuntimeKind::Http);
        unsafe_http.url = Some("http://example.com/invoke?token=x".into());
        assert!(validate_persisted_runtime(&unsafe_http, false).is_err());
    }

    #[test]
    fn runtime_timeout_is_bounded() {
        let mut config = runtime(RuntimeKind::Http);
        config.url = Some("https://agent.example.com/invoke".into());
        config.timeout_secs = Some(301);
        assert!(validate_runtime_config(&config, false).is_err());
        assert!(validate_requested_timeout(Some(0)).is_err());
        assert!(validate_requested_timeout(Some(301)).is_err());
        assert_eq!(validate_requested_timeout(None).unwrap(), 300);
    }
}
