<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/banner-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/banner-light.svg">
    <img alt="AgentOS — narrow workers on iii primitives" src="assets/banner-light.svg" width="900">
  </picture>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-apache_2.0-0c0b0a?style=flat-square&labelColor=f2ede1" alt="Apache 2.0"></a>
  <img src="https://img.shields.io/badge/workers-63-0c0b0a?style=flat-square&labelColor=f2ede1" alt="Workers">
  <img src="https://img.shields.io/badge/functions-267-0c0b0a?style=flat-square&labelColor=f2ede1" alt="Functions">
  <img src="https://img.shields.io/badge/rust_tests-1393_total-0c0b0a?style=flat-square&labelColor=f2ede1" alt="1,393 Rust tests">
  <img src="https://img.shields.io/badge/iii--sdk-0.22.1-d96e2e?style=flat-square&labelColor=f2ede1" alt="iii-sdk 0.22.1">
</p>

<p align="center">
  <a href="https://www.agentsos.sh">website</a> ·
  <a href="ARCHITECTURE.md">architecture</a> ·
  <a href="#-03--quickstart">quickstart</a> ·
  <a href="#-06--workers">workers</a>
</p>

---

## § 01 · Thesis

AgentOS isn't another agent framework. It's *what's left* when the runtime becomes someone else's problem. This repository is UnitB's independent continuation of [`iii-experimental/agentos`](https://github.com/iii-experimental/agentos), retained under Apache-2.0.

63 narrow workers — 62 Rust binaries plus one Python worker — register Functions and Triggers on the [iii engine](https://github.com/iii-hq/iii). Every capability is one shape: `register_function(...)`. The engine carries routing, retries, state, and traces.

| ~~not~~ | yes |
|---|---|
| ~~assemble a runtime from category-shaped pieces~~ | collapse the categories onto one bus |
| ~~teach the model your DSL~~ | teach it three nouns |
| ~~bespoke agent runtime~~ | narrow workers on iii |

## § 02 · Three primitives

| Primitive | What it does | Examples |
|---|---|---|
| **Worker** | One Rust binary per domain. Connects to the engine over WebSocket. | `agent-core`, `llm-router`, `realm` |
| **Function** | A named handler registered by a Worker. | `agent::chat`, `llm::route`, `memory::search` |
| **Trigger** | Binds a Function to HTTP, cron, or pub/sub. | `POST /v1/chat/completions → stream::completion` |

That's the whole protocol. Workers stay narrow; everything else lives in the engine.

## § 03 · Quickstart

```bash
# 1. clone this repository
git clone https://github.com/wunitb/unitb-iii-agentos && cd unitb-iii-agentos

# 2. install the pinned iii v0.22.1 release with checksum verification
bash scripts/install-iii.sh

# 3. configure the local model proxy
install -m 600 .env.example .env
$EDITOR .env   # set CODEX_PROXY_API_KEY for http://127.0.0.1:8317/v1

# 4. build the workspace
cargo build --workspace --release

# 5. boot engine + workers (in two terminals, or one with `&`)
iii --config config.yaml &
bash scripts/dev-up.sh

# 6. open the chat
cargo run --release -p agentos-tui
```
Quickstart uses the local Codex proxy at `http://127.0.0.1:8317/v1`. Anthropic is optional and selected only by an explicit provider/model or when no configured local default exists.

Engine boots on port 49134. 62 Rust workers and one Python worker connect. The source declares 267 literal function registrations. The TUI opens on Chat — type a message, hit Enter, the agent replies. `/help` shows the full keymap. `Ctrl+W` browses the worker catalog.

Prefer driving by HTTP? Same thing without the TUI:

```bash
curl -X POST http://127.0.0.1:3111/v1/realms \
  -H 'Content-Type: application/json' \
  -d '{"name":"prod","description":"production"}'
```

The live chat E2E test defaults to `gpt-5.6-sol`. To target a non-Codex backend, override it for the test command:

```bash
AGENTOS_E2E_MODEL=claude-sonnet-4-20250514 bun run test:e2e
```

## § 04 · Calling a function

```rust
use iii_sdk::{register_worker, InitOptions, TriggerRequest};
use serde_json::json;

let iii = register_worker("ws://localhost:49134", InitOptions::default());

let result = iii.trigger(TriggerRequest {
    function_id: "memory::recall".to_string(),
    payload: json!({"agentId": "alice", "query": "..."}),
    action: None,
    timeout_ms: None,
}).await?;
```

This is the only inter-worker contract. There is no shared in-process state.

## § 05 · Registering one

```rust
use iii_sdk::{errors::Error, register_worker, InitOptions, RegisterFunction};
use serde_json::{json, Value};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let iii = register_worker("ws://localhost:49134", InitOptions::default());

    iii.register_function(
        "analyst::summarize",
        RegisterFunction::new_async(|input: Value| async move {
            let topic = input["topic"].as_str().unwrap_or("");
            Ok::<Value, Error>(json!({ "summary": format!("on {topic}") }))
        })
        .description("Summarize a topic"),
    );

    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}
```

## § 06 · Workers

62 Rust + 1 Python, grouped by responsibility.

| Group | Workers |
|---|---|
| Reasoning | `agent-core` `llm-router` `council` `swarm` `directive` `mission` |
| State | `realm` `memory` `ledger` `vault` `context-manager` `context-cache` |
| Coordination | `orchestrator` `workflow` `hierarchy` `coordination` `task-decomposer` |
| Execution | `wasm-sandbox` `browser` `code-agent` `hand-runner` `lsp-tools` |
| Safety | `security` `security-headers` `security-map` `security-zeroize` `skill-security` `approval` `approval-tiers` `rate-limiter` `loop-guard` |
| Surfaces | `a2a` `a2a-cards` `mcp-client` `skillkit-bridge` `bridge` `streaming` |
| Channels | `channel-{bluesky,discord,email,linkedin,mastodon,matrix,reddit,signal,slack,teams,telegram,twitch,webex,whatsapp}` |
| Telemetry | `telemetry` `pulse` `session-lifecycle` `session-replay` `feedback` `eval` `evolve` `hashline` `hooks` `cron` |
| Embeddings | `embedding` (Python) |

Each worker ships `iii.worker.yaml` declaring its registry shape. CI validates conformance on every PR.

## § 07 · Sandbox surfaces

Two distinct namespaces, never overlap:

| Namespace | Worker | Semantics |
|---|---|---|
| `sandbox::*` | builtin iii-sandbox (engine) | Ephemeral microVMs from OCI rootfs |
| `wasm::*` | agentos `wasm-sandbox` | wasmtime, fuel-metered, sub-millisecond cold start |

CI's `no sandbox::* clash with builtin` job greps the workspace to enforce the boundary.

## § 08 · Layout

```
workers/         62 Rust + 1 Python (embedding)
crates/          cli, tui, http-adapter — user surfaces plus transport boundary
e2e/             vitest end-to-end suite (live engine + workers)
tests/           Rust integration tests
hands/           agent personas (TOML, consumed by hand-runner)
integrations/    MCP server configs (TOML, consumed by mcp-client)
agents/          agent templates
workflows/       workflow definitions (YAML)
plugin/          reusable agent/command/skill/hook bundles
config.yaml      iii v0.22.1 engine and configuration-worker boot list
config/           committed values for the seven built-in iii workers
website/         agentsos.sh — design.md aesthetic, three themes
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full primitive flow and worker manifest spec.

## § 09 · TUI

Chat-first terminal UI lives in `crates/tui`:

```bash
cargo run --release -p agentos-tui
```

| Key | Action |
|---|---|
| `/` | Slash command (`/agent`, `/memory`, `/worker`, `/realm`, `/skill`, `/hand`, `/help`, `/quit`) |
| `Tab` | Autocomplete current slash command against the live function registry |
| `?` | Toggle keymap overlay |
| `Ctrl+P` | Command palette (fuzzy-jump to any pane) |
| `Ctrl+W` | Worker picker — browse + install workers without leaving the TUI |
| `Esc` | Close overlay or clear input |
| `1-9 0` | Direct pane switch (Dashboard / Agents / Chat / Channels / …) |

If the engine is offline or no workers are connected, the TUI shows a first-run overlay with copy-paste commands instead of an empty list. Slash completions pull from `GET /iii/functions` so anything a worker registers is immediately discoverable.

## § 10 · Build and test

```bash
cargo build --workspace --release                                    # 62 Rust workers + CLI + TUI + HTTP adapter
cargo test --workspace --release                                     # 1,393 Rust tests; 3 live-engine checks ignored by default
uv run --no-project --with pytest python -m pytest workers/embedding/test_main.py -q  # 161 Python tests
bun install --frozen-lockfile && bun run test:e2e                    # live engine + workers; model credentials required for chat
```

## § 11 · Versioning

| | version |
|---|---|
| iii engine | pinned at `v0.22.1` by `scripts/install-iii.sh` |
| iii-sdk (Rust) | pinned at `=0.22.1` in workspace |
| iii-sdk (Node) | pinned at `0.22.1` for the e2e harness |
| iii-sdk (Python) | pinned at `0.22.1` for the embedding worker |
| agentos | `0.1.0` — first UnitB-owned release on iii v0.22.1 |

## § 12 · Provenance and license

This independent repository started from [`iii-experimental/agentos@caca2b4`](https://github.com/iii-experimental/agentos/commit/caca2b439ff62499f0d4a5af30c2601302238890) and was migrated to the `iii-hq/iii` v0.22.1 engine and SDK contracts. It is not a GitHub fork and carries its own history.

Apache-2.0. Same family as `iii-sdk` and the rest of the iii ecosystem.
