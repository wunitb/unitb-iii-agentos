//! One-command bootstrap (`agentos up`) and readiness diagnosis (`agentos doctor`).
//!
//! Every operating-system effect the policies need sits behind two traits:
//! [`Diagnostics`] for read-only probes and [`Bootstrap`] for process control.
//! `agentos doctor` is handed a [`Diagnostics`] only, so "doctor never installs,
//! builds, starts, repairs, or kills anything" is enforced by the type system,
//! and `agentos up` can be unit tested without spawning a single process.

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use colored::Colorize;
use serde_json::{Value, json};

use crate::{RunningWorker, RuntimePaths, WorkerLaunch, WorkerRuntime, WorkerSpec};

/// The engine accepts worker connections here; `config.yaml` pins the port.
pub(crate) const ENGINE_HOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
pub(crate) const ENGINE_PORT: u16 = 49134;

pub(crate) const ENGINE_INSTALL_HINT: &str =
    "install it with `bash scripts/install-iii.sh` (or reinstall AgentOS) and keep `iii` on PATH";
pub(crate) const WORKSPACE_BUILD_HINT: &str = "run `cargo build --workspace --release`";

const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_millis(400);
const API_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

fn engine_endpoint() -> SocketAddr {
    SocketAddr::from((ENGINE_HOST, ENGINE_PORT))
}

/// Read-only probes. Nothing here changes machine state.
pub(crate) trait Diagnostics {
    /// The resolved `iii` binary, or why it could not be found.
    fn engine_binary(&self) -> Result<PathBuf>;
    /// `iii --version` output, when the binary answers.
    fn engine_version(&self, binary: &Path) -> Option<String>;
    /// Whether the engine accepts connections on its worker port.
    fn engine_healthy(&self) -> bool;
    /// Workers the engine reports as connected, `None` when the API is silent.
    fn connected_workers(&self) -> Option<u64>;
    /// Worker manifests plus the release binary resolved for each of them.
    fn worker_specs(&self) -> Result<Vec<WorkerSpec>>;
    /// The `agentos-tui` binary, when it is installed or built.
    fn tui_binary(&self) -> Option<PathBuf>;
    /// Where the engine writes its log; used in failure guidance.
    fn engine_log(&self) -> PathBuf;
    /// Where the workers write their log; used in failure guidance.
    fn worker_log(&self) -> PathBuf;
}

/// Process control used by `agentos up` only.
pub(crate) trait Bootstrap: Diagnostics {
    /// Starts the engine detached and returns its pid.
    fn start_engine(&mut self, binary: &Path) -> Result<u32>;
    /// The exit status of an engine started by this process, if it died.
    fn engine_stopped(&mut self) -> Option<String>;
    /// Workers started by this process that have already exited, named with
    /// their exit status. Spawning a worker is not the same as it staying up.
    fn stopped_workers(&mut self) -> Vec<String>;
    /// Starts every Rust worker and returns how many were started.
    fn start_workers(&mut self, workers: &[WorkerSpec]) -> Result<usize>;
    /// Terminates whatever this process started; used on failure paths.
    fn shutdown_started(&mut self);
    /// Runs the TUI in the foreground and returns its exit code.
    fn run_tui(&mut self, binary: &Path) -> Result<i32>;
    fn sleep(&mut self, duration: Duration);
}

// ---------------------------------------------------------------------------
// readiness report (`agentos doctor`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct ReadinessItem {
    pub(crate) name: &'static str,
    pub(crate) passed: bool,
    pub(crate) detail: String,
    /// What to do about it, present only when the item failed.
    pub(crate) hint: Option<String>,
}

impl ReadinessItem {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            passed: true,
            detail: detail.into(),
            hint: None,
        }
    }

    fn failed(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name,
            passed: false,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Readiness {
    pub(crate) items: Vec<ReadinessItem>,
}

impl Readiness {
    pub(crate) fn passed(&self) -> bool {
        self.items.iter().all(|item| item.passed)
    }

    #[cfg(test)]
    pub(crate) fn item(&self, name: &str) -> Option<&ReadinessItem> {
        self.items.iter().find(|item| item.name == name)
    }

    pub(crate) fn to_json(&self) -> Value {
        let checks = self
            .items
            .iter()
            .map(|item| {
                json!({
                    "check": item.name,
                    "passed": item.passed,
                    "detail": item.detail,
                    "hint": item.hint,
                })
            })
            .collect::<Vec<_>>();
        json!({ "checks": checks, "passed": self.passed() })
    }

    pub(crate) fn render(&self, out: &mut dyn Write) -> Result<()> {
        writeln!(out, "\n{} AgentOS readiness\n", "→".blue())?;
        let width = self
            .items
            .iter()
            .map(|item| item.name.len())
            .max()
            .unwrap_or(0);
        for item in &self.items {
            let icon = if item.passed {
                "✓".green()
            } else {
                "✗".red()
            };
            writeln!(
                out,
                "  {icon} {:<width$}  {}",
                item.name,
                item.detail,
                width = width
            )?;
            if let Some(hint) = &item.hint {
                writeln!(out, "    {:<width$}  {hint}", "", width = width)?;
            }
        }

        let failed = self.items.iter().filter(|item| !item.passed).count();
        if failed == 0 {
            writeln!(
                out,
                "\n{} Everything is ready. Run `agentos up`.\n",
                "✓".green()
            )?;
        } else {
            writeln!(out, "\n{} {failed} check(s) need attention.\n", "✗".red())?;
        }
        Ok(())
    }
}

/// Builds the readiness report. Pure aggregation over read-only probes.
pub(crate) fn readiness(probes: &dyn Diagnostics, paths: &RuntimePaths) -> Readiness {
    let mut items = Vec::new();

    let engine_binary = probes.engine_binary();
    match &engine_binary {
        Ok(binary) => {
            let version = probes
                .engine_version(binary)
                .unwrap_or_else(|| "version unknown".to_string());
            items.push(ReadinessItem::ok(
                "Engine binary",
                format!("{version} ({})", binary.display()),
            ));
        }
        Err(error) => items.push(ReadinessItem::failed(
            "Engine binary",
            error.to_string(),
            ENGINE_INSTALL_HINT,
        )),
    }

    let endpoint = engine_endpoint();
    let healthy = probes.engine_healthy();
    items.push(if healthy {
        ReadinessItem::ok("Engine", format!("accepting connections on {endpoint}"))
    } else {
        ReadinessItem::failed(
            "Engine",
            format!("no listener on {endpoint}"),
            "start the stack with `agentos up`",
        )
    });

    let binary_dir = crate::worker_binary_dir(&paths.runtime_dir);
    let specs = probes.worker_specs();
    let required = specs.as_ref().ok().map(|workers| required_workers(workers));
    match &specs {
        Ok(workers) => {
            let missing = crate::missing_worker_binaries(workers);
            let rust_workers = required.unwrap_or(0);
            if missing.is_empty() {
                items.push(ReadinessItem::ok(
                    "Workers",
                    format!(
                        "{rust_workers} release binaries in {}",
                        binary_dir.display()
                    ),
                ));
            } else {
                items.push(ReadinessItem::failed(
                    "Workers",
                    format!(
                        "{} of {rust_workers} release binaries missing in {}: {}",
                        missing.len(),
                        binary_dir.display(),
                        missing.join(", ")
                    ),
                    WORKSPACE_BUILD_HINT,
                ));
            }
        }
        Err(error) => items.push(ReadinessItem::failed(
            "Workers",
            error.to_string(),
            "check the runtime `workers/` directory named by the config below",
        )),
    }

    // A count above zero is not readiness: the whole required set must be on
    // the bus, otherwise the stack is only partially up.
    items.push(match (probes.connected_workers(), required) {
        (Some(count), Some(required)) if count >= required && required > 0 => ReadinessItem::ok(
            "Connected",
            format!("{count} connected to the engine ({required} required)"),
        ),
        (Some(_), Some(0)) => ReadinessItem::failed(
            "Connected",
            format!(
                "the runtime declares no Rust workers in {}",
                binary_dir.display()
            ),
            "point the config at a runtime with `workers/`, or reinstall AgentOS",
        ),
        (Some(count), Some(required)) => ReadinessItem::failed(
            "Connected",
            format!("engine reports {count} of {required} required workers connected"),
            "start them with `agentos up --no-tui`",
        ),
        (Some(count), None) => ReadinessItem::failed(
            "Connected",
            format!("{count} workers connected, but the required set is unknown"),
            "fix the `workers/` directory above, then re-run `agentos doctor`",
        ),
        (None, _) => ReadinessItem::failed(
            "Connected",
            "engine API did not report a worker count",
            "start the stack with `agentos up`, then re-run `agentos doctor`",
        ),
    });

    items.push(match probes.tui_binary() {
        Some(path) => ReadinessItem::ok("TUI", path.display().to_string()),
        None => ReadinessItem::failed(
            "TUI",
            format!("{} not found", crate::TUI_BINARY),
            format!("{WORKSPACE_BUILD_HINT}, or reinstall AgentOS"),
        ),
    });

    let config_detail = format!(
        "{} — {}",
        paths.discovery.label(),
        paths.config_path.display()
    );
    items.push(if paths.config_path.is_file() {
        ReadinessItem::ok("Config", config_detail)
    } else {
        ReadinessItem::failed(
            "Config",
            format!("{config_detail} (missing)"),
            "run the installer, or point AGENTOS_CONFIG at a config.yaml",
        )
    });

    items.push(if paths.agentos_home.is_dir() {
        ReadinessItem::ok("State", paths.agentos_home.display().to_string())
    } else {
        ReadinessItem::failed(
            "State",
            format!("{} is missing", paths.agentos_home.display()),
            "create it with `agentos init`",
        )
    });

    Readiness { items }
}

/// Workers `up` launches and therefore expects to see on the bus: every Rust
/// worker in the runtime. Python workers are started by their own tooling.
fn required_workers(workers: &[WorkerSpec]) -> u64 {
    workers
        .iter()
        .filter(|worker| worker.runtime == WorkerRuntime::Rust)
        .count() as u64
}

// ---------------------------------------------------------------------------
// bootstrap policy (`agentos up`)
// ---------------------------------------------------------------------------

pub(crate) struct UpOptions {
    pub(crate) launch_tui: bool,
    /// Upper bound on each readiness wait: engine health, then worker
    /// connections.
    pub(crate) stage_timeout: Duration,
    pub(crate) poll_interval: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UpOutcome {
    /// `--no-tui`: engine and workers are ready and keep running.
    Ready,
    /// The TUI ran in the foreground and exited with this code.
    Tui(i32),
}

/// Brings the stack up in order: config, engine binary, TUI binary, engine
/// health, worker binaries, workers, TUI. A failed stage stops the sequence and
/// tears down whatever this invocation started.
pub(crate) fn run_up(
    effects: &mut dyn Bootstrap,
    paths: &RuntimePaths,
    options: &UpOptions,
    out: &mut dyn Write,
) -> Result<UpOutcome> {
    let outcome = up_stages(effects, paths, options, out);
    if outcome.is_err() {
        effects.shutdown_started();
    }
    outcome
}

fn stage_ok(out: &mut dyn Write, name: &str, detail: &str) -> Result<()> {
    writeln!(out, "  {} {:<9} {detail}", "✓".green(), name)?;
    Ok(())
}

fn stage_run(out: &mut dyn Write, detail: &str) -> Result<()> {
    writeln!(out, "  {} {detail}", "→".blue())?;
    Ok(())
}

fn up_stages(
    effects: &mut dyn Bootstrap,
    paths: &RuntimePaths,
    options: &UpOptions,
    out: &mut dyn Write,
) -> Result<UpOutcome> {
    writeln!(out, "\n{}", "AgentOS".bold().cyan())?;
    writeln!(out, "{}", "─".repeat(46).dimmed())?;

    // 1. configuration
    if !paths.config_path.is_file() {
        anyhow::bail!(
            "AgentOS config not found at {} ({}). Run the installer, or point AGENTOS_CONFIG at a config.yaml",
            paths.config_path.display(),
            paths.discovery.label()
        );
    }
    stage_ok(
        out,
        "Config",
        &format!(
            "{} — {}",
            paths.discovery.label(),
            paths.config_path.display()
        ),
    )?;

    // 2. engine binary
    let engine_binary = effects
        .engine_binary()
        .map_err(|error| anyhow::anyhow!("{error}; {ENGINE_INSTALL_HINT}"))?;
    let version = effects
        .engine_version(&engine_binary)
        .unwrap_or_else(|| "version unknown".to_string());
    stage_ok(
        out,
        "Engine",
        &format!("{version} ({})", engine_binary.display()),
    )?;

    // 3. the TUI binary is a precondition, so a missing TUI never leaves a
    //    half-started stack behind.
    let tui_binary = if options.launch_tui {
        let Some(path) = effects.tui_binary() else {
            anyhow::bail!(
                "{} not found beside the agentos binary or in {}: {WORKSPACE_BUILD_HINT}, or start without it using `agentos up --no-tui`",
                crate::TUI_BINARY,
                crate::worker_binary_dir(&paths.runtime_dir).display()
            );
        };
        stage_ok(out, "TUI", &path.display().to_string())?;
        Some(path)
    } else {
        None
    };

    // 4. engine: reuse a healthy one, otherwise start it detached and wait.
    let endpoint = engine_endpoint();
    let reused_engine = effects.engine_healthy();
    if reused_engine {
        stage_ok(out, "Bus", &format!("already healthy on {endpoint}"))?;
    } else {
        stage_run(out, &format!("Starting iii-engine on {endpoint}..."))?;
        let pid = effects.start_engine(&engine_binary)?;
        await_engine(effects, options)?;
        stage_ok(out, "Bus", &format!("healthy on {endpoint} (pid {pid})"))?;
    }

    // 5. worker binaries
    let workers = effects.worker_specs()?;
    let missing = crate::missing_worker_binaries(&workers);
    if !missing.is_empty() {
        anyhow::bail!(
            "Missing compiled workers in {}: {}. {WORKSPACE_BUILD_HINT}",
            crate::worker_binary_dir(&paths.runtime_dir).display(),
            missing.join(", ")
        );
    }
    let required = required_workers(&workers);
    if required == 0 {
        anyhow::bail!(
            "No Rust workers found in {}; {WORKSPACE_BUILD_HINT} in a checkout, or reinstall AgentOS",
            paths.runtime_dir.join("workers").display()
        );
    }
    stage_ok(out, "Workers", &format!("{required} release binaries"))?;

    // 6. workers: only a complete required set on the bus is a running stack,
    //    so any smaller count launches the workers and waits for them.
    ensure_engine_alive(effects)?;
    let already_connected = effects.connected_workers().unwrap_or(0);
    let connected = if already_connected >= required {
        stage_ok(
            out,
            "Workers",
            &format!("{already_connected} already connected; not starting duplicates"),
        )?;
        already_connected
    } else {
        if already_connected > 0 {
            stage_run(
                out,
                &format!("Starting workers ({already_connected} of {required} connected)..."),
            )?;
        } else {
            stage_run(out, "Starting workers...")?;
        }
        let started = effects.start_workers(&workers)?;
        stage_ok(out, "Workers", &format!("{started} started"))?;
        let connected = await_workers(effects, options, required)?;
        stage_ok(out, "Workers", &format!("{connected} connected"))?;
        connected
    };

    // The engine can accept one connection and then die; nothing later may
    // report success against a dead bus.
    ensure_engine_alive(effects)?;

    // 7. TUI in the foreground.
    match tui_binary {
        Some(path) => {
            stage_run(out, "Starting agentos-tui...")?;
            let code = effects.run_tui(&path)?;
            Ok(UpOutcome::Tui(code))
        }
        None => {
            writeln!(out, "{}", "─".repeat(46).dimmed())?;
            writeln!(out, "  Engine   {}  ws://{endpoint}", "●".green())?;
            writeln!(
                out,
                "  Workers  {}  {connected} of {required} connected",
                "●".green()
            )?;
            writeln!(out, "{}", "─".repeat(46).dimmed())?;
            writeln!(
                out,
                "\n  {} agentos tui           Terminal dashboard",
                "▸".dimmed()
            )?;
            writeln!(
                out,
                "  {} agentos doctor        Readiness report\n",
                "▸".dimmed()
            )?;
            Ok(UpOutcome::Ready)
        }
    }
}

/// One bounded polling budget, shared by both readiness waits.
fn poll_plan(options: &UpOptions) -> (Duration, u128) {
    let poll = options.poll_interval.max(Duration::from_millis(1));
    let attempts = options
        .stage_timeout
        .as_millis()
        .div_ceil(poll.as_millis())
        .max(1);
    (poll, attempts)
}

/// Polls engine health within a bounded number of attempts, and fails fast when
/// the engine we started has already exited.
fn await_engine(effects: &mut dyn Bootstrap, options: &UpOptions) -> Result<()> {
    let (poll, attempts) = poll_plan(options);
    for _ in 0..attempts {
        if let Some(status) = effects.engine_stopped() {
            anyhow::bail!(
                "iii-engine exited with {status} before it became healthy; check {}",
                effects.engine_log().display()
            );
        }
        if effects.engine_healthy() {
            return Ok(());
        }
        effects.sleep(poll);
    }
    anyhow::bail!(
        "iii-engine did not accept connections on {} within {}s; check {}",
        engine_endpoint(),
        options.stage_timeout.as_secs_f32(),
        effects.engine_log().display()
    )
}

/// Waits for the required workers to be on the bus within the same bounded
/// budget. Spawning is not readiness: a worker that exits, or an engine that
/// dies underneath them, fails here instead of being reported as ready.
fn await_workers(effects: &mut dyn Bootstrap, options: &UpOptions, required: u64) -> Result<u64> {
    let (poll, attempts) = poll_plan(options);
    let mut reported: Option<u64> = None;
    for _ in 0..attempts {
        ensure_engine_alive(effects)?;
        let stopped = effects.stopped_workers();
        if !stopped.is_empty() {
            anyhow::bail!(
                "worker(s) exited right after starting: {}; check {}",
                stopped.join(", "),
                effects.worker_log().display()
            );
        }
        match effects.connected_workers() {
            Some(count) if count >= required => return Ok(count),
            Some(count) => reported = Some(count),
            None => {}
        }
        effects.sleep(poll);
    }
    let seconds = options.stage_timeout.as_secs_f32();
    match reported {
        Some(count) => anyhow::bail!(
            "only {count} of {required} workers connected to the engine within {seconds}s; check {}",
            effects.worker_log().display()
        ),
        None => anyhow::bail!(
            "the engine did not report any connected workers within {seconds}s; check that AGENTOS_API_URL points at the engine API, and check {}",
            effects.worker_log().display()
        ),
    }
}

/// The engine must still be up. Health can succeed once and the process can die
/// straight after, so every stage past the first probe re-checks it.
fn ensure_engine_alive(effects: &mut dyn Bootstrap) -> Result<()> {
    if let Some(status) = effects.engine_stopped() {
        anyhow::bail!(
            "iii-engine exited with {status} after it became healthy; check {}",
            effects.engine_log().display()
        );
    }
    if !effects.engine_healthy() {
        anyhow::bail!(
            "iii-engine stopped accepting connections on {}; check {}",
            engine_endpoint(),
            effects.engine_log().display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the real machine
// ---------------------------------------------------------------------------

/// [`Diagnostics`] and [`Bootstrap`] against this machine.
pub(crate) struct SystemEffects {
    agentos_home: PathBuf,
    config_path: PathBuf,
    runtime_dir: PathBuf,
    api_base: String,
    client: reqwest::Client,
    runtime: tokio::runtime::Handle,
    engine: Option<std::process::Child>,
    workers: Vec<RunningWorker>,
}

impl SystemEffects {
    /// Must be called from inside the tokio runtime: the HTTP probes are
    /// executed there and awaited from the blocking caller.
    pub(crate) fn new(paths: &RuntimePaths, api_base: String, client: reqwest::Client) -> Self {
        Self {
            agentos_home: paths.agentos_home.clone(),
            config_path: paths.config_path.clone(),
            runtime_dir: paths.runtime_dir.clone(),
            api_base,
            client,
            runtime: tokio::runtime::Handle::current(),
            engine: None,
            workers: Vec::new(),
        }
    }

    /// Runs one future on the CLI runtime from a blocking thread.
    fn probe<T: Send + 'static>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Option<T> {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.runtime.spawn(async move {
            let _ = sender.send(future.await);
        });
        receiver.recv_timeout(API_PROBE_TIMEOUT * 2).ok()
    }
}

/// Connected workers as reported by the engine health endpoint.
fn reported_worker_count(health: &Value) -> Option<u64> {
    match &health["workers"] {
        Value::Number(number) => number.as_u64(),
        Value::Array(workers) => Some(workers.len() as u64),
        _ => None,
    }
}

impl Diagnostics for SystemEffects {
    fn engine_binary(&self) -> Result<PathBuf> {
        crate::find_iii_binary(&self.agentos_home)
    }

    fn engine_version(&self, binary: &Path) -> Option<String> {
        let output = std::process::Command::new(binary)
            .arg("--version")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        text.lines().next().map(|line| line.trim().to_string())
    }

    fn engine_healthy(&self) -> bool {
        TcpStream::connect_timeout(&engine_endpoint(), HEALTH_PROBE_TIMEOUT).is_ok()
    }

    fn connected_workers(&self) -> Option<u64> {
        let request = self
            .client
            .get(format!("{}/api/health", self.api_base))
            .timeout(API_PROBE_TIMEOUT);
        let health =
            self.probe(async move { request.send().await.ok()?.json::<Value>().await.ok() })??;
        reported_worker_count(&health)
    }

    fn worker_specs(&self) -> Result<Vec<WorkerSpec>> {
        crate::collect_worker_specs(&self.runtime_dir)
    }

    fn tui_binary(&self) -> Option<PathBuf> {
        crate::find_tui_binary(Some(&self.runtime_dir))
    }

    fn engine_log(&self) -> PathBuf {
        crate::engine_log_path(&self.agentos_home)
    }

    fn worker_log(&self) -> PathBuf {
        crate::worker_log_path(&self.agentos_home)
    }
}

impl Bootstrap for SystemEffects {
    fn start_engine(&mut self, binary: &Path) -> Result<u32> {
        let engine = crate::spawn_engine(
            binary,
            &self.config_path,
            &self.runtime_dir,
            &self.engine_log(),
            true,
        )?;
        let pid = engine.id();
        self.engine = Some(engine);
        Ok(pid)
    }

    fn engine_stopped(&mut self) -> Option<String> {
        let engine = self.engine.as_mut()?;
        engine
            .try_wait()
            .ok()
            .flatten()
            .map(|status| status.to_string())
    }

    fn stopped_workers(&mut self) -> Vec<String> {
        self.workers
            .iter_mut()
            .filter_map(|worker| {
                let status = worker.child.try_wait().ok().flatten()?;
                Some(format!("{} ({status})", worker.name))
            })
            .collect()
    }

    fn start_workers(&mut self, workers: &[WorkerSpec]) -> Result<usize> {
        let log_path = self.worker_log();
        let launch = WorkerLaunch {
            runtime_dir: &self.runtime_dir,
            log_path: &log_path,
            detached: true,
        };
        let before = self.workers.len();
        crate::launch_workers(workers, &launch, &mut self.workers)?;
        Ok(self.workers.len() - before)
    }

    fn shutdown_started(&mut self) {
        for worker in &mut self.workers {
            let _ = worker.child.kill();
            let _ = worker.child.wait();
        }
        self.workers.clear();
        if let Some(engine) = self.engine.as_mut() {
            let _ = engine.kill();
            let _ = engine.wait();
        }
        self.engine = None;
    }

    fn run_tui(&mut self, binary: &Path) -> Result<i32> {
        // Inherited stdio: the TUI owns the terminal until the user quits.
        let status = std::process::Command::new(binary)
            .current_dir(&self.runtime_dir)
            .status()
            .with_context(|| format!("Failed to start {}", binary.display()))?;
        Ok(status.code().unwrap_or(1))
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConfigDiscovery;
    use std::cell::{Cell, RefCell};

    fn spec(name: &str, runtime: WorkerRuntime, built: bool) -> WorkerSpec {
        WorkerSpec {
            name: name.to_string(),
            runtime,
            binary: built.then(|| PathBuf::from(format!("/release/agentos-{name}"))),
        }
    }

    fn paths(config_path: &Path, discovery: ConfigDiscovery) -> RuntimePaths {
        RuntimePaths {
            agentos_home: PathBuf::from("/home/user/.agentos"),
            config_path: config_path.to_path_buf(),
            runtime_dir: config_path
                .parent()
                .unwrap_or(Path::new("/runtime"))
                .to_path_buf(),
            discovery,
        }
    }

    /// A path that really is a file, so the up policy passes stage 1. The test
    /// binary itself qualifies and leaves nothing behind in the temp directory;
    /// the policy only checks existence and hands the path to the engine.
    fn existing_config() -> PathBuf {
        std::env::current_exe().expect("test binary path")
    }

    struct Fake {
        engine_binary: Option<PathBuf>,
        engine_version: Option<String>,
        /// Healthy from this probe onwards; `None` never becomes healthy.
        healthy_from: Option<usize>,
        /// Stops being healthy again from this probe onwards.
        unhealthy_from: Option<usize>,
        probes: Cell<usize>,
        /// The engine this invocation started reports exited from this liveness
        /// check onwards.
        engine_exits_after: Option<usize>,
        stop_checks: Cell<usize>,
        engine_start_error: Option<String>,
        /// Workers the engine reports; `None` when the API stays silent.
        connected: Cell<Option<u64>>,
        /// What the engine reports once this invocation started the workers.
        connected_after_start: Option<u64>,
        workers: Result<Vec<WorkerSpec>, String>,
        worker_start_error: Option<String>,
        /// Workers that exit right after being started.
        worker_exits: Vec<String>,
        started_workers: Cell<bool>,
        tui: Option<PathBuf>,
        tui_code: i32,
        events: RefCell<Vec<String>>,
        sleeps: Cell<usize>,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                engine_binary: Some(PathBuf::from("/usr/local/bin/iii")),
                engine_version: Some("iii 0.22.1".to_string()),
                healthy_from: Some(0),
                unhealthy_from: None,
                probes: Cell::new(0),
                engine_exits_after: None,
                stop_checks: Cell::new(0),
                engine_start_error: None,
                connected: Cell::new(Some(62)),
                connected_after_start: Some(2),
                workers: Ok(vec![
                    spec("core", WorkerRuntime::Rust, true),
                    spec("memory", WorkerRuntime::Rust, true),
                    spec("embedding", WorkerRuntime::Python, false),
                ]),
                worker_start_error: None,
                worker_exits: Vec::new(),
                started_workers: Cell::new(false),
                tui: Some(PathBuf::from("/usr/local/bin/agentos-tui")),
                tui_code: 0,
                events: RefCell::new(Vec::new()),
                sleeps: Cell::new(0),
            }
        }
    }

    impl Fake {
        fn record(&self, event: &str) {
            self.events.borrow_mut().push(event.to_string());
        }

        fn events(&self) -> Vec<String> {
            self.events.borrow().clone()
        }
    }

    impl Diagnostics for Fake {
        fn engine_binary(&self) -> Result<PathBuf> {
            self.engine_binary.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "iii-engine v0.22.1 is required and was not found on PATH; {ENGINE_INSTALL_HINT}"
                )
            })
        }

        fn engine_version(&self, _binary: &Path) -> Option<String> {
            self.engine_version.clone()
        }

        fn engine_healthy(&self) -> bool {
            let probe = self.probes.get();
            self.probes.set(probe + 1);
            let started = self
                .healthy_from
                .map(|threshold| probe >= threshold)
                .unwrap_or(false);
            let stopped = self
                .unhealthy_from
                .map(|threshold| probe >= threshold)
                .unwrap_or(false);
            started && !stopped
        }

        fn connected_workers(&self) -> Option<u64> {
            self.connected.get()
        }

        fn worker_specs(&self) -> Result<Vec<WorkerSpec>> {
            match &self.workers {
                Ok(workers) => Ok(workers
                    .iter()
                    .map(|worker| spec(&worker.name, worker.runtime, worker.binary.is_some()))
                    .collect()),
                Err(error) => anyhow::bail!("{error}"),
            }
        }

        fn tui_binary(&self) -> Option<PathBuf> {
            self.tui.clone()
        }

        fn engine_log(&self) -> PathBuf {
            PathBuf::from("/home/user/.agentos/logs/engine.log")
        }

        fn worker_log(&self) -> PathBuf {
            PathBuf::from("/home/user/.agentos/logs/workers.log")
        }
    }

    impl Bootstrap for Fake {
        fn start_engine(&mut self, _binary: &Path) -> Result<u32> {
            self.record("start_engine");
            match &self.engine_start_error {
                Some(error) => anyhow::bail!("{error}"),
                None => Ok(4242),
            }
        }

        fn engine_stopped(&mut self) -> Option<String> {
            let check = self.stop_checks.get();
            self.stop_checks.set(check + 1);
            let exits_after = self.engine_exits_after?;
            (check >= exits_after).then(|| "exit status: 1".to_string())
        }

        fn stopped_workers(&mut self) -> Vec<String> {
            if self.started_workers.get() {
                self.worker_exits.clone()
            } else {
                Vec::new()
            }
        }

        fn start_workers(&mut self, workers: &[WorkerSpec]) -> Result<usize> {
            self.record("start_workers");
            match &self.worker_start_error {
                Some(error) => anyhow::bail!("{error}"),
                None => {
                    self.started_workers.set(true);
                    self.connected.set(self.connected_after_start);
                    Ok(workers
                        .iter()
                        .filter(|worker| worker.binary.is_some())
                        .count())
                }
            }
        }

        fn shutdown_started(&mut self) {
            self.record("shutdown_started");
        }

        fn run_tui(&mut self, _binary: &Path) -> Result<i32> {
            self.record("run_tui");
            Ok(self.tui_code)
        }

        fn sleep(&mut self, _duration: Duration) {
            self.sleeps.set(self.sleeps.get() + 1);
        }
    }

    fn options(launch_tui: bool) -> UpOptions {
        UpOptions {
            launch_tui,
            stage_timeout: Duration::from_secs(3),
            poll_interval: Duration::from_millis(500),
        }
    }

    fn up(fake: &mut Fake, options: &UpOptions, config: &Path) -> (Result<UpOutcome>, String) {
        colored::control::set_override(false);
        let mut out = Vec::new();
        let outcome = run_up(
            fake,
            &paths(config, ConfigDiscovery::Checkout),
            options,
            &mut out,
        );
        (outcome, String::from_utf8(out).expect("utf-8 output"))
    }

    #[test]
    fn up_reports_install_hint_when_engine_binary_is_missing() {
        let config = existing_config();
        let mut fake = Fake {
            engine_binary: None,
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(true), &config);
        let error = outcome.expect_err("missing engine must fail").to_string();
        assert!(error.contains("install-iii.sh"), "{error}");
        assert!(!fake.events().contains(&"start_engine".to_string()));
        assert!(!fake.events().contains(&"start_workers".to_string()));
    }

    #[test]
    fn up_reuses_a_healthy_engine_without_spawning_another() {
        let config = existing_config();
        let mut fake = Fake {
            connected: Cell::new(Some(0)),
            ..Fake::default()
        };
        let (outcome, output) = up(&mut fake, &options(false), &config);
        assert_eq!(outcome.expect("up succeeds"), UpOutcome::Ready);
        assert_eq!(fake.events(), vec!["start_workers".to_string()]);
        assert!(
            output.contains("already healthy on 127.0.0.1:49134"),
            "{output}"
        );
    }

    #[test]
    fn up_starts_the_engine_detached_and_waits_for_health() {
        let config = existing_config();
        let mut fake = Fake {
            healthy_from: Some(2),
            connected: Cell::new(Some(0)),
            ..Fake::default()
        };
        let (outcome, output) = up(&mut fake, &options(false), &config);
        assert_eq!(outcome.expect("up succeeds"), UpOutcome::Ready);
        assert_eq!(
            fake.events(),
            vec!["start_engine".to_string(), "start_workers".to_string()]
        );
        assert!(
            output.contains("healthy on 127.0.0.1:49134 (pid 4242)"),
            "{output}"
        );
    }

    #[test]
    fn up_fails_within_the_health_timeout_and_starts_no_workers() {
        let config = existing_config();
        let mut fake = Fake {
            healthy_from: None,
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(false), &config);
        let error = outcome.expect_err("unhealthy engine must fail").to_string();
        assert!(
            error.contains("did not accept connections on 127.0.0.1:49134"),
            "{error}"
        );
        assert!(error.contains("engine.log"), "{error}");
        // 3s budget / 500ms poll: bounded, and it gave up on its own.
        assert_eq!(fake.sleeps.get(), 6);
        assert_eq!(
            fake.events(),
            vec!["start_engine".to_string(), "shutdown_started".to_string()]
        );
    }

    #[test]
    fn up_fails_when_the_engine_exits_before_it_is_healthy() {
        let config = existing_config();
        let mut fake = Fake {
            healthy_from: None,
            engine_exits_after: Some(1),
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(false), &config);
        let error = outcome.expect_err("dead engine must fail").to_string();
        assert!(error.contains("exited with exit status: 1"), "{error}");
        assert!(
            fake.sleeps.get() < 6,
            "gave up early: {}",
            fake.sleeps.get()
        );
        assert!(!fake.events().contains(&"start_workers".to_string()));
    }

    #[test]
    fn up_reports_the_build_hint_for_missing_worker_binaries() {
        let config = existing_config();
        let mut fake = Fake {
            workers: Ok(vec![
                spec("core", WorkerRuntime::Rust, true),
                spec("memory", WorkerRuntime::Rust, false),
            ]),
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(false), &config);
        let error = outcome.expect_err("missing binaries must fail").to_string();
        assert!(error.contains("agentos-memory"), "{error}");
        assert!(
            error.contains("cargo build --workspace --release"),
            "{error}"
        );
        assert!(!fake.events().contains(&"start_workers".to_string()));
    }

    #[test]
    fn up_stops_when_the_worker_launch_fails() {
        let config = existing_config();
        let mut fake = Fake {
            connected: Cell::new(Some(0)),
            worker_start_error: Some("Failed to start /release/agentos-core".to_string()),
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(true), &config);
        let error = outcome.expect_err("worker failure must fail").to_string();
        assert!(
            error.contains("Failed to start /release/agentos-core"),
            "{error}"
        );
        assert_eq!(
            fake.events(),
            vec!["start_workers".to_string(), "shutdown_started".to_string()]
        );
    }

    #[test]
    fn up_launches_the_tui_last_and_returns_its_exit_code() {
        let config = existing_config();
        let mut fake = Fake {
            connected: Cell::new(Some(0)),
            tui_code: 7,
            ..Fake::default()
        };
        let (outcome, output) = up(&mut fake, &options(true), &config);
        assert_eq!(outcome.expect("up succeeds"), UpOutcome::Tui(7));
        assert_eq!(
            fake.events(),
            vec!["start_workers".to_string(), "run_tui".to_string()]
        );
        assert!(output.contains("Starting agentos-tui"), "{output}");
    }

    #[test]
    fn up_requires_the_tui_binary_before_touching_any_process() {
        let config = existing_config();
        let mut fake = Fake {
            tui: None,
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(true), &config);
        let error = outcome.expect_err("missing TUI must fail").to_string();
        assert!(error.contains("agentos-tui not found"), "{error}");
        assert!(error.contains("--no-tui"), "{error}");
        assert_eq!(fake.events(), vec!["shutdown_started".to_string()]);
    }

    #[test]
    fn up_no_tui_skips_the_tui_check_and_launch() {
        let config = existing_config();
        let mut fake = Fake {
            tui: None,
            connected: Cell::new(Some(0)),
            ..Fake::default()
        };
        let (outcome, output) = up(&mut fake, &options(false), &config);
        assert_eq!(outcome.expect("up succeeds"), UpOutcome::Ready);
        assert_eq!(fake.events(), vec!["start_workers".to_string()]);
        assert!(!output.contains("agentos-tui"), "{output}");
        assert!(output.contains("agentos doctor"), "{output}");
    }

    #[test]
    fn up_keeps_workers_that_are_already_connected() {
        let config = existing_config();
        let mut fake = Fake {
            connected: Cell::new(Some(62)),
            ..Fake::default()
        };
        let (outcome, output) = up(&mut fake, &options(false), &config);
        assert_eq!(outcome.expect("up succeeds"), UpOutcome::Ready);
        assert!(fake.events().is_empty(), "{:?}", fake.events());
        assert!(output.contains("62 already connected"), "{output}");
    }

    #[test]
    fn up_starts_the_workers_when_only_part_of_the_required_set_is_connected() {
        let config = existing_config();
        // One of the two Rust workers is on the bus: the stack is not up.
        let mut fake = Fake {
            connected: Cell::new(Some(1)),
            ..Fake::default()
        };
        let (outcome, output) = up(&mut fake, &options(false), &config);
        assert_eq!(outcome.expect("up succeeds"), UpOutcome::Ready);
        assert_eq!(fake.events(), vec!["start_workers".to_string()]);
        assert!(output.contains("1 of 2 connected"), "{output}");
    }

    #[test]
    fn up_waits_for_the_started_workers_to_connect() {
        let config = existing_config();
        let mut fake = Fake {
            connected: Cell::new(Some(0)),
            connected_after_start: Some(2),
            ..Fake::default()
        };
        let (outcome, output) = up(&mut fake, &options(false), &config);
        assert_eq!(outcome.expect("up succeeds"), UpOutcome::Ready);
        assert!(output.contains("2 connected"), "{output}");
        assert!(output.contains("2 of 2 connected"), "{output}");
    }

    #[test]
    fn up_fails_when_the_started_workers_never_connect() {
        let config = existing_config();
        let mut fake = Fake {
            connected: Cell::new(Some(0)),
            connected_after_start: Some(1),
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(true), &config);
        let error = outcome
            .expect_err("workers that never connect must fail")
            .to_string();
        assert!(
            error.contains("only 1 of 2 workers connected to the engine within 3s"),
            "{error}"
        );
        assert!(error.contains("workers.log"), "{error}");
        // Bounded, and the TUI never took the terminal.
        assert_eq!(fake.sleeps.get(), 6);
        assert_eq!(
            fake.events(),
            vec!["start_workers".to_string(), "shutdown_started".to_string()]
        );
    }

    #[test]
    fn up_fails_when_the_engine_api_never_reports_workers() {
        let config = existing_config();
        let mut fake = Fake {
            connected: Cell::new(Some(0)),
            connected_after_start: None,
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(false), &config);
        let error = outcome.expect_err("a silent API must fail").to_string();
        assert!(
            error.contains("did not report any connected workers within 3s"),
            "{error}"
        );
        assert!(error.contains("AGENTOS_API_URL"), "{error}");
    }

    #[test]
    fn up_fails_when_a_started_worker_exits_immediately() {
        let config = existing_config();
        let mut fake = Fake {
            connected: Cell::new(Some(0)),
            connected_after_start: Some(0),
            worker_exits: vec!["core (exit status: 1)".to_string()],
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(true), &config);
        let error = outcome.expect_err("a dead worker must fail").to_string();
        assert!(
            error.contains("worker(s) exited right after starting: core (exit status: 1)"),
            "{error}"
        );
        assert!(error.contains("workers.log"), "{error}");
        // It gave up on the first check instead of burning the whole budget.
        assert_eq!(fake.sleeps.get(), 0);
        assert_eq!(
            fake.events(),
            vec!["start_workers".to_string(), "shutdown_started".to_string()]
        );
    }

    #[test]
    fn up_fails_when_the_engine_exits_after_becoming_healthy() {
        let config = existing_config();
        // Health succeeds on the second probe, then the engine dies before the
        // workers are started.
        let mut fake = Fake {
            healthy_from: Some(1),
            connected: Cell::new(Some(0)),
            engine_exits_after: Some(1),
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(true), &config);
        let error = outcome.expect_err("a dead engine must fail").to_string();
        assert!(
            error.contains("exited with exit status: 1 after it became healthy"),
            "{error}"
        );
        assert_eq!(
            fake.events(),
            vec!["start_engine".to_string(), "shutdown_started".to_string()]
        );
    }

    #[test]
    fn up_fails_when_the_engine_dies_while_the_workers_connect() {
        let config = existing_config();
        let mut fake = Fake {
            healthy_from: Some(1),
            connected: Cell::new(Some(0)),
            connected_after_start: Some(0),
            engine_exits_after: Some(4),
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(false), &config);
        let error = outcome.expect_err("a dead engine must fail").to_string();
        assert!(error.contains("exited with exit status: 1"), "{error}");
        assert!(
            fake.sleeps.get() < 6,
            "gave up early: {}",
            fake.sleeps.get()
        );
        assert_eq!(
            fake.events(),
            vec![
                "start_engine".to_string(),
                "start_workers".to_string(),
                "shutdown_started".to_string()
            ]
        );
    }

    #[test]
    fn up_fails_when_a_reused_engine_stops_listening() {
        let config = existing_config();
        // Healthy for the reuse probe, gone by the time the workers matter.
        let mut fake = Fake {
            unhealthy_from: Some(1),
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(true), &config);
        let error = outcome.expect_err("a dead engine must fail").to_string();
        assert!(
            error.contains("stopped accepting connections on 127.0.0.1:49134"),
            "{error}"
        );
        assert!(!fake.events().contains(&"start_workers".to_string()));
        assert!(!fake.events().contains(&"run_tui".to_string()));
    }

    #[test]
    fn up_fails_when_the_runtime_has_no_rust_workers() {
        let config = existing_config();
        let mut fake = Fake {
            workers: Ok(vec![spec("embedding", WorkerRuntime::Python, false)]),
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(false), &config);
        let error = outcome.expect_err("an empty runtime must fail").to_string();
        assert!(error.contains("No Rust workers found"), "{error}");
        assert!(!fake.events().contains(&"start_workers".to_string()));
    }

    #[test]
    fn up_fails_when_the_resolved_config_is_missing() {
        let missing = std::env::temp_dir().join("agentos-bootstrap-absent/config.yaml");
        let mut fake = Fake::default();
        let (outcome, _) = up(&mut fake, &options(false), &missing);
        let error = outcome.expect_err("missing config must fail").to_string();
        assert!(error.contains("AgentOS config not found"), "{error}");
        assert!(error.contains("checkout config.yaml"), "{error}");
        assert!(!fake.events().contains(&"start_engine".to_string()));
    }

    fn diagnose(fake: &Fake, discovery: ConfigDiscovery, config: &Path) -> (Readiness, String) {
        colored::control::set_override(false);
        let report = readiness(fake, &paths(config, discovery));
        let mut out = Vec::new();
        report.render(&mut out).expect("render report");
        (report, String::from_utf8(out).expect("utf-8 output"))
    }

    #[test]
    fn doctor_reports_every_item_green_when_bootstrapped() {
        let config = existing_config();
        let fake = Fake::default();
        let (report, output) = diagnose(&fake, ConfigDiscovery::Checkout, &config);
        // State is the only item that depends on a real home directory.
        let failures: Vec<&str> = report
            .items
            .iter()
            .filter(|item| !item.passed)
            .map(|item| item.name)
            .collect();
        assert_eq!(failures, vec!["State"]);
        assert!(output.contains("iii 0.22.1"), "{output}");
        assert!(
            output.contains("62 connected to the engine (2 required)"),
            "{output}"
        );
        assert!(
            output.contains("accepting connections on 127.0.0.1:49134"),
            "{output}"
        );
    }

    #[test]
    fn doctor_pinpoints_a_missing_engine_binary() {
        let config = existing_config();
        let fake = Fake {
            engine_binary: None,
            ..Fake::default()
        };
        let (report, output) = diagnose(&fake, ConfigDiscovery::Checkout, &config);
        let item = report.item("Engine binary").expect("engine binary item");
        assert!(!item.passed);
        assert_eq!(item.hint.as_deref(), Some(ENGINE_INSTALL_HINT));
        assert!(output.contains("install-iii.sh"), "{output}");
    }

    #[test]
    fn doctor_pinpoints_an_engine_that_is_not_running() {
        let config = existing_config();
        let fake = Fake {
            healthy_from: None,
            connected: Cell::new(None),
            ..Fake::default()
        };
        let (report, output) = diagnose(&fake, ConfigDiscovery::Checkout, &config);
        assert!(report.item("Engine binary").expect("binary item").passed);
        let engine = report.item("Engine").expect("engine item");
        assert!(!engine.passed);
        assert_eq!(
            engine.hint.as_deref(),
            Some("start the stack with `agentos up`")
        );
        assert!(
            output.contains("no listener on 127.0.0.1:49134"),
            "{output}"
        );
    }

    #[test]
    fn doctor_pinpoints_missing_worker_binaries() {
        let config = existing_config();
        let fake = Fake {
            workers: Ok(vec![
                spec("core", WorkerRuntime::Rust, true),
                spec("memory", WorkerRuntime::Rust, false),
                spec("pulse", WorkerRuntime::Rust, false),
            ]),
            ..Fake::default()
        };
        let (report, output) = diagnose(&fake, ConfigDiscovery::Checkout, &config);
        let workers = report.item("Workers").expect("workers item");
        assert!(!workers.passed);
        assert!(workers.detail.contains("2 of 3"), "{}", workers.detail);
        assert!(
            workers.detail.contains("agentos-memory, agentos-pulse"),
            "{}",
            workers.detail
        );
        assert_eq!(workers.hint.as_deref(), Some(WORKSPACE_BUILD_HINT));
        assert!(
            output.contains("cargo build --workspace --release"),
            "{output}"
        );
    }

    #[test]
    fn doctor_reports_an_unreadable_workers_directory() {
        let config = existing_config();
        let fake = Fake {
            workers: Err("AgentOS workers directory is missing or unreadable: /x/workers".into()),
            ..Fake::default()
        };
        let (report, _) = diagnose(&fake, ConfigDiscovery::Checkout, &config);
        let workers = report.item("Workers").expect("workers item");
        assert!(!workers.passed);
        assert!(
            workers.detail.contains("missing or unreadable"),
            "{}",
            workers.detail
        );
    }

    #[test]
    fn doctor_measures_connected_workers_against_the_required_set() {
        let config = existing_config();
        let none = Fake {
            connected: Cell::new(Some(0)),
            ..Fake::default()
        };
        let (report, output) = diagnose(&none, ConfigDiscovery::Checkout, &config);
        let item = report.item("Connected").expect("connected item");
        assert!(!item.passed);
        assert!(
            output.contains("engine reports 0 of 2 required workers connected"),
            "{output}"
        );

        // A partial stack is not ready either: one of two required workers.
        let partial = Fake {
            connected: Cell::new(Some(1)),
            ..Fake::default()
        };
        let (report, output) = diagnose(&partial, ConfigDiscovery::Checkout, &config);
        let item = report.item("Connected").expect("connected item");
        assert!(!item.passed);
        assert!(
            output.contains("engine reports 1 of 2 required workers connected"),
            "{output}"
        );
        assert_eq!(
            item.hint.as_deref(),
            Some("start them with `agentos up --no-tui`")
        );

        let some = Fake {
            connected: Cell::new(Some(3)),
            ..Fake::default()
        };
        let (report, _) = diagnose(&some, ConfigDiscovery::Checkout, &config);
        assert!(report.item("Connected").expect("connected item").passed);
    }

    #[test]
    fn doctor_says_so_when_the_runtime_declares_no_rust_workers() {
        let config = existing_config();
        let fake = Fake {
            // Only a Python worker: nothing this stack can start or count.
            workers: Ok(vec![spec("embedding", WorkerRuntime::Python, false)]),
            connected: Cell::new(Some(0)),
            ..Fake::default()
        };
        let (report, output) = diagnose(&fake, ConfigDiscovery::Checkout, &config);
        let item = report.item("Connected").expect("connected item");
        assert!(!item.passed);
        assert!(output.contains("declares no Rust workers"), "{output}");
    }

    #[test]
    fn doctor_reports_a_missing_tui_binary() {
        let config = existing_config();
        let fake = Fake {
            tui: None,
            ..Fake::default()
        };
        let (report, output) = diagnose(&fake, ConfigDiscovery::Checkout, &config);
        let tui = report.item("TUI").expect("tui item");
        assert!(!tui.passed);
        assert!(
            tui.detail.contains("agentos-tui not found"),
            "{}",
            tui.detail
        );
        assert!(output.contains("reinstall AgentOS"), "{output}");
    }

    #[test]
    fn doctor_names_the_active_config_discovery_mode() {
        let config = existing_config();
        let fake = Fake::default();
        for (discovery, label) in [
            (ConfigDiscovery::Explicit, "explicit AGENTOS_CONFIG"),
            (ConfigDiscovery::Checkout, "checkout config.yaml"),
            (
                ConfigDiscovery::Home,
                "installed runtime below AGENTOS_HOME",
            ),
        ] {
            let (report, output) = diagnose(&fake, discovery, &config);
            let item = report.item("Config").expect("config item");
            assert!(item.passed, "{}", item.detail);
            assert!(item.detail.starts_with(label), "{}", item.detail);
            assert!(output.contains(&config.display().to_string()), "{output}");
        }
    }

    #[test]
    fn doctor_reports_a_config_that_does_not_exist() {
        let missing = std::env::temp_dir().join("agentos-bootstrap-absent/config.yaml");
        let fake = Fake::default();
        let (report, _) = diagnose(&fake, ConfigDiscovery::Home, &missing);
        let item = report.item("Config").expect("config item");
        assert!(!item.passed);
        assert!(item.detail.contains("(missing)"), "{}", item.detail);
    }

    #[test]
    fn doctor_json_keeps_the_check_shape() {
        let config = existing_config();
        let fake = Fake::default();
        let (report, _) = diagnose(&fake, ConfigDiscovery::Checkout, &config);
        let json = report.to_json();
        let checks = json["checks"].as_array().expect("checks array");
        assert_eq!(checks.len(), report.items.len());
        assert!(checks.iter().any(|check| check["check"] == "Workers"));
        assert!(checks.iter().all(|check| check["passed"].is_boolean()));
        assert_eq!(json["passed"], Value::Bool(report.passed()));
    }

    #[test]
    fn engine_health_reports_worker_counts_from_either_shape() {
        assert_eq!(reported_worker_count(&json!({ "workers": 62 })), Some(62));
        assert_eq!(
            reported_worker_count(&json!({ "workers": ["a", "b"] })),
            Some(2)
        );
        assert_eq!(reported_worker_count(&json!({ "status": "ok" })), None);
    }
}
