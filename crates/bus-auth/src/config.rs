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

/// `iii-worker-manager`'s own default port (`WorkerManagerConfig::default_port`).
/// Used only when the config does not pin one.
const DEFAULT_ENGINE_BUS_PORT: u16 = 49134;

/// Hosts that mean "this machine" in an `iii-bridge` url.
const LOOPBACK_HOSTS: &[&str] = &["127.0.0.1", "localhost", "::1", "[::1]", "0.0.0.0", "[::]"];

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
    /// The document could not be parsed, so nothing can be said about it.
    ///
    /// Deliberately NOT `NotArmed`: "could not tell" and "the gate is off" are
    /// different facts, and collapsing them is how a check becomes decorative.
    /// It is not fatal, because the engine parses the same file and refuses to
    /// boot on it — the operator will not end up with a silently disarmed gate
    /// on a running stack.
    Unknown(String),
}

/// Where the config path came from. An operator who NAMED a file gets a strict
/// answer; the cwd probe is a convenience and stays lenient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// `--config=<path>`, or `AGENTOS_CONFIG`. Both are a statement of intent:
    /// if the file cannot be read, that is an error, not a reason to skip. A
    /// path typo must never be a quiet way to disable this check — it is the
    /// same defect class the check exists to catch.
    Named(PathBuf),
    /// `./config.yaml`, found because it happens to be there.
    Probed(PathBuf),
}

impl ConfigSource {
    pub fn path(&self) -> &Path {
        match self {
            ConfigSource::Named(path) | ConfigSource::Probed(path) => path,
        }
    }

    /// True when an unreadable file must fail the daemon instead of being skipped.
    pub fn is_named(&self) -> bool {
        matches!(self, ConfigSource::Named(_))
    }
}

/// Where to look for the config, in order: an explicit path, `AGENTOS_CONFIG`,
/// then `./config.yaml` (the daemon runs with the runtime directory as its cwd
/// under `agentos up`).
///
/// The first two are returned WITHOUT checking that they exist, on purpose: the
/// caller has to tell the operator that a named file is missing rather than
/// quietly fall through to the probe.
pub fn discover(explicit: Option<&str>) -> Option<ConfigSource> {
    if let Some(path) = explicit {
        return Some(ConfigSource::Named(PathBuf::from(path)));
    }
    if let Some(path) = std::env::var_os(CONFIG_PATH_ENV) {
        return Some(ConfigSource::Named(PathBuf::from(path)));
    }
    let local = Path::new("config.yaml");
    local
        .is_file()
        .then(|| ConfigSource::Probed(local.to_path_buf()))
}

/// Check one config document against the hooks this daemon serves.
pub fn inspect(yaml: &str) -> GateStatus {
    let document: Value = match serde_yaml::from_str(yaml) {
        Ok(document) => document,
        Err(error) => return GateStatus::Unknown(error.to_string()),
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
            } else if targets_the_engine_bus(url, engine_bus_port(workers)) {
                problems.push(format!(
                    "`iii-bridge.url` is `{url}`, which is the engine's OWN bus (port {}): the \
                     gate would depend on the listener it gates, and the stack deadlocks",
                    engine_bus_port(workers)
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

/// The port `iii-worker-manager` binds, from the config, or the engine's default.
///
/// Read rather than assumed: the previous version of this check substring-matched
/// the literal `49134`, so it went blind on any stack that moved the port — and
/// loose in the other direction, since `ws://10.0.49134.1:3000` contains it too.
fn engine_bus_port(workers: &[Value]) -> u16 {
    worker_config(workers, "iii-worker-manager")
        .and_then(|config| config.get("port"))
        .and_then(|port| port.as_u64().or_else(|| port.as_str()?.parse::<u64>().ok()))
        .and_then(|port| u16::try_from(port).ok())
        .unwrap_or(DEFAULT_ENGINE_BUS_PORT)
}

/// True when `url` names the engine's own bus: same port, on a host that means
/// this machine.
///
/// The host test is what keeps a genuinely remote daemon that happens to listen
/// on the same port number out of the error; the port test is what catches the
/// deadlock on a stack that does not use 49134.
fn targets_the_engine_bus(url: &str, engine_port: u16) -> bool {
    let Some((host, port)) = split_ws_authority(url) else {
        return false;
    };
    port == Some(engine_port) && LOOPBACK_HOSTS.contains(&host)
}

/// `(host, port)` of a `ws://`/`wss://` url, without pulling in a URL parser.
///
/// Handles a bracketed IPv6 literal and an optional path; returns `None` for
/// anything that does not look like a websocket url at all.
fn split_ws_authority(url: &str) -> Option<(&str, Option<u16>)> {
    let rest = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if let Some(end) = authority.find(']') {
        // `[::1]:49134` -> host keeps its brackets so the table can match either
        // spelling; the port is whatever follows.
        let host = &authority[..=end];
        let port = authority[end + 1..]
            .strip_prefix(':')
            .and_then(|port| port.parse().ok());
        return Some((host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host, port.parse().ok())),
        None => Some((authority, None)),
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
                .any(|problem| problem.contains("the engine's OWN bus")),
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

    /// The deadlock check has to follow the CONFIGURED port.
    ///
    /// The first version matched the literal `49134`, so a stack on any other
    /// port lost the check silently and the daemon printed "armed" over the
    /// exact configuration it exists to refuse.
    #[test]
    fn the_self_gating_check_follows_the_configured_port() {
        let moved = shipped_overlay()
            .replace(
                "      host: 127.0.0.1\n",
                "      host: 127.0.0.1\n      port: 39534\n",
            )
            .replace("ws://127.0.0.1:49129", "ws://127.0.0.1:39534");
        assert!(
            problems(&moved).iter().any(
                |problem| problem.contains("the engine's OWN bus") && problem.contains("39534")
            ),
            "a bridge aimed at a non-default bus port is the same deadlock"
        );

        // The same file with the bridge left where it belongs is still fine.
        let moved_ok = shipped_overlay().replace(
            "      host: 127.0.0.1\n",
            "      host: 127.0.0.1\n      port: 39534\n",
        );
        assert_eq!(inspect(&moved_ok), GateStatus::Armed);

        // And 49134 stops being special once the engine is elsewhere.
        let elsewhere = shipped_overlay()
            .replace(
                "      host: 127.0.0.1\n",
                "      host: 127.0.0.1\n      port: 39534\n",
            )
            .replace("ws://127.0.0.1:49129", "ws://127.0.0.1:49134");
        assert_eq!(
            inspect(&elsewhere),
            GateStatus::Armed,
            "49134 is only the DEFAULT bus port, not a forbidden number"
        );
    }

    #[test]
    fn the_engine_bus_port_is_read_from_the_config() {
        let workers = |yaml: &str| -> Vec<Value> {
            serde_yaml::from_str::<Value>(yaml)
                .expect("test yaml")
                .get("workers")
                .and_then(Value::as_sequence)
                .cloned()
                .unwrap_or_default()
        };
        assert_eq!(
            engine_bus_port(&workers(
                "workers:\n  - name: iii-worker-manager\n    config:\n      host: 127.0.0.1\n"
            )),
            49134,
            "an unpinned port is the engine's own default"
        );
        assert_eq!(
            engine_bus_port(&workers(
                "workers:\n  - name: iii-worker-manager\n    config:\n      port: 39534\n"
            )),
            39534
        );
        assert_eq!(
            engine_bus_port(&workers(
                "workers:\n  - name: iii-worker-manager\n    config:\n      port: \"39534\"\n"
            )),
            39534,
            "yaml may quote it"
        );
    }

    /// Substring matching was wrong in both directions; parsed ports are not.
    #[test]
    fn only_a_real_authority_match_counts_as_the_engine_bus() {
        assert!(targets_the_engine_bus("ws://127.0.0.1:49134", 49134));
        assert!(targets_the_engine_bus("ws://localhost:49134/", 49134));
        assert!(targets_the_engine_bus("ws://[::1]:49134", 49134));
        assert!(targets_the_engine_bus("ws://0.0.0.0:39534/path", 39534));

        assert!(
            !targets_the_engine_bus("ws://10.0.49134.1:3000", 49134),
            "a host that merely contains the digits is not the bus"
        );
        assert!(
            !targets_the_engine_bus("ws://x49134.internal:3000", 49134),
            "nor is a host named after it"
        );
        assert!(
            !targets_the_engine_bus("ws://127.0.0.1:49129", 49134),
            "the daemon's own port is the point of the entry"
        );
        assert!(
            !targets_the_engine_bus("ws://gateway.example:49134", 49134),
            "a remote host on the same port number is not this engine's socket"
        );
        assert!(
            !targets_the_engine_bus("http://127.0.0.1:49134", 49134),
            "not a websocket url at all"
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
    fn an_unrelated_document_is_not_reported_as_armed() {
        assert_eq!(inspect("workers: []"), GateStatus::NotArmed);
        assert_eq!(
            inspect("workers:\n  - name: iii-worker-manager\n    config:\n      host: 127.0.0.1\n"),
            GateStatus::NotArmed
        );
    }

    /// "Could not tell" must not read as "the gate is off".
    #[test]
    fn an_unparseable_document_is_unknown_rather_than_unarmed() {
        assert!(
            matches!(inspect(": not : yaml ["), GateStatus::Unknown(_)),
            "a parse failure says nothing about the gate"
        );
    }

    /// A named path is a statement of intent: the caller must be able to tell it
    /// apart from the cwd probe, because a typo in it would otherwise be a quiet
    /// way to switch this whole check off.
    #[test]
    fn a_named_path_is_distinguishable_from_the_probe() {
        let named =
            discover(Some("/tmp/explicit.yaml")).expect("an explicit path is returned as is");
        assert_eq!(named.path(), Path::new("/tmp/explicit.yaml"));
        assert!(named.is_named());

        assert!(
            discover(Some("/tmp/agentos-does-not-exist-9d1f.yaml")).is_some(),
            "a missing named path must still be returned, so the caller can refuse it"
        );

        let probe = ConfigSource::Probed(PathBuf::from("config.yaml"));
        assert!(!probe.is_named());
    }
}
