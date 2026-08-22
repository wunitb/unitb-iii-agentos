# Build 10003 Invariants

## Compatibility and ownership

- Checkout configuration uses the iii 0.22.1 canonical worker identifiers
  `queue`, `state`, and `cron`. Deprecated aliases must not coexist with, or
  stand in for, those workers; the standalone queue worker must own
  `engine::queue::enqueue` after boot.
- AgentOS owns the `agentos::llm::*` namespace end to end. Its router and source
  consumers do not register or invoke the ecosystem-level `llm::complete`,
  `llm::route`, `llm::providers`, or `llm::usage` identifiers.
- A TUI screen presents data only when this repository registers its provider.
  Sessions and Security remain provider-backed; Dashboard, Agents, Skills,
  Logs, Audit Events, and Settings identify the missing route and display an
  explicit no-provider state.

## Bootstrap and diagnostics

- `agentos up` verifies prerequisites in order, starts only missing processes,
  uses a bounded engine-health wait on port 49134, and launches the TUI in the
  foreground unless `--no-tui` is selected. Process spawning and readiness
  observations remain mockable in unit tests.
- `agentos doctor` is read-only. It reports the iii binary and version, engine
  health, required connected worker identities, TUI binary presence, and the
  active configuration-discovery mode without installing, building, or
  starting anything.
- A non-empty `AGENTOS_CONFIG` selects an explicit config. Otherwise checkout
  discovery remains available even when `AGENTOS_HOME` is set; the installed
  runtime below `AGENTOS_HOME` is the fallback.

## Portability and dependency state

- Tests compare child working directories with a canonicalized expected path,
  so macOS `/var` and `/private/var` aliases cannot create false failures.
- The root `bun.lock` is tracked, non-empty, complete for direct dependencies,
  and unchanged by repeated `bun install --frozen-lockfile` executions.
- Repository ignore policy does not hide `.agentfield-out-*` runner state. Such
  orchestration output is outside the AgentOS product and must not be silently
  folded into its source-control policy.
