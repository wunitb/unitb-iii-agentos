# Build 10010 Decisions

## D-001 — Reconcile history without changing the agreed tree

The Herdr fleet and remediation tips had converged to identical file trees but
neither tip contained the other's history. The result first merges commit
`238b423` into the feature history and then merges
`origin/issue/1897372b-remediate-artifact-1`. This preserves both lines as
ancestors while using their shared tree as the resolution oracle.

## D-002 — Resolve every conflicting file deliberately

“Herdr” means the declarative fleet/workflow line. “Remediation/main” means
the iii 0.22.1 portability, bootstrap, namespace, and fail-closed line. Where a
file carries both concerns, the combined result is kept.

| Conflicting file | Side kept | Reason |
|---|---|---|
| `README.md` | Combined | Keep Herdr architecture/workflow documentation together with canonical worker names, portable bootstrap instructions, and readiness semantics. |
| `crates/cli/src/bootstrap.rs` | Remediation/main | Preserve the #9 portable bootstrap and #12 fail-closed worker-identity implementation, including its injectable diagnostics and `Fake` regressions. |
| `crates/cli/src/main.rs` | Combined | Retain Herdr workflow commands while keeping portable path discovery, installation/upgrade behavior, and the `up`/`doctor` orchestration. |
| `crates/cli/tests/portability.rs` | Remediation/main | Keep runtime-state preservation and relocatable-install coverage, including canonicalized temporary-path expectations. |
| `e2e/full-stack.test.ts` | Combined | Preserve fleet/full-stack coverage and the deferred SDK load that lets the iii 0.22.1 boot regression control setup and cleanup safely. |
| `scripts/dev-up.sh` | Remediation/main | Use the canonical configuration-worker paths and identities expected by iii 0.22.1. |
| `tests/artifact_contract.test.ts` | Combined | Retain salvage artifact gates and add the governed build-10010 directory, trace-token, and per-conflict decision checks. |
| `workers/agent-core/src/main.rs` | Remediation/main | Keep AgentOS routing behavior while selecting `agentos::llm::*` for all LLM calls. |
| `workers/context-monitor/src/main.rs` | Remediation/main | Preserve monitoring logic with the collision-safe `agentos::llm::complete` consumer name. |
| `workers/eval/src/main.rs` | Remediation/main | Preserve evaluation logic with the collision-safe `agentos::llm::complete` consumer name. |
| `workers/evolve/src/main.rs` | Remediation/main | Preserve evolution logic and sandbox allow-list text with only AgentOS-owned LLM function names. |
| `workers/memory/src/main.rs` | Combined | Keep Herdr memory/session behavior and the remediation namespace change to `agentos::llm::complete`. |
| `workers/streaming/src/main.rs` | Remediation/main | Preserve streaming behavior while using `agentos::llm::route` and `agentos::llm::complete`. |

## D-003 — Keep reconciliation auxiliaries from the agreed tips

The committed `bun.lock` and `package-lock.json` remain the shared dependency
state rather than being regenerated during conflict resolution. The salvage
decision record remains intact because it explains the canonical worker and
LLM namespace choices inherited by this reconciliation.

## D-004 — Make the result mechanically reviewable

The build directory contains exactly `INVARIANTS.md`, `TRACES.md`,
`DECISIONS.md`, and `ATTACK_SURFACE.md`. The artifact contract checks the real
directory, all four whole-token criterion references, and all thirteen file
decisions so future edits cannot silently erase the reconciliation record.
