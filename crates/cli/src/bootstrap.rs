//! One-command bootstrap (`agentos up`) and readiness diagnosis (`agentos doctor`).
//!
//! Every operating-system effect the policies need sits behind two traits:
//! [`Diagnostics`] for read-only probes and [`Bootstrap`] for process control.
//! `agentos doctor` is handed a [`Diagnostics`] only, so "doctor never installs,
//! builds, starts, repairs, or kills anything" is enforced by the type system,
//! and `agentos up` can be unit tested without spawning a single process.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use colored::Colorize;
use serde_json::{Value, json};

use crate::{RunningWorker, RuntimePaths, WorkerLaunch, WorkerRuntime, WorkerSpec};

/// The engine accepts worker connections here; `config.yaml` pins the port.
pub(crate) const ENGINE_HOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
pub(crate) const ENGINE_PORT: u16 = 49134;

/// The bus-auth daemon: `crates/bus-auth`, binary `agentos-bus-authd`. It must
/// be listening before the engine starts, because iii 0.22.1 calls the RBAC
/// auth function for every bus connection — including the connection of any
/// worker that could have provided it.
pub(crate) const BUS_AUTH_BINARY: &str = "agentos-bus-authd";
/// Must match `agentos_bus_auth::daemon::DEFAULT_LISTEN_ADDR` and the
/// `iii-bridge` `url` in `config.yaml`.
pub(crate) const BUS_AUTH_ADDR_VARIABLE: &str = "AGENTOS_BUS_AUTH_ADDR";
const DEFAULT_BUS_AUTH_ADDR: &str = "127.0.0.1:49129";
/// The key in `config.yaml` that arms the gate. `WorkerManagerConfig` is
/// `deny_unknown_fields`, so its presence is a reliable signal.
const BUS_RBAC_MARKER: &str = "auth_function_id";

pub(crate) const ENGINE_INSTALL_HINT: &str =
    "install it with `bash scripts/install-iii.sh` (or reinstall AgentOS) and keep `iii` on PATH";
pub(crate) const WORKSPACE_BUILD_HINT: &str = "run `cargo build --workspace --release`";

const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_millis(400);
const WORKER_IDENTITIES_UNREPORTED: &str = "the engine did not report connected worker identities";

/// Reads `<runtime>/.env` for `agentos up` without mutating this process. Keys
/// already exported by the invoking shell are deliberately omitted so child
/// process inheritance keeps the explicit value. The returned values are
/// applied to the engine, workers, and TUI at spawn time.
pub(crate) fn load_dotenv(runtime_dir: &Path) -> Result<BTreeMap<String, String>> {
    let path = runtime_dir.join(".env");
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let source = std::fs::read_to_string(&path)
        .with_context(|| format!("Cannot read dotenv file {}", path.display()))?;
    parse_dotenv(&source, &path)
}

fn parse_dotenv(source: &str, path: &Path) -> Result<BTreeMap<String, String>> {
    let mut values = parse_dotenv_all(source, path)?;
    // Explicit shell exports win: dropping them here keeps the exported value
    // on the child process instead of overwriting it with the file value.
    values.retain(|key, _| std::env::var_os(key).is_none());
    Ok(values)
}

/// Every assignment in a dotenv file, ignoring what the current process
/// exports. `agentos doctor` needs the file's own view to say where a
/// credential comes from; `up` filters this down in [`parse_dotenv`].
fn parse_dotenv_all(source: &str, path: &Path) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for (index, raw_line) in source.lines().enumerate() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            line = rest.trim_start();
        }
        let Some((key, raw_value)) = line.split_once('=') else {
            anyhow::bail!(
                "Invalid dotenv assignment at {}:{}",
                path.display(),
                index + 1
            );
        };
        let key = key.trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|character| character == '_' || character.is_ascii_alphanumeric())
            || key.as_bytes()[0].is_ascii_digit()
        {
            anyhow::bail!(
                "Invalid dotenv key at {}:{}: {key}",
                path.display(),
                index + 1
            );
        }
        // First assignment wins within `.env`, matching common dotenv behavior
        // and avoiding surprising overrides.
        if values.contains_key(key) {
            continue;
        }
        let value = parse_dotenv_value(raw_value.trim(), path, index + 1)?;
        values.insert(key.to_string(), value);
    }
    Ok(values)
}

fn parse_dotenv_value(value: &str, path: &Path, line: usize) -> Result<String> {
    if let Some(quoted) = value.strip_prefix('"') {
        let Some(quoted) = quoted.strip_suffix('"') else {
            anyhow::bail!("Unclosed double quote at {}:{line}", path.display());
        };
        let mut parsed = String::new();
        let mut escaped = false;
        for character in quoted.chars() {
            if escaped {
                parsed.push(match character {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    other => other,
                });
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else {
                parsed.push(character);
            }
        }
        if escaped {
            parsed.push('\\');
        }
        return Ok(parsed);
    }
    if let Some(quoted) = value.strip_prefix('\'') {
        let Some(quoted) = quoted.strip_suffix('\'') else {
            anyhow::bail!("Unclosed single quote at {}:{line}", path.display());
        };
        return Ok(quoted.to_string());
    }
    let value = value.find(" #").map_or(value, |comment| &value[..comment]);
    Ok(value.trim_end().to_string())
}

// ---------------------------------------------------------------------------
// first run: AGENTOS_API_KEY, provider credential, default route
// ---------------------------------------------------------------------------

/// Every protected HTTP route registered through `crates/http-adapter` refuses
/// to register without this variable, so a clean machine loses almost every
/// worker when it is unset.
pub(crate) const API_KEY_VARIABLE: &str = "AGENTOS_API_KEY";
/// 32 bytes, rendered as 64 lowercase hex characters.
const API_KEY_BYTES: usize = 32;

/// Provider credentials `workers/llm-router` resolves from the process
/// environment, **in the router's automatic-routing preference order**
/// (`AUTO_ROUTE_PREFERENCE` in `workers/llm-router/src/main.rs`). `ollama` is
/// omitted: it has an empty `env_key` and needs no credential, and the router
/// deliberately never selects it automatically. Duplicated because the router
/// is a separate crate with no shared library; `default_route_*` tests pin the
/// behaviour, and `scripts/env-contract.test.ts` pins the variable names
/// against the router's own table.
const PROVIDER_VARIABLES: [(&str, &str); 10] = [
    ("anthropic", "ANTHROPIC_API_KEY"),
    ("openai", "OPENAI_API_KEY"),
    ("google", "GOOGLE_API_KEY"),
    ("codex", "CODEX_PROXY_API_KEY"),
    ("groq", "GROQ_API_KEY"),
    ("deepseek", "DEEPSEEK_API_KEY"),
    ("mistral", "MISTRAL_API_KEY"),
    ("together", "TOGETHER_API_KEY"),
    ("fireworks", "FIREWORKS_API_KEY"),
    ("openrouter", "OPENROUTER_API_KEY"),
];

/// The provider assumed when `AGENTOS_DEFAULT_MODEL` is pinned without a
/// provider (`workers/llm-router/src/main.rs`, `resolve_runtime_default`).
const CODEX_PROVIDER: &str = "codex";
const DEFAULT_CODEX_MODEL: &str = "gpt-5.6-sol";

/// Where a credential value comes from. The shell export wins at spawn time
/// (see [`parse_dotenv`]), so the two are never confused in a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialSource {
    Environment,
    Dotenv,
}

impl CredentialSource {
    fn label(self) -> &'static str {
        match self {
            Self::Environment => "process environment",
            Self::Dotenv => ".env",
        }
    }
}

/// The provider credential the router can actually use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderCredential {
    pub(crate) provider: &'static str,
    pub(crate) variable: &'static str,
    pub(crate) source: CredentialSource,
}

/// What an unqualified chat request resolves to today, mirroring
/// `workers/llm-router`: a pinned default whose own credential is present,
/// otherwise the first provider in `AUTO_ROUTE_PREFERENCE` that has one,
/// otherwise a structured `provider_credential_missing` refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DefaultRoute {
    /// `AGENTOS_DEFAULT_PROVIDER`/`AGENTOS_DEFAULT_MODEL` are pinned and the
    /// credential that provider needs is present. `model` is `None` when the
    /// router picks the provider's own default.
    Pinned {
        provider: String,
        model: Option<String>,
    },
    /// No usable pinned default; the router routes to this provider.
    Automatic { provider: &'static str },
    /// No provider credential at all: unqualified requests are refused.
    Unavailable,
}

impl DefaultRoute {
    pub(crate) fn detail(&self) -> String {
        match self {
            Self::Pinned {
                provider,
                model: Some(model),
            } => format!("{provider}/{model} (pinned by AGENTOS_DEFAULT_PROVIDER/MODEL)"),
            Self::Pinned {
                provider,
                model: None,
            } => format!("{provider} (pinned by AGENTOS_DEFAULT_PROVIDER; llm-router picks the model)"),
            Self::Automatic { provider } => format!(
                "{provider} — first provider with a credential in llm-router's preference order"
            ),
            Self::Unavailable => {
                "no provider credential is set; unqualified chat requests are refused with provider_credential_missing".to_string()
            }
        }
    }
}

/// Every credential variable an operator may set, in the router's preference
/// order. Matches the list in llm-router's `provider_credential_missing`.
fn credential_variables() -> String {
    PROVIDER_VARIABLES
        .iter()
        .map(|(_, variable)| *variable)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The credential half of first-run readiness, resolved from the active `.env`
/// plus this process's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Credentials {
    pub(crate) dotenv_path: PathBuf,
    pub(crate) api_key: Option<CredentialSource>,
    pub(crate) providers: Vec<ProviderCredential>,
    pub(crate) route: DefaultRoute,
    /// A pinned default provider whose own credential is absent, so the router
    /// ignores it. Named separately because the route above is still usable.
    pub(crate) disabled_default: Option<String>,
}

impl Credentials {
    /// Reads the active `.env` (never writes) and pairs it with the process
    /// environment. A missing file is not an error: everything is then
    /// reported as absent.
    pub(crate) fn inspect(runtime_dir: &Path) -> Result<Self> {
        let path = dotenv_path(runtime_dir);
        let values = read_dotenv_entries(&path)?;
        Ok(Self::resolve(&path, &values, |name| {
            std::env::var(name).ok()
        }))
    }

    /// Pure resolution so both `doctor` and the tests can drive it without a
    /// process environment.
    pub(crate) fn resolve<F>(path: &Path, dotenv: &BTreeMap<String, String>, environment: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let lookup = |name: &str| -> Option<(String, CredentialSource)> {
            if let Some(value) = environment(name).filter(|value| !value.trim().is_empty()) {
                return Some((value, CredentialSource::Environment));
            }
            dotenv
                .get(name)
                .filter(|value| !value.trim().is_empty())
                .map(|value| (value.clone(), CredentialSource::Dotenv))
        };

        let api_key = lookup(API_KEY_VARIABLE).map(|(_, source)| source);
        let providers = PROVIDER_VARIABLES
            .iter()
            .filter_map(|(provider, variable)| {
                lookup(variable).map(|(_, source)| ProviderCredential {
                    provider,
                    variable,
                    source,
                })
            })
            .collect::<Vec<_>>();

        let requested_provider = lookup("AGENTOS_DEFAULT_PROVIDER").map(|(value, _)| value);
        let requested_model = lookup("AGENTOS_DEFAULT_MODEL").map(|(value, _)| value);
        // A default is "requested" by either variable; a model on its own means
        // the codex provider, exactly as the router resolves it.
        let pinned = (requested_provider.is_some() || requested_model.is_some())
            .then(|| requested_provider.unwrap_or_else(|| CODEX_PROVIDER.to_string()));
        let pinned_is_usable = pinned.as_deref().is_some_and(|provider| {
            providers
                .iter()
                .any(|credential| credential.provider == provider)
        });

        let (route, disabled_default) = match (&pinned, pinned_is_usable) {
            (Some(provider), true) => {
                let model = requested_model.or_else(|| {
                    (provider == CODEX_PROVIDER).then(|| DEFAULT_CODEX_MODEL.to_string())
                });
                (
                    DefaultRoute::Pinned {
                        provider: provider.clone(),
                        model,
                    },
                    None,
                )
            }
            (pinned, _) => {
                let automatic = providers
                    .first()
                    .map(|credential| DefaultRoute::Automatic {
                        provider: credential.provider,
                    })
                    .unwrap_or(DefaultRoute::Unavailable);
                (automatic, pinned.clone())
            }
        };

        Self {
            dotenv_path: path.to_path_buf(),
            api_key,
            providers,
            route,
            disabled_default,
        }
    }

    fn provider_detail(&self) -> String {
        if self.providers.is_empty() {
            return "no provider credential is set".to_string();
        }
        self.providers
            .iter()
            .map(|credential| {
                format!(
                    "{} via {} ({})",
                    credential.provider,
                    credential.variable,
                    credential.source.label()
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub(crate) fn dotenv_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(".env")
}

/// Every assignment in the active `.env`, or an empty map when there is none.
fn read_dotenv_entries(path: &Path) -> Result<BTreeMap<String, String>> {
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("Cannot read dotenv file {}", path.display()))?;
    parse_dotenv_all(&source, path)
}

/// What [`ensure_api_key`] did, so the caller can print exactly one honest
/// line about the machine's own key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApiKeyOutcome {
    /// The shell exports a value; the file is left alone because the export
    /// wins at spawn time and a second value would be misleading.
    Inherited,
    /// The active `.env` already carries a value. Never overwritten.
    AlreadyPresent(PathBuf),
    /// A new 32-byte key was generated and written with mode 0600.
    Generated(PathBuf),
}

impl ApiKeyOutcome {
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Inherited => format!(
                "{API_KEY_VARIABLE} inherited from the process environment; .env left unchanged"
            ),
            Self::AlreadyPresent(path) => {
                format!("{API_KEY_VARIABLE} already set in {}", path.display())
            }
            Self::Generated(path) => format!(
                "generated a new 32-byte {API_KEY_VARIABLE} in {} (mode 0600)",
                path.display()
            ),
        }
    }
}

/// Guarantees the stack has an `AGENTOS_API_KEY` before it starts. Never
/// overwrites an existing value and never invents a provider key.
pub(crate) fn ensure_api_key(runtime_dir: &Path) -> Result<ApiKeyOutcome> {
    ensure_api_key_with(
        runtime_dir,
        || std::env::var(API_KEY_VARIABLE).ok(),
        random_api_key,
    )
}

fn ensure_api_key_with<E, G>(
    runtime_dir: &Path,
    environment: E,
    generate: G,
) -> Result<ApiKeyOutcome>
where
    E: Fn() -> Option<String>,
    G: Fn() -> Result<String>,
{
    if environment().is_some_and(|value| !value.trim().is_empty()) {
        return Ok(ApiKeyOutcome::Inherited);
    }
    let path = dotenv_path(runtime_dir);
    let source = if path.is_file() {
        std::fs::read_to_string(&path)
            .with_context(|| format!("Cannot read dotenv file {}", path.display()))?
    } else {
        String::new()
    };
    let Some(updated) = dotenv_with_api_key(&source, &path, &generate()?)? else {
        return Ok(ApiKeyOutcome::AlreadyPresent(path));
    };
    write_dotenv_secret(&path, &updated)?;
    Ok(ApiKeyOutcome::Generated(path))
}

/// The `.env` text carrying `key`, or `None` when a non-empty value is already
/// assigned. An empty assignment is filled in place so the operator's own
/// layout and comments survive.
fn dotenv_with_api_key(source: &str, path: &Path, key: &str) -> Result<Option<String>> {
    if read_dotenv_value(source, path, API_KEY_VARIABLE)?.is_some() {
        return Ok(None);
    }
    Ok(Some(dotenv_with_assignment(source, API_KEY_VARIABLE, key)))
}

/// `source` with `name=value` assigned exactly once: an existing assignment is
/// replaced in place, otherwise the line is appended. Comments and unrelated
/// lines are preserved.
fn dotenv_with_assignment(source: &str, name: &str, value: &str) -> String {
    let assignment = format!("{name}={value}");
    let mut lines = source.split('\n').map(str::to_string).collect::<Vec<_>>();
    let existing = lines.iter().position(|line| {
        let trimmed = line.trim_start();
        let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        trimmed
            .split_once('=')
            .is_some_and(|(candidate, _)| candidate.trim() == name)
    });
    match existing {
        Some(index) => lines[index] = assignment,
        None => {
            // `split('\n')` leaves a trailing empty element for a
            // newline-terminated file; reuse it instead of adding a blank line.
            match lines.last() {
                Some(last) if last.is_empty() => {
                    let index = lines.len() - 1;
                    lines[index] = assignment;
                    lines.push(String::new());
                }
                _ => {
                    lines.push(assignment);
                    lines.push(String::new());
                }
            }
        }
    }
    lines.join("\n")
}

/// The non-empty assignment of `name` in `source`, if any.
fn read_dotenv_value(source: &str, path: &Path, name: &str) -> Result<Option<String>> {
    Ok(parse_dotenv_all(source, path)?
        .remove(name)
        .filter(|value| !value.trim().is_empty()))
}

/// Assigns a credential in the active `.env` with mode 0600, replacing an
/// existing assignment of the same name. Used by `agentos config set-key` and
/// `agentos onboard`, which are explicit operator instructions to set a value.
pub(crate) fn set_dotenv_value(runtime_dir: &Path, name: &str, value: &str) -> Result<PathBuf> {
    let path = dotenv_path(runtime_dir);
    let source = if path.is_file() {
        std::fs::read_to_string(&path)
            .with_context(|| format!("Cannot read dotenv file {}", path.display()))?
    } else {
        String::new()
    };
    write_dotenv_secret(&path, &dotenv_with_assignment(&source, name, value))?;
    Ok(path)
}

/// The environment variable `workers/llm-router` reads for `provider`, when it
/// is one of the providers the router knows.
pub(crate) fn provider_variable(provider: &str) -> Option<&'static str> {
    PROVIDER_VARIABLES
        .iter()
        .find(|(name, _)| *name == provider)
        .map(|(_, variable)| *variable)
}

/// 32 bytes from the kernel CSPRNG as lowercase hex.
fn random_api_key() -> Result<String> {
    #[cfg(unix)]
    {
        use std::io::Read as _;
        let mut bytes = [0u8; API_KEY_BYTES];
        std::fs::File::open("/dev/urandom")
            .context("Cannot open /dev/urandom to generate AGENTOS_API_KEY")?
            .read_exact(&mut bytes)
            .context("Cannot read 32 random bytes for AGENTOS_API_KEY")?;
        let mut key = String::with_capacity(API_KEY_BYTES * 2);
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(key, "{byte:02x}");
        }
        Ok(key)
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!(
            "cannot generate {API_KEY_VARIABLE} on this platform: set it manually in the active .env"
        )
    }
}

/// Writes secret-bearing dotenv text with mode 0600 through a temporary file in
/// the same directory, so a crash cannot truncate an existing `.env` and the
/// secret is never briefly world-readable.
fn write_dotenv_secret(path: &Path, contents: &str) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        anyhow::bail!(
            "{} is a symlink; refusing to replace it. Write {API_KEY_VARIABLE} into the real file instead",
            path.display()
        );
    }
    let directory = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Invalid dotenv path {}", path.display()))?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("Cannot create {}", directory.display()))?;
    let temporary = directory.join(format!(".env.agentos-{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&temporary);
    let result = write_private_file(&temporary, contents)
        .and_then(|()| std::fs::rename(&temporary, path).map_err(anyhow::Error::from));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.with_context(|| format!("Cannot write {}", path.display()))
}

fn write_private_file(path: &Path, contents: &str) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn engine_endpoint() -> SocketAddr {
    SocketAddr::from((ENGINE_HOST, ENGINE_PORT))
}

/// Where the bus-auth daemon listens: `AGENTOS_BUS_AUTH_ADDR` when set,
/// otherwise the daemon's own default. A value that is not a socket address is
/// an error, never a silent fallback: the engine would fail closed against the
/// wrong port and refuse every worker.
fn parse_bus_auth_addr(raw: Option<&str>) -> Result<SocketAddr> {
    let raw = raw
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_BUS_AUTH_ADDR);
    raw.parse().map_err(|error| {
        anyhow::anyhow!("{BUS_AUTH_ADDR_VARIABLE} is not a host:port address ({raw}): {error}")
    })
}

/// The `host:port` an `iii-bridge` entry in `config.yaml` forwards the RBAC
/// hooks to, so a mismatch with the daemon's address can be reported instead of
/// silently failing every bus connection closed.
fn bridge_url_endpoint(config: &str) -> Option<String> {
    config.lines().find_map(|line| {
        let value = line.trim().strip_prefix("url:")?.trim();
        let value = value.trim_matches(['"', '\''].as_slice());
        let rest = value
            .strip_prefix("ws://")
            .or_else(|| value.strip_prefix("wss://"))?;
        Some(rest.trim_end_matches('/').to_string())
    })
}

/// Read-only probes. Nothing here changes machine state.
pub(crate) trait Diagnostics {
    /// The resolved `iii` binary, or why it could not be found.
    fn engine_binary(&self) -> Result<PathBuf>;
    /// `iii --version` output, when the binary answers.
    fn engine_version(&self, binary: &Path) -> Option<String>;
    /// Whether the engine accepts connections on its worker port.
    fn engine_healthy(&self) -> bool;
    /// Stable names of connected non-engine workers, `None` when the engine
    /// cannot answer `engine::functions::list`.
    fn connected_worker_ids(&self) -> Option<BTreeSet<String>>;
    /// The keys stored in a state scope, `None` when the engine cannot answer
    /// `state::list_keys`. Read-only: `doctor` never writes state.
    fn state_keys(&self, scope: &str) -> Option<Vec<String>>;
    /// The `agentos-bus-authd` binary, when it is installed or built.
    fn bus_auth_binary(&self) -> Option<PathBuf>;
    /// Where the bus-auth daemon should listen.
    fn bus_auth_addr(&self) -> Result<SocketAddr>;
    /// Whether something already accepts connections there.
    fn bus_auth_healthy(&self) -> bool;
    /// The active `config.yaml`, when it can be read: used to tell whether bus
    /// RBAC is armed and which address `iii-bridge` forwards to.
    fn engine_config(&self) -> Option<String>;
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
    /// Starts the bus-auth daemon detached on `addr` and returns its pid.
    fn start_bus_auth(&mut self, binary: &Path, addr: SocketAddr) -> Result<u32>;
    /// The exit status of a daemon started by this process, if it died. It
    /// refuses to start without `AGENTOS_API_KEY`, which is a boot-stopping
    /// condition once the gate is armed.
    fn bus_auth_stopped(&mut self) -> Option<String>;
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
    /// Monotonic time. Fakes advance this when `sleep` is called.
    fn now(&self) -> Instant;
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
pub(crate) fn readiness(
    probes: &dyn Diagnostics,
    paths: &RuntimePaths,
    credentials: &Credentials,
) -> Readiness {
    let mut items = Vec::new();

    // The API key comes first: without it `crates/http-adapter` refuses to
    // register protected routes and almost every worker exits at startup, which
    // otherwise surfaces as an unexplained pile of missing identities.
    items.push(match credentials.api_key {
        Some(source) => ReadinessItem::ok(
            "API key",
            format!("{API_KEY_VARIABLE} set ({})", source.label()),
        ),
        None => ReadinessItem::failed(
            "API key",
            format!(
                "{API_KEY_VARIABLE} is not set in {} or the environment; workers with protected HTTP routes exit at startup",
                credentials.dotenv_path.display()
            ),
            "run `agentos up`, which generates one into the active .env with mode 0600",
        ),
    });

    items.push(if credentials.providers.is_empty() {
        ReadinessItem::failed(
            "Provider",
            credentials.provider_detail(),
            format!("set one of {} in the active .env", credential_variables()),
        )
    } else {
        ReadinessItem::ok("Provider", credentials.provider_detail())
    });

    let route_detail = match &credentials.disabled_default {
        Some(provider) => format!(
            "{}; pinned default '{provider}' is disabled because {} is not set",
            credentials.route.detail(),
            provider_variable(provider).unwrap_or("its credential")
        ),
        None => credentials.route.detail(),
    };
    items.push(match &credentials.route {
        DefaultRoute::Unavailable => ReadinessItem::failed(
            "Route",
            route_detail,
            format!(
                "set one of {} in the active .env, then re-run `agentos doctor`",
                credential_variables()
            ),
        ),
        _ if credentials.disabled_default.is_some() => ReadinessItem::failed(
            "Route",
            route_detail,
            "set the pinned provider's credential, or clear AGENTOS_DEFAULT_PROVIDER/MODEL",
        ),
        _ => ReadinessItem::ok("Route", route_detail),
    });

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
    let required = specs
        .as_ref()
        .ok()
        .map(|workers| required_worker_ids(workers));
    match &specs {
        Ok(workers) => {
            let missing = crate::missing_worker_binaries(workers);
            let rust_workers = required.as_ref().map_or(0, BTreeSet::len);
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

    // Counts can be satisfied by unrelated or duplicate workers. Readiness is
    // the complete set of canonical manifest identities.
    items.push(match (probes.connected_worker_ids(), required) {
        (Some(connected), Some(required)) => {
            if required.is_empty() {
                ReadinessItem::failed(
                    "Connected",
                    format!(
                        "the runtime declares no Rust workers in {}",
                        binary_dir.display()
                    ),
                    "point the config at a runtime with `workers/`, or reinstall AgentOS",
                )
            } else {
                let missing = missing_worker_ids(&required, &connected);
                if missing.is_empty() {
                    ReadinessItem::ok(
                        "Connected",
                        format!(
                            "{} connected; all {} required worker identities present",
                            connected.len(),
                            required.len()
                        ),
                    )
                } else {
                    ReadinessItem::failed(
                        "Connected",
                        format!(
                            "{} connected; missing {} of {} required identities: {}",
                            connected.len(),
                            missing.len(),
                            required.len(),
                            missing.join(", ")
                        ),
                        missing_identity_hint(credentials.api_key.is_some()),
                    )
                }
            }
        }
        (Some(connected), None) => ReadinessItem::failed(
            "Connected",
            format!(
                "{} workers connected, but the required set is unknown",
                connected.len()
            ),
            "fix the `workers/` directory above, then re-run `agentos doctor`",
        ),
        (None, _) => ReadinessItem::failed(
            "Connected",
            WORKER_IDENTITIES_UNREPORTED,
            "start the stack with `agentos up`, then re-run `agentos doctor`",
        ),
    });

    items.push(bus_auth_item(probes));

    items.push(capability_item(probes));

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

/// The foreground (`agentos start`) half of the bus-auth stage: reuse a daemon
/// that is already listening, start one when the gate is armed, and refuse to
/// continue when the gate is armed and the daemon is not built. Returns the
/// child so the caller can stop it with the rest of the stack.
pub(crate) fn start_bus_auth_for_foreground(
    config_path: &Path,
    runtime_dir: &Path,
    log_path: &Path,
    launch_env: &BTreeMap<String, String>,
) -> Result<Option<std::process::Child>> {
    let addr = parse_bus_auth_addr(
        std::env::var(BUS_AUTH_ADDR_VARIABLE)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| launch_env.get(BUS_AUTH_ADDR_VARIABLE).cloned())
            .as_deref(),
    )?;
    let config = std::fs::read_to_string(config_path).unwrap_or_default();
    let armed = config.contains(BUS_RBAC_MARKER);

    if TcpStream::connect_timeout(&addr, HEALTH_PROBE_TIMEOUT).is_ok() {
        println!(
            "{} bus-auth daemon already listening on {addr}",
            "✓".green()
        );
        return Ok(None);
    }
    if !armed {
        return Ok(None);
    }
    let Some(binary) = crate::find_sibling_binary(BUS_AUTH_BINARY, Some(runtime_dir)) else {
        anyhow::bail!(
            "{} arms bus RBAC ({BUS_RBAC_MARKER}) but {BUS_AUTH_BINARY} was not found: {WORKSPACE_BUILD_HINT}, or reinstall AgentOS",
            config_path.display()
        );
    };
    let daemon = crate::spawn_bus_auth(&binary, addr, runtime_dir, log_path, launch_env, false)?;
    println!(
        "{} bus-auth daemon listening on {addr} (pid {})",
        "✓".green(),
        daemon.id()
    );
    Ok(Some(daemon))
}

/// Read-only report on the bus-auth daemon. Bus RBAC is fail-closed: with the
/// gate armed in `config.yaml` and no daemon listening, the engine refuses every
/// worker connection, which otherwise looks like 62 workers that will not start.
fn bus_auth_item(probes: &dyn Diagnostics) -> ReadinessItem {
    let addr = match probes.bus_auth_addr() {
        Ok(addr) => addr,
        Err(error) => {
            return ReadinessItem::failed(
                "Bus auth",
                error.to_string(),
                format!("set {BUS_AUTH_ADDR_VARIABLE} to a host:port address, or unset it"),
            );
        }
    };
    let config = probes.engine_config();
    let armed = config
        .as_deref()
        .is_some_and(|config| config.contains(BUS_RBAC_MARKER));
    let bridge = config.as_deref().and_then(bridge_url_endpoint);
    let mismatch = match &bridge {
        Some(bridge) if *bridge != addr.to_string() => {
            format!("; the iii-bridge url is {bridge}, which is a different address")
        }
        _ => String::new(),
    };

    match (probes.bus_auth_healthy(), armed) {
        (true, _) => ReadinessItem::ok(
            "Bus auth",
            format!("bus-auth daemon: listening on {addr}{mismatch}"),
        ),
        (false, true) => ReadinessItem::failed(
            "Bus auth",
            format!(
                "bus-auth daemon: not running on {addr} (bus RBAC is fail-closed — the engine will refuse every worker while it is down){mismatch}"
            ),
            format!(
                "start the stack with `agentos up`, which starts {BUS_AUTH_BINARY} before the engine"
            ),
        ),
        (false, false) => ReadinessItem::ok(
            "Bus auth",
            format!(
                "bus-auth daemon: not running on {addr}; bus RBAC is not armed in the active config"
            ),
        ),
    }
}

/// The agent id the TUI falls back to when no agent exists
/// (`crates/tui/src/main.rs:889,1966,1724`).
const FALLBACK_AGENT_ID: &str = "default";
/// State scope holding one capability document per agent (CONTRACT I1:
/// scope `capabilities`, key `<agent_id>`, value `{"tools": [...], ...}`).
const CAPABILITY_SCOPE: &str = "capabilities";
/// State scope holding the agent records themselves
/// (`workers/agent-core/src/main.rs:271,296,459`).
const AGENT_SCOPE: &str = "agents";

/// Read-only report on capability documents. An agent without one has every
/// tool call denied, which is invisible from the outside: chat simply refuses
/// to use tools and says nothing about why.
fn capability_item(probes: &dyn Diagnostics) -> ReadinessItem {
    let (Some(agents), Some(capabilities)) = (
        probes.state_keys(AGENT_SCOPE),
        probes.state_keys(CAPABILITY_SCOPE),
    ) else {
        return ReadinessItem::failed(
            "Capabilities",
            "the engine did not answer state::list_keys",
            "start the stack with `agentos up`, then re-run `agentos doctor`",
        );
    };

    if agents.is_empty() {
        return if capabilities.iter().any(|key| key == FALLBACK_AGENT_ID) {
            ReadinessItem::ok(
                "Capabilities",
                format!("no agent records yet; '{FALLBACK_AGENT_ID}' has a capability document"),
            )
        } else {
            ReadinessItem::failed(
                "Capabilities",
                format!(
                    "no agent has a capability document; the TUI falls back to '{FALLBACK_AGENT_ID}', so every tool call for it is denied"
                ),
                "create an agent with `agentos agent new`, then re-run `agentos doctor`",
            )
        };
    }

    let missing = agents
        .iter()
        .filter(|agent| !capabilities.contains(agent))
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        ReadinessItem::ok(
            "Capabilities",
            format!(
                "{} of {} agent(s) have a capability document",
                agents.len(),
                agents.len()
            ),
        )
    } else {
        ReadinessItem::failed(
            "Capabilities",
            format!(
                "{} of {} agent(s) have no capability document: {}; every tool call for them is denied",
                missing.len(),
                agents.len(),
                missing.join(", ")
            ),
            "recreate them with `agentos agent new`, which writes the capability document",
        )
    }
}

/// Workers `up` launches and therefore expects to see on the bus: every Rust
/// worker in the runtime. Python workers are started by their own tooling.
fn required_worker_ids(workers: &[WorkerSpec]) -> BTreeSet<String> {
    workers
        .iter()
        .filter(|worker| worker.runtime == WorkerRuntime::Rust)
        .map(|worker| worker.name.clone())
        .collect()
}

/// The most likely cause of missing identities. Without `AGENTOS_API_KEY`
/// almost every worker exits during route registration, so "start them" is the
/// wrong advice and hides the real fault.
fn missing_identity_hint(api_key_present: bool) -> String {
    if api_key_present {
        "start them with `agentos up --no-tui`".to_string()
    } else {
        format!(
            "{API_KEY_VARIABLE} is unset: workers with protected HTTP routes exit before they connect. Run `agentos up`, which generates one, then re-run `agentos doctor`"
        )
    }
}

fn missing_worker_ids(required: &BTreeSet<String>, connected: &BTreeSet<String>) -> Vec<String> {
    required.difference(connected).cloned().collect()
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

    // 4. bus-auth daemon, before the engine: iii 0.22.1 calls the RBAC auth
    //    function for every bus connection, so a daemon that is not listening
    //    when the engine starts refuses every worker (fail-closed by design).
    bus_auth_stage(effects, options, paths, out)?;

    // 5. engine: reuse a healthy one, otherwise start it detached and wait.
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

    // 6. worker binaries
    let workers = effects.worker_specs()?;
    let missing = crate::missing_worker_binaries(&workers);
    if !missing.is_empty() {
        anyhow::bail!(
            "Missing compiled workers in {}: {}. {WORKSPACE_BUILD_HINT}",
            crate::worker_binary_dir(&paths.runtime_dir).display(),
            missing.join(", ")
        );
    }
    let required = required_worker_ids(&workers);
    if required.is_empty() {
        anyhow::bail!(
            "No Rust workers found in {}; {WORKSPACE_BUILD_HINT} in a checkout, or reinstall AgentOS",
            paths.runtime_dir.join("workers").display()
        );
    }
    stage_ok(
        out,
        "Workers",
        &format!("{} release binaries", required.len()),
    )?;

    // 7. workers: compare canonical identities, never aggregate counts. On a
    //    partial stack only missing workers are launched, preserving the
    //    already-connected processes and avoiding duplicate registrations.
    let already_connected = await_worker_identity_report(effects, options)?;
    let missing = missing_worker_ids(&required, &already_connected);
    let connected = if missing.is_empty() {
        stage_ok(
            out,
            "Workers",
            &format!(
                "all {} required identities already connected; not starting duplicates",
                required.len()
            ),
        )?;
        already_connected
    } else {
        if !already_connected.is_empty() {
            stage_run(
                out,
                &format!(
                    "Starting {} missing workers ({} connected)...",
                    missing.len(),
                    already_connected.len()
                ),
            )?;
        } else {
            stage_run(out, "Starting workers...")?;
        }
        let workers_to_start = workers
            .iter()
            .filter(|worker| missing.binary_search(&worker.name).is_ok())
            .cloned()
            .collect::<Vec<_>>();
        let started = effects.start_workers(&workers_to_start)?;
        stage_ok(out, "Workers", &format!("{started} started"))?;
        let connected = await_workers(effects, options, &required)?;
        stage_ok(
            out,
            "Workers",
            &format!(
                "{} connected; all {} required identities present",
                connected.len(),
                required.len()
            ),
        )?;
        connected
    };

    // The engine can accept one connection and then die; nothing later may
    // report success against a dead bus.
    ensure_engine_alive(effects)?;

    // 8. TUI in the foreground.
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
                "  Workers  {}  {} connected; {} required identities present",
                "●".green(),
                connected.len(),
                required.len()
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

/// Starts (or reuses) the bus-auth daemon and reports what happened. A missing
/// binary is fatal only when the active config arms the gate: an armed config
/// without a daemon cannot boot at all, and failing here names the cause
/// instead of leaving 62 workers refused by the bus.
fn bus_auth_stage(
    effects: &mut dyn Bootstrap,
    options: &UpOptions,
    paths: &RuntimePaths,
    out: &mut dyn Write,
) -> Result<()> {
    let addr = effects.bus_auth_addr()?;
    let config = effects.engine_config();
    let armed = config
        .as_deref()
        .is_some_and(|config| config.contains(BUS_RBAC_MARKER));

    if let Some(bridge) = config.as_deref().and_then(bridge_url_endpoint)
        && bridge != addr.to_string()
    {
        writeln!(
            out,
            "  {} bus-auth address {addr} does not match the iii-bridge url {bridge} in {}; the engine would forward the RBAC hooks somewhere else",
            "!".yellow(),
            paths.config_path.display()
        )?;
    }

    if effects.bus_auth_healthy() {
        stage_ok(out, "Bus auth", &format!("already listening on {addr}"))?;
        return Ok(());
    }

    let Some(binary) = effects.bus_auth_binary() else {
        if armed {
            anyhow::bail!(
                "{} arms bus RBAC ({BUS_RBAC_MARKER}) but {BUS_AUTH_BINARY} was not found: {WORKSPACE_BUILD_HINT}, or reinstall AgentOS. With the gate armed and no daemon listening on {addr}, the engine refuses every bus connection",
                paths.config_path.display()
            );
        }
        stage_ok(
            out,
            "Bus auth",
            &format!("not started: {BUS_AUTH_BINARY} is not built and bus RBAC is not armed"),
        )?;
        return Ok(());
    };

    if !armed {
        stage_ok(
            out,
            "Bus auth",
            &format!(
                "not started: bus RBAC is not armed in {}",
                paths.config_path.display()
            ),
        )?;
        return Ok(());
    }

    stage_run(out, &format!("Starting {BUS_AUTH_BINARY} on {addr}..."))?;
    let pid = effects.start_bus_auth(&binary, addr)?;
    await_bus_auth(effects, options, addr)?;
    stage_ok(out, "Bus auth", &format!("listening on {addr} (pid {pid})"))?;
    Ok(())
}

/// Waits for the daemon to accept connections, failing fast when it exited —
/// which is what it does when `AGENTOS_API_KEY` is missing.
fn await_bus_auth(
    effects: &mut dyn Bootstrap,
    options: &UpOptions,
    addr: SocketAddr,
) -> Result<()> {
    let (poll, deadline) = poll_plan(options);
    loop {
        if let Some(status) = effects.bus_auth_stopped() {
            anyhow::bail!(
                "{BUS_AUTH_BINARY} exited with {status} before it listened on {addr}; it refuses to start without {API_KEY_VARIABLE}"
            );
        }
        if effects.bus_auth_healthy() {
            return Ok(());
        }
        let now = effects.now();
        if now >= deadline {
            break;
        }
        effects.sleep(poll.min(deadline.saturating_duration_since(now)));
    }
    anyhow::bail!(
        "{BUS_AUTH_BINARY} did not accept connections on {addr} within {:?}",
        options.stage_timeout
    )
}

/// Builds a wall-clock polling budget for each readiness wait. A deadline caps
/// slow probes too; deriving only an attempt count can exceed the timeout by
/// the cumulative duration of those probes.
fn poll_plan(options: &UpOptions) -> (Duration, Instant) {
    let poll = options.poll_interval.max(Duration::from_millis(1));
    let now = Instant::now();
    let deadline = now.checked_add(options.stage_timeout).unwrap_or(now);
    (poll, deadline)
}

/// Polls engine health within a bounded number of attempts, and fails fast when
/// the engine we started has already exited.
fn await_engine(effects: &mut dyn Bootstrap, options: &UpOptions) -> Result<()> {
    let (poll, deadline) = poll_plan(options);
    loop {
        if let Some(status) = effects.engine_stopped() {
            anyhow::bail!(
                "iii-engine exited with {status} before it became healthy; check {}",
                effects.engine_log().display()
            );
        }
        if effects.engine_healthy() {
            return Ok(());
        }
        let now = effects.now();
        if now >= deadline {
            break;
        }
        effects.sleep(poll.min(deadline.saturating_duration_since(now)));
    }
    anyhow::bail!(
        "iii-engine did not accept connections on {} within {}s; check {}",
        engine_endpoint(),
        options.stage_timeout.as_secs_f32(),
        effects.engine_log().display()
    )
}

/// Waits for the engine to answer the worker-identity query before deciding
/// what to launch. `None` is an unknown state, while `Some(empty)` is a valid
/// report that means every required worker still needs to be started.
fn await_worker_identity_report(
    effects: &mut dyn Bootstrap,
    options: &UpOptions,
) -> Result<BTreeSet<String>> {
    let (poll, deadline) = poll_plan(options);
    loop {
        ensure_engine_alive(effects)?;
        if let Some(connected) = effects.connected_worker_ids() {
            return Ok(connected);
        }
        let now = effects.now();
        if now >= deadline {
            break;
        }
        effects.sleep(poll.min(deadline.saturating_duration_since(now)));
    }
    anyhow::bail!(WORKER_IDENTITIES_UNREPORTED)
}

/// Waits for the required workers to be on the bus within the same bounded
/// budget. Spawning is not readiness: a worker that exits, or an engine that
/// dies underneath them, fails here instead of being reported as ready.
fn await_workers(
    effects: &mut dyn Bootstrap,
    options: &UpOptions,
    required: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let (poll, deadline) = poll_plan(options);
    let mut reported: Option<BTreeSet<String>> = None;
    loop {
        ensure_engine_alive(effects)?;
        let stopped = effects.stopped_workers();
        if !stopped.is_empty() {
            anyhow::bail!(
                "worker(s) exited right after starting: {}; check {}",
                stopped.join(", "),
                effects.worker_log().display()
            );
        }
        if let Some(connected) = effects.connected_worker_ids() {
            if missing_worker_ids(required, &connected).is_empty() {
                return Ok(connected);
            }
            reported = Some(connected);
        }
        let now = effects.now();
        if now >= deadline {
            break;
        }
        effects.sleep(poll.min(deadline.saturating_duration_since(now)));
    }
    let seconds = options.stage_timeout.as_secs_f32();
    match reported {
        Some(connected) => {
            let missing = missing_worker_ids(required, &connected);
            anyhow::bail!(
                "{} worker identities are still missing within {seconds}s: {}; check {}",
                missing.len(),
                missing.join(", "),
                effects.worker_log().display()
            )
        }
        None => anyhow::bail!(
            "the engine did not report connected worker identities within {seconds}s; check {}",
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
    launch_env: BTreeMap<String, String>,
    engine: Option<std::process::Child>,
    bus_auth: Option<std::process::Child>,
    workers: Vec<RunningWorker>,
}

impl SystemEffects {
    pub(crate) fn new(paths: &RuntimePaths, launch_env: BTreeMap<String, String>) -> Self {
        Self {
            agentos_home: paths.agentos_home.clone(),
            config_path: paths.config_path.clone(),
            runtime_dir: paths.runtime_dir.clone(),
            launch_env,
            engine: None,
            bus_auth: None,
            workers: Vec::new(),
        }
    }

    /// Values this invocation would hand to a child process: the active `.env`
    /// first, then whatever this process already exports.
    fn launch_value(&self, name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                self.launch_env
                    .get(name)
                    .filter(|value| !value.trim().is_empty())
                    .cloned()
            })
    }
}

fn reported_worker_ids(registry: &Value) -> Option<BTreeSet<String>> {
    if let Some(functions) = registry.get("functions").and_then(Value::as_array) {
        return Some(
            functions
                .iter()
                .filter_map(|function| function["worker_name"].as_str())
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect(),
        );
    }

    // Keep accepting the richer worker registry used by newer iii builds and
    // by older AgentOS test fakes. iii 0.22.1 readiness uses the function
    // registry above because its stable wire identity is `worker_name`.
    let workers = registry.get("workers")?.as_array()?;
    Some(
        workers
            .iter()
            .filter(|worker| worker["status"].as_str() == Some("connected"))
            .filter(|worker| worker["runtime"].as_str() != Some("engine"))
            .filter_map(|worker| worker["name"].as_str())
            .map(str::to_owned)
            .collect(),
    )
}

/// `state::list_keys` answers `{"keys": [...]}` on iii 0.22.1 (verified against
/// the pinned engine). `state::list` returns a bare array of *values* with no
/// key, so it cannot answer "which agent has a document" and is not used here.
fn reported_state_keys(response: &Value) -> Option<Vec<String>> {
    Some(
        response
            .get("keys")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .filter(|key| !key.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

fn parse_registry_output(output: &[u8]) -> Option<Value> {
    serde_json::from_slice(output).ok().or_else(|| {
        let text = String::from_utf8_lossy(output);
        let start = text.find('{')?;
        let end = text.rfind('}')?;
        serde_json::from_str(&text[start..=end]).ok()
    })
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

    fn connected_worker_ids(&self) -> Option<BTreeSet<String>> {
        let binary = self.engine_binary().ok()?;
        let output = std::process::Command::new(binary)
            .args([
                "trigger",
                "engine::functions::list",
                "--json",
                "{}",
                "--timeout-ms",
                "1000",
            ])
            .envs(&self.launch_env)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        reported_worker_ids(&parse_registry_output(&output.stdout)?)
    }

    fn state_keys(&self, scope: &str) -> Option<Vec<String>> {
        let binary = self.engine_binary().ok()?;
        let payload = json!({ "scope": scope }).to_string();
        let output = std::process::Command::new(binary)
            .args([
                "trigger",
                "state::list_keys",
                "--json",
                &payload,
                "--timeout-ms",
                "1000",
            ])
            .envs(&self.launch_env)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        reported_state_keys(&parse_registry_output(&output.stdout)?)
    }

    fn bus_auth_binary(&self) -> Option<PathBuf> {
        crate::find_sibling_binary(BUS_AUTH_BINARY, Some(&self.runtime_dir))
    }

    fn bus_auth_addr(&self) -> Result<SocketAddr> {
        parse_bus_auth_addr(self.launch_value(BUS_AUTH_ADDR_VARIABLE).as_deref())
    }

    fn bus_auth_healthy(&self) -> bool {
        self.bus_auth_addr()
            .is_ok_and(|addr| TcpStream::connect_timeout(&addr, HEALTH_PROBE_TIMEOUT).is_ok())
    }

    fn engine_config(&self) -> Option<String> {
        std::fs::read_to_string(&self.config_path).ok()
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
    fn start_bus_auth(&mut self, binary: &Path, addr: SocketAddr) -> Result<u32> {
        let daemon = crate::spawn_bus_auth(
            binary,
            addr,
            &self.runtime_dir,
            &self.worker_log(),
            &self.launch_env,
            true,
        )?;
        let pid = daemon.id();
        self.bus_auth = Some(daemon);
        Ok(pid)
    }

    fn bus_auth_stopped(&mut self) -> Option<String> {
        let daemon = self.bus_auth.as_mut()?;
        daemon
            .try_wait()
            .ok()
            .flatten()
            .map(|status| status.to_string())
    }

    fn start_engine(&mut self, binary: &Path) -> Result<u32> {
        let engine = crate::spawn_engine(
            binary,
            &self.config_path,
            &self.runtime_dir,
            &self.engine_log(),
            &self.launch_env,
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
            env: &self.launch_env,
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
        // The daemon goes last: it is the gate the rest of the stack talks
        // through, and killing it first would refuse their shutdown traffic.
        if let Some(daemon) = self.bus_auth.as_mut() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        self.bus_auth = None;
    }

    fn run_tui(&mut self, binary: &Path) -> Result<i32> {
        // Inherited stdio: the TUI owns the terminal until the user quits.
        let status = std::process::Command::new(binary)
            .current_dir(&self.runtime_dir)
            .envs(&self.launch_env)
            .status()
            .with_context(|| format!("Failed to start {}", binary.display()))?;
        Ok(status.code().unwrap_or(1))
    }

    fn now(&self) -> Instant {
        Instant::now()
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

    fn ids(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
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
        /// Stable worker identities the engine reports; `None` when silent.
        connected: RefCell<Option<BTreeSet<String>>>,
        /// The bus-auth daemon binary, when it is built.
        bus_auth_binary: Option<PathBuf>,
        /// Listening from this probe onwards; `None` never listens.
        bus_auth_healthy_from: Option<usize>,
        bus_auth_probes: Cell<usize>,
        /// The daemon this invocation started reports exited.
        bus_auth_exits: bool,
        /// Contents of the active `config.yaml`.
        engine_config: Option<String>,
        /// Keys in state scope `agents`; `None` when the engine cannot answer.
        agent_keys: Option<Vec<String>>,
        /// Keys in state scope `capabilities`.
        capability_keys: Option<Vec<String>>,
        identity_probes: Cell<usize>,
        /// Overrides `connected` from this zero-based identity probe onwards.
        connected_from_probe: Option<(usize, BTreeSet<String>)>,
        /// What the engine reports once this invocation started the workers.
        connected_after_start: Option<BTreeSet<String>>,
        workers: Result<Vec<WorkerSpec>, String>,
        worker_start_error: Option<String>,
        /// Workers that exit right after being started.
        worker_exits: Vec<String>,
        started_workers: Cell<bool>,
        started_worker_names: RefCell<Vec<String>>,
        tui: Option<PathBuf>,
        tui_code: i32,
        events: RefCell<Vec<String>>,
        sleeps: Cell<usize>,
        clock_elapsed: Cell<Duration>,
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
                connected: RefCell::new(Some(ids(&["core", "memory"]))),
                bus_auth_binary: Some(PathBuf::from("/release/agentos-bus-authd")),
                bus_auth_healthy_from: Some(0),
                bus_auth_probes: Cell::new(0),
                bus_auth_exits: false,
                engine_config: Some(
                    "workers:\n  - name: iii-worker-manager\n    config:\n      host: 127.0.0.1\n      rbac:\n        auth_function_id: agentos::bus_auth\n  - name: iii-bridge\n    config:\n      url: ws://127.0.0.1:49129\n"
                        .to_string(),
                ),
                agent_keys: Some(vec!["default".to_string()]),
                capability_keys: Some(vec!["default".to_string()]),
                identity_probes: Cell::new(0),
                connected_from_probe: None,
                connected_after_start: Some(ids(&["core", "memory"])),
                workers: Ok(vec![
                    spec("core", WorkerRuntime::Rust, true),
                    spec("memory", WorkerRuntime::Rust, true),
                    spec("embedding", WorkerRuntime::Python, false),
                ]),
                worker_start_error: None,
                worker_exits: Vec::new(),
                started_workers: Cell::new(false),
                started_worker_names: RefCell::new(Vec::new()),
                tui: Some(PathBuf::from("/usr/local/bin/agentos-tui")),
                tui_code: 0,
                events: RefCell::new(Vec::new()),
                sleeps: Cell::new(0),
                clock_elapsed: Cell::new(Duration::ZERO),
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

        fn connected_worker_ids(&self) -> Option<BTreeSet<String>> {
            let probe = self.identity_probes.get();
            self.identity_probes.set(probe + 1);
            let connected = self.connected.borrow().clone();
            if connected.is_some() {
                return connected;
            }
            if let Some((threshold, connected)) = &self.connected_from_probe
                && probe >= *threshold
            {
                return Some(connected.clone());
            }
            None
        }

        fn bus_auth_binary(&self) -> Option<PathBuf> {
            self.bus_auth_binary.clone()
        }

        fn bus_auth_addr(&self) -> Result<SocketAddr> {
            parse_bus_auth_addr(None)
        }

        fn bus_auth_healthy(&self) -> bool {
            let probe = self.bus_auth_probes.get();
            self.bus_auth_probes.set(probe + 1);
            self.bus_auth_healthy_from
                .is_some_and(|threshold| probe >= threshold)
        }

        fn engine_config(&self) -> Option<String> {
            self.engine_config.clone()
        }

        fn state_keys(&self, scope: &str) -> Option<Vec<String>> {
            match scope {
                AGENT_SCOPE => self.agent_keys.clone(),
                CAPABILITY_SCOPE => self.capability_keys.clone(),
                other => panic!("unexpected state scope probed by doctor: {other}"),
            }
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
        fn start_bus_auth(&mut self, _binary: &Path, addr: SocketAddr) -> Result<u32> {
            self.record(&format!("start_bus_auth {addr}"));
            Ok(4242)
        }

        fn bus_auth_stopped(&mut self) -> Option<String> {
            self.bus_auth_exits.then(|| "exit status: 1".to_string())
        }

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
                    *self.connected.borrow_mut() = self.connected_after_start.clone();
                    *self.started_worker_names.borrow_mut() =
                        workers.iter().map(|worker| worker.name.clone()).collect();
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

        fn now(&self) -> Instant {
            Instant::now() + self.clock_elapsed.get()
        }

        fn sleep(&mut self, duration: Duration) {
            self.sleeps.set(self.sleeps.get() + 1);
            self.clock_elapsed.set(self.clock_elapsed.get() + duration);
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
    fn up_stops_at_an_engine_spawn_error_and_cleans_up() {
        let config = existing_config();
        let mut fake = Fake {
            healthy_from: None,
            engine_start_error: Some("permission denied while starting iii".to_string()),
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(false), &config);
        let error = outcome
            .expect_err("engine spawn error must fail")
            .to_string();
        assert!(error.contains("permission denied"), "{error}");
        assert_eq!(
            fake.events(),
            vec!["start_engine".to_string(), "shutdown_started".to_string()]
        );
        assert!(!fake.events().contains(&"start_workers".to_string()));
    }

    #[test]
    fn up_fails_closed_when_the_initial_worker_identity_query_is_unanswered() {
        let config = existing_config();
        let mut fake = Fake {
            connected: RefCell::new(None),
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(false), &config);
        assert!(!matches!(&outcome, Ok(UpOutcome::Ready)));
        let error = outcome
            .expect_err("an unanswered identity query must fail")
            .to_string();
        assert_eq!(error, WORKER_IDENTITIES_UNREPORTED);
        assert!(
            fake.identity_probes.get() > 1,
            "identity query was not retried"
        );
        assert_eq!(fake.sleeps.get(), 6);
        assert_eq!(fake.events(), vec!["shutdown_started".to_string()]);
        assert!(!fake.started_workers.get());
        assert!(fake.started_worker_names.borrow().is_empty());
    }

    #[test]
    fn up_accepts_an_identity_report_that_arrives_within_the_poll_budget() {
        let config = existing_config();
        let mut fake = Fake {
            connected: RefCell::new(None),
            connected_from_probe: Some((2, BTreeSet::new())),
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(false), &config);
        assert_eq!(
            outcome.expect("the third identity query answers"),
            UpOutcome::Ready
        );
        assert_eq!(fake.identity_probes.get(), 4);
        assert_eq!(fake.sleeps.get(), 2);
        assert_eq!(fake.events(), vec!["start_workers".to_string()]);
        assert_eq!(&*fake.started_worker_names.borrow(), &["core", "memory"]);
    }

    #[test]
    fn zero_identity_timeout_probes_once_and_fails_without_sleeping_or_spawning() {
        let config = existing_config();
        let mut fake = Fake {
            connected: RefCell::new(None),
            ..Fake::default()
        };
        let zero_timeout = UpOptions {
            launch_tui: false,
            stage_timeout: Duration::ZERO,
            poll_interval: Duration::ZERO,
        };
        let (outcome, _) = up(&mut fake, &zero_timeout, &config);
        assert_eq!(
            outcome
                .expect_err("an unanswered zero-budget probe must fail")
                .to_string(),
            WORKER_IDENTITIES_UNREPORTED
        );
        assert_eq!(fake.identity_probes.get(), 1);
        assert_eq!(fake.sleeps.get(), 0);
        assert_eq!(fake.events(), vec!["shutdown_started".to_string()]);
        assert!(!fake.started_workers.get());
    }

    #[test]
    fn up_fails_fast_if_the_engine_dies_during_identity_discovery() {
        let config = existing_config();
        let mut fake = Fake {
            unhealthy_from: Some(2),
            connected: RefCell::new(None),
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(false), &config);
        let error = outcome
            .expect_err("engine death must interrupt identity discovery")
            .to_string();
        assert!(
            error.contains("stopped accepting connections on 127.0.0.1:49134"),
            "{error}"
        );
        assert_eq!(fake.identity_probes.get(), 1);
        assert_eq!(fake.sleeps.get(), 1);
        assert_eq!(fake.events(), vec!["shutdown_started".to_string()]);
        assert!(!fake.started_workers.get());
    }

    #[test]
    fn up_reuses_a_healthy_engine_without_spawning_another() {
        let config = existing_config();
        let mut fake = Fake {
            connected: RefCell::new(Some(BTreeSet::new())),
            ..Fake::default()
        };
        let (outcome, output) = up(&mut fake, &options(false), &config);
        assert_eq!(outcome.expect("up succeeds"), UpOutcome::Ready);
        assert_eq!(fake.events(), vec!["start_workers".to_string()]);
        assert_eq!(&*fake.started_worker_names.borrow(), &["core", "memory"]);
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
            connected: RefCell::new(Some(BTreeSet::new())),
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
    fn zero_timeout_and_zero_poll_interval_still_make_one_bounded_probe() {
        let options = UpOptions {
            launch_tui: false,
            stage_timeout: Duration::ZERO,
            poll_interval: Duration::ZERO,
        };
        let (poll, deadline) = poll_plan(&options);
        assert_eq!(poll, Duration::from_millis(1));
        assert!(deadline <= Instant::now());

        let config = existing_config();
        let mut fake = Fake {
            healthy_from: None,
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options, &config);
        assert!(outcome.is_err());
        assert_eq!(fake.sleeps.get(), 0);
        assert!(!fake.events().contains(&"start_workers".to_string()));
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
            connected: RefCell::new(Some(BTreeSet::new())),
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
            connected: RefCell::new(Some(BTreeSet::new())),
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
            connected: RefCell::new(Some(BTreeSet::new())),
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
            connected: RefCell::new(Some(ids(&["core", "memory"]))),
            ..Fake::default()
        };
        let (outcome, output) = up(&mut fake, &options(false), &config);
        assert_eq!(outcome.expect("up succeeds"), UpOutcome::Ready);
        assert!(fake.events().is_empty(), "{:?}", fake.events());
        assert!(
            output.contains("2 required identities already connected"),
            "{output}"
        );
    }

    #[test]
    fn up_starts_the_workers_when_only_part_of_the_required_set_is_connected() {
        let config = existing_config();
        // One of the two Rust workers is on the bus: the stack is not up.
        let mut fake = Fake {
            connected: RefCell::new(Some(ids(&["core"]))),
            ..Fake::default()
        };
        let (outcome, output) = up(&mut fake, &options(false), &config);
        assert_eq!(outcome.expect("up succeeds"), UpOutcome::Ready);
        assert_eq!(fake.events(), vec!["start_workers".to_string()]);
        assert!(
            output.contains("Starting 1 missing workers (1 connected)"),
            "{output}"
        );
        assert_eq!(&*fake.started_worker_names.borrow(), &["memory"]);
    }

    #[test]
    fn up_does_not_accept_an_unrelated_worker_count_as_readiness() {
        let config = existing_config();
        // This would have passed the old count-based readiness check: two
        // workers are connected, but neither required identity is present.
        let mut fake = Fake {
            connected: RefCell::new(Some(ids(&["foreign-a", "foreign-b"]))),
            connected_after_start: Some(ids(&["core", "memory", "foreign-a", "foreign-b"])),
            ..Fake::default()
        };
        let (outcome, output) = up(&mut fake, &options(false), &config);
        assert_eq!(outcome.expect("up succeeds"), UpOutcome::Ready);
        assert_eq!(fake.events(), vec!["start_workers".to_string()]);
        assert_eq!(&*fake.started_worker_names.borrow(), &["core", "memory"]);
        assert!(
            output.contains("Starting 2 missing workers (2 connected)"),
            "{output}"
        );
    }

    #[test]
    fn up_waits_for_the_started_workers_to_connect() {
        let config = existing_config();
        let mut fake = Fake {
            connected: RefCell::new(Some(BTreeSet::new())),
            connected_after_start: Some(ids(&["core", "memory"])),
            ..Fake::default()
        };
        let (outcome, output) = up(&mut fake, &options(false), &config);
        assert_eq!(outcome.expect("up succeeds"), UpOutcome::Ready);
        assert!(output.contains("2 connected"), "{output}");
        assert!(output.contains("2 required identities present"), "{output}");
    }

    #[test]
    fn up_fails_when_the_started_workers_never_connect() {
        let config = existing_config();
        let mut fake = Fake {
            connected: RefCell::new(Some(BTreeSet::new())),
            connected_after_start: Some(ids(&["core"])),
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(true), &config);
        let error = outcome
            .expect_err("workers that never connect must fail")
            .to_string();
        assert!(
            error.contains("1 worker identities are still missing within 3s: memory"),
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
    fn up_fails_when_the_engine_never_reports_worker_identities() {
        let config = existing_config();
        let mut fake = Fake {
            connected: RefCell::new(Some(BTreeSet::new())),
            connected_after_start: None,
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(false), &config);
        let error = outcome.expect_err("a silent engine must fail").to_string();
        assert!(
            error.contains("did not report connected worker identities within 3s"),
            "{error}"
        );
        assert!(error.contains("workers.log"), "{error}");
    }

    #[test]
    fn up_fails_when_a_started_worker_exits_immediately() {
        let config = existing_config();
        let mut fake = Fake {
            connected: RefCell::new(Some(BTreeSet::new())),
            connected_after_start: Some(BTreeSet::new()),
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
            connected: RefCell::new(Some(BTreeSet::new())),
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
            connected: RefCell::new(Some(BTreeSet::new())),
            connected_after_start: Some(BTreeSet::new()),
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

    /// A fully configured machine: its own API key plus one provider
    /// credential, both from the active `.env`.
    fn configured_credentials() -> Credentials {
        let mut values = BTreeMap::new();
        values.insert(API_KEY_VARIABLE.to_string(), "machine-key".to_string());
        values.insert("ANTHROPIC_API_KEY".to_string(), "cloud-key".to_string());
        Credentials::resolve(Path::new("/runtime/.env"), &values, |_| None)
    }

    fn diagnose(fake: &Fake, discovery: ConfigDiscovery, config: &Path) -> (Readiness, String) {
        diagnose_with(fake, discovery, config, &configured_credentials())
    }

    fn diagnose_with(
        fake: &Fake,
        discovery: ConfigDiscovery,
        config: &Path,
        credentials: &Credentials,
    ) -> (Readiness, String) {
        colored::control::set_override(false);
        let report = readiness(fake, &paths(config, discovery), credentials);
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
            output.contains("2 connected; all 2 required worker identities present"),
            "{output}"
        );
        assert!(
            output.contains("accepting connections on 127.0.0.1:49134"),
            "{output}"
        );
    }

    #[test]
    fn doctor_passes_when_every_readiness_input_is_green() {
        let config = existing_config();
        let paths = RuntimePaths {
            agentos_home: config
                .parent()
                .expect("test executable has a parent")
                .to_path_buf(),
            config_path: config.clone(),
            runtime_dir: config
                .parent()
                .expect("test executable has a parent")
                .to_path_buf(),
            discovery: ConfigDiscovery::Checkout,
        };
        let report = readiness(&Fake::default(), &paths, &configured_credentials());
        assert!(
            report.passed(),
            "unexpected failures: {:?}",
            report
                .items
                .iter()
                .filter(|item| !item.passed)
                .map(|item| (item.name, item.detail.as_str()))
                .collect::<Vec<_>>()
        );
        assert!(report.items.iter().all(|item| item.hint.is_none()));
        assert_eq!(report.to_json()["passed"], Value::Bool(true));
    }

    #[test]
    fn doctor_reports_unknown_engine_version_without_failing_binary_presence() {
        let config = existing_config();
        let fake = Fake {
            engine_version: None,
            ..Fake::default()
        };
        let (report, output) = diagnose(&fake, ConfigDiscovery::Checkout, &config);
        let binary = report.item("Engine binary").expect("engine binary item");
        assert!(binary.passed);
        assert!(
            binary.detail.contains("version unknown"),
            "{}",
            binary.detail
        );
        assert!(output.contains("version unknown"), "{output}");
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
            connected: RefCell::new(None),
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
            connected: RefCell::new(Some(BTreeSet::new())),
            ..Fake::default()
        };
        let (report, output) = diagnose(&none, ConfigDiscovery::Checkout, &config);
        let item = report.item("Connected").expect("connected item");
        assert!(!item.passed);
        assert!(
            output.contains("0 connected; missing 2 of 2 required identities: core, memory"),
            "{output}"
        );

        // A partial stack is not ready either: one of two required workers.
        let partial = Fake {
            connected: RefCell::new(Some(ids(&["core"]))),
            ..Fake::default()
        };
        let (report, output) = diagnose(&partial, ConfigDiscovery::Checkout, &config);
        let item = report.item("Connected").expect("connected item");
        assert!(!item.passed);
        assert!(
            output.contains("1 connected; missing 1 of 2 required identities: memory"),
            "{output}"
        );
        assert_eq!(
            item.hint.as_deref(),
            Some("start them with `agentos up --no-tui`")
        );

        let some = Fake {
            connected: RefCell::new(Some(ids(&["core", "memory", "unrelated"]))),
            ..Fake::default()
        };
        let (report, _) = diagnose(&some, ConfigDiscovery::Checkout, &config);
        assert!(report.item("Connected").expect("connected item").passed);
    }

    #[test]
    fn doctor_rejects_misleading_counts_and_silent_identity_reports() {
        let config = existing_config();
        let unrelated = Fake {
            connected: RefCell::new(Some(ids(&["foreign-a", "foreign-b"]))),
            ..Fake::default()
        };
        let (report, output) = diagnose(&unrelated, ConfigDiscovery::Checkout, &config);
        let connected = report.item("Connected").expect("connected item");
        assert!(!connected.passed);
        assert!(
            output.contains("2 connected; missing 2 of 2 required identities: core, memory"),
            "{output}"
        );

        let silent = Fake {
            connected: RefCell::new(None),
            ..Fake::default()
        };
        let (report, output) = diagnose(&silent, ConfigDiscovery::Checkout, &config);
        let connected = report.item("Connected").expect("connected item");
        assert!(!connected.passed);
        assert_eq!(
            connected.hint.as_deref(),
            Some("start the stack with `agentos up`, then re-run `agentos doctor`")
        );
        assert_eq!(connected.detail, WORKER_IDENTITIES_UNREPORTED);
        assert!(
            output.contains("engine did not report connected worker identities"),
            "{output}"
        );
    }

    #[test]
    fn doctor_says_so_when_the_runtime_declares_no_rust_workers() {
        let config = existing_config();
        let fake = Fake {
            // Only a Python worker: nothing this stack can start or count.
            workers: Ok(vec![spec("embedding", WorkerRuntime::Python, false)]),
            connected: RefCell::new(Some(BTreeSet::new())),
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
    fn dotenv_parser_handles_quotes_comments_and_shell_precedence() {
        let values = parse_dotenv(
            "# comment\nPLAIN=value\nexport QUOTED=\"line\\nnext\"\nSINGLE='literal value'\nINLINE=yes # note\nPATH=must-not-override-shell\n",
            Path::new("/runtime/.env"),
        )
        .expect("parse dotenv");
        assert_eq!(values.get("PLAIN").map(String::as_str), Some("value"));
        assert_eq!(values.get("QUOTED").map(String::as_str), Some("line\nnext"));
        assert_eq!(
            values.get("SINGLE").map(String::as_str),
            Some("literal value")
        );
        assert_eq!(values.get("INLINE").map(String::as_str), Some("yes"));
        assert!(!values.contains_key("PATH"), "shell PATH must win");
    }

    #[test]
    fn dotenv_parser_rejects_malformed_assignments() {
        let error = parse_dotenv("NOT AN ASSIGNMENT", Path::new("/runtime/.env"))
            .expect_err("invalid dotenv must fail")
            .to_string();
        assert!(error.contains("/runtime/.env:1"), "{error}");
    }

    #[test]
    fn dotenv_parser_handles_empty_input_duplicates_and_blank_values() {
        assert!(
            parse_dotenv("", Path::new("/runtime/.env"))
                .expect("empty dotenv is valid")
                .is_empty()
        );
        let values = parse_dotenv(
            "AGENTOS_TEST_DUPLICATE=first\nAGENTOS_TEST_DUPLICATE=second\nAGENTOS_TEST_EMPTY=\n",
            Path::new("/runtime/.env"),
        )
        .expect("parse edge-case dotenv");
        assert_eq!(
            values.get("AGENTOS_TEST_DUPLICATE").map(String::as_str),
            Some("first")
        );
        assert_eq!(
            values.get("AGENTOS_TEST_EMPTY").map(String::as_str),
            Some("")
        );
        assert!(
            load_dotenv(Path::new("/agentos-test-path-that-does-not-exist"))
                .expect("an absent dotenv is optional")
                .is_empty()
        );
    }

    #[test]
    fn dotenv_parser_rejects_invalid_keys_and_unclosed_quotes() {
        for (source, expected) in [
            ("9INVALID=value", "Invalid dotenv key"),
            ("BAD-KEY=value", "Invalid dotenv key"),
            ("KEY='unterminated", "Unclosed single quote"),
            ("KEY=\"unterminated", "Unclosed double quote"),
        ] {
            let error = parse_dotenv(source, Path::new("/runtime/.env"))
                .expect_err("malformed dotenv must fail")
                .to_string();
            assert!(error.contains(expected), "{source:?}: {error}");
            assert!(error.contains("/runtime/.env:1"), "{source:?}: {error}");
        }
    }

    // -----------------------------------------------------------------------
    // first run: AGENTOS_API_KEY, provider credential, default route
    // -----------------------------------------------------------------------

    /// A private directory for dotenv tests. Nothing here touches the process
    /// environment, so these run in parallel with everything else.
    fn temporary_runtime(label: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agentos-first-run-{label}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temporary runtime");
        path
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .expect("stat file")
            .permissions()
            .mode()
            & 0o777
    }

    fn fixed_key() -> Result<String> {
        Ok("f".repeat(64))
    }

    #[test]
    fn api_key_is_generated_into_an_absent_dotenv_with_mode_0600() {
        let runtime = temporary_runtime("generate");
        let outcome = ensure_api_key_with(&runtime, || None, fixed_key).expect("generate key");
        let path = dotenv_path(&runtime);
        assert_eq!(outcome, ApiKeyOutcome::Generated(path.clone()));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read dotenv"),
            format!("{API_KEY_VARIABLE}={}\n", "f".repeat(64))
        );
        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600);
        assert!(
            outcome.describe().contains(&path.display().to_string()),
            "the message must name the file it wrote: {}",
            outcome.describe()
        );
        std::fs::remove_dir_all(&runtime).expect("clean up");
    }

    #[test]
    fn api_key_generation_never_overwrites_an_existing_value() {
        let runtime = temporary_runtime("keep");
        let path = dotenv_path(&runtime);
        std::fs::write(&path, "AGENTOS_API_KEY=operator-secret\n").expect("write dotenv");
        let outcome = ensure_api_key_with(&runtime, || None, fixed_key).expect("inspect key");
        assert_eq!(outcome, ApiKeyOutcome::AlreadyPresent(path.clone()));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read dotenv"),
            "AGENTOS_API_KEY=operator-secret\n"
        );
        std::fs::remove_dir_all(&runtime).expect("clean up");
    }

    #[test]
    fn api_key_generation_fills_an_empty_assignment_in_place() {
        let runtime = temporary_runtime("fill");
        let path = dotenv_path(&runtime);
        // The shape `.env.example` ships: the key is declared and empty.
        std::fs::write(
            &path,
            "# comment\nANTHROPIC_API_KEY=cloud\nAGENTOS_API_KEY=\nIII_URL=ws://localhost:49134\n",
        )
        .expect("write dotenv");
        let outcome = ensure_api_key_with(&runtime, || None, fixed_key).expect("generate key");
        assert_eq!(outcome, ApiKeyOutcome::Generated(path.clone()));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read dotenv"),
            format!(
                "# comment\nANTHROPIC_API_KEY=cloud\n{API_KEY_VARIABLE}={}\nIII_URL=ws://localhost:49134\n",
                "f".repeat(64)
            )
        );
        std::fs::remove_dir_all(&runtime).expect("clean up");
    }

    #[test]
    fn api_key_generation_defers_to_an_exported_value() {
        let runtime = temporary_runtime("inherited");
        let outcome = ensure_api_key_with(
            &runtime,
            || Some("from-shell".to_string()),
            || panic!("must not generate a key when the environment exports one"),
        )
        .expect("inherit key");
        assert_eq!(outcome, ApiKeyOutcome::Inherited);
        assert!(
            !dotenv_path(&runtime).exists(),
            "an exported key must not cause a second, inert value in .env"
        );
        std::fs::remove_dir_all(&runtime).expect("clean up");
    }

    #[test]
    fn generated_api_keys_are_random_and_32_bytes_wide() {
        let first = random_api_key().expect("first key");
        let second = random_api_key().expect("second key");
        assert_eq!(first.len(), API_KEY_BYTES * 2, "{first}");
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(first, second);
    }

    #[test]
    fn dotenv_writes_refuse_to_replace_a_symlink() {
        let runtime = temporary_runtime("symlink");
        let target = runtime.join("real.env");
        std::fs::write(&target, "AGENTOS_API_KEY=\n").expect("write target");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, dotenv_path(&runtime)).expect("create symlink");
        let error = ensure_api_key_with(&runtime, || None, fixed_key)
            .expect_err("a symlinked .env must be refused")
            .to_string();
        assert!(error.contains("symlink"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&target).expect("read target"),
            "AGENTOS_API_KEY=\n",
            "the symlink target must be left untouched"
        );
        std::fs::remove_dir_all(&runtime).expect("clean up");
    }

    #[test]
    fn set_dotenv_value_replaces_one_assignment_and_keeps_the_rest() {
        let runtime = temporary_runtime("set-value");
        let path = dotenv_path(&runtime);
        std::fs::write(
            &path,
            "ANTHROPIC_API_KEY=old\n# keep me\nOPENAI_API_KEY=other\n",
        )
        .expect("write dotenv");
        let written = set_dotenv_value(&runtime, "ANTHROPIC_API_KEY", "new").expect("set value");
        assert_eq!(written, path);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read dotenv"),
            "ANTHROPIC_API_KEY=new\n# keep me\nOPENAI_API_KEY=other\n"
        );
        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600);
        std::fs::remove_dir_all(&runtime).expect("clean up");
    }

    #[test]
    fn provider_variables_match_the_llm_router_table() {
        assert_eq!(provider_variable("anthropic"), Some("ANTHROPIC_API_KEY"));
        assert_eq!(provider_variable("codex"), Some("CODEX_PROXY_API_KEY"));
        assert_eq!(provider_variable("openai"), Some("OPENAI_API_KEY"));
        // `ollama` has an empty env_key in the router table, and an unknown
        // provider must not be silently written into the environment.
        assert_eq!(provider_variable("ollama"), None);
        assert_eq!(provider_variable("not-a-provider"), None);
    }

    fn credentials_for(pairs: &[(&str, &str)]) -> Credentials {
        let values = pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect::<BTreeMap<_, _>>();
        Credentials::resolve(Path::new("/runtime/.env"), &values, |_| None)
    }

    fn route_for(pairs: &[(&str, &str)]) -> DefaultRoute {
        credentials_for(pairs).route
    }

    #[test]
    fn default_route_mirrors_the_llm_router_resolution() {
        // A pinned default is live when the credential *that provider* needs is
        // present - not when CODEX_PROXY_API_KEY happens to be set.
        assert_eq!(
            route_for(&[
                ("CODEX_PROXY_API_KEY", "proxy"),
                ("AGENTOS_DEFAULT_PROVIDER", "codex"),
                ("AGENTOS_DEFAULT_MODEL", "gpt-5.6-sol"),
            ]),
            DefaultRoute::Pinned {
                provider: "codex".to_string(),
                model: Some("gpt-5.6-sol".to_string()),
            }
        );
        assert_eq!(
            route_for(&[
                ("OPENAI_API_KEY", "cloud"),
                ("AGENTOS_DEFAULT_PROVIDER", "openai"),
            ]),
            DefaultRoute::Pinned {
                provider: "openai".to_string(),
                model: None,
            },
            "a pinned provider with its own credential must not need the codex proxy key"
        );
        // A model on its own means the codex provider, as the router resolves it.
        assert_eq!(
            route_for(&[("CODEX_PROXY_API_KEY", "proxy")]),
            DefaultRoute::Automatic {
                provider: CODEX_PROVIDER
            },
            "no default was requested, so routing is automatic"
        );

        // `.env.example`'s shipped shape: codex is pinned, its proxy key is
        // empty, and the first credential in preference order takes over.
        let disabled = credentials_for(&[
            ("CODEX_PROXY_API_KEY", ""),
            ("AGENTOS_DEFAULT_PROVIDER", "codex"),
            ("AGENTOS_DEFAULT_MODEL", "gpt-5.6-sol"),
            ("ANTHROPIC_API_KEY", "cloud"),
        ]);
        assert_eq!(
            disabled.route,
            DefaultRoute::Automatic {
                provider: "anthropic"
            }
        );
        assert_eq!(disabled.disabled_default.as_deref(), Some("codex"));

        // Preference order decides, and it is llm-router's AUTO_ROUTE_PREFERENCE.
        assert_eq!(
            route_for(&[("OPENROUTER_API_KEY", "x"), ("OPENAI_API_KEY", "y")]),
            DefaultRoute::Automatic { provider: "openai" }
        );
        assert_eq!(
            route_for(&[("OPENROUTER_API_KEY", "x")]),
            DefaultRoute::Automatic {
                provider: "openrouter"
            }
        );

        // Nothing configured at all: unqualified chat is refused, and doctor
        // must say that instead of naming a provider that cannot answer.
        assert_eq!(route_for(&[]), DefaultRoute::Unavailable);
        let pinned_without_credential =
            credentials_for(&[("AGENTOS_DEFAULT_MODEL", "gpt-5.6-sol")]);
        assert_eq!(pinned_without_credential.route, DefaultRoute::Unavailable);
        assert_eq!(
            pinned_without_credential.disabled_default.as_deref(),
            Some(CODEX_PROVIDER)
        );
        assert!(
            pinned_without_credential
                .route
                .detail()
                .contains("provider_credential_missing"),
            "doctor must use llm-router's own refusal vocabulary: {}",
            pinned_without_credential.route.detail()
        );
    }

    #[test]
    fn credential_variables_are_listed_in_router_preference_order() {
        // llm-router's provider_credential_missing lists the same variables in
        // the same order; a user who reads either message sees one contract.
        assert_eq!(
            credential_variables(),
            "ANTHROPIC_API_KEY, OPENAI_API_KEY, GOOGLE_API_KEY, CODEX_PROXY_API_KEY, GROQ_API_KEY, DEEPSEEK_API_KEY, MISTRAL_API_KEY, TOGETHER_API_KEY, FIREWORKS_API_KEY, OPENROUTER_API_KEY"
        );
    }

    #[test]
    fn credentials_prefer_the_process_environment_over_the_file() {
        let mut values = BTreeMap::new();
        values.insert(API_KEY_VARIABLE.to_string(), "from-file".to_string());
        values.insert("ANTHROPIC_API_KEY".to_string(), "  ".to_string());
        let credentials = Credentials::resolve(Path::new("/runtime/.env"), &values, |name| {
            (name == "ANTHROPIC_API_KEY").then(|| "from-shell".to_string())
        });
        assert_eq!(credentials.api_key, Some(CredentialSource::Dotenv));
        assert_eq!(
            credentials.providers,
            vec![ProviderCredential {
                provider: "anthropic",
                variable: "ANTHROPIC_API_KEY",
                source: CredentialSource::Environment,
            }],
            "a blank file value must not mask the exported credential"
        );
    }

    #[test]
    fn dotenv_parsing_separates_the_file_view_from_the_process_view() {
        let path = Path::new("/runtime/.env");
        let source = "AGENTOS_API_KEY=from-file\nANTHROPIC_API_KEY=cloud\n";
        let all = parse_dotenv_all(source, path).expect("parse every assignment");
        assert_eq!(
            all.get(API_KEY_VARIABLE).map(String::as_str),
            Some("from-file")
        );
        // SAFETY: single-threaded assertion on a variable this crate owns.
        unsafe { std::env::set_var("AGENTOS_TEST_DOTENV_VIEW", "exported") };
        let exported = parse_dotenv(
            "AGENTOS_TEST_DOTENV_VIEW=from-file\nANTHROPIC_API_KEY=cloud\n",
            path,
        )
        .expect("parse spawn view");
        unsafe { std::env::remove_var("AGENTOS_TEST_DOTENV_VIEW") };
        assert!(
            !exported.contains_key("AGENTOS_TEST_DOTENV_VIEW"),
            "an exported key must keep its exported value at spawn time"
        );
        assert_eq!(
            exported.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("cloud")
        );
    }

    #[test]
    fn doctor_reports_the_api_key_provider_and_route() {
        let config = existing_config();
        let mut values = BTreeMap::new();
        values.insert(API_KEY_VARIABLE.to_string(), "machine-key".to_string());
        values.insert("CODEX_PROXY_API_KEY".to_string(), "proxy".to_string());
        values.insert(
            "AGENTOS_DEFAULT_MODEL".to_string(),
            "gpt-5.6-sol".to_string(),
        );
        let credentials = Credentials::resolve(Path::new("/runtime/.env"), &values, |_| None);
        let (report, output) = diagnose_with(
            &Fake::default(),
            ConfigDiscovery::Checkout,
            &config,
            &credentials,
        );

        let key = report.item("API key").expect("API key item");
        assert!(key.passed, "{}", key.detail);
        assert!(key.detail.contains(".env"), "{}", key.detail);
        let provider = report.item("Provider").expect("provider item");
        assert!(provider.passed);
        assert!(
            provider.detail.contains("codex via CODEX_PROXY_API_KEY"),
            "{}",
            provider.detail
        );
        let route = report.item("Route").expect("route item");
        assert!(route.passed);
        assert_eq!(
            route.detail,
            "codex/gpt-5.6-sol (pinned by AGENTOS_DEFAULT_PROVIDER/MODEL)"
        );
        assert!(output.contains("codex/gpt-5.6-sol"), "{output}");
    }

    #[test]
    fn doctor_names_the_missing_api_key_as_the_cause_of_missing_identities() {
        let config = existing_config();
        let credentials =
            Credentials::resolve(Path::new("/runtime/.env"), &BTreeMap::new(), |_| None);
        let fake = Fake {
            // The engine is up and one of the two required workers exited,
            // exactly what an unset AGENTOS_API_KEY produces.
            connected: RefCell::new(Some(ids(&["core"]))),
            connected_after_start: Some(ids(&["core"])),
            ..Fake::default()
        };
        let (report, output) =
            diagnose_with(&fake, ConfigDiscovery::Checkout, &config, &credentials);

        let key = report.item("API key").expect("API key item");
        assert!(!key.passed);
        assert!(
            key.detail.contains("protected HTTP routes"),
            "{}",
            key.detail
        );
        let connected = report.item("Connected").expect("connected item");
        assert!(!connected.passed);
        let hint = connected.hint.clone().expect("missing identities hint");
        assert!(
            hint.contains(API_KEY_VARIABLE),
            "the hint must name the real cause, not just 'start them': {hint}"
        );
        let provider = report.item("Provider").expect("provider item");
        assert!(!provider.passed);
        assert_eq!(provider.detail, "no provider credential is set");
        let route = report.item("Route").expect("route item");
        assert!(!route.passed);
        assert!(
            route.detail.contains("provider_credential_missing"),
            "{}",
            route.detail
        );
        assert!(
            route
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("ANTHROPIC_API_KEY")),
            "the hint must name the variables llm-router names: {:?}",
            route.hint
        );
        assert!(output.contains(API_KEY_VARIABLE), "{output}");
    }

    #[test]
    fn doctor_keeps_the_start_hint_when_the_api_key_is_present() {
        let config = existing_config();
        let fake = Fake {
            connected: RefCell::new(Some(ids(&["core"]))),
            connected_after_start: Some(ids(&["core"])),
            ..Fake::default()
        };
        let (report, _) = diagnose(&fake, ConfigDiscovery::Checkout, &config);
        let connected = report.item("Connected").expect("connected item");
        assert_eq!(
            connected.hint.as_deref(),
            Some("start them with `agentos up --no-tui`")
        );
    }

    #[test]
    fn state_list_keys_shape_matches_iii_0_22_1() {
        // Verified against the pinned engine: `state::list_keys` answers
        // {"keys": [...]}, `state::list` answers a bare array of values, and
        // `state::list_groups` answers {"groups": [...]}.
        assert_eq!(
            reported_state_keys(&json!({ "keys": ["default", "researcher"] })),
            Some(vec!["default".to_string(), "researcher".to_string()])
        );
        assert_eq!(
            reported_state_keys(&json!({ "keys": [] })),
            Some(Vec::new())
        );
        assert_eq!(
            reported_state_keys(&json!({ "keys": ["", 7, "kept"] })),
            Some(vec!["kept".to_string()]),
            "non-string and empty keys must not become agent ids"
        );
        // The bare-array shape belongs to `state::list`; reading it as keys
        // would silently report every agent as unprovisioned.
        assert_eq!(reported_state_keys(&json!(["default"])), None);
        assert_eq!(
            reported_state_keys(&json!({ "groups": ["capabilities"] })),
            None
        );
    }

    #[test]
    fn doctor_names_agents_without_a_capability_document() {
        let config = existing_config();
        let fake = Fake {
            agent_keys: Some(vec!["default".to_string(), "researcher".to_string()]),
            capability_keys: Some(vec!["default".to_string()]),
            ..Fake::default()
        };
        let (report, output) = diagnose(&fake, ConfigDiscovery::Checkout, &config);
        let item = report.item("Capabilities").expect("capabilities item");
        assert!(!item.passed);
        assert!(item.detail.contains("researcher"), "{}", item.detail);
        assert!(
            item.detail.contains("every tool call for them is denied"),
            "{}",
            item.detail
        );
        assert!(output.contains("researcher"), "{output}");
    }

    #[test]
    fn doctor_flags_the_tui_fallback_agent_when_nothing_is_provisioned() {
        let config = existing_config();
        let fake = Fake {
            agent_keys: Some(Vec::new()),
            capability_keys: Some(Vec::new()),
            ..Fake::default()
        };
        let (report, _) = diagnose(&fake, ConfigDiscovery::Checkout, &config);
        let item = report.item("Capabilities").expect("capabilities item");
        assert!(!item.passed);
        assert!(
            item.detail.contains(FALLBACK_AGENT_ID),
            "the fresh-install case must name the agent the TUI would use: {}",
            item.detail
        );
        assert_eq!(
            item.hint.as_deref(),
            Some("create an agent with `agentos agent new`, then re-run `agentos doctor`")
        );
    }

    #[test]
    fn doctor_accepts_a_provisioned_fallback_agent_without_agent_records() {
        let config = existing_config();
        let fake = Fake {
            agent_keys: Some(Vec::new()),
            capability_keys: Some(vec![FALLBACK_AGENT_ID.to_string()]),
            ..Fake::default()
        };
        let (report, _) = diagnose(&fake, ConfigDiscovery::Checkout, &config);
        let item = report.item("Capabilities").expect("capabilities item");
        assert!(item.passed, "{}", item.detail);
    }

    #[test]
    fn doctor_reports_capabilities_as_unknown_when_the_engine_is_silent() {
        let config = existing_config();
        let fake = Fake {
            agent_keys: None,
            capability_keys: None,
            ..Fake::default()
        };
        let (report, _) = diagnose(&fake, ConfigDiscovery::Checkout, &config);
        let item = report.item("Capabilities").expect("capabilities item");
        assert!(!item.passed);
        assert_eq!(item.detail, "the engine did not answer state::list_keys");
    }

    // -----------------------------------------------------------------------
    // bus RBAC: the auth daemon starts before the engine
    // -----------------------------------------------------------------------

    /// A `config.yaml` with the RBAC block, optionally pointing the bridge
    /// somewhere else than the daemon.
    fn armed_config(bridge_url: &str) -> String {
        format!(
            "workers:\n  - name: iii-worker-manager\n    config:\n      host: 127.0.0.1\n      rbac:\n        auth_function_id: agentos::bus_auth\n  - name: iii-bridge\n    config:\n      url: {bridge_url}\n"
        )
    }

    #[test]
    fn bus_auth_address_comes_from_the_environment_or_the_daemon_default() {
        assert_eq!(
            parse_bus_auth_addr(None).expect("default"),
            "127.0.0.1:49129"
                .parse::<SocketAddr>()
                .expect("default addr")
        );
        assert_eq!(
            parse_bus_auth_addr(Some("  ")).expect("blank falls back"),
            "127.0.0.1:49129"
                .parse::<SocketAddr>()
                .expect("default addr")
        );
        assert_eq!(
            parse_bus_auth_addr(Some("127.0.0.1:49999")).expect("explicit"),
            "127.0.0.1:49999"
                .parse::<SocketAddr>()
                .expect("explicit addr")
        );
        // Never silently fall back: the engine would fail closed against the
        // wrong port and refuse every worker.
        let error = parse_bus_auth_addr(Some("not-an-address"))
            .expect_err("garbage must not resolve")
            .to_string();
        assert!(error.contains(BUS_AUTH_ADDR_VARIABLE), "{error}");
    }

    #[test]
    fn bridge_url_endpoint_reads_the_forwarding_target() {
        assert_eq!(
            bridge_url_endpoint(&armed_config("ws://127.0.0.1:49129")).as_deref(),
            Some("127.0.0.1:49129")
        );
        assert_eq!(
            bridge_url_endpoint("    url: \"wss://example.test:8443/\"\n").as_deref(),
            Some("example.test:8443")
        );
        assert_eq!(bridge_url_endpoint("workers: []\n"), None);
    }

    #[test]
    fn up_starts_the_bus_auth_daemon_before_the_engine() {
        let config = existing_config();
        let mut fake = Fake {
            // Nothing is listening until the daemon this invocation starts.
            bus_auth_healthy_from: Some(1),
            healthy_from: Some(1),
            ..Fake::default()
        };
        let (outcome, output) = up(&mut fake, &options(false), &config);
        assert!(outcome.is_ok(), "{outcome:?}");

        let events = fake.events();
        let daemon = events
            .iter()
            .position(|event| event.starts_with("start_bus_auth"))
            .expect("daemon must be started");
        let engine = events
            .iter()
            .position(|event| event == "start_engine")
            .expect("engine must be started");
        assert!(
            daemon < engine,
            "the daemon must be listening before the engine accepts its first bus connection: {events:?}"
        );
        assert!(
            events[daemon].contains("127.0.0.1:49129"),
            "{:?}",
            events[daemon]
        );
        assert!(output.contains("Bus auth"), "{output}");
        assert!(output.contains("listening on 127.0.0.1:49129"), "{output}");
    }

    #[test]
    fn up_reuses_a_bus_auth_daemon_that_is_already_listening() {
        let config = existing_config();
        let mut fake = Fake::default();
        let (outcome, output) = up(&mut fake, &options(false), &config);
        assert!(outcome.is_ok(), "{outcome:?}");
        assert!(
            !fake
                .events()
                .iter()
                .any(|event| event.starts_with("start_bus_auth")),
            "a second daemon would fail to bind the same port: {:?}",
            fake.events()
        );
        assert!(
            output.contains("already listening on 127.0.0.1:49129"),
            "{output}"
        );
    }

    #[test]
    fn up_fails_when_the_gate_is_armed_and_the_daemon_is_not_built() {
        let config = existing_config();
        let mut fake = Fake {
            bus_auth_binary: None,
            bus_auth_healthy_from: None,
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(false), &config);
        let error = outcome
            .expect_err("an armed gate without a daemon cannot boot")
            .to_string();
        assert!(error.contains(BUS_AUTH_BINARY), "{error}");
        assert!(error.contains("refuses every bus connection"), "{error}");
        assert!(
            !fake.events().contains(&"start_engine".to_string()),
            "the engine must not start into a gate nothing answers"
        );
    }

    #[test]
    fn up_skips_the_daemon_when_bus_rbac_is_not_armed() {
        let config = existing_config();
        let mut fake = Fake {
            bus_auth_healthy_from: None,
            engine_config: Some("workers:\n  - name: state\n".to_string()),
            ..Fake::default()
        };
        let (outcome, output) = up(&mut fake, &options(false), &config);
        assert!(outcome.is_ok(), "{outcome:?}");
        assert!(
            !fake
                .events()
                .iter()
                .any(|event| event.starts_with("start_bus_auth")),
            "{:?}",
            fake.events()
        );
        assert!(output.contains("bus RBAC is not armed"), "{output}");
    }

    #[test]
    fn up_fails_when_the_daemon_exits_before_it_listens() {
        let config = existing_config();
        let mut fake = Fake {
            bus_auth_healthy_from: None,
            bus_auth_exits: true,
            ..Fake::default()
        };
        let (outcome, _) = up(&mut fake, &options(false), &config);
        let error = outcome
            .expect_err("a dead daemon must stop the boot")
            .to_string();
        assert!(error.contains(API_KEY_VARIABLE), "{error}");
        assert!(!fake.events().contains(&"start_engine".to_string()));
    }

    #[test]
    fn up_warns_when_the_bridge_url_points_somewhere_else() {
        let config = existing_config();
        let mut fake = Fake {
            engine_config: Some(armed_config("ws://127.0.0.1:49999")),
            ..Fake::default()
        };
        let (outcome, output) = up(&mut fake, &options(false), &config);
        assert!(outcome.is_ok(), "{outcome:?}");
        assert!(
            output.contains("does not match the iii-bridge url 127.0.0.1:49999"),
            "{output}"
        );
    }

    #[test]
    fn doctor_reports_the_bus_auth_daemon() {
        let config = existing_config();

        let (report, output) = diagnose(&Fake::default(), ConfigDiscovery::Checkout, &config);
        let item = report.item("Bus auth").expect("bus auth item");
        assert!(item.passed);
        assert!(
            item.detail
                .contains("bus-auth daemon: listening on 127.0.0.1:49129"),
            "{}",
            item.detail
        );
        assert!(output.contains("bus-auth daemon"), "{output}");

        let down = Fake {
            bus_auth_healthy_from: None,
            ..Fake::default()
        };
        let (report, _) = diagnose(&down, ConfigDiscovery::Checkout, &config);
        let item = report.item("Bus auth").expect("bus auth item");
        assert!(!item.passed);
        assert!(
            item.detail.contains(
                "bus-auth daemon: not running on 127.0.0.1:49129 (bus RBAC is fail-closed"
            ),
            "{}",
            item.detail
        );
        assert!(
            item.detail.contains("refuse every worker while it is down"),
            "{}",
            item.detail
        );

        // Not armed: a daemon that is not running is not a fault.
        let unarmed = Fake {
            bus_auth_healthy_from: None,
            engine_config: Some("workers:\n  - name: state\n".to_string()),
            ..Fake::default()
        };
        let (report, _) = diagnose(&unarmed, ConfigDiscovery::Checkout, &config);
        let item = report.item("Bus auth").expect("bus auth item");
        assert!(item.passed, "{}", item.detail);
        assert!(item.detail.contains("not armed"), "{}", item.detail);
    }

    #[test]
    fn doctor_reports_a_bridge_url_that_cannot_be_reached_by_the_daemon() {
        let config = existing_config();
        let fake = Fake {
            engine_config: Some(armed_config("ws://127.0.0.1:49999")),
            ..Fake::default()
        };
        let (report, _) = diagnose(&fake, ConfigDiscovery::Checkout, &config);
        let item = report.item("Bus auth").expect("bus auth item");
        assert!(
            item.detail
                .contains("the iii-bridge url is 127.0.0.1:49999"),
            "{}",
            item.detail
        );
    }

    #[test]
    fn engine_function_list_reports_iii_0_22_1_worker_identities() {
        assert_eq!(
            reported_worker_ids(&json!({
                "functions": [
                    {"function_id": "core::run", "worker_name": "core"},
                    {"function_id": "core::status", "worker_name": "core"},
                    {"function_id": "memory::recall", "worker_name": "memory"}
                ]
            })),
            Some(ids(&["core", "memory"]))
        );
        assert_eq!(
            reported_worker_ids(&json!({ "functions": [] })),
            Some(BTreeSet::new()),
            "an answered empty registry must remain distinct from no answer"
        );
        assert_eq!(
            reported_worker_ids(&json!({
                "functions": [
                    {"function_id": "missing::identity"},
                    {"function_id": "empty::identity", "worker_name": ""},
                    {"function_id": "wrong::identity", "worker_name": 7},
                    {"function_id": "core::run", "worker_name": "core"}
                ]
            })),
            Some(ids(&["core"]))
        );
        assert_eq!(reported_worker_ids(&json!({ "functions": 62 })), None);
    }

    #[test]
    fn engine_worker_list_fallback_reports_connected_non_engine_identities() {
        assert_eq!(
            reported_worker_ids(&json!({
                "workers": [
                    {"name": "core", "runtime": "rust", "status": "connected"},
                    {"name": "memory", "runtime": "rust", "status": "disconnected"},
                    {"name": "queue", "runtime": "engine", "status": "connected"}
                ]
            })),
            Some(ids(&["core"]))
        );
        assert_eq!(reported_worker_ids(&json!({ "workers": 62 })), None);
        assert_eq!(reported_worker_ids(&json!({ "status": "ok" })), None);
    }

    #[test]
    fn engine_registry_parser_handles_wrapped_empty_and_malformed_output() {
        let wrapped = b"iii 0.22.1 diagnostics\n{\"functions\":[{\"function_id\":\"core::run\",\"worker_name\":\"core\"}]}\n";
        let parsed = parse_registry_output(wrapped).expect("parse wrapped JSON output");
        assert_eq!(reported_worker_ids(&parsed), Some(ids(&["core"])));
        assert_eq!(parse_registry_output(b""), None);
        assert_eq!(parse_registry_output(b"diagnostic only"), None);
        assert_eq!(parse_registry_output(b"prefix {not-json} suffix"), None);
    }
}
