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
/// is exact ids. Two tests keep it honest: `deny_set_covers_the_tree` fails when
/// a new id appears in a contract I1 family without being listed here, and
/// `no_denied_id_is_fired_by_a_registry_worker_trigger` fails if an entry here is
/// also a cron or queue trigger TARGET — because those are fired by an
/// engine-spawned worker through its own untrusted session, and denying one
/// silently stops a scheduled job.
///
/// # What is deliberately NOT here, and why
///
/// * `state::*` and `engine::*` (contract I1 families) — the engine's own
///   registry workers call them and cannot authenticate. Forbidding them breaks
///   the boot.
/// * The registry-fired trigger targets: `cron::cleanup_stale_sessions`,
///   `cron::aggregate_daily_costs`, `cron::reset_rate_limits`, `workflow::run`,
///   `memory::consolidate`, `memory::evict`, `feedback::auto_review`,
///   `lifecycle::check_all`, `pulse::tick`, `hand::run::<id>` (cron) and
///   `agent::chat` (queue). The factory halves — `cron::create|patch|delete`,
///   `workflow::create` — ARE denied, which is what closes the composition
///   route without stopping the schedule. `memory::consolidate` and
///   `memory::evict` are destructive and still reachable; that is the price of
///   an unauthenticatable cron worker and it is recorded as a residual risk.
///
/// # What earns an entry
///
/// (A) executes code or spawns a process, (B) mutates security or authorization
/// state, (C) reads or destroys stored content across tenants, or (D) is a
/// factory that composes (A)-(C) from a trusted session. `workflow::create` is
/// the (D) case that a whole-tree review found: `workers/workflow` dispatches
/// `step.function_id` verbatim from its OWN trusted session, so an untrusted
/// `workflow::create` + an allowed `workflow::run` reaches every id on this list.
pub const UNTRUSTED_FORBIDDEN_FUNCTIONS: &[&str] = &[
    // vault — the credential store itself. (C)
    "vault::backup",
    "vault::delete",
    "vault::get",
    "vault::init",
    "vault::list",
    "vault::restore",
    "vault::rotate",
    "vault::set",
    // shell / coder — arbitrary execution. (A) Not booted by default; listed so
    // an operator who opts in does not silently open it to every local process.
    "shell::exec",
    "shell::exec_bg",
    "shell::pty::open",
    "shell::fs::write",
    "shell::fs::rm",
    "coder::apply",
    // harness — autonomous agent turns and filesystem grants. (A)(B) Opt-in.
    "harness::spawn",
    "harness::send",
    "harness::function::trigger",
    "harness::filesystem::grant",
    "harness::filesystem::revoke",
    "harness::bindings::store",
    // mcp — spawns external servers with a caller-supplied command. (A)
    "mcp::call_tool",
    "mcp::connect",
    "mcp::disconnect",
    "mcp::list_connections",
    "mcp::list_tools",
    "mcp::serve",
    "mcp::serve_handler",
    "mcp::unserve",
    // hooks — arbitrary function ids fired on lifecycle events. (D)
    "hook::fire",
    "hook::list",
    "hook::register",
    "hook::toggle",
    "hook::unregister",
    "hook::update_priority",
    // trigger / cron / workflow factories — the `POST /api/triggers` route
    // factory one layer down, and the workflow step dispatcher. (D)
    "cron::create",
    "cron::delete",
    "cron::list",
    "cron::patch",
    "trigger::create",
    "trigger::delete",
    "trigger::list",
    "control::rehydrate",
    "workflow::create",
    // bridge — invoke-anything proxies, including the engine's own `iii-bridge`
    // builtin (dotted ids), which is the transport this policy rides on. (D)
    "bridge::cancel",
    "bridge::invoke",
    "bridge::list",
    "bridge::register",
    "bridge::run",
    "bridge.invoke",
    "bridge.invoke_async",
    // code / wasm / browser / orchestrator — execution, host file writes and
    // outbound fetch. (A)
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
    "agent::code_execute",
    "orchestrator::execute",
    "orchestrator::workspace_write",
    "skillkit::install",
    "skillkit::run",
    "skillkit::uninstall",
    "security::docker_exec",
    "hand::trigger",
    "task::spawn_workers",
    // authorization, approval and audit state. (B) `agent::create` is the second
    // writer of the contract I1 capability document; denying only
    // `security::set_capabilities` would leave the door next to it open. An
    // approval a caller can grant itself is not a gate.
    "agent::create",
    "agent::delete",
    "approval::decide",
    "approval::decide_tier",
    "approval::decide_tier_request",
    "approval::set_policy",
    "council::override",
    "policy::set_rules",
    "realm::import",
    "security::audit",
    "security::list_capabilities",
    "security::map_respond",
    "security::set_capabilities",
    "security::sign_manifest",
    "taint::declassify",
    // stored content across every agent — no tenancy on these ids. (C)
    // `memory::consolidate` and `memory::evict` are absent on purpose: both are
    // cron targets fired by the untrusted registry worker.
    "memory::kg::add",
    "memory::kg::query",
    "memory::kv::delete",
    "memory::kv::get",
    "memory::kv::list",
    "memory::kv::set",
    "memory::list",
    "memory::recall",
    "memory::session::compact",
    "memory::session::delete",
    "memory::session::history",
    "memory::session::list",
    "memory::session::repair",
    "memory::store",
    // worker management — `worker::add` + `worker::start` make the engine fetch
    // and run a registry binary, which is process spawn by another name. (A)
    // The read half (list/status/logs/schema/validate) stays reachable: it is
    // how the untrusted tier's own supervisor works.
    "worker::add",
    "worker::clear",
    "worker::remove",
    "worker::start",
    "worker::stop",
    "worker::update",
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
/// really does register `engine::queue::*` and `iii::queue::*`, while
/// `engine::log::info` — an id every worker's logging goes through — must stay
/// unclaimable by a credential-less session.
///
/// # These are FUNCTION-ID namespaces, not worker names
///
/// The first version of this list held worker names (`llm-router`,
/// `session-manager`, `iii-directory`, `context-manager`, `provider-anthropic`).
/// The engine's registration hook receives the FUNCTION id, whose namespace is
/// different — `router::`, `session::`, `directory::`, `context::`,
/// `provider::anthropic::` — so an armed stack refused 107 registrations and
/// came up with 36 functions and no LLM routing at all. `registry_surface.txt`
/// and the test that reads it exist so that can never be a review finding again.
///
/// # What this costs, stated plainly
///
/// Admitting these namespaces means a credential-less local process can also
/// CLAIM ids in them: it can impersonate the LLM router (seeing every prompt and
/// answering with a poisoned completion), the session store, the directory, or
/// the worker-management surface, in the same way it can already impersonate
/// `state::*`. At 0.22.1 there is no way to tell those workers from an attacker
/// — the measured alternative is a stack with no routing, no sessions, no
/// directory and no context assembly.
pub const UNTRUSTED_REGISTRATION_PREFIXES: &[&str] = &[
    "configuration",
    "context",
    "cron",
    "directory",
    "engine::queue",
    "iii::durable",
    "iii::queue",
    "provider",
    "queue",
    "router",
    "session",
    "state",
    "stream",
    "worker",
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

/// Console-UI asset handlers, which the registry workers register under their
/// WORKER name rather than their function namespace: `state::ui-content` but
/// also `llm-router::ui-content`, `context-manager::ui-content`,
/// `iii-directory::ui-content`, `provider-openai-codex::ui-content` — the four
/// that a live armed boot refused after the namespace fix. Exactly two segments,
/// the second `ui-content`, so this cannot admit anything else.
///
/// A claim on one of these serves an asset into the console UI, which is opt-in
/// and off by default in `config.yaml`; the alternative is admitting five more
/// worker-name prefixes wholesale.
fn is_console_ui_asset(function_id: &str) -> bool {
    let mut segments = function_id.split("::");
    let Some(namespace) = segments.next() else {
        return false;
    };
    !namespace.is_empty() && segments.next() == Some("ui-content") && segments.next().is_none()
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
    if is_console_ui_asset(function_id) {
        return true;
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
            // Fired by the same untrusted cron worker (workers/memory,
            // workers/feedback, workers/session-lifecycle, workers/pulse).
            "memory::consolidate",
            "memory::evict",
            "feedback::auto_review",
            "lifecycle::check_all",
            "pulse::tick",
            // Fired by the untrusted queue worker (workers/agent-core).
            "agent::chat",
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
            "queue::define",
            "engine::queue::enqueue",
            "engine::queue::dlq_messages",
            "iii::queue::redrive",
            "iii::durable::publish",
            // The five workers an armed stack refused when this list held
            // worker names instead of id namespaces.
            "router::chat",
            "router::models::list",
            "session::create",
            "directory::skills::list",
            "context::assemble",
            "provider::anthropic::chat",
            "worker::list",
        ] {
            assert!(
                function_registration_allowed(id, &untrusted),
                "{id} is registered by an engine-spawned worker"
            );
        }
    }

    #[test]
    fn console_ui_assets_are_registrable_under_the_worker_name() {
        let untrusted = json!({ TIER_CONTEXT_KEY: TIER_UNTRUSTED });
        for id in [
            "state::ui-content",
            "llm-router::ui-content",
            "context-manager::ui-content",
            "iii-directory::ui-content",
            "provider-openai-codex::ui-content",
        ] {
            assert!(function_registration_allowed(id, &untrusted), "{id}");
        }
        for id in [
            "vault::ui-content::extra",
            "::ui-content",
            "ui-content",
            "vault::ui-contentx",
            "vault::get",
        ] {
            assert!(
                !function_registration_allowed(id, &untrusted),
                "{id} is not a console asset id"
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
    fn every_denied_id_is_a_contract_i1_family_or_a_named_exception() {
        // Families outside contract I1's deny-by-default set need a reason to be
        // here. Each one below maps to a clause of the "what earns an entry"
        // rule in the constant's docs; a new family cannot be added silently.
        const JUSTIFIED_FAMILIES: &[&str] = &[
            "agent",        // (A) code_execute, (B) create/delete write the I1 document
            "approval",     // (B) an approval a caller grants itself is not a gate
            "control",      // (D) rehydrate replays the trigger factory
            "coder",        // (A) the second surface of the shell binary
            "council",      // (B) override rewrites a decision
            "hand",         // (A) trigger runs an automation on demand
            "memory",       // (C) no tenancy on any of these ids
            "orchestrator", // (A) executes a plan and writes host files
            "policy",       // (B) set_rules rewrites the rule set
            "realm",        // (B) import overwrites a realm document
            "security",     // (B) capabilities, audit chain, signing oracle
            "skillkit",     // (A) install/run spawn npx
            "taint",        // (B) declassify removes a label
            "task",         // (A) spawn_workers starts work
            "trigger",      // (D) the mint factory
            "worker",       // (A) add + start fetch and run a registry binary
            "workflow",     // (D) create dispatches step ids from a trusted session
        ];
        const JUSTIFIED_IDS: &[&str] = &[
            // The engine's builtin bridge registers dotted ids, which have no
            // `::` family at all.
            "bridge.invoke",
            "bridge.invoke_async",
            AUTH_FUNCTION_ID,
            FUNCTION_REGISTRATION_HOOK_ID,
            TRIGGER_REGISTRATION_HOOK_ID,
        ];
        for id in UNTRUSTED_FORBIDDEN_FUNCTIONS {
            let family = id.split("::").next().unwrap_or_default();
            assert!(
                agentos_http_adapter::policy::is_deny_by_default(id)
                    || JUSTIFIED_FAMILIES.contains(&family)
                    || JUSTIFIED_IDS.contains(id),
                "{id} is neither a contract I1 family nor a justified exception"
            );
        }
    }

    /// The bypass a whole-tree review found: `workflow::run` is allowed because
    /// the cron worker fires it, and `workers/workflow` dispatches each step id
    /// from its own TRUSTED session. If `workflow::create` ever leaves the deny
    /// list, every id on it becomes reachable through a two-call composition.
    #[test]
    fn the_workflow_factory_is_denied_while_its_runner_stays_allowed() {
        assert!(
            UNTRUSTED_FORBIDDEN_FUNCTIONS.contains(&"workflow::create"),
            "workflow::create composes every other denied id from a trusted session"
        );
        assert!(
            !UNTRUSTED_FORBIDDEN_FUNCTIONS.contains(&"workflow::run"),
            "workflow::run is a cron target; denying it stops scheduled workflows"
        );
        for factory in ["cron::create", "trigger::create", "hook::register"] {
            assert!(
                UNTRUSTED_FORBIDDEN_FUNCTIONS.contains(&factory),
                "{factory} is the same class of factory"
            );
        }
    }
}
