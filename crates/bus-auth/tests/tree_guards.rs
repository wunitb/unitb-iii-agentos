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
