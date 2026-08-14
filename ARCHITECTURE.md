# UnitB AgentOS architecture

This codebase is an independent continuation of `iii-experimental/agentos`, migrated to iii v0.22.1. AgentOS is an agent operating system built on the [iii engine](https://github.com/iii-hq/iii). The repo ships **63 narrow workers** (62 Rust workers and one Python worker), declarative config (hands, integrations, agents), and two surfaces (`crates/cli`, `crates/tui`). Everything coordinates through iii primitives — `register_function`, `register_trigger`, `iii.trigger` — over the engine's WebSocket on port 49134.

## Repository layout

```
unitb-iii-agentos/
├── workers/                  62 Rust workers + 1 Python worker
├── crates/
│   ├── cli/                  Command-line client (HTTP → iii-http on 3111)
│   ├── http-adapter/         HTTP trigger and response-envelope boundary
│   └── tui/                  Terminal dashboard
├── e2e/                      vitest end-to-end suite (live engine + workers)
├── tests/                    Rust integration tests
├── hands/                    Agent personas (TOML, consumed by hand-runner)
├── integrations/             MCP server configs (TOML, consumed by mcp-client)
├── agents/                   Agent templates (markdown)
├── workflows/                Pre-defined workflow YAMLs
├── plugin/                   Reusable agent/command/skill/hook bundles
├── config.yaml               iii engine boot config
└── .github/workflows/ci.yml  Build + test + e2e
```

## Workers

| Group | Workers | Function namespaces |
|---|---|---|
| Reasoning | `agent-core` `llm-router` `council` `swarm` `directive` `mission` | `agent::*` `llm::*` `council::*` `swarm::*` `directive::*` `mission::*` |
| State | `realm` `memory` `ledger` `vault` `context-manager` `context-cache` | `realm::*` `memory::*` `ledger::*` `vault::*` `context::*` |
| Coordination | `orchestrator` `workflow` `hierarchy` `coordination` `task-decomposer` | `orchestrator::*` `workflow::*` `hierarchy::*` `task::*` |
| Execution | `wasm-sandbox` `browser` `code-agent` `hand-runner` `lsp-tools` | `wasm::*` `browser::*` `code::*` `hand::*` `lsp::*` |
| Safety | `security` `security-headers` `security-map` `security-zeroize` `skill-security` `approval` `approval-tiers` `rate-limiter` `loop-guard` | `security::*` `approval::*` `rate::*` `loop::*` |
| Surfaces | `a2a` `a2a-cards` `mcp-client` `skillkit-bridge` `bridge` `streaming` | `a2a::*` `mcp::*` `skillkit::*` `bridge::*` `stream::*` |
| Channels | `channel-{bluesky, discord, email, linkedin, mastodon, matrix, reddit, signal, slack, teams, telegram, twitch, webex, whatsapp}` | `channel::*` |
| Telemetry | `telemetry` `pulse` `session-lifecycle` `session-replay` `feedback` `eval` `evolve` `hashline` `hooks` `cron` | `telemetry::*` `pulse::*` `session::*` `eval::*` `feedback::*` |
| Embeddings | `embedding` (Python) | `embedding::*` |

The source declares 267 literal function registrations across 63 workers (62 Rust + 1 Python).

## Worker manifest

Every directory under `workers/` ships an `iii.worker.yaml`:

```yaml
iii: v1
name: <name>           # must equal the folder name
runtime:
  kind: rust           # rust | python
scripts:
  install: cargo build --release
  start: cargo run --release
description: ...
```

CI's `validate iii.worker.yaml` job enforces this on every PR.

## Engine boot

`config.yaml` uses iii v0.22.1's configuration-worker layout. It declares the file-backed configuration store plus seven baseline workers: `iii-http`, `iii-state`, `iii-stream`, `iii-queue`, `iii-pubsub`, `iii-cron`, and `iii-observability`. Their committed values live in `config/iii-*.yaml`; `config.yaml` keeps only worker entries and migration breadcrumbs. AgentOS workers spawn alongside as separate processes — each connects to the engine WebSocket via `register_worker` and stays resident.

The engine WebSocket endpoint is configurable via `III_URL` (default `ws://localhost:49134`).

## Calling a function from another worker

```rust
iii.trigger(TriggerRequest {
    function_id: "memory::recall".to_string(),
    payload: json!({ "agentId": "alice", "query": "..." }),
    action: None,
    timeout_ms: None,
}).await?
```

Fire-and-forget:

```rust
let iii_c = iii.clone();
tokio::spawn(async move {
    let _ = iii_c.trigger(TriggerRequest {
        function_id: "security::audit".to_string(),
        payload: json!({ "type": "..." }),
        action: None,
        timeout_ms: None,
    }).await;
});
```

This is the only inter-worker contract. There is no shared in-process state.

## Sandbox primitives — two surfaces

| namespace | worker | semantics |
|---|---|---|
| `sandbox::create` / `sandbox::exec` / `sandbox::list` / `sandbox::stop` | **builtin** iii-sandbox (v0.22.1) | Ephemeral microVMs from OCI rootfs (Python, Node presets). Full Linux. |
| `wasm::execute` / `wasm::validate` / `wasm::list_modules` | agentos `wasm-sandbox` | wasmtime, fuel-metered, sub-millisecond cold start. |

CI's `no sandbox::* clash with builtin` job greps the workspace to ensure no agentos worker registers `sandbox::*`.

## Atomic state ops

iii v0.22.1 exposes `state::update` / `stream::update` with `UpdateOp::set`, `UpdateOp::increment`, `UpdateOp::append`, plus nested shallow-merge paths. Workers prefer these over `state::list + state::set` race patterns when mutating lists or counters.

`council::activity` still uses a manual hash-chain on `state::list + state::set` — a separate refactor will move it onto `UpdateOp::append` once the chain protocol tolerates concurrent appends without compare-and-swap.

## Surfaces (cli, tui)

`crates/cli` and `crates/tui` are clients, not workers. They speak HTTP to `iii-http` on port 3111. They register no functions. Future work moves them onto the iii client SDK so they call workers via `iii.trigger` directly.

## Hands, integrations, agents, workflows

These are **declarative config**, not workers:

- `hands/<name>/HAND.toml` — agent persona (system prompt, allowed function ids, schedule), consumed by the `hand-runner` worker.
- `integrations/<name>.toml` — MCP server connection details (transport, command, OAuth scopes), consumed by the `mcp-client` worker.
- `agents/<name>/...` — markdown templates for spawning agent personas.
- `workflows/<name>.yaml` — pre-defined workflow definitions for the `workflow` worker.

None ship as registered functions; they configure workers that do.

## Versioning

- iii engine: **v0.22.1**, pinned and checksum-verified by `scripts/install-iii.sh`
- iii-sdk (Rust): **=0.22.1** in workspace `Cargo.toml`
- iii-sdk (Node): **0.22.1** in root `package.json` (e2e tests only)
- iii-sdk (Python): **=0.22.1** in `workers/embedding/pyproject.toml`
- agentos workspace: **0.1.0**, inherited by every Rust crate from `[workspace.package]`

## CI

Eight jobs run on every PR:

| job | gate |
|---|---|
| `rust build + test` | `cargo check --all-targets` + `cargo build --release` + `cargo test --workspace --release` (1,382 tests; 3 live-engine checks ignored by default) |
| `python worker tests` | `pytest workers/embedding/test_main.py` |
| `website build` | `npm ci` + `npm run build` in `website/` |
| `installer shellcheck` | `shellcheck --severity=warning` over `scripts/install.sh`, `scripts/dev-up.sh`, `website/public/install.sh`, `scripts/install-iii.sh` |
| `validate iii.worker.yaml` | every `workers/<name>/iii.worker.yaml` parses, matches its folder, declares `runtime.kind` of `rust` or `python`, and carries a `scripts.start` string |
| `no sandbox::* clash with builtin` | grep ensures no agentos worker registers `sandbox::*` |
| `e2e smoke (no LLM key required)` | typechecks and tests the Node examples and startup configuration, then starts engine + workers, asserts ports listen, ≥30 functions register, no namespace clash |
| `e2e full (requires AGENTOS_API_KEY secret)` | runs vitest e2e suite against the live stack — gated on `AGENTOS_API_KEY` secret |

Plus `.github/workflows/vercel-deploy.yml`: pushes to `main` touching `website/**` trigger a Vercel Deploy Hook.

## Dependencies (declarative chain-install)

The v1 `iii.worker.yaml` schema supports a `dependencies:` map that lets `iii worker add ./workers/agent-core` chain-install `llm-router`, `memory`, `security` from the registry. AgentOS workers do not yet declare deps because they aren't published to the registry — once publishing lands, agent-core gets:

```yaml
dependencies:
  llm-router: ^0.1.0
  memory: ^0.1.0
  security: ^0.1.0
```

## File-by-file responsibilities

For deeper detail on any worker, read its `src/main.rs` (Rust) or `main.py` (Python). Each is intentionally small (5–10 registered functions, 300–2000 LOC).
