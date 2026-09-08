//! Two guards that fail when the tree drifts away from the bus policy.
//!
//! Both walk the repository from `CARGO_MANIFEST_DIR`, so they run in CI the
//! same way they run locally, and both are written to fail LOUDLY rather than to
//! be quietly satisfied: an empty scan is itself an assertion failure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agentos_bus_auth::policy::UNTRUSTED_FORBIDDEN_FUNCTIONS;

fn ci_runs() -> Vec<String> {
    let source = std::fs::read_to_string(repository_root().join(".github/workflows/ci.yml"))
        .expect("read CI");
    let workflow: serde_yaml::Value = serde_yaml::from_str(&source).expect("parse CI");
    let jobs = workflow["jobs"].as_mapping().expect("CI jobs");
    let runs: Vec<String> = jobs
        .values()
        .flat_map(|job| job["steps"].as_sequence().expect("job steps"))
        .filter_map(|step| step["run"].as_str().map(str::to_owned))
        .collect();
    assert!(!runs.is_empty(), "CI run-step scan must not be empty");
    runs
}

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

#[test]
fn ci_product_boots_use_oci_instead_of_unowned_host_engines() {
    let runs = ci_runs();
    assert!(
        runs.iter()
            .any(|run| run.contains("bash scripts/boot-smoke.sh"))
    );
    assert!(
        runs.iter()
            .any(|run| run.contains("python3 scripts/oci-smoke.py --report"))
    );
    for run in runs {
        assert!(
            !run.contains("iii --config config.yaml"),
            "product engines must not start in the checkout"
        );
        assert!(
            !run.contains("pkill -f"),
            "CI must not sweep unrelated processes"
        );
    }
}

#[test]
fn oci_boots_use_product_generated_private_machine_keys() {
    let root = repository_root();
    let entry =
        std::fs::read_to_string(root.join("scripts/container-entrypoint.py")).expect("entrypoint");
    let smoke = std::fs::read_to_string(root.join("scripts/oci-smoke.py")).expect("smoke");
    assert!(entry.contains(r#"["agentos", "up", "--no-tui", "--timeout", "240"]"#));
    assert!(smoke.contains("AGENTOS_API_KEY="));
    assert!(smoke.contains("AUDIT_HMAC_KEY="));
    assert!(smoke.contains("read_api_key(home)"));
    assert!(smoke.contains("runtime dotenv is not private"));
}

#[test]
fn e2e_smoke_requires_the_private_fake_provider_and_json_result_gate() {
    let source =
        std::fs::read_to_string(repository_root().join(".github/workflows/ci.yml")).expect("CI");
    let workflow: serde_yaml::Value = serde_yaml::from_str(&source).expect("parse CI");
    let job = &workflow["jobs"]["e2e-smoke"];
    assert!(job["env"]["AGENTOS_API_KEY"].is_null());
    assert!(job["env"]["ANTHROPIC_API_KEY"].is_null());
    let steps = job["steps"].as_sequence().expect("e2e smoke steps");
    let run = steps
        .iter()
        .find(|step| step["name"].as_str() == Some("credential-free worker integration tests"))
        .expect("actual acceptance step")["run"]
        .as_str()
        .expect("run body");
    assert!(run.contains("python3 scripts/oci-smoke.py --report"));
    assert!(run.contains("bun scripts/assert-oci-results.ts"));
    assert!(!run.contains("--live-e2e"));
}

#[test]
fn boot_smoke_keeps_security_views_and_owned_cleanup_as_required_checks() {
    let root = repository_root();
    let wrapper = std::fs::read_to_string(root.join("scripts/boot-smoke.sh")).expect("wrapper");
    let smoke = std::fs::read_to_string(root.join("scripts/oci-smoke.py")).expect("smoke");
    assert!(wrapper.contains("exec python3"));
    assert!(wrapper.contains("oci-smoke.py"));
    for marker in [
        "UNTRUSTED_DENIED_FUNCTION_IDS",
        "WORKER_MUTATION_FUNCTION_IDS",
        "configuration::set",
        "agent::chat",
        "validate_access(registry, public)",
        "include_internal",
        "owned teardown failed",
    ] {
        assert!(smoke.contains(marker), "OCI smoke lacks {marker}");
    }
    assert!(!smoke.contains("process.kill"));
    assert!(!smoke.contains("pkill"));
}

#[test]
fn every_ci_job_has_a_bounded_timeout() {
    let workflow = std::fs::read_to_string(repository_root().join(".github/workflows/ci.yml"))
        .expect("read ci.yml");
    let jobs = workflow.split("\njobs:\n").nth(1).expect("jobs section");
    let lines: Vec<&str> = jobs.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            line.starts_with("  ") && !line.starts_with("    ") && line.trim_end().ends_with(':')
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(starts.len(), 12, "guard the complete CI job inventory");
    for (position, start) in starts.iter().enumerate() {
        let end = starts.get(position + 1).copied().unwrap_or(lines.len());
        let block = lines[*start..end].join("\n");
        assert!(
            block.contains("timeout-minutes:"),
            "CI job {} has no bounded timeout",
            lines[*start].trim_end_matches(':').trim()
        );
    }
}

#[test]
fn ci_only_cancels_superseded_pull_request_runs() {
    let workflow = std::fs::read_to_string(repository_root().join(".github/workflows/ci.yml"))
        .expect("read ci.yml");
    assert!(workflow.contains("cancel-in-progress: ${{ github.event_name == 'pull_request' }}"));
}

#[test]
fn portable_bundle_contains_the_runtime_security_contract() {
    let workflow = std::fs::read_to_string(repository_root().join(".github/workflows/ci.yml"))
        .expect("read ci.yml");
    for required in [
        "./bin/agentos-bus-authd",
        "./runtime/.env.example",
        "./runtime/iii.lock",
        "./runtime/workers/env.allowlist",
    ] {
        assert!(
            workflow.contains(required),
            "portable bundle does not validate {required}"
        );
    }
    assert!(
        workflow.contains(r"/^\[workspace\.package\]/"),
        "portable bundle version must be parsed from Cargo.toml's workspace package table"
    );
    assert!(!workflow.contains("version=\"0.1.0\""));
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
///
/// It scans REGISTRATION SITES, not string literals. The first version read
/// every quoted `a::b` in the tree and reported `vault::read` and `code::run`
/// from `workers/workflow`'s `a_deny_by_default_step_needs_an_approval_decision`
/// fixture — two ids that do not exist anywhere. A guard that flags
/// documentation and test data is a guard someone eventually weakens, so this
/// one derives its input the same way `scripts/counts.ts` derives the published
/// function count: blank `#[cfg(test)]` modules, then take the literal first
/// argument of `register_function(`.
#[test]
fn deny_set_covers_the_tree() {
    // `state::*` and `engine::*` are I1 families that are deliberately NOT
    // denied: the engine's own registry workers call them and cannot present a
    // credential. See the policy module docs.
    const SCANNED_FAMILIES: [&str; 10] = [
        "shell", "bridge", "mcp", "hook", "cron", "vault", "code", "harness", "browser", "wasm",
    ];
    // Cron job targets the registry `cron` worker fires through its own
    // untrusted session. Denying them would stop the schedule.
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
        for candidate in registered_function_ids(&source) {
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
        "the scan found only {scanned} privileged registrations — it is not looking at the tree"
    );
    assert!(
        unlisted.is_empty(),
        "these privileged function ids are registered by a worker and are reachable from a \
         credential-less bus session; add them to UNTRUSTED_FORBIDDEN_FUNCTIONS or justify them:\n{}",
        unlisted
            .iter()
            .map(|(id, path)| format!("  {id}  ({path})"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The scan reads what the shipped binary registers, and nothing else.
///
/// Both halves matter: a `#[cfg(test)]` fixture id must not be reported (the
/// false positive this test exists to pin), and a real `register_function(` call
/// must be.
#[test]
fn the_registration_scan_reads_shipped_code_only() {
    let source = r#"
        fn main() {
            iii.register_function(
                "vault::get",
                RegisterFunction::new_async(|input| async move { Ok(input) }),
            );
        }

        #[cfg(test)]
        mod tests {
            #[test]
            fn a_deny_by_default_step_needs_an_approval_decision() {
                for function_id in ["vault::read", "code::run"] {
                    assert!(is_denied(function_id));
                }
            }

            #[test]
            fn registrations_inside_a_test_module_are_not_shipped() {
                iii.register_function("vault::not_shipped", handler());
            }
        }
    "#;

    let ids = registered_function_ids(source);
    assert_eq!(
        ids,
        vec!["vault::get".to_string()],
        "only the shipped registration may be reported"
    );
    for fixture in ["vault::read", "code::run", "vault::not_shipped"] {
        assert!(
            !ids.iter().any(|id| id == fixture),
            "{fixture} is test data, not a registration"
        );
    }
}

/// Literal ids of `register_function("id", ...)` call sites in shipped code.
///
/// Same derivation as `scripts/counts.ts::collectRegistrations`, so the two
/// agree on what "a function this repository registers" means. Call sites that
/// build their id at runtime (`workers/hand-runner`, `crates/http-adapter`) have
/// no literal and are not reported — they also cannot be exact deny entries.
fn registered_function_ids(source: &str) -> Vec<String> {
    let shipped = without_test_modules(source);
    let mut ids = Vec::new();
    for (index, _) in shipped.match_indices("register_function(") {
        let rest = shipped[index + "register_function(".len()..].trim_start();
        let Some(literal) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = literal.find('"') else {
            continue;
        };
        let id = &literal[..end];
        if id.contains("::") {
            ids.push(id.to_string());
        }
    }
    ids
}

/// Blank every `#[cfg(test)]` block, preserving newlines so nothing shifts.
///
/// Ported from `scripts/counts.ts::withoutTestModules`, including its handling
/// of `#[cfg(test)] use ...;`, which annotates an item with no block.
fn without_test_modules(source: &str) -> String {
    const ATTRIBUTE: &str = "#[cfg(test)]";
    let mut result = source.to_string();
    loop {
        let Some(attribute) = result.find(ATTRIBUTE) else {
            break;
        };
        let open = result[attribute..].find('{').map(|at| attribute + at);
        let terminator = result[attribute..].find(';').map(|at| attribute + at);
        let Some(open) = open else {
            result.replace_range(
                attribute..attribute + ATTRIBUTE.len(),
                &" ".repeat(ATTRIBUTE.len()),
            );
            continue;
        };
        if terminator.is_some_and(|end| end < open) {
            result.replace_range(
                attribute..attribute + ATTRIBUTE.len(),
                &" ".repeat(ATTRIBUTE.len()),
            );
            continue;
        }
        let mut depth = 0usize;
        let mut close = result.len() - 1;
        for (index, character) in result[open..].char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = open + index;
                        break;
                    }
                }
                _ => {}
            }
        }
        let blanked: String = result[attribute..=close]
            .chars()
            .map(|c| if c == '\n' { '\n' } else { ' ' })
            .collect();
        result.replace_range(attribute..=close, &blanked);
    }
    result
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
        // Test modules describe triggers they never register; blank them for
        // the same reason the registration scan does.
        let source =
            without_test_modules(&std::fs::read_to_string(&path).expect("read worker source"));
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
    // Every denied target is checked, including agent::chat. The old
    // credential-less agent.inbox deputy has been removed from agent-core.
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
    assert!(
        ids.iter().all(|id| !id.starts_with("configuration::")),
        "the live capture proves configuration registers in-process; do not reopen its prefix"
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

/// Every trigger type the shipped registry workers register must survive the
/// trigger-TYPE hook, for the same reason `registry_surface.txt` exists: a hook
/// that refuses a legitimate registration takes the stack down quietly.
#[test]
fn every_shipped_trigger_type_stays_registrable_without_a_credential() {
    let fixture = include_str!("trigger_type_surface.txt");
    let untrusted = serde_json::json!({
        agentos_bus_auth::policy::TIER_CONTEXT_KEY: agentos_bus_auth::policy::TIER_UNTRUSTED,
    });

    let ids: Vec<&str> = fixture
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    assert!(
        ids.len() >= 10,
        "the capture holds only {} trigger types - an emptied fixture must not pass",
        ids.len()
    );

    let refused: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|id| !agentos_bus_auth::policy::trigger_type_registration_allowed(id, &untrusted))
        .collect();
    assert!(
        refused.is_empty(),
        "an armed stack would refuse these trigger types and lose whatever they carry:\n  {}",
        refused.join("\n  ")
    );

    // The types an IN-PROCESS engine worker registers never reach the hook, so
    // admitting them here would only ever help an attacker.
    for in_process in [
        "http",
        "stream",
        "stream:join",
        "stream:leave",
        "subscribe",
        "log",
        "trace",
        "configuration",
        "engine::functions-available",
        "engine::workers-available",
    ] {
        assert!(
            !agentos_bus_auth::policy::trigger_type_registration_allowed(in_process, &untrusted),
            "{in_process} is registered in-process; a credential-less session must not claim it"
        );
    }
}
