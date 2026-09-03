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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_yaml::Value;

use crate::policy::{ARMED_HOOKS, RBAC_CONFIG_KEYS};

/// Environment variable `agentos up` uses to point at the active config.
pub const CONFIG_PATH_ENV: &str = "AGENTOS_CONFIG";

/// `iii-worker-manager`'s own default port (`WorkerManagerConfig::default_port`).
/// Used only when the config does not pin one.
const DEFAULT_ENGINE_BUS_PORT: u16 = 49134;

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

    let managers = worker_entries(workers, WORKER_MANAGER);
    let armed: Vec<(&str, &serde_yaml::Mapping, u16)> = managers
        .iter()
        .filter_map(|(name, config)| {
            let rbac = config.get("rbac")?.as_mapping()?;
            Some((name.as_str(), rbac, entry_port(config)))
        })
        .collect();

    let malformed: Vec<&str> = managers
        .iter()
        .filter(|(_, config)| config.get("rbac").is_some_and(|rbac| !rbac.is_mapping()))
        .map(|(name, _)| name.as_str())
        .collect();

    if armed.is_empty() && malformed.is_empty() {
        return GateStatus::NotArmed;
    }

    let mut problems = Vec::new();
    // Name every problem when more than one manager is declared, so "which
    // one" is never a guess.
    let label = |name: &str| -> String {
        if managers.len() > 1 {
            format!("`{name}` ")
        } else {
            String::new()
        }
    };
    for name in malformed {
        problems.push(format!(
            "{}`rbac:` is present but is not a mapping",
            label(name)
        ));
    }

    for (name, rbac, _) in &armed {
        let at = label(name);

        // 1. Keys the engine does not know. It ignores them without a word, so
        //    this is the only place a typo can surface.
        for key in rbac.keys() {
            let Some(key) = key.as_str() else { continue };
            if !RBAC_CONFIG_KEYS.contains(&key) {
                problems.push(format!(
                    "{at}`rbac.{key}` is not a key the engine knows ({}). The nested rbac struct is \
                     not deny_unknown_fields, so the engine ignores it silently and the gate it was \
                     meant to arm stays off",
                    RBAC_CONFIG_KEYS.join(", ")
                ));
            }
        }

        // 2. Every hook this daemon serves must be armed, and must name the id
        //    the daemon actually answers.
        for (key, expected) in ARMED_HOOKS {
            match rbac.get(Value::from(*key)).and_then(Value::as_str) {
                Some(id) if id == *expected => {}
                Some(id) => problems.push(format!(
                    "{at}`rbac.{key}` names `{id}`, but this daemon serves `{expected}`; the engine \
                     would call a function nothing answers and refuse every bus connection"
                )),
                None => problems.push(format!(
                    "{at}`rbac.{key}` is missing; `{expected}` would never be called and that \
                     surface stays ungated"
                )),
            }
        }

        // 3. `expose_functions` is what keeps ordinary calls working once
        //    `rbac:` exists: with the key absent the engine denies every
        //    non-infrastructure id to every session, armed or not.
        if !rbac.contains_key(Value::from("expose_functions")) {
            problems.push(format!(
                "{at}`rbac.expose_functions` is missing; with an `rbac:` block present the engine \
                 denies every function that is not in a session's exact allow list"
            ));
        }
    }

    // 4. The bridge is how the engine reaches this daemon at all.
    let bridges = worker_entries(workers, BRIDGE);
    if bridges.is_empty() {
        problems.push(
            "no `iii-bridge` worker entry: the hooks are named but nothing forwards them to this \
             daemon, so every bus connection is refused"
                .to_string(),
        );
    }

    let mut forwarded: BTreeSet<(&str, &str)> = BTreeSet::new();
    for (name, bridge) in &bridges {
        let at = if bridges.len() > 1 {
            format!("`{name}` ")
        } else {
            String::new()
        };
        let url = bridge
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if url.is_empty() {
            problems.push(format!("{at}`iii-bridge.url` is missing"));
        } else {
            for (manager, _, port) in &armed {
                if targets_the_engine_bus(url, *port) {
                    problems.push(format!(
                        "{at}`iii-bridge.url` is `{url}`, which is the OWN bus of `{manager}` (port \
                         {port}): the gate would depend on the listener it gates, and the stack \
                         deadlocks"
                    ));
                }
            }
        }
        forwarded.extend(
            bridge
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
                }),
        );
    }
    if !bridges.is_empty() {
        for (key, expected) in ARMED_HOOKS {
            if !forwarded.contains(&(expected, expected)) {
                problems.push(format!(
                    "no `iii-bridge` forwards `{expected}` -> `{expected}`, so the `rbac.{key}` hook \
                     resolves to nothing"
                ));
            }
        }
    }

    if problems.is_empty() {
        GateStatus::Armed
    } else {
        GateStatus::Inconsistent(problems)
    }
}

/// Base names of the two entries this check reads.
///
/// The engine allows several instances of one worker: `WorkerEntry::worker_type`
/// strips a `#instance` suffix (`engine/src/workers/config.rs:457-461`), and
/// upstream's own docs teach `iii-worker-manager#rbac`. A checker that matched
/// only the bare name found nothing on such a config and stayed silently inert —
/// which is why matching is by base name here, and why every armed entry is
/// validated rather than the first one.
const WORKER_MANAGER: &str = "iii-worker-manager";
const BRIDGE: &str = "iii-bridge";

/// Every `(display name, config mapping)` whose worker type is `base`.
///
/// The display name mirrors `assign_instance_ids` in the engine
/// (`engine/src/workers/config.rs`): duplicate names keep the first occurrence
/// as written and get `#1`, `#2` … appended after that, so a problem message
/// names the entry the engine will name in its own logs.
fn worker_entries<'a>(workers: &'a [Value], base: &str) -> Vec<(String, &'a Value)> {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    let mut found = Vec::new();
    for entry in workers {
        let Some(name) = entry.get("name").and_then(Value::as_str) else {
            continue;
        };
        let count = seen.entry(name).or_insert(0);
        let display = if *count > 0 {
            format!("{name}#{count}")
        } else {
            name.to_string()
        };
        *count += 1;
        let worker_type = name.split('#').next().unwrap_or(name);
        if worker_type == base
            && let Some(config) = entry.get("config")
        {
            found.push((display, config));
        }
    }
    found
}

/// The port one `iii-worker-manager` entry binds, or the engine's default.
///
/// Read rather than assumed: an earlier version substring-matched the literal
/// `49134`, so it went blind on any stack that moved the port — and loose in the
/// other direction, since `ws://10.0.49134.1:3000` contains it too.
fn entry_port(config: &Value) -> u16 {
    config
        .get("port")
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
    port == Some(engine_port) && host_means_this_machine(host)
}

/// True when `host` resolves to this machine for the purpose of the deadlock
/// check: `localhost` in any case, any loopback or unspecified IP, and the
/// `inet_aton` spellings glibc still accepts.
///
/// A table of literal strings missed `LOCALHOST`, `127.0.0.2`,
/// `127.000.000.001` and `0x7f000001` — all of which the bridge's own resolver
/// would happily connect to. This is an operator footgun, not an attacker path,
/// but a check that only recognises the spelling in our own docs is not a check.
fn host_means_this_machine(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        return address.is_loopback() || address.is_unspecified();
    }
    // `127.1`, `127.000.000.001`, `0x7f000001`, `2130706433`: Rust's parser
    // rejects them, getaddrinfo does not.
    inet_aton(host).is_some_and(|address| address.is_loopback() || address.is_unspecified())
}

/// The historical `inet_aton` forms: 1-4 parts, each decimal, octal (`0…`) or
/// hex (`0x…`), with the last part filling the remaining bytes.
fn inet_aton(host: &str) -> Option<std::net::Ipv4Addr> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut value: u32 = 0;
    for (index, part) in parts.iter().enumerate() {
        let number = parse_inet_part(part)?;
        if index + 1 == parts.len() {
            // The last part fills every byte the earlier parts did not.
            let remaining = 4 - index;
            let limit = if remaining >= 4 {
                u32::MAX as u64
            } else {
                (1u64 << (8 * remaining)) - 1
            };
            if number > limit {
                return None;
            }
            value |= number as u32;
        } else {
            if number > u8::MAX as u64 {
                return None;
            }
            value |= (number as u32) << (8 * (3 - index));
        }
    }
    Some(std::net::Ipv4Addr::from(value))
}

fn parse_inet_part(part: &str) -> Option<u64> {
    let lower = part.to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix("0x") {
        return (!hex.is_empty()).then(|| u64::from_str_radix(hex, 16).ok())?;
    }
    if lower.len() > 1 && lower.starts_with('0') {
        return u64::from_str_radix(&lower[1..], 8).ok();
    }
    lower.parse::<u64>().ok()
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
                .any(|problem| problem.contains("forwards `agentos::bus_on_trigger_type`")),
            "the forward list and the rbac block have to agree"
        );

        let self_gating = shipped_overlay().replace("ws://127.0.0.1:49129", "ws://127.0.0.1:49134");
        assert!(
            problems(&self_gating)
                .iter()
                .any(|problem| problem.contains("OWN bus of `iii-worker-manager`")),
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
            problems(&moved).iter().any(|problem| {
                problem.contains("OWN bus of `iii-worker-manager`") && problem.contains("39534")
            }),
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
    fn the_engine_bus_port_is_read_from_the_entry() {
        let port_of = |yaml: &str| -> u16 {
            let document: Value = serde_yaml::from_str(yaml).expect("test yaml");
            let workers = document
                .get("workers")
                .and_then(Value::as_sequence)
                .cloned()
                .unwrap_or_default();
            let entries = worker_entries(&workers, WORKER_MANAGER);
            entry_port(entries.first().expect("one manager entry").1)
        };
        assert_eq!(
            port_of("workers:\n  - name: iii-worker-manager\n    config:\n      host: 127.0.0.1\n"),
            49134,
            "an unpinned port is the engine's own default"
        );
        assert_eq!(
            port_of("workers:\n  - name: iii-worker-manager\n    config:\n      port: 39534\n"),
            39534
        );
        assert_eq!(
            port_of("workers:\n  - name: iii-worker-manager\n    config:\n      port: \"39534\"\n"),
            39534,
            "yaml may quote it"
        );
        assert_eq!(
            port_of(
                "workers:\n  - name: iii-worker-manager#rbac\n    config:\n      port: 39534\n"
            ),
            39534,
            "an instance suffix is still an iii-worker-manager"
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

        // P2: the host table used to be literal strings, so these were missed.
        for spelling in [
            "ws://LOCALHOST:49134",
            "ws://127.0.0.2:49134",
            "ws://127.000.000.001:49134",
            "ws://0x7f000001:49134",
            "ws://2130706433:49134",
            "ws://127.1:49134",
            "ws://[::1]:49134",
            "ws://[0:0:0:0:0:0:0:1]:49134",
        ] {
            assert!(
                targets_the_engine_bus(spelling, 49134),
                "{spelling} is this machine, whatever the spelling"
            );
        }
        for elsewhere in [
            "ws://126.0.0.1:49134",
            "ws://0x08080808:49134",
            "ws://example.com:49134",
            "ws://127.0.0.1.example.com:49134",
        ] {
            assert!(
                !targets_the_engine_bus(elsewhere, 49134),
                "{elsewhere} is not this machine"
            );
        }
        assert!(
            !targets_the_engine_bus("http://127.0.0.1:49134", 49134),
            "not a websocket url at all"
        );
    }
    /// P1-a: `.find` validated only the FIRST manager, so an armed second entry
    /// was reported as "not armed" — an affirmative claim that is false.
    #[test]
    fn every_worker_manager_entry_is_validated_not_just_the_first() {
        let two = format!(
            "workers:\n  - name: iii-worker-manager\n    config:\n      host: 127.0.0.1\n{}",
            armed_entry_yaml("iii-worker-manager", "auth_function_idd")
        );
        let reported = problems(&two);
        assert!(
            reported
                .iter()
                .any(|problem| problem.contains("rbac.auth_function_idd")),
            "the armed SECOND manager must be checked: {reported:?}"
        );
        assert!(
            reported
                .iter()
                .any(|problem| problem.starts_with("`iii-worker-manager#1` ")),
            "the engine renames a duplicate entry to `#1`, and so must the message: {reported:?}"
        );
    }

    /// Upstream's documented multi-instance form. Matching the bare name only
    /// made the whole check silently inert on a config shaped this way.
    #[test]
    fn an_instance_suffixed_manager_is_still_a_manager() {
        let suffixed = format!(
            "workers:\n  - name: state\n    config: {{}}\n{}",
            armed_entry_yaml("iii-worker-manager#rbac", "auth_function_idd")
        );
        assert!(
            problems(&suffixed)
                .iter()
                .any(|problem| problem.contains("rbac.auth_function_idd")),
            "`iii-worker-manager#rbac` is an iii-worker-manager"
        );

        // And a correctly armed suffixed manager still reads as armed.
        let good = format!(
            "workers:\n{}{}",
            armed_entry_yaml("iii-worker-manager#rbac", "auth_function_id"),
            bridge_entry_yaml("ws://127.0.0.1:49129")
        );
        assert_eq!(inspect(&good), GateStatus::Armed);
    }

    /// A second manager on its own port is its own deadlock surface.
    #[test]
    fn the_self_gating_check_covers_every_armed_manager() {
        let two = format!(
            "workers:\n  - name: iii-worker-manager\n    config:\n      port: 49134\n{}{}",
            armed_entry_yaml_with_port("iii-worker-manager#rbac", "auth_function_id", 39534),
            bridge_entry_yaml("ws://127.0.0.1:39534")
        );
        assert!(
            problems(&two).iter().any(|problem| {
                problem.contains("OWN bus of `iii-worker-manager#rbac`")
                    && problem.contains("39534")
            }),
            "the bridge points at the second manager's own listener"
        );
    }

    /// A fully armed entry, with one hook key spelled as the caller asks.
    fn armed_entry_yaml(name: &str, auth_key: &str) -> String {
        armed_entry_yaml_with_port(name, auth_key, 49134)
    }

    fn armed_entry_yaml_with_port(name: &str, auth_key: &str, port: u16) -> String {
        let mut entry = format!(
            "  - name: {name}\n    config:\n      host: 127.0.0.1\n      port: {port}\n      rbac:\n"
        );
        for (key, id) in ARMED_HOOKS {
            let key = if *key == "auth_function_id" {
                auth_key
            } else {
                key
            };
            entry.push_str(&format!("        {key}: {id}\n"));
        }
        entry.push_str("        expose_functions:\n          - match(\"*\")\n");
        entry
    }

    fn bridge_entry_yaml(url: &str) -> String {
        let mut entry =
            format!("  - name: iii-bridge\n    config:\n      url: {url}\n      forward:\n");
        for (_, id) in ARMED_HOOKS {
            entry.push_str(&format!(
                "        - local_function: {id}\n          remote_function: {id}\n"
            ));
        }
        entry
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
