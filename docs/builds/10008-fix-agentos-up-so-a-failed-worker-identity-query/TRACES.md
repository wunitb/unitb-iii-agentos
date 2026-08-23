# Build 10008 Traceability

| ISC identifier | Behavior | Repository evidence |
|---|---|---|
| ISC-000 | `agentos up` distinguishes an unanswered identity query from a reported empty identity set, retries `None` within the bounded poll plan, and fails before worker launch with the readiness diagnostic if the state remains unknown. | `await_worker_identity_report` and `WORKER_IDENTITIES_UNREPORTED` in `crates/cli/src/bootstrap.rs`. |
| ISC-001 | Regression coverage proves persistent `None` is retried, never yields `UpOutcome::Ready`, and spawns no worker, while `Some(empty)` starts the required workers. | `up_fails_closed_when_the_initial_worker_identity_query_is_unanswered` and `up_reuses_a_healthy_engine_without_spawning_another` in `crates/cli/src/bootstrap.rs`. |
| ISC-002 | The duplicate-registration threat and its fail-closed mitigation are recorded in a complete four-file artifact set. | `docs/builds/10008-fix-agentos-up-so-a-failed-worker-identity-query/ATTACK_SURFACE.md`, `INVARIANTS.md`, `DECISIONS.md`, and this trace matrix. |

## Required verification

Run `cargo test -p agentos-cli`, `bunx tsc --noEmit`, and `bun test`. Confirm
that the governed directory contains exactly the four required Markdown files.
