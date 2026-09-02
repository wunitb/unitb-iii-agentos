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
//!       expose_functions: ['match("*")']
//! - name: iii-bridge
//!   config:
//!     url: ws://127.0.0.1:49129
//!     forward:
//!       - { local_function: agentos::bus_auth, remote_function: agentos::bus_auth }
//!       - { local_function: agentos::bus_on_register, remote_function: agentos::bus_on_register }
//!       - { local_function: agentos::bus_on_trigger, remote_function: agentos::bus_on_trigger }
//! ```
//!
//! Exits non-zero when `AGENTOS_API_KEY` is unset: a daemon without a key would
//! file every connection under the untrusted tier, which reads as "RBAC is on"
//! while granting nobody the trusted tier. Refusing to start is the honest
//! failure, and it fails the boot closed.

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

    let addr = std::env::args()
        .skip(1)
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

    let listener = TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "bus-auth daemon listening");
    serve(listener, key).await
}
