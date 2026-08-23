# Build 10000 Traceability

This matrix traces the five normalized acceptance identifiers to recovered
repository behavior and durable tests. It describes static evidence; the final
delivery report records the fresh command outcomes.

| ISC identifier | Recovered behavior | Repository evidence |
|---|---|---|
| ISC-000 | Provide one-command bootstrap and read-only readiness diagnosis. | `crates/cli/src/bootstrap.rs`, `crates/cli/src/main.rs`, `crates/cli/tests/up.rs`, and `tests/quickstart.test.ts` cover ordered startup, bounded waits, ownership-aware cleanup, diagnostics, and the documented quickstart. |
| ISC-001 | Restore compatibility with iii 0.22.1 without worker or LLM namespace collisions. | `config.yaml`, canonical files under `config/`, `workers/llm-router/src/main.rs`, `tests/config.test.ts`, `tests/config_boot.test.ts`, and `tests/llm_namespace.test.ts` enforce canonical identifiers and live boot behavior. |
| ISC-002 | Harden startup readiness, configuration discovery, dotenv propagation, and protected TUI requests. | Unit tests in `crates/cli/src/bootstrap.rs` and `crates/cli/src/main.rs` cover identity-set readiness and discovery edge cases; TUI tests cover bearer-header construction. |
| ISC-003 | Align the Rust toolchain and faithfully render security capability payloads. | `Cargo.toml` and `.github/workflows/release.yml` select Rust 1.90; `crates/tui/src/main.rs` tests object, list, scalar, empty, and absent security payloads. |
| ISC-004 | Replace misleading TUI data calls and harden recovered edge cases. | `tests/tui_surfaces.test.ts`, `docs/decisions/2026-08-22-salvage-batch.md`, and focused Rust unit tests verify provider ownership, explicit unavailable states, malformed input handling, and canonical worker identity reporting. |

## Required verification

Run `bunx tsc --noEmit` and `bun test` from the result checkout. Both commands
must exit zero; live credential-dependent E2E cases may remain explicitly
skipped under their existing environment guards.
