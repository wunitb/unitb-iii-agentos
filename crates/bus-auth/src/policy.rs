//! Bus RBAC policy for the engine's `iii-worker-manager.rbac` hooks.
//!
//! # Why this exists
//!
//! Every AgentOS function lands on one WebSocket bus (`ws://127.0.0.1:49134`).
//! Binding it to loopback removed the remote attacker; it did not remove the
//! local one. iii 0.22.1 ships the mechanism that does: `rbac.auth_function_id`
//! is called once per connection with `{headers, query_params, ip_address}` and,
//! unlike the stream's `auth_function`, it FAILS CLOSED — an error, a missing
//! function or an undeserialisable answer makes the engine send an error frame
//! and close the socket.
//!
//! # The constraint that shapes this policy (measured, 2026-09-02, iii 0.22.1)
//!
//! The engine spawns its own registry workers (`state`, `queue`, `cron`,
//! `llm-router`, `context-manager`, `session-manager`, `iii-directory`,
//! `provider-*`) as child processes and hands them a hardcoded
//! `III_URL=ws://127.0.0.1:<port>` (engine `registry_worker.rs::spawn_url_env`).
//! Those binaries expose no token, header or URL-query knob, and their handshake
//! carries only `{connection, host, sec-websocket-key, sec-websocket-version,
//! upgrade}` — captured live from the shipped `state` worker. A local attacker
//! presents exactly the same thing from the same address, so **a credential-less
//! connection cannot be told apart from a registry worker**.
//!
//! A strict "no credential ⇒ refuse" policy therefore stops the product from
//! booting (proven: `state::*` never registers). The policy below is TIERED
//! instead:
//!
//! * **trusted** — presented `Authorization: Bearer $AGENTOS_API_KEY`. Full
//!   access; this is every in-tree AgentOS worker.
//! * **untrusted** — no credential. Admitted, because the registry workers must
//!   be, but (a) it may not invoke any id in [`UNTRUSTED_FORBIDDEN_FUNCTIONS`],
//!   and (b) it may only register functions/triggers inside
//!   [`UNTRUSTED_REGISTRATION_NAMESPACES`].
//!
//! What that buys: a credential-less local process can no longer read the vault,
//! mint a cron/HTTP trigger, register an MCP server, or re-register (hijack) an
//! id such as `vault::get` that a real worker owns. What it does NOT buy: that
//! process may still do anything a registry worker may do — call `state::*`,
//! `queue::*`, `configuration::*` and register ids inside those namespaces. Say
//! so out loud; do not call the bus "authenticated".

use serde_json::{Map, Value, json};
use subtle::ConstantTimeEq;

/// Environment variable carrying the shared bus credential. Generated at first
/// run into a 0600 `.env` by the CLI and exported to every worker by
/// `agentos up`; never written into `config.yaml`.
pub const API_KEY_ENV: &str = "AGENTOS_API_KEY";

/// Engine-side id of the per-connection auth function
/// (`rbac.auth_function_id`).
pub const AUTH_FUNCTION_ID: &str = "agentos::bus_auth";

/// Engine-side id of the function-registration hook
/// (`rbac.on_function_registration_function_id`).
pub const FUNCTION_REGISTRATION_HOOK_ID: &str = "agentos::bus_on_register";

/// Engine-side id of the trigger-registration hook
/// (`rbac.on_trigger_registration_function_id`).
pub const TRIGGER_REGISTRATION_HOOK_ID: &str = "agentos::bus_on_trigger";

/// Key the auth function writes into the session `context`, and that the engine
/// echoes back to both registration hooks.
pub const TIER_CONTEXT_KEY: &str = "agentosBusTier";

/// `context.agentosBusTier` value for a session that presented the credential.
pub const TIER_TRUSTED: &str = "trusted";
/// `context.agentosBusTier` value for a session that did not.
pub const TIER_UNTRUSTED: &str = "untrusted";

/// Exact function ids a credential-less session may never invoke.
///
/// The engine compares `forbidden_functions` with `==` (no globs), so this list
/// is exact ids, and `deny_set_covers_the_tree` in the tests fails when a new id
/// appears in one of these families without being listed here.
///
/// Two deny-by-default families from contract I1 are deliberately ABSENT:
///
/// * `state::*` and `engine::*` — the engine's own registry workers call them
///   and cannot authenticate (see the module docs). Forbidding them would break
///   the boot, so they stay reachable and that is stated as a residual risk.
/// * `cron::cleanup_stale_sessions`, `cron::aggregate_daily_costs`,
///   `cron::reset_rate_limits` and `workflow::run` — these are the four cron job
///   TARGETS that sec-perimeter's mint allowlist permits. The registry `cron`
///   worker fires them through its own (untrusted) session, so denying them
///   would stop every scheduled job. `cron::create|patch|delete`, the factory
///   half, IS denied.
pub const UNTRUSTED_FORBIDDEN_FUNCTIONS: &[&str] = &[
    // vault — the credential store itself.
    "vault::backup",
    "vault::delete",
    "vault::get",
    "vault::init",
    "vault::list",
    "vault::restore",
    "vault::rotate",
    "vault::set",
    // shell / coder — arbitrary execution. Not booted by default; listed so an
    // operator who opts in does not silently open it to every local process.
    "shell::exec",
    "shell::exec_bg",
    "shell::pty::open",
    "shell::fs::write",
    "shell::fs::rm",
    "coder::apply",
    // harness — autonomous agent turns and filesystem grants. Also opt-in.
    "harness::spawn",
    "harness::send",
    "harness::function::trigger",
    "harness::filesystem::grant",
    "harness::filesystem::revoke",
    "harness::bindings::store",
    // mcp — spawns external servers with a caller-supplied command.
    "mcp::call_tool",
    "mcp::connect",
    "mcp::disconnect",
    "mcp::list_connections",
    "mcp::list_tools",
    "mcp::serve",
    "mcp::serve_handler",
    "mcp::unserve",
    // hooks — arbitrary function ids fired on lifecycle events.
    "hook::fire",
    "hook::list",
    "hook::register",
    "hook::toggle",
    "hook::unregister",
    "hook::update_priority",
    // trigger / cron factory — the `POST /api/triggers` route factory, one layer
    // down. The four cron job targets stay callable (see above).
    "cron::create",
    "cron::delete",
    "cron::list",
    "cron::patch",
    "trigger::create",
    "trigger::delete",
    "trigger::list",
    "control::rehydrate",
    // bridge — invoke-anything proxies, including the engine's own `iii-bridge`
    // builtin (dotted ids), which is the transport this policy rides on.
    "bridge::cancel",
    "bridge::invoke",
    "bridge::list",
    "bridge::register",
    "bridge::run",
    "bridge.invoke",
    "bridge.invoke_async",
    // code / wasm / browser — execution and outbound fetch surfaces.
    "code::write",
    "wasm::execute",
    "wasm::list_modules",
    "wasm::run",
    "wasm::validate",
    "browser::click",
    "browser::close",
    "browser::create_session",
    "browser::list_sessions",
    "browser::navigate",
    "browser::read_page",
    "browser::screenshot",
    "browser::type",
    // authorization store and signing oracle.
    "security::set_capabilities",
    "security::list_capabilities",
    "security::map_respond",
    "security::sign_manifest",
    // this policy's own surface: answering it is a credential oracle.
    AUTH_FUNCTION_ID,
    FUNCTION_REGISTRATION_HOOK_ID,
    TRIGGER_REGISTRATION_HOOK_ID,
];

/// Id prefixes a credential-less session may register a function under, or point
/// a trigger at: exactly the surfaces the engine's own registry workers own.
///
/// An entry matches its own namespace and everything below it, on a `::`
/// boundary — `engine::queue` allows `engine::queue::enqueue` and nothing else
/// under `engine::`. That precision is the point: the shipped `queue` worker
/// really does register `engine::queue::*` and `iii::queue::*` (observed live on
/// a booting stack), while `engine::log::info` — an id every worker's logging
/// goes through — must stay unclaimable by a credential-less session.
///
/// Anything outside this list — `vault::get`, `agent::chat`, `memory::store` —
/// is refused, so a local process cannot wait for a real worker to drop and
/// claim its ids.
pub const UNTRUSTED_REGISTRATION_PREFIXES: &[&str] = &[
    "configuration",
    "context-manager",
    "cron",
    "engine::queue",
    "iii-directory",
    "iii::durable",
    "iii::queue",
    "llm-router",
    "provider-anthropic",
    "provider-openai",
    "provider-openai-codex",
    "queue",
    "session-manager",
    "state",
    "stream",
];

/// The bus credential from the environment, rejecting an empty value.
///
/// There is no default and no fallback: a daemon without a key must refuse to
/// start rather than admit everyone as trusted.
pub fn expected_api_key() -> Option<String> {
    std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
}

/// Case-insensitive lookup of the `authorization` header in the engine's
/// `headers` map (values arrive as strings; a list is tolerated and its first
/// entry used).
fn authorization_header(headers: &Value) -> Option<&str> {
    let map: &Map<String, Value> = headers.as_object()?;
    map.iter().find_map(|(name, value)| {
        if !name.eq_ignore_ascii_case("authorization") {
            return None;
        }
        value
            .as_str()
            .or_else(|| value.as_array()?.first()?.as_str())
    })
}

/// Constant-time bearer check.
///
/// Requires the `Bearer` scheme (case-insensitive), rejects an empty token, and
/// rejects everything when `expected` is empty — a missing credential must never
/// authenticate a caller.
pub fn bearer_is_valid(headers: &Value, expected: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    let Some(header) = authorization_header(headers) else {
        return false;
    };
    let Some((scheme, token)) = header.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }
    let token = token.trim();
    if token.is_empty() || token.len() != expected.len() {
        return false;
    }
    token.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// Decide the tier for one connection from the engine's auth input.
pub fn tier_for(auth_input: &Value, expected: Option<&str>) -> &'static str {
    let headers = auth_input.get("headers").cloned().unwrap_or(Value::Null);
    match expected {
        Some(key) if bearer_is_valid(&headers, key) => TIER_TRUSTED,
        _ => TIER_UNTRUSTED,
    }
}

/// Build the engine's `AuthResult` for one connection.
///
/// Field-for-field the shape `rbac_session::AuthResult` deserialises. Note
/// `allow_trigger_type_registration` is `true` for both tiers: the registry
/// workers register the `state`, `cron`, `queue` and `stream` trigger TYPES, and
/// the engine's serde default for that field is `false`, so omitting it would
/// break the boot.
pub fn auth_result(auth_input: &Value, expected: Option<&str>) -> Value {
    let tier = tier_for(auth_input, expected);
    let forbidden: Vec<&str> = if tier == TIER_TRUSTED {
        Vec::new()
    } else {
        UNTRUSTED_FORBIDDEN_FUNCTIONS.to_vec()
    };
    json!({
        "allowed_functions": [],
        "forbidden_functions": forbidden,
        "allow_trigger_type_registration": true,
        "allow_function_registration": true,
        "context": { TIER_CONTEXT_KEY: tier },
    })
}

/// The tier the engine echoes back on a registration hook input.
///
/// Anything that is not the exact trusted marker is untrusted: a hook input
/// without a context, or with a context this policy did not write, is treated as
/// a credential-less session.
pub fn tier_of_context(context: &Value) -> &'static str {
    match context.get(TIER_CONTEXT_KEY).and_then(Value::as_str) {
        Some(TIER_TRUSTED) => TIER_TRUSTED,
        _ => TIER_UNTRUSTED,
    }
}

/// True when `function_id` sits under a prefix a credential-less session owns.
///
/// Matching is on whole `::` segments. An un-namespaced id (`publish`,
/// `bridge.invoke`) is never claimable, and a prefix never matches a longer
/// segment that merely starts with it (`engine::queuex`).
fn prefix_is_untrusted_owned(function_id: &str) -> bool {
    if !function_id.contains("::") {
        return false;
    }
    UNTRUSTED_REGISTRATION_PREFIXES.iter().any(|prefix| {
        function_id
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with("::") && rest.len() > 2)
    })
}

/// May this session register (or re-register) `function_id`?
///
/// Trusted sessions may register anything. A credential-less session is confined
/// to the registry namespaces, which is what closes the last-writer-wins hijack:
/// the engine's `RegisterFunction` handler overwrites the previous owner, so
/// without this hook any local process could claim `vault::get` the moment the
/// real worker reconnects.
pub fn function_registration_allowed(function_id: &str, context: &Value) -> bool {
    if function_id.is_empty() {
        return false;
    }
    if tier_of_context(context) == TIER_TRUSTED {
        return true;
    }
    prefix_is_untrusted_owned(function_id)
}

/// May this session bind a trigger to `function_id`?
///
/// Same rule as function registration, applied to the trigger's TARGET: a
/// credential-less session cannot mint a trigger that fires `vault::get`.
pub fn trigger_registration_allowed(function_id: &str, context: &Value) -> bool {
    function_registration_allowed(function_id, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: Value) -> Value {
        json!({ "headers": value, "query_params": {}, "ip_address": "127.0.0.1" })
    }

    #[test]
    fn a_valid_bearer_is_trusted() {
        let input = headers(json!({ "authorization": "Bearer secret" }));
        assert_eq!(tier_for(&input, Some("secret")), TIER_TRUSTED);
        let input = headers(json!({ "Authorization": "bearer secret" }));
        assert_eq!(
            tier_for(&input, Some("secret")),
            TIER_TRUSTED,
            "header name and scheme are case-insensitive"
        );
    }

    #[test]
    fn everything_else_is_untrusted() {
        for value in [
            json!({}),
            json!({ "authorization": "" }),
            json!({ "authorization": "secret" }),
            json!({ "authorization": "Bearer " }),
            json!({ "authorization": "Bearer wrong" }),
            json!({ "authorization": "Basic secret" }),
            json!({ "authorization": "Bearer secretx" }),
            json!({ "x-api-key": "secret" }),
        ] {
            let input = headers(value.clone());
            assert_eq!(
                tier_for(&input, Some("secret")),
                TIER_UNTRUSTED,
                "{value} must not authenticate"
            );
        }
    }

    #[test]
    fn no_configured_key_means_nobody_is_trusted() {
        let input = headers(json!({ "authorization": "Bearer secret" }));
        assert_eq!(tier_for(&input, None), TIER_UNTRUSTED);
        assert_eq!(tier_for(&input, Some("")), TIER_UNTRUSTED);
    }

    #[test]
    fn auth_result_matches_the_engine_shape() {
        let trusted = auth_result(
            &headers(json!({ "authorization": "Bearer secret" })),
            Some("secret"),
        );
        assert_eq!(trusted["forbidden_functions"], json!([]));
        assert_eq!(trusted["allow_function_registration"], json!(true));
        assert_eq!(
            trusted["allow_trigger_type_registration"],
            json!(true),
            "the engine defaults this to false; the registry workers need it"
        );
        assert_eq!(trusted["context"][TIER_CONTEXT_KEY], json!(TIER_TRUSTED));

        let untrusted = auth_result(&headers(json!({})), Some("secret"));
        assert_eq!(
            untrusted["context"][TIER_CONTEXT_KEY],
            json!(TIER_UNTRUSTED)
        );
        let forbidden = untrusted["forbidden_functions"].as_array().unwrap();
        assert_eq!(forbidden.len(), UNTRUSTED_FORBIDDEN_FUNCTIONS.len());
        for id in [
            "vault::get",
            "cron::create",
            "mcp::connect",
            "hook::register",
        ] {
            assert!(forbidden.contains(&json!(id)), "{id} must be forbidden");
        }
    }

    #[test]
    fn the_registry_worker_surface_stays_callable_without_a_credential() {
        // Everything the engine's own workers need, and the four cron job
        // targets they fire, must NOT be on the deny list.
        for id in [
            "state::set",
            "state::get",
            "queue::send",
            "configuration::get",
            "llm-router::route",
            "engine::log::info",
            "cron::cleanup_stale_sessions",
            "cron::aggregate_daily_costs",
            "cron::reset_rate_limits",
            "workflow::run",
        ] {
            assert!(
                !UNTRUSTED_FORBIDDEN_FUNCTIONS.contains(&id),
                "{id} is fired by an engine-spawned worker that cannot authenticate"
            );
        }
    }

    #[test]
    fn untrusted_sessions_cannot_claim_a_worker_id() {
        let untrusted = json!({ TIER_CONTEXT_KEY: TIER_UNTRUSTED });
        for id in [
            "vault::get",
            "agent::chat",
            "memory::store",
            "security::check_capability",
            "bridge.invoke",
            "",
            "novanamespace",
        ] {
            assert!(
                !function_registration_allowed(id, &untrusted),
                "{id} must not be claimable by a credential-less session"
            );
            assert!(!trigger_registration_allowed(id, &untrusted), "{id}");
        }
    }

    #[test]
    fn untrusted_sessions_keep_their_own_namespaces() {
        let untrusted = json!({ TIER_CONTEXT_KEY: TIER_UNTRUSTED });
        // Every one of these was observed being registered by an engine-spawned
        // registry worker on a live 0.22.1 stack.
        for id in [
            "state::set",
            "state::ui-content",
            "queue::send",
            "llm-router::route",
            "provider-openai::chat",
            "engine::queue::enqueue",
            "engine::queue::dlq_messages",
            "iii::queue::redrive",
            "iii::durable::publish",
        ] {
            assert!(
                function_registration_allowed(id, &untrusted),
                "{id} is registered by an engine-spawned worker"
            );
        }
    }

    /// The `engine::queue` carve-out must not become "the engine namespace".
    #[test]
    fn the_queue_carve_out_does_not_open_the_engine_namespace() {
        let untrusted = json!({ TIER_CONTEXT_KEY: TIER_UNTRUSTED });
        for id in [
            "engine::log::info",
            "engine::workers::register",
            "engine::channels::create",
            "engine::queuex::enqueue",
            "engine::queue",
            "iii::durable",
            "statex::set",
        ] {
            assert!(
                !function_registration_allowed(id, &untrusted),
                "{id} must not be claimable by a credential-less session"
            );
        }
    }

    #[test]
    fn a_missing_or_forged_context_is_untrusted() {
        for context in [
            json!({}),
            json!(null),
            json!({ TIER_CONTEXT_KEY: "TRUSTED" }),
            json!({ "trusted": true }),
        ] {
            assert_eq!(tier_of_context(&context), TIER_UNTRUSTED, "{context}");
            assert!(!function_registration_allowed("vault::get", &context));
        }
    }

    #[test]
    fn trusted_sessions_register_anything() {
        let trusted = json!({ TIER_CONTEXT_KEY: TIER_TRUSTED });
        for id in ["vault::get", "agent::chat", "state::set"] {
            assert!(function_registration_allowed(id, &trusted), "{id}");
        }
        assert!(
            !function_registration_allowed("", &trusted),
            "an empty id is never registrable"
        );
    }

    #[test]
    fn every_denied_agentos_id_is_deny_by_default_or_justified() {
        // Ids outside contract I1's deny-by-default families need a reason to be
        // here; these are the four that have one, spelled out in the constant.
        const JUSTIFIED: &[&str] = &[
            "bridge.invoke",
            "bridge.invoke_async",
            "coder::apply",
            "trigger::create",
            "trigger::delete",
            "trigger::list",
            "control::rehydrate",
            "security::set_capabilities",
            "security::list_capabilities",
            "security::map_respond",
            "security::sign_manifest",
            AUTH_FUNCTION_ID,
            FUNCTION_REGISTRATION_HOOK_ID,
            TRIGGER_REGISTRATION_HOOK_ID,
        ];
        for id in UNTRUSTED_FORBIDDEN_FUNCTIONS {
            assert!(
                agentos_http_adapter::policy::is_deny_by_default(id) || JUSTIFIED.contains(id),
                "{id} is neither a contract I1 family nor a justified exception"
            );
        }
    }
}
