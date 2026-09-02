//! Two guards that fail when the tree drifts away from the bus policy.
//!
//! Both walk the repository from `CARGO_MANIFEST_DIR`, so they run in CI the
//! same way they run locally, and both are written to fail LOUDLY rather than to
//! be quietly satisfied: an empty scan is itself an assertion failure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agentos_bus_auth::policy::UNTRUSTED_FORBIDDEN_FUNCTIONS;

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/bus-auth is two levels below the repository root")
        .to_path_buf()
}

/// Every `.rs` file under `<root>/<dir>/*/src`.
fn worker_sources(root: &Path, dir: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
        return files;
    };
    for entry in entries.flatten() {
        collect_rust_files(&entry.path().join("src"), &mut files);
    }
    files.sort();
    files
}

fn collect_rust_files(dir: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, into);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            into.push(path);
        }
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// The bus credential only exists if every worker actually sends it.
///
/// Before the migration all 62 workers called `register_worker(&ws_url,
/// InitOptions::default())`, so turning `rbac.auth_function_id` on would have
/// filed the whole product under the untrusted tier. This test is the grep that
/// keeps it that way: a new worker copied from an old one fails here.
#[test]
fn no_worker_joins_the_bus_without_the_credential() {
    let root = repository_root();
    let mut offenders: Vec<String> = Vec::new();
    let mut registrations = 0usize;

    for path in worker_sources(&root, "workers")
        .into_iter()
        .chain(worker_sources(&root, "crates"))
    {
        let source = std::fs::read_to_string(&path).expect("read worker source");
        for (index, line) in source.lines().enumerate() {
            if !line.contains("register_worker(") {
                continue;
            }
            registrations += 1;
            if line.contains("InitOptions::default()") {
                offenders.push(format!(
                    "{}:{}: {}",
                    relative(&root, &path),
                    index + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        registrations > 50,
        "the scan found only {registrations} register_worker call sites — it is not looking at the tree"
    );
    assert!(
        offenders.is_empty(),
        "these bus registrations send no credential; use `agentos_bus_auth::init_options()`:\n{}",
        offenders.join("\n")
    );
}

/// A new privileged function must not silently become reachable from a
/// credential-less bus session.
///
/// `forbidden_functions` is an exact-id list in the engine — there are no globs
/// — so a function added to one of contract I1's deny-by-default families is
/// callable by any local process until it is listed in the policy. This test
/// makes that omission a build failure.
#[test]
fn deny_set_covers_the_tree() {
    // `state::*` and `engine::*` are I1 families that are deliberately NOT
    // denied: the engine's own registry workers call them and cannot present a
    // credential. See the policy module docs.
    const SCANNED_FAMILIES: [&str; 10] = [
        "shell", "bridge", "mcp", "hook", "cron", "vault", "code", "harness", "browser", "wasm",
    ];
    // The four cron job targets the registry `cron` worker fires through its own
    // untrusted session. Denying them would stop every scheduled job.
    const CALLABLE_BY_THE_CRON_WORKER: [&str; 4] = [
        "cron::aggregate_daily_costs",
        "cron::cleanup_stale_sessions",
        "cron::reset_rate_limits",
        "workflow::run",
    ];

    let root = repository_root();
    let mut unlisted: BTreeMap<String, String> = BTreeMap::new();
    let mut scanned = 0usize;

    for path in worker_sources(&root, "workers") {
        let source = std::fs::read_to_string(&path).expect("read worker source");
        for candidate in quoted_function_ids(&source) {
            let family = candidate.split("::").next().unwrap_or_default();
            if !SCANNED_FAMILIES.contains(&family) {
                continue;
            }
            scanned += 1;
            if UNTRUSTED_FORBIDDEN_FUNCTIONS.contains(&candidate.as_str())
                || CALLABLE_BY_THE_CRON_WORKER.contains(&candidate.as_str())
            {
                continue;
            }
            unlisted.insert(candidate, relative(&root, &path));
        }
    }

    assert!(
        scanned > 30,
        "the scan found only {scanned} privileged ids — it is not looking at the tree"
    );
    assert!(
        unlisted.is_empty(),
        "these privileged function ids are reachable from a credential-less bus session; \
         add them to UNTRUSTED_FORBIDDEN_FUNCTIONS or justify them here:\n{}",
        unlisted
            .iter()
            .map(|(id, path)| format!("  {id}  ({path})"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Quoted `namespace::id` literals, skipping capability globs (`vault::*`).
fn quoted_function_ids(source: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for chunk in source.split('"').skip(1).step_by(2) {
        if !chunk.contains("::") || chunk.contains('*') || chunk.contains(' ') {
            continue;
        }
        if chunk
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-' | '.'))
        {
            ids.push(chunk.to_string());
        }
    }
    ids
}

/// A denied id that is also a trigger TARGET is a silently broken schedule.
///
/// Cron and queue triggers are fired by engine-spawned registry workers through
/// their own untrusted sessions, so `forbidden_functions` applies to them. The
/// deny list gained `memory::*` in review; `memory::consolidate` and
/// `memory::evict` are cron targets and had to stay out. This test derives the
/// target set from the tree so the next such addition fails here instead of on a
/// live stack six hours later.
#[test]
fn no_denied_id_is_fired_by_a_registry_worker_trigger() {
    let root = repository_root();
    let mut targets: BTreeMap<String, String> = BTreeMap::new();

    for path in worker_sources(&root, "workers") {
        let source = std::fs::read_to_string(&path).expect("read worker source");
        let origin = relative(&root, &path);
        for target in cron_trigger_targets(&source) {
            targets.insert(target, origin.clone());
        }
        for target in registry_fired_trigger_targets(&source) {
            targets.insert(target, origin.clone());
        }
    }

    assert!(
        targets.len() >= 8,
        "found only {} trigger targets - the scan is not looking at the tree",
        targets.len()
    );
    let denied: Vec<String> = targets
        .iter()
        .filter(|(id, _)| UNTRUSTED_FORBIDDEN_FUNCTIONS.contains(&id.as_str()))
        .map(|(id, path)| format!("  {id}  ({path})"))
        .collect();
    assert!(
        denied.is_empty(),
        "these ids are fired by an engine-spawned worker that cannot authenticate, \
         so denying them stops the schedule instead of the attacker:\n{}",
        denied.join("\n")
    );
}

/// Every id the shipped registry workers register must survive the hook.
///
/// `tests/registry_surface.txt` is a capture from a live armed boot (the header
/// of the file records how to regenerate it). The first version of
/// `UNTRUSTED_REGISTRATION_PREFIXES` held worker names instead of id namespaces
/// and refused 107 of these, leaving a stack with no LLM routing; this test is
/// the one that would have caught it in CI.
#[test]
fn every_shipped_registry_id_stays_registrable_without_a_credential() {
    let fixture = include_str!("registry_surface.txt");
    let untrusted = serde_json::json!({
        agentos_bus_auth::policy::TIER_CONTEXT_KEY: agentos_bus_auth::policy::TIER_UNTRUSTED,
    });

    let ids: Vec<&str> = fixture
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    assert!(
        ids.len() >= 100,
        "the capture holds only {} ids - an emptied fixture must not pass",
        ids.len()
    );

    let refused: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|id| !agentos_bus_auth::policy::function_registration_allowed(id, &untrusted))
        .collect();
    assert!(
        refused.is_empty(),
        "an armed stack would refuse these registrations and boot without them:\n  {}",
        refused.join("\n  ")
    );
}

/// First quoted `namespace::id` argument of every `register_cron_trigger` call.
fn cron_trigger_targets(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (index, _) in source.match_indices("register_cron_trigger") {
        let window = &source[index..source.len().min(index + 400)];
        let Some(open) = window.find('(') else {
            continue;
        };
        if let Some(id) = first_quoted_id(&window[open..]) {
            found.push(id);
        }
    }
    found
}

/// `function_id` of every trigger bound to a type an engine-spawned worker
/// provides (`cron`, `queue`). `subscribe`, `stream:join` and `state` come from
/// in-process engine workers, which fire without a session and are not gated.
fn registry_fired_trigger_targets(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for marker in ["trigger_type: \"cron\"", "trigger_type: \"queue\""] {
        for (index, _) in source.match_indices(marker) {
            let window = &source[index..source.len().min(index + 400)];
            let Some(field) = window.find("function_id:") else {
                continue;
            };
            if let Some(id) = first_quoted_id(&window[field..]) {
                found.push(id);
            }
        }
    }
    found
}

/// First `"namespace::id"` literal in `window`, ignoring format placeholders.
fn first_quoted_id(window: &str) -> Option<String> {
    let mut rest = window;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        let end = after.find('"')?;
        let candidate = &after[..end];
        if candidate.contains("::") && !candidate.contains('{') && !candidate.contains(' ') {
            return Some(candidate.to_string());
        }
        rest = &after[end + 1..];
    }
    None
}
