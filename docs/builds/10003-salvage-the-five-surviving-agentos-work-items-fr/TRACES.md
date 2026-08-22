# Build 10003 Traceability

This matrix maps every acceptance identifier to the recovered implementation
and its durable regression evidence. Fresh command outcomes belong in the
delivery report rather than in this static artifact.

| Acceptance identifier | Delivered behavior | Repository evidence |
|---|---|---|
| ISC-000 | Migrate the checkout to canonical iii 0.22.1 worker names and prove the standalone queue provider survives a real boot. | `config.yaml` and `config/{queue,state,cron}.yaml`; `tests/config.test.ts` rejects deprecated aliases and canonical collisions; `tests/config_boot.test.ts` boots iii 0.22.1 and waits for `engine::queue::enqueue` from `queue-engine`. |
| ISC-001 | Namespace AgentOS router registrations and every source call site below `agentos::llm::*`; record collision-error handling as an upstream-only proposal for the principal. | `workers/llm-router/src/main.rs` and its consumers under `workers/` and `crates/tui/`; `tests/llm_namespace.test.ts`; `docs/decisions/2026-08-22-salvage-batch.md`. |
| ISC-002 | Keep Sessions and Security provider-backed; degrade Dashboard, Agents, Skills, Logs, Audit Events, and Settings to an explicit no-provider state. | `workers/memory/src/main.rs`, `workers/security/src/main.rs`, `crates/tui/src/main.rs`, `tests/tui_surfaces.test.ts`, and the per-screen table in `docs/decisions/2026-08-22-salvage-batch.md`. |
| ISC-003 | Add ordered `agentos up`/`--no-tui` orchestration, read-only `agentos doctor`, documented config precedence, and the new quickstart. | `crates/cli/src/bootstrap.rs`, `crates/cli/src/main.rs`, `crates/cli/tests/up.rs`, `crates/cli/tests/portability.rs`, `README.md`, and `tests/quickstart.test.ts` cover mockable spawning, readiness decisions, broken states, and `AGENTOS_HOME` checkout discovery. |
| ISC-004 | Canonicalize the expected runtime directory in portability tests before comparing it with a child process's `$PWD`. | `crates/cli/tests/portability.rs` canonicalizes both affected expected runtime paths, covering the macOS `/var` versus `/private/var` alias. |
| ISC-005 | Commit a reproducible root Bun lockfile while removing the unrelated AgentField ignore rule. | `bun.lock`, `.gitignore`, and `tests/bun_lockfile.test.ts` enforce tracking, root direct-dependency coverage, and the rule that runner output is not hidden. Repeated frozen installs, TypeScript checking, and the Bun suite are the delivery commands. |

## Required verification

Run `bun install --frozen-lockfile` twice, confirm that the second run changes
no tracked product file, run `bunx tsc --noEmit`, and run `bun test`. For the
trace gate, each identifier above must be discoverable as its own token.
