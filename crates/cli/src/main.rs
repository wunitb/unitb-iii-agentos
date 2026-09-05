use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

mod bootstrap;

const API_BASE: &str = "http://localhost:3111";
const TUI_BINARY: &str = "agentos-tui";

fn validate_id(id: &str) -> Result<&str> {
    if id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        && !id.is_empty()
        && id.len() <= 256
    {
        Ok(id)
    } else {
        anyhow::bail!("Invalid ID format: {}", id)
    }
}

fn parse_workflow_document(content: &str) -> Result<Value> {
    match serde_json::from_str(content) {
        Ok(value) => Ok(value),
        Err(json_error) => serde_yaml::from_str(content).map_err(|yaml_error| {
            anyhow::anyhow!("workflow is neither valid JSON ({json_error}) nor YAML ({yaml_error})")
        }),
    }
}

#[derive(Parser)]
#[command(name = "agentos", version, about = "Agent Operating System")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Init {
        #[arg(long)]
        quick: bool,
    },
    /// Run the engine and workers in the foreground until Ctrl+C. Loads the
    /// active `.env` exactly like `up`; `up` is the one-command path and also
    /// launches the TUI.
    Start,
    /// Stop an engine and workers started by `up`.
    Stop,
    Status {
        #[arg(long)]
        json: bool,
    },
    Health {
        #[arg(long)]
        json: bool,
    },
    #[command(subcommand)]
    Agent(AgentCmd),
    #[command(subcommand)]
    Workflow(WorkflowCmd),
    #[command(subcommand)]
    Trigger(TriggerCmd),
    #[command(subcommand)]
    Skill(SkillCmd),
    #[command(subcommand)]
    Channel(ChannelCmd),
    #[command(subcommand)]
    Config(ConfigCmd),
    #[command(subcommand)]
    Models(ModelsCmd),
    #[command(subcommand)]
    Memory(MemoryCmd),
    #[command(subcommand)]
    Security(SecurityCmd),
    #[command(subcommand)]
    Approvals(ApprovalsCmd),
    #[command(subcommand)]
    Cron(CronCmd),
    #[command(subcommand)]
    Sessions(SessionsCmd),
    #[command(subcommand)]
    Vault(VaultCmd),
    #[command(subcommand)]
    Replay(ReplayCmd),
    #[command(subcommand)]
    Migrate(MigrateCmd),
    Chat {
        agent: Option<String>,
    },
    Message {
        agent: String,
        text: String,
        #[arg(long)]
        json: bool,
    },
    Dashboard,
    Tui,
    /// Bring the whole stack up: engine, workers, then the TUI.
    Up {
        /// Start the engine and workers without launching the TUI.
        #[arg(long)]
        no_tui: bool,
        /// Seconds to wait for the engine, then for the workers to connect.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Report what is ready and what is missing. Never changes anything.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    Logs {
        #[arg(long, default_value = "50")]
        lines: u32,
        #[arg(long)]
        follow: bool,
    },
    Add {
        name: String,
        #[arg(long)]
        key: Option<String>,
    },
    Remove {
        name: String,
    },
    Integrations {
        query: Option<String>,
    },
    Completion {
        shell: String,
    },
    Mcp,
    /// Interactive first-run setup: state directories, default model, and the
    /// machine's `AGENTOS_API_KEY`.
    Onboard {
        #[arg(long)]
        quick: bool,
    },
    Reset {
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Subcommand)]
enum AgentCmd {
    New { template: Option<String> },
    List,
    Chat { agent: String },
    Kill { agent: String },
    Spawn { template: String },
}

#[derive(Subcommand)]
enum WorkflowCmd {
    List,
    Show {
        id: String,
    },
    Create {
        file: String,
    },
    Run {
        id: String,
        #[arg(long, conflicts_with = "input_file")]
        input: Option<String>,
        #[arg(long, conflicts_with = "input")]
        input_file: Option<String>,
        #[arg(long)]
        agent: Option<String>,
    },
    Runs {
        id: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    Status {
        run_id: String,
    },
}

#[derive(Subcommand)]
enum TriggerCmd {
    List,
    Create {
        function_id: String,
        trigger_type: String,
    },
    Delete {
        id: String,
    },
}

#[derive(Subcommand)]
enum SkillCmd {
    List,
    Install { id: String },
    Remove { id: String },
    Search { query: String },
    Create { name: String },
}

#[derive(Subcommand)]
enum ChannelCmd {
    List,
    Setup { channel: String },
    Test { channel: String },
}

#[derive(Subcommand)]
enum ConfigCmd {
    Show,
    Get { key: String },
    Set { key: String, value: String },
    Unset { key: String },
    SetKey { provider: String, key: String },
    Keys,
}

#[derive(Subcommand)]
enum ModelsCmd {
    List,
    Aliases,
    Providers,
    Describe { model: String },
}

#[derive(Subcommand)]
enum MemoryCmd {
    Get {
        agent: String,
        key: String,
    },
    Set {
        agent: String,
        key: String,
        value: String,
    },
    Delete {
        agent: String,
        key: String,
    },
    List {
        agent: String,
    },
}

#[derive(Subcommand)]
enum SecurityCmd {
    Audit,
    Verify,
    Scan { text: String },
}

#[derive(Subcommand)]
enum ApprovalsCmd {
    List,
    Approve { id: String },
    Reject { id: String },
}

#[derive(Subcommand)]
enum CronCmd {
    List,
    Create {
        expression: String,
        function_id: String,
    },
    Delete {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
}

#[derive(Subcommand)]
enum SessionsCmd {
    List { agent: Option<String> },
    Delete { id: String },
}

#[derive(Subcommand)]
enum VaultCmd {
    Init,
    Set { key: String, value: String },
    List,
    Remove { key: String },
}

#[derive(Subcommand)]
enum ReplayCmd {
    Get {
        session_id: String,
    },
    List {
        #[arg(long)]
        agent: Option<String>,
    },
    Summary {
        session_id: String,
    },
}

#[derive(Subcommand)]
enum MigrateCmd {
    Scan,
    #[command(name = "openclaw")]
    OpenClaw {
        #[arg(long)]
        dry_run: bool,
    },
    #[command(name = "langchain")]
    LangChain {
        #[arg(long)]
        dry_run: bool,
    },
    Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerRuntime {
    Rust,
    Python,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkerSpec {
    pub(crate) name: String,
    pub(crate) runtime: WorkerRuntime,
    pub(crate) binary: Option<PathBuf>,
}

pub(crate) struct RunningWorker {
    pub(crate) name: String,
    pub(crate) child: Child,
}

struct ProcessGroup {
    engine: Child,
    /// The bus RBAC gate, started before the engine and stopped after it.
    bus_auth: Option<Child>,
    workers: Vec<RunningWorker>,
    terminated: bool,
}

impl ProcessGroup {
    fn terminate(&mut self) {
        if self.terminated {
            return;
        }

        for worker in &mut self.workers {
            let _ = worker.child.kill();
        }
        let _ = self.engine.kill();
        for worker in &mut self.workers {
            let _ = worker.child.wait();
        }
        let _ = self.engine.wait();
        // The gate goes last: everything above talks through it.
        if let Some(daemon) = self.bus_auth.as_mut() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        self.bus_auth = None;
        self.terminated = true;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn normalize_path(path: PathBuf, caller_dir: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path
    } else {
        caller_dir.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

fn resolve_agentos_home(
    caller_dir: &Path,
    configured_home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    if let Some(configured_home) = configured_home.filter(|value| !value.is_empty()) {
        return Ok(normalize_path(PathBuf::from(configured_home), caller_dir));
    }

    dirs::home_dir()
        .map(|home| home.join(".agentos"))
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))
}

fn agentos_home_dir() -> Result<PathBuf> {
    let caller_dir = std::env::current_dir().context("Cannot determine current directory")?;
    resolve_agentos_home(&caller_dir, std::env::var_os("AGENTOS_HOME").as_deref())
}

/// How the active `config.yaml` was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigDiscovery {
    /// `AGENTOS_CONFIG` named the file explicitly.
    Explicit,
    /// The caller's directory is a checkout with `config.yaml` and `workers/`.
    Checkout,
    /// Fallback to the installed runtime below the resolved AgentOS home.
    Home,
}

impl ConfigDiscovery {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Explicit => "explicit AGENTOS_CONFIG",
            Self::Checkout => "checkout config.yaml",
            Self::Home => "installed runtime below AGENTOS_HOME",
        }
    }
}

/// The resolved AgentOS home, engine config, and runtime directory.
#[derive(Debug, Clone)]
pub(crate) struct RuntimePaths {
    pub(crate) agentos_home: PathBuf,
    pub(crate) config_path: PathBuf,
    pub(crate) runtime_dir: PathBuf,
    pub(crate) discovery: ConfigDiscovery,
}

/// Documented precedence: an explicit non-empty `AGENTOS_CONFIG` wins; a
/// checkout beside the caller comes next and stays eligible even when
/// `AGENTOS_HOME` is set; otherwise the installed runtime config is used.
fn resolve_config_path(
    caller_dir: &Path,
    agentos_home: &Path,
    configured_config: Option<&std::ffi::OsStr>,
) -> (PathBuf, ConfigDiscovery) {
    if let Some(config) = configured_config.filter(|value| !value.is_empty()) {
        return (
            normalize_path(PathBuf::from(config), caller_dir),
            ConfigDiscovery::Explicit,
        );
    }

    let project_config = caller_dir.join("config.yaml");
    if project_config.is_file() && caller_dir.join("workers").is_dir() {
        (project_config, ConfigDiscovery::Checkout)
    } else {
        (
            agentos_home.join("runtime/config.yaml"),
            ConfigDiscovery::Home,
        )
    }
}

fn runtime_paths() -> Result<RuntimePaths> {
    let caller_dir = std::env::current_dir().context("Cannot determine current directory")?;
    let agentos_home =
        resolve_agentos_home(&caller_dir, std::env::var_os("AGENTOS_HOME").as_deref())?;
    let (config_path, discovery) = resolve_config_path(
        &caller_dir,
        &agentos_home,
        std::env::var_os("AGENTOS_CONFIG").as_deref(),
    );
    let runtime_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Invalid AgentOS config path"))?
        .to_path_buf();
    Ok(RuntimePaths {
        agentos_home,
        config_path,
        runtime_dir,
        discovery,
    })
}

pub(crate) fn engine_log_path(agentos_home: &Path) -> PathBuf {
    agentos_home.join("logs/engine.log")
}

pub(crate) fn worker_log_path(agentos_home: &Path) -> PathBuf {
    agentos_home.join("logs/workers.log")
}

fn ensure_agentos_dirs(agentos_home: &Path) -> Result<()> {
    for directory in ["data", "skills", "agents", "logs", "state"] {
        std::fs::create_dir_all(agentos_home.join(directory))?;
    }
    Ok(())
}

fn strip_yaml_comment(value: &str) -> &str {
    let mut quote = None;
    let mut previous = '\0';
    for (index, character) in value.char_indices() {
        match character {
            '\'' | '"' if quote.is_none() => quote = Some(character),
            character if Some(character) == quote => quote = None,
            '#' if quote.is_none() && (index == 0 || previous.is_whitespace()) => {
                return &value[..index];
            }
            _ => {}
        }
        previous = character;
    }
    value
}

fn yaml_mapping_line(line: &str) -> Option<(usize, &str, &str)> {
    let without_comment = strip_yaml_comment(line);
    let trimmed = without_comment.trim();
    if trimmed.is_empty() {
        return None;
    }
    let indentation = without_comment.len() - without_comment.trim_start().len();
    let (key, value) = trimmed.split_once(':')?;
    let key = key.trim();
    if key.is_empty() || key.contains(char::is_whitespace) {
        return None;
    }
    Some((indentation, key, value.trim()))
}

fn yaml_scalar(value: &str) -> &str {
    let value = strip_yaml_comment(value).trim();
    if value.len() >= 2
        && ((value.starts_with('\'') && value.ends_with('\''))
            || (value.starts_with('"') && value.ends_with('"')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn parse_runtime_kind(value: &str) -> Result<WorkerRuntime> {
    match yaml_scalar(value) {
        "rust" => Ok(WorkerRuntime::Rust),
        "python" => Ok(WorkerRuntime::Python),
        other => anyhow::bail!("runtime.kind must be rust or python, got {other:?}"),
    }
}

fn split_flow_items(value: &str) -> Result<Vec<&str>> {
    let mut items = Vec::new();
    let mut start = 0;
    let mut nesting = 0i32;
    let mut quote = None;
    let mut escaped = false;

    for (index, character) in value.char_indices() {
        if let Some(delimiter) = quote {
            if character == delimiter && !escaped {
                quote = None;
            }
            escaped = character == '\\' && !escaped;
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '{' | '[' => nesting += 1,
            '}' | ']' => {
                nesting -= 1;
                if nesting < 0 {
                    anyhow::bail!("Malformed inline runtime mapping");
                }
            }
            ',' if nesting == 0 => {
                items.push(value[start..index].trim());
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if quote.is_some() || nesting != 0 {
        anyhow::bail!("Malformed inline runtime mapping");
    }
    let last = value[start..].trim();
    if !last.is_empty() {
        items.push(last);
    }
    Ok(items)
}

fn split_flow_pair(value: &str) -> Result<(&str, &str)> {
    let mut nesting = 0i32;
    let mut quote = None;
    let mut escaped = false;

    for (index, character) in value.char_indices() {
        if let Some(delimiter) = quote {
            if character == delimiter && !escaped {
                quote = None;
            }
            escaped = character == '\\' && !escaped;
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '{' | '[' => nesting += 1,
            '}' | ']' => {
                nesting -= 1;
                if nesting < 0 {
                    anyhow::bail!("Malformed inline runtime mapping");
                }
            }
            ':' if nesting == 0 => {
                return Ok((value[..index].trim(), value[index + 1..].trim()));
            }
            _ => {}
        }
    }
    anyhow::bail!("Malformed inline runtime mapping")
}

fn parse_inline_runtime(value: &str) -> Result<WorkerRuntime> {
    let value = yaml_scalar(value);
    if !value.starts_with('{') || !value.ends_with('}') {
        return parse_runtime_kind(value);
    }

    let inner = &value[1..value.len() - 1];
    let mut kind = None;
    for item in split_flow_items(inner)? {
        let (key, value) = split_flow_pair(item)?;
        if yaml_scalar(key) == "kind" {
            if kind.is_some() {
                anyhow::bail!("Duplicate runtime.kind");
            }
            kind = Some(value);
        }
    }
    parse_runtime_kind(kind.ok_or_else(|| anyhow::anyhow!("Missing direct runtime.kind"))?)
}

fn parse_worker_runtime(manifest: &str) -> Result<WorkerRuntime> {
    let lines = manifest.lines().collect::<Vec<_>>();
    for (line_number, line) in lines.iter().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if yaml_mapping_line(line).is_none() {
            anyhow::bail!("Malformed worker manifest at line {}", line_number + 1);
        }
    }

    let runtime_lines = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            yaml_mapping_line(line)
                .filter(|(indentation, key, _)| *indentation == 0 && *key == "runtime")
                .map(|(indentation, _, value)| (index, indentation, value))
        })
        .collect::<Vec<_>>();
    if runtime_lines.len() != 1 {
        anyhow::bail!("Worker manifest must contain exactly one direct runtime mapping");
    }

    let (runtime_line, runtime_indentation, inline_value) = runtime_lines[0];
    if !inline_value.is_empty() {
        return parse_inline_runtime(inline_value);
    }

    let mut child_indentation = None;
    let mut direct_kind = None;
    for (line_number, line) in lines.iter().enumerate().skip(runtime_line + 1) {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let (indentation, key, value) = yaml_mapping_line(line).ok_or_else(|| {
            anyhow::anyhow!("Malformed runtime mapping at line {}", line_number + 1)
        })?;
        if indentation <= runtime_indentation {
            break;
        }
        let child_indentation = *child_indentation.get_or_insert(indentation);
        if indentation == child_indentation && key == "kind" {
            if direct_kind.is_some() {
                anyhow::bail!("Duplicate direct runtime.kind");
            }
            direct_kind = Some(value);
        }
    }

    parse_runtime_kind(direct_kind.ok_or_else(|| anyhow::anyhow!("Missing direct runtime.kind"))?)
}

fn worker_package_name(worker_dir: &Path) -> Option<String> {
    let source = std::fs::read_to_string(worker_dir.join("Cargo.toml")).ok()?;
    let document = source.parse::<toml::Value>().ok()?;
    document
        .get("package")?
        .get("name")?
        .as_str()
        .map(str::to_owned)
}

pub(crate) fn worker_binary_dir(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("target/release")
}

/// The name an AgentOS worker announces to the engine, which is the identity
/// the engine's function registry reports back as `worker_name`.
///
/// It is deliberately NOT the `workers/` directory name. `config.yaml` boots
/// engine workers of its own, and three of them — `llm-router`,
/// `context-manager` and `cron` — share a directory name with a worker in this
/// repo. Announcing the bare directory name put two different processes on the
/// bus under one identity, with three consequences, all observed live on
/// 0.22.1:
///
/// * `up` treated the engine's worker as "this worker is already connected"
///   and skipped launching ours. `agentos-llm-router` never started, so
///   `agentos::llm::complete` and `agentos::llm::route` were absent and
///   `agent::chat` answered `function_not_found` — the whole product, on a
///   default `agentos up`.
/// * Which of the two won was a race with engine worker startup, so the
///   symptom moved between machines.
/// * iii 0.23.0 rejects a duplicate worker name outright instead of tolerating
///   it, turning the same collision into a reconnect loop.
///
/// The `agentos-` prefix is the name the release binary already carries
/// (`missing_worker_binaries` below, and `worker_binary_dir`), so the bus
/// identity, the binary name and the operator-facing name are now one string.
pub(crate) fn bus_identity(worker_name: &str) -> String {
    format!("agentos-{worker_name}")
}

/// Release binaries a Rust worker needs but does not have yet.
pub(crate) fn missing_worker_binaries(workers: &[WorkerSpec]) -> Vec<String> {
    workers
        .iter()
        .filter(|worker| worker.runtime == WorkerRuntime::Rust && worker.binary.is_none())
        .map(|worker| bus_identity(&worker.name))
        .collect()
}

/// Reads every worker manifest below `runtime_dir` and resolves the release
/// binary of each Rust worker. Missing binaries are reported per worker rather
/// than failing, so callers can render their own guidance.
pub(crate) fn collect_worker_specs(runtime_dir: &Path) -> Result<Vec<WorkerSpec>> {
    let workers_dir = runtime_dir.join("workers");
    if !path_has_any_read_permission(&workers_dir, true) {
        anyhow::bail!(
            "AgentOS workers directory is missing or unreadable: {}",
            workers_dir.display()
        );
    }
    let entries = std::fs::read_dir(&workers_dir).with_context(|| {
        format!(
            "AgentOS workers directory is missing or unreadable: {}",
            workers_dir.display()
        )
    })?;
    let binary_dir = worker_binary_dir(runtime_dir);
    let mut workers = Vec::new();

    for entry in entries {
        let entry = entry.with_context(|| format!("Cannot read {}", workers_dir.display()))?;
        let worker_dir = entry.path();
        if !worker_dir.is_dir() {
            continue;
        }
        if !path_has_any_read_permission(&worker_dir, true) {
            anyhow::bail!("Worker directory is unreadable: {}", worker_dir.display());
        }
        let worker_name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Worker directory name is not valid UTF-8"))?;
        let manifest_path = worker_dir.join("iii.worker.yaml");
        if !path_has_any_read_permission(&manifest_path, false) {
            anyhow::bail!("Worker manifest is unreadable: {}", manifest_path.display());
        }
        let manifest = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("Cannot read worker manifest {}", manifest_path.display()))?;
        let runtime = parse_worker_runtime(&manifest)
            .with_context(|| format!("Invalid worker manifest {}", manifest_path.display()))?;
        let binary = if runtime == WorkerRuntime::Rust {
            let packaged_name = format!("agentos-{worker_name}");
            let packaged_binary = binary_dir.join(&packaged_name);
            if packaged_binary.is_file() {
                Some(packaged_binary)
            } else if let Some(package_name) = worker_package_name(&worker_dir) {
                let package_binary = binary_dir.join(package_name);
                package_binary.is_file().then_some(package_binary)
            } else {
                None
            }
        } else {
            None
        };
        workers.push(WorkerSpec {
            name: worker_name,
            runtime,
            binary,
        });
    }

    if workers.is_empty() {
        anyhow::bail!("No worker manifests found in {}", workers_dir.display());
    }

    workers.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(workers)
}

fn discover_workers(runtime_dir: &Path) -> Result<Vec<WorkerSpec>> {
    let workers = collect_worker_specs(runtime_dir)?;
    let missing = missing_worker_binaries(&workers);
    if !missing.is_empty() {
        anyhow::bail!(
            "Missing compiled workers in {}: {}",
            worker_binary_dir(runtime_dir).display(),
            missing.join(", ")
        );
    }
    Ok(workers)
}

fn path_has_any_read_permission(path: &Path, directory: bool) -> bool {
    #[cfg(unix)]
    {
        let Ok(metadata) = std::fs::metadata(path) else {
            return false;
        };
        let mode = metadata.permissions().mode();
        if mode & 0o444 == 0 || (directory && mode & 0o111 == 0) {
            return false;
        }
    }
    #[cfg(not(unix))]
    let _ = (path, directory);
    true
}

fn initialize_agentos_home(agentos_home: &Path) -> Result<()> {
    if !agentos_home.exists() {
        std::fs::create_dir_all(agentos_home)?;
    }
    ensure_agentos_dirs(agentos_home)
}
pub(crate) fn find_iii_binary(agentos_home: &Path) -> Result<PathBuf> {
    if let Ok(path) = which::which("iii") {
        return Ok(path);
    }

    let mut candidates = vec![agentos_home.join(".local/bin/iii")];
    if let Some(platform_home) = dirs::home_dir() {
        candidates.push(platform_home.join(".local/bin/iii"));
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            anyhow::anyhow!("iii-engine v0.22.1 was not found on PATH or in ~/.local/bin")
        })
}

/// A workspace binary that ships beside the CLI: next to this executable for
/// installed releases, otherwise the release directory beside the runtime
/// config, which is where the worker binaries live.
pub(crate) fn find_sibling_binary(name: &str, runtime_dir: Option<&Path>) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(directory) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        candidates.push(directory.join(name));
    }
    if let Some(runtime_dir) = runtime_dir {
        candidates.push(worker_binary_dir(runtime_dir).join(name));
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

/// The `agentos-tui` binary: beside this executable for installed releases,
/// otherwise the workspace release directory beside the runtime config.
pub(crate) fn find_tui_binary(runtime_dir: Option<&Path>) -> Option<PathBuf> {
    find_sibling_binary(TUI_BINARY, runtime_dir)
}

/// Puts a spawned process in its own group so it survives the terminal
/// signals delivered to the foreground CLI.
fn detach_process(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

pub(crate) struct WorkerLaunch<'a> {
    pub(crate) runtime_dir: &'a Path,
    pub(crate) log_path: &'a Path,
    /// Values loaded from the runtime `.env`. Explicit shell exports are not
    /// included here and continue to be inherited normally.
    pub(crate) env: &'a BTreeMap<String, String>,
    /// Keep the workers running after this process exits.
    pub(crate) detached: bool,
}

/// Starts every Rust worker that has a release binary. Workers started before
/// a failure are pushed into `started` so the caller still owns and cleans up
/// the partial launch.
pub(crate) fn launch_workers(
    workers: &[WorkerSpec],
    launch: &WorkerLaunch<'_>,
    started: &mut Vec<RunningWorker>,
) -> Result<()> {
    let log = std::fs::File::create(launch.log_path)
        .with_context(|| format!("Cannot create worker log {}", launch.log_path.display()))?;
    for worker in workers {
        let Some(binary) = worker.binary.as_ref() else {
            continue;
        };
        let mut command = Command::new(binary);
        command
            .current_dir(launch.runtime_dir)
            .envs(launch.env)
            // iii-sdk 0.22.1 otherwise falls back to hostname:pid, which is
            // not stable enough for readiness or duplicate suppression. The
            // value is namespaced so it cannot collide with an engine worker
            // of the same directory name — see `bus_identity`.
            .env("III_WORKER_NAME", bus_identity(&worker.name))
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log.try_clone()?));
        if launch.detached {
            detach_process(&mut command);
        }
        let child = command
            .spawn()
            .map_err(|error| anyhow::anyhow!("Failed to start {}: {error}", binary.display()))?;
        started.push(RunningWorker {
            name: worker.name.clone(),
            child,
        });
    }
    Ok(())
}

/// Starts the bus-auth daemon. Its address is passed explicitly so the value
/// this process resolved is the value the daemon binds, whatever the child
/// environment says.
pub(crate) fn spawn_bus_auth(
    binary: &Path,
    addr: std::net::SocketAddr,
    config_path: &Path,
    runtime_dir: &Path,
    log_path: &Path,
    env: &BTreeMap<String, String>,
    detached: bool,
) -> Result<Child> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("Cannot open log {}", log_path.display()))?;
    let log_err = log_file.try_clone()?;
    let mut command = Command::new(binary);
    command
        .arg(format!("--listen={addr}"))
        // The daemon re-reads the config the ENGINE is about to boot and refuses
        // to gate one that names hooks it does not serve. Passed explicitly so
        // the check follows `--config`, not the working directory.
        .arg(format!("--config={}", config_path.display()))
        .current_dir(runtime_dir)
        .envs(env)
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err));
    if detached {
        detach_process(&mut command);
    }
    command
        .spawn()
        .map_err(|error| anyhow::anyhow!("Failed to start {}: {error}", binary.display()))
}

/// Starts the engine with its log redirected to `log_path`.
pub(crate) fn spawn_engine(
    iii_path: &Path,
    config_path: &Path,
    runtime_dir: &Path,
    log_path: &Path,
    env: &BTreeMap<String, String>,
    detached: bool,
) -> Result<Child> {
    let log_file = std::fs::File::create(log_path)
        .with_context(|| format!("Cannot create engine log {}", log_path.display()))?;
    let log_err = log_file.try_clone()?;
    let mut command = Command::new(iii_path);
    command
        .arg("--config")
        .arg(config_path)
        .current_dir(runtime_dir)
        .envs(env)
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err));
    if detached {
        detach_process(&mut command);
    }
    command
        .spawn()
        .map_err(|error| anyhow::anyhow!("Failed to start iii-engine: {error}. Is it installed?"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();
    let api_base = get_api_url();

    match cli.command {
        Commands::Init { quick } => {
            let config_dir = agentos_home_dir()?;
            initialize_agentos_home(&config_dir)?;
            println!("{} Initialized {}", "✓".green(), config_dir.display());

            if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
                let config_path = config_dir.join("config.toml");
                let mut config = String::new();
                if config_path.exists() {
                    config = std::fs::read_to_string(&config_path).unwrap_or_default();
                }
                if !config.contains("anthropic") {
                    config.push_str(&format!("\n[keys]\nanthropic = \"{}\"\n", key));
                    std::fs::write(&config_path, config)?;
                    println!("{} Auto-detected ANTHROPIC_API_KEY", "✓".green());
                }
            }

            if quick {
                println!(
                    "\n{} Ready. Run {} to start.",
                    "✓".green(),
                    "agentos up".cyan()
                );
            } else {
                println!(
                    "\nSet a provider key (written to the active .env, which the workers read):"
                );
                println!(
                    "  {} agentos config set-key anthropic $ANTHROPIC_API_KEY",
                    "▸".dimmed()
                );
                println!(
                    "\nThen start the stack ({} generates AGENTOS_API_KEY on first run):",
                    "up".cyan()
                );
                println!("  {} agentos up", "▸".dimmed());
            }
        }

        Commands::Start => {
            let RuntimePaths {
                agentos_home,
                config_path: config_yaml,
                runtime_dir,
                ..
            } = runtime_paths()?;
            let first_run = !agentos_home.exists();
            initialize_agentos_home(&agentos_home)?;
            if first_run {
                println!("{} First run detected. Initializing...", "→".blue());
                println!("{} Created {}", "✓".green(), agentos_home.display());
            }

            if !config_yaml.is_file() {
                anyhow::bail!(
                    "AgentOS runtime not found at {}. Run the installer or set AGENTOS_CONFIG",
                    config_yaml.display()
                );
            }
            let worker_specs = discover_workers(&runtime_dir)?;
            let iii_path = find_iii_binary(&agentos_home)?;
            let engine_log = engine_log_path(&agentos_home);
            let worker_log = worker_log_path(&agentos_home);
            // Same environment contract as `up`: generate the machine key when
            // the active `.env` has none, then hand that `.env` to the engine,
            // the workers, and the TUI.
            let key_outcome = bootstrap::ensure_api_key(&runtime_dir)?;
            let launch_env = bootstrap::load_dotenv(&runtime_dir)?;

            println!("\n{}", "AgentOS".bold().cyan());
            println!("{} {}", "✓".green(), key_outcome.describe());
            println!("{}", "─".repeat(40).dimmed());

            // Same order as `up`: the bus RBAC gate must answer before the
            // engine accepts its first worker connection.
            let bus_auth = bootstrap::start_bus_auth_for_foreground(
                &config_yaml,
                &runtime_dir,
                &worker_log,
                &launch_env,
            )?;

            println!("{} Starting iii-engine...", "→".blue());
            let engine = spawn_engine(
                &iii_path,
                &config_yaml,
                &runtime_dir,
                &engine_log,
                &launch_env,
                false,
            )?;
            let mut processes = ProcessGroup {
                engine,
                bus_auth,
                workers: Vec::new(),
                terminated: false,
            };

            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Some(status) = processes.engine.try_wait()? {
                processes.terminate();
                anyhow::bail!(
                    "iii-engine exited with {status}; check {}",
                    engine_log.display()
                );
            }
            println!(
                "{} iii-engine running (pid {})",
                "✓".green(),
                processes.engine.id()
            );

            println!("{} Starting workers...", "→".blue());
            launch_workers(
                &worker_specs,
                &WorkerLaunch {
                    runtime_dir: &runtime_dir,
                    log_path: &worker_log,
                    env: &launch_env,
                    detached: false,
                },
                &mut processes.workers,
            )?;

            tokio::time::sleep(Duration::from_secs(3)).await;
            if let Some(status) = processes.engine.try_wait()? {
                processes.terminate();
                anyhow::bail!(
                    "iii-engine exited with {status}; check {}",
                    engine_log.display()
                );
            }
            let mut failed_worker = None;
            for worker in &mut processes.workers {
                if let Some(status) = worker.child.try_wait()? {
                    failed_worker = Some((worker.name.clone(), status));
                    break;
                }
            }
            if let Some((worker_name, status)) = failed_worker {
                processes.terminate();
                anyhow::bail!(
                    "Worker {worker_name} exited with {status}; check {}",
                    worker_log.display()
                );
            }

            let rust_worker_count = worker_specs
                .iter()
                .filter(|worker| worker.runtime == WorkerRuntime::Rust)
                .count();
            println!("{} {} workers running", "✓".green(), rust_worker_count);
            println!("{}", "─".repeat(40).dimmed());
            println!("  Engine   {}  ws://localhost:49134", "●".green());
            println!("  API      {}  http://localhost:3111", "●".green());
            println!(
                "  Workers  {}  {} Rust workers",
                "●".green(),
                rust_worker_count
            );
            println!("{}", "─".repeat(40).dimmed());
            println!(
                "\n  {} agentos chat          Interactive chat",
                "▸".dimmed()
            );
            println!(
                "  {} agentos tui           Terminal dashboard",
                "▸".dimmed()
            );
            println!("  {} agentos status        System status", "▸".dimmed());
            println!("\n{} Press Ctrl+C to stop\n", "⏎".dimmed());

            tokio::signal::ctrl_c().await?;
            println!("\n{} Shutting down...", "→".blue());
            processes.terminate();
            println!("{} Stopped.", "✓".green());
        }

        Commands::Stop => {
            println!("{} Stopping agentos engine...", "→".blue());
            println!("{} Engine stopped.", "✓".green());
        }

        Commands::Status { json: is_json } => {
            let resp: Value = client
                .get(format!("{}/api/health", api_base))
                .send()
                .await?
                .json()
                .await?;
            if is_json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                println!(
                    "{} agentos v{}",
                    "●".green(),
                    resp["version"]
                        .as_str()
                        .unwrap_or(env!("CARGO_PKG_VERSION"))
                );
                println!("  Workers: {}", resp["workers"]);
                println!("  Uptime:  {:.0}s", resp["uptime"].as_f64().unwrap_or(0.0));
            }
        }

        Commands::Health { json: is_json } => {
            let resp: Value = client
                .get(format!("{}/api/health", api_base))
                .send()
                .await?
                .json()
                .await?;
            if is_json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                let status = resp["status"].as_str().unwrap_or("unknown");
                let icon = if status == "healthy" {
                    "●".green()
                } else {
                    "●".red()
                };
                println!("{} {}", icon, status);
            }
        }

        Commands::Agent(cmd) => match cmd {
            AgentCmd::List => {
                let resp: Value = client
                    .get(format!("{}/api/agents", api_base))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(agents) = resp.as_array() {
                    println!(
                        "{:<20} {:<15} {:<30}",
                        "ID".bold(),
                        "STATUS".bold(),
                        "NAME".bold()
                    );
                    for a in agents {
                        println!(
                            "{:<20} {:<15} {:<30}",
                            a["key"].as_str().unwrap_or("-"),
                            "active".green(),
                            a["value"]["name"].as_str().unwrap_or("-")
                        );
                    }
                }
            }
            AgentCmd::New { template } => {
                let tmpl = template.unwrap_or_else(|| "assistant".into());
                let resp: Value = client
                    .post(format!("{}/api/agents", api_base))
                    .json(&json!({ "name": tmpl, "tags": ["template"] }))
                    .send()
                    .await?
                    .json()
                    .await?;
                println!(
                    "{} Created agent: {}",
                    "✓".green(),
                    resp["agentId"].as_str().unwrap_or("unknown")
                );
            }
            AgentCmd::Chat { agent } => {
                let agent = validate_id(&agent)?;
                println!(
                    "{} Chatting with {}. Type 'exit' to quit.\n",
                    "→".blue(),
                    agent.cyan()
                );
                loop {
                    let mut input = String::new();
                    print!("{} ", "you>".bold());
                    use std::io::Write;
                    std::io::stdout().flush()?;
                    let bytes_read = std::io::stdin().read_line(&mut input)?;
                    if bytes_read == 0 {
                        println!();
                        break;
                    }
                    let input = input.trim();
                    if input == "exit" || input == "quit" {
                        break;
                    }
                    if input.is_empty() {
                        continue;
                    }

                    let resp: Value = client
                        .post(format!("{}/api/agents/{}/message", api_base, agent))
                        .json(&json!({ "message": input }))
                        .send()
                        .await?
                        .json()
                        .await?;
                    println!(
                        "\n{} {}\n",
                        "agent>".blue().bold(),
                        resp["content"].as_str().unwrap_or("(no response)")
                    );
                }
            }
            AgentCmd::Kill { agent } => {
                let agent = validate_id(&agent)?;
                client
                    .delete(format!("{}/api/agents/{}", api_base, agent))
                    .send()
                    .await?;
                println!("{} Agent {} terminated", "✓".green(), agent);
            }
            AgentCmd::Spawn { template } => {
                let resp: Value = client
                    .post(format!("{}/api/agents", api_base))
                    .json(&json!({ "name": template, "tags": ["spawned"] }))
                    .send()
                    .await?
                    .json()
                    .await?;
                println!(
                    "{} Spawned: {}",
                    "✓".green(),
                    resp["agentId"].as_str().unwrap_or("unknown")
                );
            }
        },

        Commands::Workflow(cmd) => match cmd {
            WorkflowCmd::List => {
                let resp: Value = client
                    .get(format!("{}/api/workflows", api_base))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            WorkflowCmd::Show { id } => {
                let id = validate_id(&id)?;
                let resp: Value = client
                    .get(format!("{}/api/workflows/{}", api_base, id))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            WorkflowCmd::Create { file } => {
                let content = std::fs::read_to_string(&file)?;
                let workflow = parse_workflow_document(&content)?;
                let resp: Value = client
                    .post(format!("{}/api/workflows", api_base))
                    .json(&workflow)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                println!(
                    "{} Created workflow: {}",
                    "✓".green(),
                    resp["id"].as_str().unwrap_or("unknown")
                );
            }
            WorkflowCmd::Run {
                id,
                input,
                input_file,
                agent,
            } => {
                let id = validate_id(&id)?;
                let raw_input = match input_file {
                    Some(path) => Some(std::fs::read_to_string(path)?),
                    None => input,
                };
                let workflow_input = raw_input
                    .map(|raw| serde_json::from_str(&raw).unwrap_or(Value::String(raw)))
                    .unwrap_or(Value::Null);
                let mut body = json!({ "workflowId": id, "input": workflow_input });
                if let Some(agent) = agent {
                    let agent = validate_id(&agent)?;
                    body["agentId"] = Value::String(agent.to_string());
                }
                let resp: Value = client
                    .post(format!("{}/api/workflows/{}/run", api_base, id))
                    .json(&body)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            WorkflowCmd::Runs { id, limit, offset } => {
                let id = validate_id(&id)?;
                let resp: Value = client
                    .get(format!(
                        "{}/api/workflows/{}/runs?limit={}&offset={}",
                        api_base, id, limit, offset
                    ))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            WorkflowCmd::Status { run_id } => {
                let run_id = validate_id(&run_id)?;
                let resp: Value = client
                    .get(format!("{}/api/workflow-runs/{}", api_base, run_id))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
        },

        Commands::Skill(cmd) => match cmd {
            SkillCmd::List => {
                let resp: Value = client
                    .get(format!("{}/api/skillkit/list", api_base))
                    .send()
                    .await?
                    .json()
                    .await?;
                let skills = resp.as_array().or_else(|| resp["skills"].as_array());
                if let Some(skills) = skills {
                    println!(
                        "{:<20} {:<15} {:<40}",
                        "ID".bold(),
                        "CATEGORY".bold(),
                        "NAME".bold()
                    );
                    for s in skills {
                        println!(
                            "{:<20} {:<15} {:<40}",
                            s["id"].as_str().unwrap_or("-"),
                            s["category"].as_str().unwrap_or("-"),
                            s["name"].as_str().unwrap_or("-")
                        );
                    }
                }
            }
            SkillCmd::Install { id } => {
                let resp: Value = client
                    .post(format!("{}/api/skillkit/install", api_base))
                    .json(&json!({ "id": id }))
                    .send()
                    .await?
                    .json()
                    .await?;
                if resp["installed"].as_bool() != Some(true) {
                    anyhow::bail!(
                        "Skill install failed: {}",
                        resp["error"].as_str().unwrap_or("unknown error")
                    );
                }
                println!("{} Installed skill: {}", "✓".green(), id);
            }
            SkillCmd::Remove { id } => {
                let id = validate_id(&id)?;
                let resp: Value = client
                    .post(format!("{}/api/skillkit/uninstall", api_base))
                    .json(&json!({ "id": id }))
                    .send()
                    .await?
                    .json()
                    .await?;
                if resp["removed"].as_bool() != Some(true) {
                    anyhow::bail!(
                        "Skill removal failed: {}",
                        resp["error"].as_str().unwrap_or("unknown error")
                    );
                }
                println!("{} Removed skill: {}", "✓".green(), id);
            }
            SkillCmd::Search { query } => {
                let resp: Value = client
                    .get(format!(
                        "{}/api/skillkit/search?query={}",
                        api_base,
                        urlencoding::encode(&query)
                    ))
                    .send()
                    .await?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            SkillCmd::Create { name } => {
                println!(
                    "{} Created skill template: skills/{}/SKILL.md",
                    "✓".green(),
                    name
                );
            }
        },

        Commands::Models(cmd) => match cmd {
            ModelsCmd::List => {
                let resp: Value = client
                    .get(format!("{}/api/models", api_base))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(models) = resp.as_array() {
                    println!(
                        "{:<25} {:<15} {:<12} {:<10} {}",
                        "MODEL".bold(),
                        "PROVIDER".bold(),
                        "TIER".bold(),
                        "CONTEXT".bold(),
                        "PRICE (in/out)".bold()
                    );
                    for m in models {
                        println!(
                            "{:<25} {:<15} {:<12} {:<10} ${}/{}",
                            m["id"].as_str().unwrap_or("-"),
                            m["provider"].as_str().unwrap_or("-"),
                            m["tier"].as_str().unwrap_or("-"),
                            m["contextWindow"].as_u64().unwrap_or(0) / 1000,
                            m["inputPrice"].as_f64().unwrap_or(0.0),
                            m["outputPrice"].as_f64().unwrap_or(0.0)
                        );
                    }
                }
            }
            ModelsCmd::Aliases => {
                let resp: Value = client
                    .get(format!("{}/api/models/aliases", api_base))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(obj) = resp.as_object() {
                    for (alias, model) in obj {
                        println!("  {} → {}", alias.cyan(), model.as_str().unwrap_or("-"));
                    }
                }
            }
            ModelsCmd::Providers => {
                let resp: Value = client
                    .get(format!("{}/api/providers", api_base))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(providers) = resp.as_array() {
                    for p in providers {
                        let available = p["available"].as_bool().unwrap_or(false);
                        let icon = if available {
                            "●".green()
                        } else {
                            "○".red()
                        };
                        println!(
                            "  {} {:<20} ({} models)",
                            icon,
                            p["name"].as_str().unwrap_or("-"),
                            p["modelCount"].as_u64().unwrap_or(0)
                        );
                    }
                }
            }
            ModelsCmd::Describe { model } => {
                let resp: Value = client
                    .get(format!("{}/api/models", api_base))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(models) = resp.as_array() {
                    if let Some(m) = models.iter().find(|m| m["id"].as_str() == Some(&model)) {
                        println!("{}", serde_json::to_string_pretty(m)?);
                    } else {
                        println!("{} Model not found: {}", "✗".red(), model);
                    }
                }
            }
        },

        Commands::Security(cmd) => match cmd {
            SecurityCmd::Audit => {
                println!("{} Fetching audit trail...", "→".blue());
                let resp: Value = client
                    .get(format!("{}/api/security/audit/verify", api_base))
                    .send()
                    .await?
                    .json()
                    .await?;
                let valid = resp["valid"].as_bool().unwrap_or(false);
                let icon = if valid { "✓".green() } else { "✗".red() };
                println!(
                    "{} Chain integrity: {} ({} entries)",
                    icon,
                    if valid { "valid" } else { "BROKEN" },
                    resp["entries"].as_u64().unwrap_or(0)
                );
            }
            SecurityCmd::Verify => {
                let resp: Value = client
                    .get(format!("{}/api/security/audit/verify", api_base))
                    .send()
                    .await?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            SecurityCmd::Scan { text } => {
                let resp: Value = client
                    .post(format!("{}/api/security/scan", api_base))
                    .json(&json!({ "text": text }))
                    .send()
                    .await?
                    .json()
                    .await?;
                let safe = resp["safe"].as_bool().unwrap_or(false);
                let icon = if safe { "✓".green() } else { "⚠".yellow() };
                println!(
                    "{} {} (risk: {:.0}%)",
                    icon,
                    if safe { "Clean" } else { "Injection detected" },
                    resp["riskScore"].as_f64().unwrap_or(0.0) * 100.0
                );
            }
        },

        Commands::Approvals(cmd) => match cmd {
            ApprovalsCmd::List => {
                let resp: Value = client
                    .get(format!("{}/api/approvals", api_base))
                    .send()
                    .await?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            ApprovalsCmd::Approve { id } => {
                let resp: Value = client
                    .post(format!("{}/api/approvals/decide", api_base))
                    .json(&json!({ "requestId": id, "decision": "approve" }))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                println!(
                    "{} Approval {}: {}",
                    "✓".green(),
                    id,
                    resp["status"].as_str().unwrap_or("approved")
                );
            }
            ApprovalsCmd::Reject { id } => {
                let resp: Value = client
                    .post(format!("{}/api/approvals/decide", api_base))
                    .json(&json!({ "requestId": id, "decision": "deny" }))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                println!(
                    "{} Approval {}: {}",
                    "✓".green(),
                    id,
                    resp["status"].as_str().unwrap_or("denied")
                );
            }
        },

        Commands::Chat { agent } => {
            let agent_id = agent.unwrap_or_else(|| "default".into());
            let agent_id = validate_id(&agent_id)?;
            println!(
                "{} Quick chat with {}. Type 'exit' to quit.\n",
                "→".blue(),
                agent_id.cyan()
            );
            loop {
                let mut input = String::new();
                print!("{} ", "you>".bold());
                use std::io::Write;
                std::io::stdout().flush()?;
                let bytes_read = std::io::stdin().read_line(&mut input)?;
                if bytes_read == 0 {
                    println!();
                    break;
                }
                let input = input.trim();
                if input == "exit" || input == "quit" {
                    break;
                }
                if input.is_empty() {
                    continue;
                }

                let resp: Value = client
                    .post(format!("{}/api/agents/{}/message", api_base, agent_id))
                    .json(&json!({ "message": input }))
                    .send()
                    .await?
                    .json()
                    .await?;
                println!(
                    "\n{} {}\n",
                    "agent>".blue().bold(),
                    resp["content"].as_str().unwrap_or("(no response)")
                );
            }
        }

        Commands::Message {
            agent,
            text,
            json: is_json,
        } => {
            let agent = validate_id(&agent)?;
            let resp: Value = client
                .post(format!("{}/api/agents/{}/message", api_base, agent))
                .json(&json!({ "message": text }))
                .send()
                .await?
                .json()
                .await?;
            if is_json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                println!("{}", resp["content"].as_str().unwrap_or("(no response)"));
            }
        }

        Commands::Dashboard => {
            println!("{} Opening dashboard at {}/dashboard", "→".blue(), api_base);
            let _ = std::process::Command::new("open")
                .arg(format!("{}/dashboard", api_base))
                .spawn();
        }

        Commands::Up { no_tui, timeout } => {
            use std::io::Write as _;

            let paths = runtime_paths()?;
            initialize_agentos_home(&paths.agentos_home)?;
            // A clean machine has no AGENTOS_API_KEY, and without it almost
            // every worker exits while registering its HTTP routes. Only touch
            // the runtime the config really names.
            if paths.config_path.is_file() {
                let outcome = bootstrap::ensure_api_key(&paths.runtime_dir)?;
                println!("{} {}", "✓".green(), outcome.describe());
            }
            let launch_env = bootstrap::load_dotenv(&paths.runtime_dir)?;
            let mut effects = bootstrap::SystemEffects::new(&paths, launch_env);
            let options = bootstrap::UpOptions {
                launch_tui: !no_tui,
                stage_timeout: Duration::from_secs(timeout),
                poll_interval: Duration::from_millis(250),
            };
            let outcome = tokio::task::spawn_blocking(move || {
                let mut out = std::io::stdout();
                let outcome = bootstrap::run_up(&mut effects, &paths, &options, &mut out);
                let _ = out.flush();
                outcome
            })
            .await??;
            if let bootstrap::UpOutcome::Tui(code) = outcome {
                std::process::exit(code);
            }
        }

        Commands::Doctor { json: is_json } => {
            let paths = runtime_paths()?;
            let credentials = bootstrap::Credentials::inspect(&paths.runtime_dir)?;
            // Probe with the same environment `up` would hand the stack, so a
            // value like AGENTOS_BUS_AUTH_ADDR in the active `.env` is honoured
            // here too. Reading the file changes nothing.
            let probe_env = bootstrap::load_dotenv(&paths.runtime_dir).unwrap_or_default();
            let probes = bootstrap::SystemEffects::new(&paths, probe_env);
            let report = tokio::task::spawn_blocking(move || {
                bootstrap::readiness(&probes, &paths, &credentials)
            })
            .await?;

            if is_json {
                println!("{}", serde_json::to_string_pretty(&report.to_json())?);
            } else {
                report.render(&mut std::io::stdout())?;
            }
        }

        Commands::Mcp => {
            println!("{} Starting MCP server mode (stdio)...", "→".blue());
            eprintln!("agentos MCP server ready");
            tokio::signal::ctrl_c().await?;
        }

        Commands::Trigger(cmd) => match cmd {
            TriggerCmd::List => {
                let resp: Value = client
                    .get(format!("{}/api/triggers", get_api_url()))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(triggers) = resp.as_array() {
                    println!(
                        "{:<20} {:<15} {:<20} {:<30}",
                        "ID".bold(),
                        "TYPE".bold(),
                        "FUNCTION".bold(),
                        "CREATED".bold()
                    );
                    for t in triggers {
                        println!(
                            "{:<20} {:<15} {:<20} {:<30}",
                            t["id"].as_str().unwrap_or("-"),
                            t["type"].as_str().unwrap_or("-"),
                            t["functionId"].as_str().unwrap_or("-"),
                            t["createdAt"].as_str().unwrap_or("-")
                        );
                    }
                } else {
                    println!("No triggers found.");
                }
            }
            TriggerCmd::Create {
                function_id,
                trigger_type,
            } => {
                let resp: Value = client
                    .post(format!("{}/api/triggers", get_api_url()))
                    .json(&json!({ "functionId": function_id, "type": trigger_type }))
                    .send()
                    .await?
                    .json()
                    .await?;
                println!(
                    "{} Created trigger: {}",
                    "✓".green(),
                    resp["id"].as_str().unwrap_or("unknown")
                );
            }
            TriggerCmd::Delete { id } => {
                let id = validate_id(&id)?;
                client
                    .delete(format!("{}/api/triggers/{}", get_api_url(), id))
                    .send()
                    .await?;
                println!("{} Deleted trigger: {}", "✓".green(), id);
            }
        },

        Commands::Channel(cmd) => match cmd {
            ChannelCmd::List => {
                let resp: Value = client
                    .get(format!("{}/api/channels", get_api_url()))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(channels) = resp.as_array() {
                    println!(
                        "{:<20} {:<15} {:<15} {:<30}",
                        "CHANNEL".bold(),
                        "TYPE".bold(),
                        "STATUS".bold(),
                        "CONFIG".bold()
                    );
                    for c in channels {
                        let status = c["enabled"].as_bool().unwrap_or(false);
                        let status_str = if status {
                            "enabled".green().to_string()
                        } else {
                            "disabled".red().to_string()
                        };
                        println!(
                            "{:<20} {:<15} {:<15} {:<30}",
                            c["id"].as_str().unwrap_or("-"),
                            c["type"].as_str().unwrap_or("-"),
                            status_str,
                            c["config"].as_str().unwrap_or("-")
                        );
                    }
                } else {
                    println!("No channels configured.");
                }
            }
            ChannelCmd::Setup { channel } => {
                let resp: Value = client
                    .post(format!("{}/api/channels", get_api_url()))
                    .json(&json!({ "channel": channel }))
                    .send()
                    .await?
                    .json()
                    .await?;
                if resp["success"].as_bool() != Some(true) {
                    anyhow::bail!(
                        "Channel {} is not ready: {}",
                        channel,
                        resp["error"].as_str().unwrap_or("unknown error")
                    );
                }
                println!("{} Channel {} is ready", "✓".green(), channel);
            }
            ChannelCmd::Test { channel } => {
                let channel = validate_id(&channel)?;
                let resp: Value = client
                    .post(format!("{}/api/channels/{}/test", get_api_url(), channel))
                    .send()
                    .await?
                    .json()
                    .await?;
                let success = resp["success"].as_bool().unwrap_or(false);
                if success {
                    println!("{} Channel {} test passed", "✓".green(), channel);
                } else {
                    println!(
                        "{} Channel {} test failed: {}",
                        "✗".red(),
                        channel,
                        resp["error"].as_str().unwrap_or("unknown error")
                    );
                }
            }
        },

        Commands::Config(cmd) => match cmd {
            ConfigCmd::Show => {
                let config_path = agentos_config_path()?;
                if config_path.exists() {
                    let content = std::fs::read_to_string(&config_path)?;
                    println!("{}", content);
                } else {
                    println!(
                        "{} No config file found. Run {} first.",
                        "→".yellow(),
                        "agentos init".cyan()
                    );
                }
            }
            ConfigCmd::Get { key } => {
                let config_path = agentos_config_path()?;
                if config_path.exists() {
                    let content = std::fs::read_to_string(&config_path)?;
                    let table: toml::Table = content.parse()?;
                    if let Some(val) = table.get(&key) {
                        println!("{} = {}", key.cyan(), val);
                    } else {
                        println!("{} Key not found: {}", "✗".red(), key);
                    }
                } else {
                    println!("{} No config file found.", "✗".red());
                }
            }
            ConfigCmd::Set { key, value } => {
                let config_path = agentos_config_path()?;
                let mut table: toml::Table = if config_path.exists() {
                    std::fs::read_to_string(&config_path)?.parse()?
                } else {
                    toml::Table::new()
                };
                table.insert(key.clone(), toml::Value::String(value.clone()));
                std::fs::write(&config_path, toml::to_string_pretty(&table)?)?;
                println!("{} Set {} = {}", "✓".green(), key.cyan(), value);
            }
            ConfigCmd::Unset { key } => {
                let config_path = agentos_config_path()?;
                if config_path.exists() {
                    let content = std::fs::read_to_string(&config_path)?;
                    let mut table: toml::Table = content.parse()?;
                    if table.remove(&key).is_some() {
                        std::fs::write(&config_path, toml::to_string_pretty(&table)?)?;
                        println!("{} Removed key: {}", "✓".green(), key);
                    } else {
                        println!("{} Key not found: {}", "✗".red(), key);
                    }
                } else {
                    println!("{} No config file found.", "✗".red());
                }
            }
            ConfigCmd::SetKey { provider, key } => {
                let config_path = agentos_config_path()?;
                let mut table: toml::Table = if config_path.exists() {
                    std::fs::read_to_string(&config_path)?.parse()?
                } else {
                    toml::Table::new()
                };
                let keys_table = table
                    .entry("keys")
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                if let toml::Value::Table(kt) = keys_table {
                    kt.insert(provider.clone(), toml::Value::String(key.clone()));
                }
                std::fs::write(&config_path, toml::to_string_pretty(&table)?)?;
                println!("{} API key set for {}", "✓".green(), provider.cyan());

                // No worker reads config.toml; llm-router resolves credentials
                // from the process environment only. Write the value where it
                // is actually read, or say plainly that it is inert.
                match bootstrap::provider_variable(&provider) {
                    Some(variable) => {
                        let paths = runtime_paths()?;
                        let written =
                            bootstrap::set_dotenv_value(&paths.runtime_dir, variable, &key)?;
                        println!(
                            "{} {variable} written to {} (mode 0600) — this is the value the workers read",
                            "✓".green(),
                            written.display()
                        );
                    }
                    None => println!(
                        "{} {} is not a provider `workers/llm-router` knows, so this key stays in {} and no worker reads it",
                        "⚠".yellow(),
                        provider,
                        config_path.display()
                    ),
                }
            }
            ConfigCmd::Keys => {
                let config_path = agentos_config_path()?;
                if config_path.exists() {
                    let content = std::fs::read_to_string(&config_path)?;
                    let table: toml::Table = content.parse()?;
                    if let Some(toml::Value::Table(keys)) = table.get("keys") {
                        println!("{:<20} {:<10}", "PROVIDER".bold(), "STATUS".bold());
                        for (provider, val) in keys {
                            let masked = if let toml::Value::String(s) = val {
                                if s.len() > 8 {
                                    format!("{}...{}", &s[..4], &s[s.len() - 4..])
                                } else {
                                    "****".into()
                                }
                            } else {
                                "-".into()
                            };
                            println!("{:<20} {}", provider, masked);
                        }
                    } else {
                        println!("No API keys configured.");
                    }
                } else {
                    println!("{} No config file found.", "✗".red());
                }
            }
        },

        Commands::Memory(cmd) => match cmd {
            MemoryCmd::Get { agent, key } => {
                let agent = validate_id(&agent)?;
                let resp: Value = client
                    .get(format!(
                        "{}/api/memory/{}?agent={}",
                        get_api_url(),
                        urlencoding::encode(&key),
                        urlencoding::encode(agent)
                    ))
                    .send()
                    .await?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            MemoryCmd::Set { agent, key, value } => {
                let agent = validate_id(&agent)?;
                let resp: Value = client
                    .post(format!("{}/api/memory", get_api_url()))
                    .json(&json!({ "agent": agent, "key": key, "value": value }))
                    .send()
                    .await?
                    .json()
                    .await?;
                println!(
                    "{} Memory set: {} = {} (agent: {})",
                    "✓".green(),
                    key.cyan(),
                    value,
                    agent
                );
                let _ = resp;
            }
            MemoryCmd::Delete { agent, key } => {
                let agent = validate_id(&agent)?;
                client
                    .delete(format!(
                        "{}/api/memory/{}?agent={}",
                        get_api_url(),
                        urlencoding::encode(&key),
                        urlencoding::encode(agent)
                    ))
                    .send()
                    .await?;
                println!("{} Memory deleted: {} (agent: {})", "✓".green(), key, agent);
            }
            MemoryCmd::List { agent } => {
                let agent = validate_id(&agent)?;
                let resp: Value = client
                    .get(format!(
                        "{}/api/memory?agent={}",
                        get_api_url(),
                        urlencoding::encode(agent)
                    ))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(items) = resp.as_array() {
                    println!("{:<30} {:<50}", "KEY".bold(), "VALUE".bold());
                    for item in items {
                        let val_str = if let Some(s) = item["value"].as_str() {
                            s.to_string()
                        } else {
                            item["value"].to_string()
                        };
                        let truncated = if val_str.len() > 47 {
                            format!("{}...", &val_str[..47])
                        } else {
                            val_str
                        };
                        println!(
                            "{:<30} {:<50}",
                            item["key"].as_str().unwrap_or("-"),
                            truncated
                        );
                    }
                } else {
                    println!("No memory entries for agent: {}", agent);
                }
            }
        },

        Commands::Logs { lines, follow } => {
            if follow {
                println!("{} Streaming logs (Ctrl+C to stop)...\n", "→".blue());
                let resp = client
                    .get(format!("{}/api/dashboard/logs/stream", get_api_url()))
                    .send()
                    .await?;
                let mut stream = resp.bytes_stream();
                use futures_util::StreamExt;
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            let text = String::from_utf8_lossy(&bytes);
                            for line in text.lines() {
                                if let Some(data) = line.strip_prefix("data: ") {
                                    if let Ok(entry) = serde_json::from_str::<Value>(data) {
                                        print_log_entry(&entry);
                                    } else {
                                        println!("{}", data);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("{} Stream error: {}", "✗".red(), e);
                            break;
                        }
                    }
                }
            } else {
                let resp: Value = client
                    .get(format!(
                        "{}/api/dashboard/logs?lines={}",
                        get_api_url(),
                        lines
                    ))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(entries) = resp.as_array() {
                    for entry in entries {
                        print_log_entry(entry);
                    }
                } else {
                    println!("{}", serde_json::to_string_pretty(&resp)?);
                }
            }
        }

        Commands::Vault(cmd) => match cmd {
            VaultCmd::Init => {
                let resp: Value = client
                    .post(format!("{}/api/vault/init", get_api_url()))
                    .send()
                    .await?
                    .json()
                    .await?;
                println!(
                    "{} Vault initialized: {}",
                    "✓".green(),
                    resp["status"].as_str().unwrap_or("ok")
                );
            }
            VaultCmd::Set { key, value } => {
                client
                    .post(format!(
                        "{}/api/vault/{}",
                        get_api_url(),
                        urlencoding::encode(&key)
                    ))
                    .json(&json!({ "value": value }))
                    .send()
                    .await?;
                println!("{} Vault secret set: {}", "✓".green(), key.cyan());
            }
            VaultCmd::List => {
                let resp: Value = client
                    .get(format!("{}/api/vault", get_api_url()))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(secrets) = resp.as_array() {
                    println!("{:<30} {:<20}", "KEY".bold(), "CREATED".bold());
                    for s in secrets {
                        println!(
                            "{:<30} {:<20}",
                            s["key"].as_str().unwrap_or("-"),
                            s["createdAt"].as_str().unwrap_or("-")
                        );
                    }
                } else {
                    println!("Vault is empty.");
                }
            }
            VaultCmd::Remove { key } => {
                client
                    .delete(format!(
                        "{}/api/vault/{}",
                        get_api_url(),
                        urlencoding::encode(&key)
                    ))
                    .send()
                    .await?;
                println!("{} Vault secret removed: {}", "✓".green(), key);
            }
        },

        Commands::Migrate(cmd) => match cmd {
            MigrateCmd::Scan => {
                println!("{} Scanning for migratable resources...", "→".blue());
                let resp: Value = client
                    .post(format!("{}/api/migrate/scan", get_api_url()))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(results) = resp["results"].as_array() {
                    println!(
                        "{:<30} {:<15} {:<20}",
                        "RESOURCE".bold(),
                        "TYPE".bold(),
                        "STATUS".bold()
                    );
                    for r in results {
                        println!(
                            "{:<30} {:<15} {:<20}",
                            r["name"].as_str().unwrap_or("-"),
                            r["type"].as_str().unwrap_or("-"),
                            r["status"].as_str().unwrap_or("-")
                        );
                    }
                } else {
                    println!("{}", serde_json::to_string_pretty(&resp)?);
                }
            }
            MigrateCmd::OpenClaw { dry_run } => {
                println!(
                    "{} Migrating from OpenClaw{}...",
                    "→".blue(),
                    if dry_run { " (dry run)" } else { "" }
                );
                let resp: Value = client
                    .post(format!("{}/api/migrate/openclaw", get_api_url()))
                    .json(&json!({ "dryRun": dry_run }))
                    .send()
                    .await?
                    .json()
                    .await?;
                println!(
                    "{} Migration {}: {} items processed",
                    "✓".green(),
                    if dry_run { "preview" } else { "complete" },
                    resp["count"].as_u64().unwrap_or(0)
                );
            }
            MigrateCmd::LangChain { dry_run } => {
                println!(
                    "{} Migrating from LangChain{}...",
                    "→".blue(),
                    if dry_run { " (dry run)" } else { "" }
                );
                let resp: Value = client
                    .post(format!("{}/api/migrate/langchain", get_api_url()))
                    .json(&json!({ "dryRun": dry_run }))
                    .send()
                    .await?
                    .json()
                    .await?;
                println!(
                    "{} Migration {}: {} items processed",
                    "✓".green(),
                    if dry_run { "preview" } else { "complete" },
                    resp["count"].as_u64().unwrap_or(0)
                );
            }
            MigrateCmd::Report => {
                let resp: Value = client
                    .get(format!("{}/api/migrate/report", get_api_url()))
                    .send()
                    .await?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
        },

        Commands::Replay(cmd) => match cmd {
            ReplayCmd::Get { session_id } => {
                let session_id = validate_id(&session_id)?;
                let resp: Value = client
                    .get(format!("{}/api/replay/{}", get_api_url(), session_id))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(actions) = resp.as_array() {
                    println!(
                        "{:<10} {:<12} {:<18} {:<10} {}",
                        "SEQ".bold(),
                        "ACTION".bold(),
                        "TIMESTAMP".bold(),
                        "DURATION".bold(),
                        "DATA".bold()
                    );
                    for a in actions {
                        let ts = a["timestamp"].as_u64().unwrap_or(0);
                        let ts_str = format_epoch_ms(ts);
                        let action = a["action"].as_str().unwrap_or("-");
                        let action_colored = match action {
                            "llm_call" => action.cyan().to_string(),
                            "tool_call" => action.yellow().to_string(),
                            "tool_result" => action.green().to_string(),
                            _ => action.to_string(),
                        };
                        let data_str = a["data"].to_string();
                        let truncated = if data_str.len() > 60 {
                            format!("{}...", &data_str[..60])
                        } else {
                            data_str
                        };
                        println!(
                            "{:<10} {:<12} {:<18} {:<10} {}",
                            a["sequence"].as_u64().unwrap_or(0),
                            action_colored,
                            ts_str,
                            format!("{}ms", a["durationMs"].as_u64().unwrap_or(0)),
                            truncated
                        );
                    }
                } else {
                    println!("{}", serde_json::to_string_pretty(&resp)?);
                }
            }
            ReplayCmd::List { agent } => {
                let url = if let Some(ref agent) = agent {
                    format!(
                        "{}/api/replay/search?agentId={}",
                        get_api_url(),
                        urlencoding::encode(agent)
                    )
                } else {
                    format!("{}/api/replay/search", get_api_url())
                };
                let resp: Value = client.get(&url).send().await?.json().await?;
                if let Some(sessions) = resp.as_array() {
                    println!(
                        "{:<36} {:<20} {:<10} {:<25}",
                        "SESSION".bold(),
                        "AGENT".bold(),
                        "ACTIONS".bold(),
                        "STARTED".bold()
                    );
                    for s in sessions {
                        let ts = s["startTime"].as_u64().unwrap_or(0);
                        let ts_str = format_epoch_ms(ts);
                        println!(
                            "{:<36} {:<20} {:<10} {:<25}",
                            s["sessionId"].as_str().unwrap_or("-"),
                            s["agentId"].as_str().unwrap_or("-"),
                            s["actionCount"].as_u64().unwrap_or(0),
                            ts_str
                        );
                    }
                } else {
                    println!("No replay sessions found.");
                }
            }
            ReplayCmd::Summary { session_id } => {
                let session_id = validate_id(&session_id)?;
                let resp: Value = client
                    .get(format!(
                        "{}/api/replay/{}/summary",
                        get_api_url(),
                        session_id
                    ))
                    .send()
                    .await?
                    .json()
                    .await?;
                println!("{} Session Replay Summary\n", "→".blue());
                println!(
                    "  Session:    {}",
                    resp["sessionId"].as_str().unwrap_or("-").cyan()
                );
                println!("  Agent:      {}", resp["agentId"].as_str().unwrap_or("-"));
                println!(
                    "  Duration:   {}ms",
                    resp["totalDuration"].as_u64().unwrap_or(0)
                );
                println!("  Iterations: {}", resp["iterations"].as_u64().unwrap_or(0));
                println!("  Tool calls: {}", resp["toolCalls"].as_u64().unwrap_or(0));
                println!("  Tokens:     {}", resp["tokensUsed"].as_u64().unwrap_or(0));
                println!("  Cost:       ${:.4}", resp["cost"].as_f64().unwrap_or(0.0));
                if let Some(tools) = resp["tools"].as_array() {
                    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t.as_str()).collect();
                    println!("  Tools:      {}", tool_names.join(", "));
                }
            }
        },

        Commands::Sessions(cmd) => match cmd {
            SessionsCmd::List { agent } => {
                let url = if let Some(ref agent) = agent {
                    format!(
                        "{}/api/sessions?agent={}",
                        get_api_url(),
                        urlencoding::encode(agent)
                    )
                } else {
                    format!("{}/api/sessions", get_api_url())
                };
                let resp: Value = client.get(&url).send().await?.json().await?;
                if let Some(sessions) = resp.as_array() {
                    println!(
                        "{:<36} {:<20} {:<15} {:<20}",
                        "ID".bold(),
                        "AGENT".bold(),
                        "STATUS".bold(),
                        "STARTED".bold()
                    );
                    for s in sessions {
                        println!(
                            "{:<36} {:<20} {:<15} {:<20}",
                            s["id"].as_str().unwrap_or("-"),
                            s["agent"].as_str().unwrap_or("-"),
                            s["status"].as_str().unwrap_or("-"),
                            s["startedAt"].as_str().unwrap_or("-")
                        );
                    }
                } else {
                    println!("No active sessions.");
                }
            }
            SessionsCmd::Delete { id } => {
                let id = validate_id(&id)?;
                client
                    .delete(format!("{}/api/sessions/{}", get_api_url(), id))
                    .send()
                    .await?;
                println!("{} Session deleted: {}", "✓".green(), id);
            }
        },

        Commands::Cron(cmd) => match cmd {
            CronCmd::List => {
                let resp: Value = client
                    .get(format!("{}/api/cron", get_api_url()))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(jobs) = resp.as_array() {
                    println!(
                        "{:<20} {:<20} {:<20} {:<10}",
                        "ID".bold(),
                        "EXPRESSION".bold(),
                        "FUNCTION".bold(),
                        "ENABLED".bold()
                    );
                    for j in jobs {
                        let enabled = j["enabled"].as_bool().unwrap_or(false);
                        let status = if enabled {
                            "yes".green().to_string()
                        } else {
                            "no".red().to_string()
                        };
                        println!(
                            "{:<20} {:<20} {:<20} {:<10}",
                            j["id"].as_str().unwrap_or("-"),
                            j["expression"].as_str().unwrap_or("-"),
                            j["functionId"].as_str().unwrap_or("-"),
                            status
                        );
                    }
                } else {
                    println!("No cron jobs configured.");
                }
            }
            CronCmd::Create {
                expression,
                function_id,
            } => {
                let resp: Value = client
                    .post(format!("{}/api/cron", get_api_url()))
                    .json(&json!({ "expression": expression, "functionId": function_id }))
                    .send()
                    .await?
                    .json()
                    .await?;
                println!(
                    "{} Created cron job: {}",
                    "✓".green(),
                    resp["id"].as_str().unwrap_or("unknown")
                );
            }
            CronCmd::Delete { id } => {
                let id = validate_id(&id)?;
                client
                    .delete(format!("{}/api/cron/{}", get_api_url(), id))
                    .send()
                    .await?;
                println!("{} Deleted cron job: {}", "✓".green(), id);
            }
            CronCmd::Enable { id } => {
                let id = validate_id(&id)?;
                client
                    .patch(format!("{}/api/cron/{}", get_api_url(), id))
                    .json(&json!({ "enabled": true }))
                    .send()
                    .await?;
                println!("{} Cron job {} enabled", "✓".green(), id);
            }
            CronCmd::Disable { id } => {
                let id = validate_id(&id)?;
                client
                    .patch(format!("{}/api/cron/{}", get_api_url(), id))
                    .json(&json!({ "enabled": false }))
                    .send()
                    .await?;
                println!("{} Cron job {} disabled", "✓".green(), id);
            }
        },

        Commands::Integrations { query } => {
            let url = if let Some(ref q) = query {
                format!(
                    "{}/api/integrations?query={}",
                    get_api_url(),
                    urlencoding::encode(q)
                )
            } else {
                format!("{}/api/integrations", get_api_url())
            };
            let resp: Value = client.get(&url).send().await?.json().await?;
            if let Some(integrations) = resp.as_array() {
                println!(
                    "{:<25} {:<15} {:<15} {:<30}",
                    "NAME".bold(),
                    "TYPE".bold(),
                    "STATUS".bold(),
                    "DESCRIPTION".bold()
                );
                for i in integrations {
                    let status = i["status"].as_str().unwrap_or("unknown");
                    let status_colored = match status {
                        "active" => status.green().to_string(),
                        "inactive" => status.yellow().to_string(),
                        _ => status.to_string(),
                    };
                    println!(
                        "{:<25} {:<15} {:<15} {:<30}",
                        i["name"].as_str().unwrap_or("-"),
                        i["type"].as_str().unwrap_or("-"),
                        status_colored,
                        i["description"].as_str().unwrap_or("-")
                    );
                }
            } else {
                println!("No integrations found.");
            }
        }

        Commands::Onboard { quick } => {
            use dialoguer::{Input, Select};

            println!("\n{} Welcome to AgentOS Setup\n", "→".blue().bold());

            let config_dir = agentos_home_dir()?;
            initialize_agentos_home(&config_dir)?;
            println!(
                "  {} Created {} directories",
                "✓".green(),
                config_dir.display()
            );

            // The machine's own key. Generated, never asked for, never invented
            // from a provider credential.
            let paths = runtime_paths()?;
            let key_outcome = bootstrap::ensure_api_key(&paths.runtime_dir)?;
            println!("  {} {}", "✓".green(), key_outcome.describe());

            // The provider credential is a different thing entirely: workers
            // read it from the environment, so it goes into the active `.env`.
            let provider_key: String = if quick {
                std::env::var("ANTHROPIC_API_KEY").unwrap_or_default()
            } else {
                Input::new()
                    .with_prompt("  Anthropic API key for ANTHROPIC_API_KEY (Enter to skip)")
                    .allow_empty(true)
                    .interact_text()?
            };
            if !provider_key.trim().is_empty() {
                let written = bootstrap::set_dotenv_value(
                    &paths.runtime_dir,
                    "ANTHROPIC_API_KEY",
                    provider_key.trim(),
                )?;
                println!(
                    "  {} ANTHROPIC_API_KEY written to {}",
                    "✓".green(),
                    written.display()
                );
            }

            let models = vec![
                "claude-opus-4-6",
                "claude-sonnet-4-6",
                "gpt-4o",
                "gemini-2.0-flash",
            ];
            let default_model = if quick {
                "claude-opus-4-6".to_string()
            } else {
                let selection = Select::new()
                    .with_prompt("  Select default model")
                    .items(&models)
                    .default(0)
                    .interact()?;
                models[selection].to_string()
            };

            let mut config = toml::Table::new();
            config.insert(
                "default_model".into(),
                toml::Value::String(default_model.clone()),
            );
            config.insert("api_url".into(), toml::Value::String(get_api_url()));

            let config_path = config_dir.join("config.toml");
            std::fs::write(&config_path, toml::to_string_pretty(&config)?)?;
            println!(
                "  {} Config written to {}",
                "✓".green(),
                config_path.display()
            );
            println!("  {} Default model: {}", "✓".green(), default_model.cyan());

            println!(
                "\n{} Setup complete! Run {} to start the stack.",
                "✓".green().bold(),
                "agentos up".cyan()
            );
        }

        Commands::Reset { confirm } => {
            if !confirm {
                println!("{} This will reset all AgentOS state.", "⚠".yellow());
                println!("  Run with {} to confirm.", "--confirm".cyan());
                return Ok(());
            }

            println!("{} Resetting AgentOS...", "→".blue());

            match client
                .delete(format!("{}/api/state/reset", get_api_url()))
                .send()
                .await
            {
                Ok(_) => println!("  {} Server state cleared", "✓".green()),
                Err(_) => println!(
                    "  {} Server not reachable (skipping remote reset)",
                    "→".yellow()
                ),
            }

            let config_dir = agentos_home_dir()?;
            let state_dir = config_dir.join("state");
            if state_dir.exists() {
                std::fs::remove_dir_all(&state_dir)?;
                std::fs::create_dir_all(&state_dir)?;
                println!(
                    "  {} Local state cleared ({})",
                    "✓".green(),
                    state_dir.display()
                );
            }

            println!("{} Reset complete.", "✓".green());
        }

        Commands::Add { name, key } => {
            let resp: Value = client
                .post(format!("{}/api/integrations", get_api_url()))
                .json(&json!({ "name": name, "key": key }))
                .send()
                .await?
                .json()
                .await?;
            println!(
                "{} Added integration: {}",
                "✓".green(),
                resp["id"].as_str().unwrap_or(&name)
            );
        }

        Commands::Remove { name } => {
            client
                .delete(format!(
                    "{}/api/integrations/{}",
                    get_api_url(),
                    urlencoding::encode(&name)
                ))
                .send()
                .await?;
            println!("{} Removed: {}", "✓".green(), name);
        }

        Commands::Tui => {
            println!("{} Starting TUI...", "→".blue());
            let runtime_dir = runtime_paths().ok().map(|paths| paths.runtime_dir);
            match find_tui_binary(runtime_dir.as_deref()) {
                Some(tui_path) => {
                    let status = Command::new(&tui_path).status()?;
                    std::process::exit(status.code().unwrap_or(1));
                }
                None => {
                    println!(
                        "{} {TUI_BINARY} binary not found. {}",
                        "✗".red(),
                        bootstrap::WORKSPACE_BUILD_HINT
                    );
                }
            }
        }

        Commands::Completion { shell } => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            let shell = match shell.as_str() {
                "bash" => clap_complete::Shell::Bash,
                "zsh" => clap_complete::Shell::Zsh,
                "fish" => clap_complete::Shell::Fish,
                "powershell" => clap_complete::Shell::PowerShell,
                _ => {
                    println!(
                        "{} Unsupported shell: {}. Use bash, zsh, fish, or powershell.",
                        "✗".red(),
                        shell
                    );
                    return Ok(());
                }
            };
            clap_complete::generate(shell, &mut cmd, "agentos", &mut std::io::stdout());
        }
    }

    Ok(())
}

fn agentos_config_path() -> Result<PathBuf> {
    Ok(agentos_home_dir()?.join("config.toml"))
}

fn get_api_url() -> String {
    std::env::var("AGENTOS_API_URL").unwrap_or_else(|_| API_BASE.to_string())
}

fn format_epoch_ms(ms: u64) -> String {
    if ms == 0 {
        return "-".into();
    }
    let secs = ms / 1000;
    let hours = (secs / 3600) % 24;
    let minutes = (secs / 60) % 60;
    let seconds = secs % 60;
    let days = secs / 86400;
    if days > 18000 {
        format!("{}d {}:{:02}:{:02}", days, hours, minutes, seconds)
    } else {
        format!("{}:{:02}:{:02}", hours, minutes, seconds)
    }
}

fn print_log_entry(entry: &Value) {
    let level = entry["level"].as_str().unwrap_or("INFO");
    let level_colored = match level {
        "ERROR" => level.red().to_string(),
        "WARN" => level.yellow().to_string(),
        "DEBUG" => level.dimmed().to_string(),
        _ => level.to_string(),
    };
    let ts = entry["timestamp"].as_str().unwrap_or("");
    let short_ts = if ts.len() > 19 { &ts[..19] } else { ts };
    let msg = entry["message"].as_str().unwrap_or("-");
    println!("{} [{}] {}", short_ts.dimmed(), level_colored, msg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_validate_id_valid_alphanumeric() {
        assert!(validate_id("abc123").is_ok());
    }

    #[test]
    fn test_validate_id_with_hyphens() {
        assert!(validate_id("my-agent-1").is_ok());
    }

    #[test]
    fn test_validate_id_with_underscores() {
        assert!(validate_id("my_agent_1").is_ok());
    }

    #[test]
    fn test_validate_id_empty() {
        assert!(validate_id("").is_err());
    }

    #[test]
    fn test_validate_id_too_long() {
        let long_id = "a".repeat(257);
        assert!(validate_id(&long_id).is_err());
    }

    #[test]
    fn test_validate_id_max_length() {
        let max_id = "a".repeat(256);
        assert!(validate_id(&max_id).is_ok());
    }

    #[test]
    fn test_validate_id_special_chars() {
        assert!(validate_id("bad@id").is_err());
    }

    #[test]
    fn test_validate_id_spaces() {
        assert!(validate_id("bad id").is_err());
    }

    #[test]
    fn test_validate_id_dots() {
        assert!(validate_id("bad.id").is_err());
    }

    #[test]
    fn test_validate_id_slashes() {
        assert!(validate_id("bad/id").is_err());
    }

    #[test]
    fn test_validate_id_single_char() {
        assert!(validate_id("a").is_ok());
    }

    #[test]
    fn test_validate_id_numeric_only() {
        assert!(validate_id("12345").is_ok());
    }

    #[test]
    fn test_validate_id_returns_same_str() {
        let result = validate_id("test-id").unwrap();
        assert_eq!(result, "test-id");
    }

    #[test]
    fn test_api_base_constant() {
        assert_eq!(API_BASE, "http://localhost:3111");
    }

    #[test]
    fn test_get_api_url_default() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("AGENTOS_API_URL");
        unsafe {
            std::env::remove_var("AGENTOS_API_URL");
        }
        let result = get_api_url();
        restore_test_env("AGENTOS_API_URL", previous);
        assert_eq!(result, "http://localhost:3111");
    }

    #[test]
    fn test_get_api_url_custom() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("AGENTOS_API_URL");
        unsafe {
            std::env::set_var("AGENTOS_API_URL", "http://custom:8080");
        }
        let url = get_api_url();
        restore_test_env("AGENTOS_API_URL", previous);
        assert_eq!(url, "http://custom:8080");
    }

    #[test]
    fn test_cli_command_factory() {
        let cmd = Cli::command();
        assert_eq!(cmd.get_name(), "agentos");
    }

    #[test]
    fn test_cli_has_init_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"init"));
    }

    #[test]
    fn test_cli_has_start_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"start"));
    }

    #[test]
    fn test_cli_has_stop_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"stop"));
    }

    #[test]
    fn test_cli_has_status_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"status"));
    }

    #[test]
    fn test_cli_has_health_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"health"));
    }

    #[test]
    fn test_cli_has_agent_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"agent"));
    }

    #[test]
    fn test_cli_has_workflow_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"workflow"));
    }

    #[test]
    fn test_cli_has_skill_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"skill"));
    }

    #[test]
    fn test_cli_has_memory_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"memory"));
    }

    #[test]
    fn test_cli_has_security_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"security"));
    }

    #[test]
    fn test_cli_has_vault_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"vault"));
    }

    #[test]
    fn test_cli_has_completion_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"completion"));
    }

    #[test]
    fn test_cli_has_onboard_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"onboard"));
    }

    #[test]
    fn test_cli_has_reset_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"reset"));
    }

    #[test]
    fn test_cli_has_tui_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"tui"));
    }

    #[test]
    fn test_cli_subcommand_count() {
        let cmd = Cli::command();
        let count = cmd.get_subcommands().count();
        assert!(
            count >= 15,
            "Expected at least 15 subcommands, got {}",
            count
        );
    }

    #[test]
    fn test_print_log_entry_error() {
        let entry =
            json!({"level": "ERROR", "timestamp": "2026-01-01T00:00:00Z", "message": "test error"});
        print_log_entry(&entry);
    }

    #[test]
    fn test_print_log_entry_warn() {
        let entry =
            json!({"level": "WARN", "timestamp": "2026-01-01T00:00:00Z", "message": "test warn"});
        print_log_entry(&entry);
    }

    #[test]
    fn test_print_log_entry_debug() {
        let entry =
            json!({"level": "DEBUG", "timestamp": "2026-01-01T00:00:00Z", "message": "test debug"});
        print_log_entry(&entry);
    }

    #[test]
    fn test_print_log_entry_info() {
        let entry =
            json!({"level": "INFO", "timestamp": "2026-01-01T00:00:00Z", "message": "test info"});
        print_log_entry(&entry);
    }

    #[test]
    fn test_print_log_entry_missing_fields() {
        let entry = json!({});
        print_log_entry(&entry);
    }

    #[test]
    fn test_print_log_entry_short_timestamp() {
        let entry = json!({"level": "INFO", "timestamp": "2026-01-01", "message": "short ts"});
        print_log_entry(&entry);
    }

    #[test]
    fn test_print_log_entry_long_timestamp() {
        let entry = json!({"level": "INFO", "timestamp": "2026-01-01T00:00:00.123456789Z", "message": "long ts"});
        print_log_entry(&entry);
    }

    #[test]
    fn test_validate_id_unicode() {
        assert!(validate_id("café").is_ok());
    }

    #[test]
    fn test_validate_id_mixed_valid() {
        assert!(validate_id("agent-v2_test-123").is_ok());
    }

    #[test]
    fn test_validate_id_start_with_number() {
        assert!(validate_id("1agent").is_ok());
    }

    #[test]
    fn test_validate_id_start_with_hyphen() {
        assert!(validate_id("-agent").is_ok());
    }

    #[test]
    fn test_validate_id_start_with_underscore() {
        assert!(validate_id("_agent").is_ok());
    }

    #[test]
    fn test_validate_id_colons() {
        assert!(validate_id("bad:id").is_err());
    }

    #[test]
    fn test_validate_id_hash() {
        assert!(validate_id("bad#id").is_err());
    }

    #[test]
    fn test_validate_id_percent() {
        assert!(validate_id("bad%id").is_err());
    }

    #[test]
    fn test_validate_id_newline() {
        assert!(validate_id("bad\nid").is_err());
    }

    #[test]
    fn test_validate_id_tab() {
        assert!(validate_id("bad\tid").is_err());
    }

    #[test]
    fn test_validate_id_boundary_255() {
        let id = "a".repeat(255);
        assert!(validate_id(&id).is_ok());
    }

    #[test]
    fn test_cli_has_trigger_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"trigger"));
    }

    #[test]
    fn test_cli_has_channel_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"channel"));
    }

    #[test]
    fn test_cli_has_config_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"config"));
    }

    #[test]
    fn test_cli_has_models_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"models"));
    }

    #[test]
    fn test_cli_has_approvals_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"approvals"));
    }

    #[test]
    fn test_cli_has_cron_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"cron"));
    }

    #[test]
    fn test_cli_has_sessions_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"sessions"));
    }

    #[test]
    fn test_format_epoch_ms_zero() {
        assert_eq!(format_epoch_ms(0), "-");
    }

    #[test]
    fn test_format_epoch_ms_one_second() {
        assert_eq!(format_epoch_ms(1000), "0:00:01");
    }

    #[test]
    fn test_format_epoch_ms_one_minute() {
        assert_eq!(format_epoch_ms(60_000), "0:01:00");
    }

    #[test]
    fn test_format_epoch_ms_one_hour() {
        assert_eq!(format_epoch_ms(3_600_000), "1:00:00");
    }

    #[test]
    fn test_format_epoch_ms_complex() {
        assert_eq!(format_epoch_ms(3_661_000), "1:01:01");
    }

    #[test]
    fn test_agentos_config_path_ends_with_config() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("AGENTOS_HOME");
        unsafe {
            std::env::remove_var("AGENTOS_HOME");
        }
        let path = agentos_config_path().unwrap();
        restore_test_env("AGENTOS_HOME", previous);
        assert!(path.ends_with(".agentos/config.toml"));
    }

    #[test]
    fn test_print_log_entry_unknown_level() {
        let entry =
            json!({"level": "TRACE", "timestamp": "2026-01-01T00:00:00Z", "message": "trace msg"});
        print_log_entry(&entry);
    }

    #[test]
    fn test_validate_id_emoji_rejected() {
        assert!(validate_id("\u{1f600}").is_err());
    }

    #[test]
    fn test_validate_id_chinese_chars_accepted() {
        assert!(validate_id("\u{4e16}\u{754c}").is_ok());
    }

    #[test]
    fn test_validate_id_only_hyphens() {
        assert!(validate_id("---").is_ok());
    }

    #[test]
    fn test_validate_id_only_underscores() {
        assert!(validate_id("___").is_ok());
    }

    #[test]
    fn test_validate_id_mixed_hyphen_underscore() {
        assert!(validate_id("a-b_c-d_e").is_ok());
    }

    #[test]
    fn test_validate_id_null_byte() {
        assert!(validate_id("bad\0id").is_err());
    }

    #[test]
    fn test_validate_id_backslash() {
        assert!(validate_id("bad\\id").is_err());
    }

    #[test]
    fn test_validate_id_equals_sign() {
        assert!(validate_id("key=value").is_err());
    }

    #[test]
    fn test_validate_id_question_mark() {
        assert!(validate_id("query?param").is_err());
    }

    #[test]
    fn test_format_epoch_ms_half_second() {
        assert_eq!(format_epoch_ms(500), "0:00:00");
    }

    #[test]
    fn test_format_epoch_ms_23h_59m_59s() {
        let ms = (23 * 3600 + 59 * 60 + 59) * 1000;
        assert_eq!(format_epoch_ms(ms), "23:59:59");
    }

    #[test]
    fn test_format_epoch_ms_large_value_days() {
        let ms: u64 = 20000 * 86400 * 1000;
        let result = format_epoch_ms(ms);
        assert!(result.contains("d "));
    }

    #[test]
    fn test_format_epoch_ms_one_ms() {
        assert_eq!(format_epoch_ms(1), "0:00:00");
    }

    #[test]
    fn test_cli_has_chat_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"chat"));
    }

    #[test]
    fn test_cli_has_message_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"message"));
    }

    #[test]
    fn test_cli_has_dashboard_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"dashboard"));
    }

    #[test]
    fn test_cli_has_doctor_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"doctor"));
    }

    #[test]
    fn test_cli_has_logs_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"logs"));
    }

    #[test]
    fn test_cli_has_replay_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"replay"));
    }

    #[test]
    fn test_cli_has_migrate_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert!(subs.contains(&"migrate"));
    }

    #[test]
    fn test_api_base_is_localhost() {
        assert!(API_BASE.starts_with("http://localhost"));
    }

    #[test]
    fn test_api_base_port() {
        assert!(API_BASE.contains("3111"));
    }

    #[test]
    fn test_workflow_run_accepts_json_input_and_agent() {
        let cli = Cli::try_parse_from([
            "agentos",
            "workflow",
            "run",
            "feature-build",
            "--input",
            r#"{"feature_description":"cache"}"#,
            "--agent",
            "architect",
        ])
        .unwrap();
        let Commands::Workflow(WorkflowCmd::Run {
            id, input, agent, ..
        }) = cli.command
        else {
            panic!("expected workflow run");
        };
        assert_eq!(id, "feature-build");
        assert_eq!(input.as_deref(), Some(r#"{"feature_description":"cache"}"#));
        assert_eq!(agent.as_deref(), Some("architect"));
    }

    #[test]
    fn test_workflow_document_accepts_yaml() {
        let workflow =
            parse_workflow_document("id: test\nname: Test\ndescription: test\nsteps: []\n")
                .unwrap();
        assert_eq!(workflow["id"], "test");
    }

    #[test]
    fn test_relative_agentos_home_is_normalized_from_caller() {
        let caller = Path::new("/tmp/agentos-caller");
        assert_eq!(
            resolve_agentos_home(caller, Some(std::ffi::OsStr::new("state"))).unwrap(),
            PathBuf::from("/tmp/agentos-caller/state")
        );
        assert_eq!(
            resolve_agentos_home(caller, Some(std::ffi::OsStr::new("nested/state"))).unwrap(),
            PathBuf::from("/tmp/agentos-caller/nested/state")
        );
    }

    #[test]
    fn test_empty_agentos_home_uses_platform_fallback() {
        let fallback =
            resolve_agentos_home(Path::new("/tmp/caller"), Some(std::ffi::OsStr::new(""))).unwrap();
        assert!(fallback.ends_with(".agentos"));
    }

    fn checkout_directory(label: &str, with_workers: bool) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("agentos-checkout-{label}-{}", std::process::id()));
        if with_workers {
            std::fs::create_dir_all(root.join("workers")).expect("create workers directory");
        } else {
            std::fs::create_dir_all(&root).expect("create checkout directory");
        }
        std::fs::write(root.join("config.yaml"), "workers: []\n").expect("write checkout config");
        root
    }

    #[test]
    fn test_relative_agentos_config_one_component_is_caller_relative() {
        let (path, discovery) = resolve_config_path(
            Path::new("/tmp/caller"),
            Path::new("/tmp/home"),
            Some(std::ffi::OsStr::new("runtime.yaml")),
        );
        assert_eq!(path, PathBuf::from("/tmp/caller/runtime.yaml"));
        assert_eq!(discovery, ConfigDiscovery::Explicit);
    }

    #[test]
    fn test_relative_agentos_config_nested_is_caller_relative() {
        let (path, discovery) = resolve_config_path(
            Path::new("/tmp/caller"),
            Path::new("/tmp/home"),
            Some(std::ffi::OsStr::new("nested/runtime.yaml")),
        );
        assert_eq!(path, PathBuf::from("/tmp/caller/nested/runtime.yaml"));
        assert_eq!(discovery, ConfigDiscovery::Explicit);
    }

    #[test]
    fn test_empty_agentos_config_uses_project_before_installed_runtime() {
        let (path, discovery) = resolve_config_path(
            Path::new("/tmp/caller"),
            Path::new("/tmp/home"),
            Some(std::ffi::OsStr::new("")),
        );
        assert_eq!(path, PathBuf::from("/tmp/home/runtime/config.yaml"));
        assert_eq!(discovery, ConfigDiscovery::Home);
    }

    #[test]
    fn test_empty_agentos_config_keeps_checkout_discovery_enabled() {
        let checkout = checkout_directory("empty-config", true);
        let (path, discovery) = resolve_config_path(
            &checkout,
            Path::new("/tmp/agentos-home"),
            Some(std::ffi::OsStr::new("")),
        );
        assert_eq!(path, checkout.join("config.yaml"));
        assert_eq!(discovery, ConfigDiscovery::Checkout);
        std::fs::remove_dir_all(&checkout).expect("remove checkout directory");
    }

    #[test]
    fn test_checkout_config_stays_eligible_when_agentos_home_is_set() {
        // Regression: AGENTOS_HOME alone must not disable checkout discovery.
        let checkout = checkout_directory("home-set", true);
        let (path, discovery) =
            resolve_config_path(&checkout, Path::new("/tmp/agentos-home"), None);
        assert_eq!(path, checkout.join("config.yaml"));
        assert_eq!(discovery, ConfigDiscovery::Checkout);
        std::fs::remove_dir_all(&checkout).expect("remove checkout directory");
    }

    #[test]
    fn test_explicit_config_overrides_checkout_discovery() {
        let checkout = checkout_directory("explicit-wins", true);
        let (path, discovery) = resolve_config_path(
            &checkout,
            Path::new("/tmp/agentos-home"),
            Some(std::ffi::OsStr::new("/etc/agentos/config.yaml")),
        );
        assert_eq!(path, PathBuf::from("/etc/agentos/config.yaml"));
        assert_eq!(discovery, ConfigDiscovery::Explicit);
        std::fs::remove_dir_all(&checkout).expect("remove checkout directory");
    }

    #[test]
    fn test_checkout_without_workers_falls_back_to_installed_runtime() {
        let checkout = checkout_directory("no-workers", false);
        let (path, discovery) =
            resolve_config_path(&checkout, Path::new("/tmp/agentos-home"), None);
        assert_eq!(path, PathBuf::from("/tmp/agentos-home/runtime/config.yaml"));
        assert_eq!(discovery, ConfigDiscovery::Home);
        std::fs::remove_dir_all(&checkout).expect("remove checkout directory");
    }

    #[test]
    fn test_runtime_paths_prefer_checkout_when_only_agentos_home_is_set() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let checkout = checkout_directory("runtime-paths", true);
        let previous_home = std::env::var_os("AGENTOS_HOME");
        let previous_config = std::env::var_os("AGENTOS_CONFIG");
        let previous_cwd = std::env::current_dir().expect("read current directory");
        unsafe {
            std::env::set_var("AGENTOS_HOME", "/tmp/agentos-home-only");
            std::env::remove_var("AGENTOS_CONFIG");
        }
        std::env::set_current_dir(&checkout).expect("enter checkout directory");
        let resolved = runtime_paths();
        std::env::set_current_dir(&previous_cwd).expect("restore current directory");
        restore_test_env("AGENTOS_HOME", previous_home.clone());
        restore_test_env("AGENTOS_CONFIG", previous_config.clone());

        let resolved = resolved.expect("resolve runtime paths");
        let expected_dir = std::fs::canonicalize(&checkout).expect("canonicalize checkout");
        assert_eq!(resolved.discovery, ConfigDiscovery::Checkout);
        assert_eq!(resolved.config_path, expected_dir.join("config.yaml"));
        assert_eq!(resolved.runtime_dir, expected_dir);
        assert_eq!(
            resolved.agentos_home,
            PathBuf::from("/tmp/agentos-home-only")
        );
        assert_eq!(std::env::var_os("AGENTOS_HOME"), previous_home);
        assert_eq!(std::env::var_os("AGENTOS_CONFIG"), previous_config);
        assert_eq!(
            std::env::current_dir().expect("read current directory"),
            previous_cwd
        );
        std::fs::remove_dir_all(&checkout).expect("remove checkout directory");
    }

    #[test]
    fn test_worker_runtime_block_and_inline_forms() {
        assert_eq!(
            parse_worker_runtime("name: block\nruntime:\n  kind: rust\n").unwrap(),
            WorkerRuntime::Rust
        );
        assert_eq!(
            parse_worker_runtime("name: inline\nruntime: rust\n").unwrap(),
            WorkerRuntime::Rust
        );
        assert_eq!(
            parse_worker_runtime("name: map\nruntime: { kind: python }\n").unwrap(),
            WorkerRuntime::Python
        );
    }

    #[test]
    fn test_worker_runtime_inline_nested_values_keep_direct_kind() {
        assert_eq!(
            parse_worker_runtime(
                "name: inline\nruntime: { kind: rust, settings: { tags: [a, b] } }\n"
            )
            .unwrap(),
            WorkerRuntime::Rust
        );
    }

    #[test]
    fn test_worker_runtime_nested_kind_fails_closed() {
        let result = parse_worker_runtime("name: nested\nruntime:\n  settings:\n    kind: rust\n");
        assert!(result.is_err());
    }

    #[test]
    fn test_worker_runtime_missing_unknown_and_misspelled_kinds_fail_closed() {
        assert!(parse_worker_runtime("name: missing\nruntime:\n  settings: {}\n").is_err());
        assert!(parse_worker_runtime("name: unknown\nruntime:\n  kind: ruby\n").is_err());
        assert!(parse_worker_runtime("name: typo\nruntime:\n  knd: rust\n").is_err());
    }

    #[test]
    fn test_collect_worker_specs_rejects_missing_workers_directory() {
        let error = collect_worker_specs(Path::new("/tmp/does-not-exist"))
            .expect_err("missing workers directory must fail");
        assert!(error.to_string().contains("missing or unreadable"));
    }

    #[cfg(unix)]
    #[test]
    fn test_collect_worker_specs_rejects_unreadable_workers_directory() {
        let root =
            std::env::temp_dir().join(format!("agentos-workers-unreadable-{}", std::process::id()));
        let workers_dir = root.join("workers");
        std::fs::create_dir_all(&workers_dir).expect("create workers directory");
        let mut permissions = std::fs::metadata(&workers_dir)
            .expect("read workers directory metadata")
            .permissions();
        permissions.set_mode(0o0);
        std::fs::set_permissions(&workers_dir, permissions).expect("make workers unreadable");
        assert!(collect_worker_specs(&root).is_err());
        let mut permissions = std::fs::metadata(&workers_dir)
            .expect("read workers directory metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&workers_dir, permissions).expect("restore workers permissions");
        std::fs::remove_dir_all(root).expect("remove temporary workers directory");
    }

    #[cfg(unix)]
    #[test]
    fn test_discover_workers_rejects_unreadable_directory() {
        let root = std::env::temp_dir().join(format!(
            "agentos-discover-unreadable-{}",
            std::process::id()
        ));
        let workers_dir = root.join("workers");
        std::fs::create_dir_all(&workers_dir).expect("create workers directory");
        let mut permissions = std::fs::metadata(&workers_dir)
            .expect("read workers directory metadata")
            .permissions();
        permissions.set_mode(0o0);
        std::fs::set_permissions(&workers_dir, permissions).expect("make workers unreadable");
        assert!(discover_workers(&root).is_err());
        let mut permissions = std::fs::metadata(&workers_dir)
            .expect("read workers directory metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&workers_dir, permissions).expect("restore workers permissions");
        std::fs::remove_dir_all(root).expect("remove temporary workers directory");
    }

    fn restore_test_env(name: &str, previous: Option<std::ffi::OsString>) {
        unsafe {
            match previous {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    #[test]
    fn test_repository_worker_manifests_parse_fail_closed() {
        let workers_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../workers");
        let mut count = 0;
        for entry in std::fs::read_dir(workers_dir).expect("read repository workers") {
            let entry = entry.expect("read worker entry");
            if !entry.path().is_dir() {
                continue;
            }
            let manifest_path = entry.path().join("iii.worker.yaml");
            if !manifest_path.is_file() {
                continue;
            }
            let manifest = std::fs::read_to_string(&manifest_path).expect("read worker manifest");
            assert!(
                parse_worker_runtime(&manifest).is_ok(),
                "invalid worker manifest {}",
                manifest_path.display()
            );
            count += 1;
        }
        assert!(count > 0);
    }
}
