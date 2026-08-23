# Build 10010 Traceability

This matrix binds each reconciliation criterion to durable source and test
evidence. Command outcomes are reported at delivery time rather than claimed
by this static artifact.

| ISC identifier | Preserved behavior | Repository evidence |
|---|---|---|
| ISC-000 | Preserve both required histories, commit `238b423`, and a marker-free repository. | Merge parents in Git history plus the ancestry and repository-wide marker checks in the delivery procedure. |
| ISC-001 | Keep canonical worker identities, AgentOS-owned LLM names, fail-closed unanswered identity handling, and portable upgrade/install behavior. | `config.yaml`, `config/{state,queue,cron}.yaml`, worker manifests and consumers, `crates/cli/src/bootstrap.rs`, `crates/cli/tests/{up,portability}.rs`, and namespace/config tests. |
| ISC-002 | Keep the pinned Rust workspace, TypeScript, Bun, frozen-install, and real iii 0.22.1 boot regressions green. | The Rust 1.90 pin in `Cargo.toml` and release CI, `Cargo.lock`, `bun.lock`, `tests/config_boot.test.ts`, `tests/bun_lockfile.test.ts`, and the documented verification commands. |
| ISC-003 | Supply the four governed build-10010 artifacts and record every conflict decision. | This directory and the build-10010 cases in `tests/artifact_contract.test.ts`. |

## Required verification

Run both required `git merge-base --is-ancestor` checks and the explicit
`238b423` ancestry check. Scan repository files for all three conflict-marker
forms, run `cargo test --workspace` with the pinned toolchain, run
`bunx tsc --noEmit` and `bun test`, then run
`bun install --frozen-lockfile` and confirm `git status --porcelain` is empty.
