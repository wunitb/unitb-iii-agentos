//! `agentos-bus-authd` — answers the engine's bus RBAC hooks.
//!
//! Started before the engine by `agentos up`; the engine reaches it through its
//! builtin `iii-bridge` worker:
//!
//! ```yaml
//! - name: iii-worker-manager
//!   config:
//!     host: 127.0.0.1
//!     rbac:
//!       auth_function_id: agentos::bus_auth
//!       on_function_registration_function_id: agentos::bus_on_register
//!       on_trigger_registration_function_id: agentos::bus_on_trigger
//!       on_trigger_type_registration_function_id: agentos::bus_on_trigger_type
//!       expose_functions: ['match("*")']
//! - name: iii-bridge
//!   config:
//!     url: ws://127.0.0.1:49129
//!     forward:
//!       - { local_function: agentos::bus_auth, remote_function: agentos::bus_auth }
//!       - { local_function: agentos::bus_on_register, remote_function: agentos::bus_on_register }
//!       - { local_function: agentos::bus_on_trigger, remote_function: agentos::bus_on_trigger }
//!       - { local_function: agentos::bus_on_trigger_type, remote_function: agentos::bus_on_trigger_type }
//! ```
//!
//! It also refuses to start when the config it is about to gate names hooks this
//! binary does not serve. The engine cannot do that check for us: the nested
//! `rbac` struct is not `deny_unknown_fields`, so a typo there disarms the gate
//! in silence. See `agentos_bus_auth::config`.
//!
//! Exits non-zero when `AGENTOS_API_KEY` is unset: a daemon without a key would
//! file every connection under the untrusted tier, which reads as "RBAC is on"
//! while granting nobody the trusted tier. Refusing to start is the honest
//! failure, and it fails the boot closed.

use agentos_bus_auth::config::{self, GateStatus};
use agentos_bus_auth::daemon::{DEFAULT_LISTEN_ADDR, serve};
use agentos_bus_auth::policy::{API_KEY_ENV, expected_api_key};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Deliberately not `env-filter`: that feature pulls a new package into
    // Cargo.lock for a daemon whose whole log surface is one line per bus
    // connection. RUST_LOG picks the level, nothing finer.
    let level = match std::env::var("RUST_LOG").unwrap_or_default().as_str() {
        "trace" => tracing::Level::TRACE,
        "debug" => tracing::Level::DEBUG,
        "warn" => tracing::Level::WARN,
        "error" => tracing::Level::ERROR,
        _ => tracing::Level::INFO,
    };
    tracing_subscriber::fmt().with_max_level(level).init();

    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let addr = arguments
        .iter()
        .fold(None, |found, arg| {
            arg.strip_prefix("--listen=").map(str::to_string).or(found)
        })
        .or_else(|| std::env::var("AGENTOS_BUS_AUTH_ADDR").ok())
        .unwrap_or_else(|| DEFAULT_LISTEN_ADDR.to_string());

    let Some(key) = expected_api_key() else {
        anyhow::bail!(
            "{API_KEY_ENV} is not set. The bus-auth daemon refuses to start without it: \
             every bus connection would fall into the untrusted tier and no worker \
             could reach the vault, the hooks or the trigger factory. Run `agentos up`, \
             which generates the key into the active .env, or export it yourself."
        );
    };

    let config_path = arguments
        .iter()
        .find_map(|arg| arg.strip_prefix("--config="))
        .map(str::to_string);
    check_config(config_path.as_deref())?;

    let listener = TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "bus-auth daemon listening");
    serve(listener, key).await
}

/// Refuse to gate a config that would not do what it says.
///
/// The engine accepts an unknown key inside `rbac:` and a hook id nothing
/// answers, in both cases without an error, so this is the only place the
/// operator can be told. A config that is not armed at all, or that cannot be
/// found, is not an error: the shipped default is unarmed.
fn check_config(explicit: Option<&str>) -> anyhow::Result<()> {
    let Some(source) = config::discover(explicit) else {
        tracing::info!(
            "no engine config found (pass --config=<path> or set {}); the armed-gate check was skipped",
            config::CONFIG_PATH_ENV
        );
        return Ok(());
    };
    let path = source.path().to_path_buf();
    let document = match std::fs::read_to_string(&path) {
        Ok(document) => document,
        // A path someone NAMED and this daemon cannot read is a typo away from
        // switching the whole check off, which is the defect class this check
        // exists to catch. Only the `./config.yaml` probe may be skipped.
        Err(error) if source.is_named() => anyhow::bail!(
            "cannot read {} ({error}). It was named by --config= or {}, and this daemon will not \
             gate a config it cannot check: a typo in that path would silently disable the check \
             that catches typos.",
            path.display(),
            config::CONFIG_PATH_ENV
        ),
        Err(error) => {
            tracing::info!(path = %path.display(), %error, "engine config unreadable; the armed-gate check was skipped");
            return Ok(());
        }
    };
    match config::inspect(&document) {
        GateStatus::NotArmed => {
            tracing::info!(path = %path.display(), "bus RBAC is not armed in this config");
            Ok(())
        }
        GateStatus::Armed => {
            tracing::info!(path = %path.display(), "bus RBAC is armed and names the hooks this daemon serves");
            Ok(())
        }
        // The engine parses the same file and refuses to boot on it, so this is
        // loud without being fatal here.
        GateStatus::Unknown(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "engine config could not be parsed, so the armed-gate check could not run; the engine will refuse this file too"
            );
            Ok(())
        }
        GateStatus::Inconsistent(problems) => anyhow::bail!(
            "{} arms bus RBAC in a way this daemon cannot honour, and the engine would NOT tell \
             you: the nested `rbac` struct is not deny_unknown_fields, so it ignores what it does \
             not know and admits every bus connection.\n{}",
            path.display(),
            problems
                .iter()
                .map(|problem| format!("  - {problem}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}
