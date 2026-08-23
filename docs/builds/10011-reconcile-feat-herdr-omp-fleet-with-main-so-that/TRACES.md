# Build 10011 Traceability

| Criterion | Preserved behavior | Evidence |
|---|---|---|
| ISC-000 | Required histories and `238b423` are ancestors; source files are marker-free. | Merge commits, `tests/reconciliation_contract.test.ts`, explicit ancestry commands, and repository-wide marker scan. |
| ISC-001 | Canonical worker identities, complete `agentos::llm::*` migration, fail-closed worker discovery, portable upgrades, native provider tool payloads, and normalized function calls. | `config.yaml`, worker manifests, `crates/cli/src/bootstrap.rs`, `crates/cli/tests/{up,portability}.rs`, the three LLM consumers/router files, `tests/llm_namespace.test.ts`, install tests, and router unit tests. |
| ISC-002 | Pinned Rust workspace, TypeScript, Bun, iii 0.22.1 boot regression, and frozen dependency installation remain reproducible. | `cargo +1.90.0 test --workspace`, `bunx tsc --noEmit`, `bun test`, `tests/config_boot.test.ts`, and `bun install --frozen-lockfile` followed by a clean status check. |
| ISC-003 | The governed build contains all four evidence documents and records every conflict decision. | This directory and `DECISIONS.md`. |

## Delivery checks

The delivery report records command exit status rather than embedding a stale
success claim here. The required checks cover both merge-base assertions,
`238b423`, conflict markers, legacy LLM call sites, the pinned Rust workspace,
TypeScript, Bun tests, and frozen-install cleanliness.
