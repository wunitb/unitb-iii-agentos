//! Reading the armed `rbac:` block back, and refusing to run against one that
//! would not do what it says.
//!
//! # Why this module exists
//!
//! The engine's OUTER `WorkerManagerConfig` is `#[serde(deny_unknown_fields)]`,
//! so `hosts:` instead of `host:` fails the boot with a clear error. The NESTED
//! `RbacConfig` is NOT — verified in the vendored 0.22.1 source
//! (`engine/src/workers/worker/rbac_config.rs` carries no `deny_unknown_fields`)
//! and live on both 0.22.1 and 0.23.0, where `rbac: { auth_function_id: a::b,
//! bogus_rbac_key: 1 }` boots without a word.
//!
//! So a typo inside `rbac:` — `auth_function_idd`, or an id the daemon does not
//! serve — does not fail anything. It SILENTLY DISARMS THE GATE: the engine
//! admits every bus connection, `agentos doctor` still sees an `rbac:` block,
//! and nothing in the stack says otherwise. Prose in a YAML comment cannot fix
//! that. This can: the daemon reads the config it is about to gate and refuses
//! to start when the two disagree.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_yaml::Value;

use crate::policy::{ARMED_HOOKS, RBAC_CONFIG_KEYS};

/// Environment variable `agentos up` uses to point at the active config.
pub const CONFIG_PATH_ENV: &str = "AGENTOS_CONFIG";

/// The engine's own bus port. An `iii-bridge` that forwarded here would depend
/// on the listener it is gating.
const ENGINE_BUS_PORT: &str = "49134";

/// What the daemon found in the config it was pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateStatus {
    /// No `rbac:` block on `iii-worker-manager`: the gate is off, which is the
    /// shipped default and not an error.
    NotArmed,
    /// Armed and consistent with what this daemon serves.
    Armed,
    /// Armed, but the config would not do what it claims. Each entry is one
    /// operator-actionable sentence.
    Inconsistent(Vec<String>),
}

/// Where to look for the config, in order: an explicit path, `AGENTOS_CONFIG`,
/// then `./config.yaml` (the daemon runs with the runtime directory as its cwd
/// under `agentos up`).
pub fn discover(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os(CONFIG_PATH_ENV) {
        return Some(PathBuf::from(path));
    }
    let local = Path::new("config.yaml");
    local.is_file().then(|| local.to_path_buf())
}

/// Check one config document against the hooks this daemon serves.
pub fn inspect(yaml: &str) -> GateStatus {
    let document: Value = match serde_yaml::from_str(yaml) {
        Ok(document) => document,
        // A config this daemon cannot parse is the engine's problem to report:
        // the engine reads the same file and fails loudly on it.
        Err(_) => return GateStatus::NotArmed,
    };
    let workers = document
        .get("workers")
        .and_then(Value::as_sequence)
        .map(Vec::as_slice)
        .unwrap_or_default();

    let Some(rbac) =
        worker_config(workers, "iii-worker-manager").and_then(|config| config.get("rbac").cloned())
    else {
        return GateStatus::NotArmed;
    };

    let mut problems = Vec::new();
    let Some(rbac) = rbac.as_mapping() else {
        problems.push("`rbac:` is present but is not a mapping".to_string());
        return GateStatus::Inconsistent(problems);
    };

    // 1. Keys the engine does not know. It ignores them without a word, so this
    //    is the only place a typo can surface.
    for key in rbac.keys() {
        let Some(key) = key.as_str() else { continue };
        if !RBAC_CONFIG_KEYS.contains(&key) {
            problems.push(format!(
                "`rbac.{key}` is not a key the engine knows ({}). The nested rbac struct is not \
                 deny_unknown_fields, so the engine ignores it silently and the gate it was meant \
                 to arm stays off",
                RBAC_CONFIG_KEYS.join(", ")
            ));
        }
    }

    // 2. Every hook this daemon serves must be armed, and must name the id the
    //    daemon actually answers.
    for (key, expected) in ARMED_HOOKS {
        match rbac.get(Value::from(*key)).and_then(Value::as_str) {
            Some(id) if id == *expected => {}
            Some(id) => problems.push(format!(
                "`rbac.{key}` names `{id}`, but this daemon serves `{expected}`; the engine would \
                 call a function nothing answers and refuse every bus connection"
            )),
            None => problems.push(format!(
                "`rbac.{key}` is missing; `{expected}` would never be called and that surface stays \
                 ungated"
            )),
        }
    }

    // 3. `expose_functions` is what keeps ordinary calls working once `rbac:`
    //    exists: with the key absent the engine denies every non-infrastructure
    //    id to every session, armed or not.
    if !rbac.contains_key(Value::from("expose_functions")) {
        problems.push(
            "`rbac.expose_functions` is missing; with an `rbac:` block present the engine denies \
             every function that is not in a session's exact allow list"
                .to_string(),
        );
    }

    // 4. The bridge is how the engine reaches this daemon at all.
    match worker_config(workers, "iii-bridge") {
        None => problems.push(
            "no `iii-bridge` worker entry: the hooks are named but nothing forwards them to this \
             daemon, so every bus connection is refused"
                .to_string(),
        ),
        Some(bridge) => {
            let url = bridge
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if url.is_empty() {
                problems.push("`iii-bridge.url` is missing".to_string());
            } else if url.contains(ENGINE_BUS_PORT) {
                problems.push(format!(
                    "`iii-bridge.url` is `{url}`, the engine's own bus: the gate would depend on \
                     the listener it gates"
                ));
            }
            let forwarded: BTreeSet<(&str, &str)> = bridge
                .get("forward")
                .and_then(Value::as_sequence)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|entry| {
                    Some((
                        entry.get("local_function")?.as_str()?,
                        entry.get("remote_function")?.as_str()?,
                    ))
                })
                .collect();
            for (key, expected) in ARMED_HOOKS {
                if !forwarded.contains(&(expected, expected)) {
                    problems.push(format!(
                        "`iii-bridge.forward` has no `{expected}` -> `{expected}` entry, so the \
                         `rbac.{key}` hook resolves to nothing"
                    ));
                }
            }
        }
    }

    if problems.is_empty() {
        GateStatus::Armed
    } else {
        GateStatus::Inconsistent(problems)
    }
}

/// The `config:` mapping of the named worker entry.
fn worker_config<'a>(workers: &'a [Value], name: &str) -> Option<&'a Value> {
    workers
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|entry| entry.get("config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The overlay this repository ships, byte for byte.
    fn shipped_overlay() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("crates/bus-auth is two levels below the repository root")
            .join("bus-rbac.overlay.yaml");
        std::fs::read_to_string(path).expect("read bus-rbac.overlay.yaml")
    }

    #[test]
    fn the_shipped_overlay_arms_every_hook_this_daemon_serves() {
        assert_eq!(inspect(&shipped_overlay()), GateStatus::Armed);
    }

    #[test]
    fn the_shipped_config_is_deliberately_unarmed() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("repository root")
            .join("config.yaml");
        let config = std::fs::read_to_string(path).expect("read config.yaml");
        assert_eq!(
            inspect(&config),
            GateStatus::NotArmed,
            "config.yaml must stay unarmed: armed with no daemon listening, nothing boots"
        );
    }

    fn problems(yaml: &str) -> Vec<String> {
        match inspect(yaml) {
            GateStatus::Inconsistent(problems) => problems,
            other => panic!("expected an inconsistent gate, got {other:?}"),
        }
    }

    /// The defect this module exists for: the engine accepts this file happily.
    #[test]
    fn a_typo_inside_rbac_is_reported_because_the_engine_never_will() {
        let typo = shipped_overlay().replace("auth_function_id:", "auth_function_idd:");
        let reported = problems(&typo);
        assert!(
            reported
                .iter()
                .any(|problem| problem.contains("rbac.auth_function_idd")),
            "{reported:?}"
        );
        assert!(
            reported
                .iter()
                .any(|problem| problem.contains("`rbac.auth_function_id` is missing")),
            "{reported:?}"
        );
    }

    #[test]
    fn an_id_the_daemon_does_not_serve_is_reported() {
        let wrong = shipped_overlay().replace("agentos::bus_on_trigger_type", "agentos::typo");
        let reported = problems(&wrong);
        assert!(
            reported.iter().any(|problem| {
                problem.contains("names `agentos::typo`") && problem.contains("bus_on_trigger_type")
            }),
            "{reported:?}"
        );
    }

    #[test]
    fn an_unarmed_hook_is_reported_rather_than_left_silent() {
        let dropped = shipped_overlay().replace(
            "        on_trigger_type_registration_function_id: agentos::bus_on_trigger_type\n",
            "",
        );
        let reported = problems(&dropped);
        assert!(
            reported.iter().any(|problem| problem
                .contains("`rbac.on_trigger_type_registration_function_id` is missing")),
            "{reported:?}"
        );
    }

    #[test]
    fn a_bridge_that_cannot_reach_the_daemon_is_reported() {
        let no_forward = shipped_overlay().replace(
            "        - local_function: agentos::bus_on_trigger_type\n          remote_function: agentos::bus_on_trigger_type\n          timeout_ms: 5000\n",
            "",
        );
        assert!(
            problems(&no_forward)
                .iter()
                .any(|problem| problem.contains("has no `agentos::bus_on_trigger_type`")),
            "the forward list and the rbac block have to agree"
        );

        let self_gating = shipped_overlay().replace("ws://127.0.0.1:49129", "ws://127.0.0.1:49134");
        assert!(
            problems(&self_gating)
                .iter()
                .any(|problem| problem.contains("the engine's own bus")),
            "a bridge pointed at the gated listener is a deadlock"
        );

        let no_bridge =
            shipped_overlay().replace("  - name: iii-bridge", "  - name: iii-unrelated");
        assert!(
            problems(&no_bridge)
                .iter()
                .any(|problem| problem.contains("no `iii-bridge` worker entry")),
            "the hooks are useless without the bridge"
        );
    }

    #[test]
    fn a_missing_expose_functions_is_reported() {
        let narrowed =
            shipped_overlay().replace("        expose_functions:\n          - match(\"*\")\n", "");
        assert!(
            problems(&narrowed)
                .iter()
                .any(|problem| problem.contains("expose_functions")),
            "without it the engine denies every ordinary call"
        );
    }

    #[test]
    fn an_unreadable_or_unrelated_document_is_not_reported_as_armed() {
        assert_eq!(inspect(": not : yaml ["), GateStatus::NotArmed);
        assert_eq!(inspect("workers: []"), GateStatus::NotArmed);
        assert_eq!(
            inspect("workers:\n  - name: iii-worker-manager\n    config:\n      host: 127.0.0.1\n"),
            GateStatus::NotArmed
        );
    }

    #[test]
    fn discovery_prefers_the_explicit_path_then_the_environment() {
        assert_eq!(
            discover(Some("/tmp/explicit.yaml")),
            Some(PathBuf::from("/tmp/explicit.yaml"))
        );
    }
}
