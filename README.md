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
  <img src="https://img.shields.io/badge/functions-301-0c0b0a?style=flat-square&labelColor=f2ede1" alt="Functions">
  <img src="https://img.shields.io/badge/rust_tests-1998_total-0c0b0a?style=flat-square&labelColor=f2ede1" alt="1,998 Rust tests">
  <img src="https://img.shields.io/badge/iii--sdk-0.22.1-d96e2e?style=flat-square&labelColor=f2ede1" alt="iii-sdk 0.22.1">
</p>

<p align="center">
  <a href="https://www.agentsos.sh">website</a> ·
  <a href="ARCHITECTURE.md">architecture</a> ·
  <a href="INSTALL_STACK.md">complete stack install</a> ·
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
| **Function** | A named handler registered by a Worker. | `agent::chat`, `agentos::llm::route`, `memory::search` |
| **Trigger** | Binds a Function to HTTP, cron, or pub/sub. | `POST /v1/chat/completions → stream::completion` |

There is exactly one chat pipeline. `stream::chat`, `stream::completion` and
`stream::sse` all delegate to `agent::chat`, so every HTTP caller gets the same
tool loop, injection scan, memory and metering the TUI gets. The transport is
**buffered, not token streaming**: `stream::sse` frames a completed answer and
every response carries `x-agentos-stream: buffered`. Incremental delivery needs
a streaming provider driver, which does not exist yet.

That's the whole protocol. Workers stay narrow; everything else lives in the engine.

## § 03 · Quickstart

```bash
# 1. clone this repository
git clone https://github.com/wunitb/unitb-iii-agentos && cd unitb-iii-agentos

# 2. install pinned platform-matched iii, worker, and console binaries
#    (iii-init is installed only on Linux; upstream macOS assets are Linux binaries)
bash scripts/install-iii.sh

# 3. configure a model credential
install -m 600 .env.example .env
$EDITOR .env   # set CODEX_PROXY_API_KEY for http://127.0.0.1:8317/v1

# 4. build the workspace
cargo build --workspace --release

# 5. bring the stack up: engine, workers, then the chat TUI
./target/release/agentos up
```

Step 3 is about the **model** credential only. You never write
`AGENTOS_API_KEY` by hand.

Quickstart uses the local Codex proxy at `http://127.0.0.1:8317/v1`, which is
just the credential that happens to be easiest to get. A request that names no
provider and no model is routed automatically: `llm-router` walks a fixed
preference order — `anthropic`, `openai`, `google`, `codex`, `groq`, `deepseek`,
`mistral`, `together`, `fireworks`, `openrouter` — and takes the **first
provider whose credential is present**. If none is present it refuses with
`provider_credential_missing`, naming the variables you could set, instead of
sending a request that would come back as somebody else's 401. Naming a provider
and model explicitly always wins over the automatic choice.

Every HTTP route is authenticated except routes explicitly registered with
`auth: false` such as `/api/health`. There is no global auth-disable switch.

### First run — what generates what

`AGENTOS_API_KEY` is AgentOS's own bearer token between the TUI/CLI and the
engine's HTTP routes. It is **not** a model credential, and the first run
creates it for you:

| you run | it does |
|---|---|
| `agentos up`, `agentos onboard`, `agentos start` | If `AGENTOS_API_KEY` is absent or empty in the active `.env`, generate a fresh 32-byte random key, write it into that `.env` — filling an existing empty `AGENTOS_API_KEY=` line **in place**, appending the line only when the name is absent — set the file to mode `0600`, and print the path it wrote to. An existing non-empty value is never overwritten. Writing in place matters: `.env.example` ships `AGENTOS_API_KEY=` already declared, so appending would produce two assignments of one name and `scripts/dev-up.sh` would then refuse to start with `duplicate dotenv variable`. |
| any command | **Never** invents a provider credential. `CODEX_PROXY_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` and friends are yours to supply; AgentOS only reads them. |
| `agentos doctor` | Reports, explicitly and separately: (a) whether `AGENTOS_API_KEY` is present, (b) which provider credential is present, (c) the default route that results — the first provider in the preference order above whose credential is present, or `provider_credential_missing` when none is. A missing `AGENTOS_API_KEY` is reported as *the cause*, not as "missing identities". |
| `agentos start` | Loads the same active `.env` as `agentos up` and generates the key the same way. There is exactly one configuration path. |

So the shortest honest first run is: install iii, put one model credential in
`.env`, `cargo build --workspace --release`, `agentos up`. The bearer token
appears in `.env` on its own, mode `0600`, and `agentos doctor` tells you which
of the three things above is missing when something does not work.

`agentos up` runs one ordered policy and never builds or installs anything: it
resolves the config, verifies the iii binary (missing → `bash scripts/install-iii.sh`),
reuses an engine already healthy on port 49134 or boots `iii --config config.yaml`
detached and waits for health with a bounded timeout, verifies every Rust worker
release binary (missing → `cargo build --workspace --release`), starts the workers
unless their canonical identities are already connected, waits for the complete
identity set to register on the bus, and only then hands the terminal to
`agentos-tui`. A partial stack starts only its missing workers. `up` loads the
active runtime's `.env` without overriding explicit shell exports and passes those
values to the engine, workers, and TUI; the TUI sends `AGENTOS_API_KEY` as a bearer
token on protected routes. Each stage reports its own failure and stops before the
next one; `--timeout` (default 30s) bounds the engine wait and then the worker wait.

```bash
agentos up --no-tui   # engine + workers only; leaves them running, no TUI
agentos doctor        # readiness report; diagnostic only, changes nothing
```

`agentos doctor` prints the iii binary path and version, engine health, the
connected worker count and any missing canonical identities, worker and TUI
binary readiness, which config discovery mode is in effect, and the three
first-run facts above: `AGENTOS_API_KEY` presence, the provider credential in
use, and the resulting default route.
`scripts/dev-up.sh` still starts only the workers against an engine you booted
yourself.

Engine boots on port 49134. `agentos up` starts the 62 Rust workers; the Python embedding worker is packaged separately and needs its Python `>=3.11` venv setup before it can connect. The source registers 301 literal function ids, which resolve to 301 distinct function ids (`bun run counts`). The TUI opens on Chat — type a message, hit Enter, the agent replies. `/help` shows the full keymap. `Ctrl+W` browses the worker catalog.

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

### Installed releases and portability

The full-stack release installer supports Linux `x86_64` and `aarch64`, and
macOS `aarch64`. Upstream iii `v0.22.1` does not publish the required
`iii-worker` runtime for macOS `x86_64`:

```bash
curl -fsSL https://raw.githubusercontent.com/wunitb/unitb-iii-agentos/main/scripts/install.sh | bash
agentos init --quick
agentos up
```

The installer needs network access, `curl`, `tar`, and `sha256sum` or `shasum`.
Every release also publishes an SPDX JSON SBOM and GitHub build-provenance
attestation for each native bundle; CI verifies both checksum and attestation
before publication.
It installs the CLI in `$HOME/.local/bin` by default and places the replaceable
runtime payload at `$HOME/.agentos/runtime`. Set `BIN_DIR` or `PREFIX` for the
CLI destination and set `AGENTOS_HOME` for the runtime/state root. A non-empty
relative `AGENTOS_HOME` is resolved against the directory from which `agentos`
was invoked, before any engine or worker changes directory; an empty override
uses the platform home plus `.agentos`.

`agentos init`, `agentos onboard`, `agentos doctor`, `agentos up`,
`agentos start`, `agentos reset`, and `agentos config ...` all use the same
resolved `AGENTOS_HOME`. `AGENTOS_CONFIG` has precedence over runtime
discovery. A non-empty relative value is resolved against the caller's current
directory; an empty value is ignored. Without it, a checkout `config.yaml` is
used only when the checkout also contains `workers/` — setting `AGENTOS_HOME`
alone does not disable that checkout discovery; otherwise
`$AGENTOS_HOME/runtime/config.yaml` is used. `agentos doctor` names the mode
and the resolved path. This lets an installed release
start from any working directory without a checkout. Upgrades replace release
payload while retaining operator configuration, `$AGENTOS_HOME/runtime/data/**`,
and the runtime `.env` file.

The engine and `iii-worker` runtime must match the stable version in
`.iii-version` (`v0.22.1`), installed in `PATH` or by
`bash scripts/install-iii.sh` (which downloads and verifies both binaries).
Installers reject prerelease pins unless a maintainer explicitly changes the
repository contract. The embedding
worker needs Python `>=3.11`, a working `venv` module, and `ensurepip`.
Its setup installs the core `iii-sdk` dependency without downloading the
optional `sentence-transformers`/`torch` model stack; absent those packages,
the worker deliberately uses its hash-based fallback. Install the optional
model dependencies separately when model-quality embeddings are required.

Rust format checks and cached Cargo builds/tests can run offline; Cargo still
needs the locked registry and source artifacts in its cache. Installing iii,
installing the release, installing Bun/npm/Python dependencies, and live
engine or chat E2E checks are connected or credential-dependent operations.
Local verification on one host does not prove the other two release targets;
the release workflow builds and inspects all three target bundles.

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
| State | `realm` `memory` `ledger` `vault` `context-manager` `context-cache` `context-monitor` |
| Coordination | `orchestrator` `workflow` `hierarchy` `coordination` `task-decomposer` |
| Execution | `wasm-sandbox` `browser` `code-agent` `hand-runner` `lsp-tools` |
| Safety | `security` `security-headers` `security-map` `security-zeroize` `skill-security` `approval` `approval-tiers` `rate-limiter` `loop-guard` |
| Surfaces | `a2a` `a2a-cards` `mcp-client` `skillkit-bridge` `bridge` `streaming` |
| Channels | `channel-{bluesky,discord,email,linkedin,mastodon,matrix,reddit,signal,slack,teams,telegram,twitch,webex,whatsapp}` |
| Telemetry | `telemetry` `pulse` `session-lifecycle` `session-replay` `feedback` `eval` `evolve` `hashline` `hooks` `cron` |
| Embeddings | `embedding` (Python) |

Each worker ships `iii.worker.yaml` declaring its registry shape. CI validates conformance on every PR.

The `workflow` worker auto-loads `workflows/*.yaml` at startup, validates step and agent references, executes dependency-ordered `sequential`, `parallel`, `fanout`, and bounded `loop` steps, and checkpoints run state after every step. Use `AGENTOS_WORKFLOWS_DIR` to override the bundled directory. The CLI exposes the complete lifecycle:

```bash
agentos workflow list
agentos workflow show feature-build
agentos workflow run feature-build --input '{"feature_description":"add caching"}'
agentos workflow runs feature-build --limit 20
agentos workflow status <run-id>
agentos workflow create workflows/feature-build.yaml
```

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
config/           committed values for ten iii worker configurations
website/         agentsos.sh — design.md aesthetic, three themes
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full primitive flow and worker manifest spec.

## § 09 · How changes reach this repository

Nothing in this repository launches an agent session of its own, and no work is merged that the repository's own gates have not passed.

1. **Intent** is recorded by the maintainer.
2. **Execution** is fanned out to parallel git worktrees — one branch per work package, with a written file-ownership contract so two packages never edit the same file. A package that needs a change in someone else's file files a request instead of editing it. Each package runs the gates for the crates it touched before it reports.
3. **Integration** merges the packages into one branch and re-runs the complete gate set on the merged tree: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --workspace --release`, `bun run test:unit`, `bun run test:governance`, `bun run counts:check`, `bun run typecheck`. Per-package green is not evidence; the merged tree is.
4. **Delivery** is a pull request. `main` is protected, takes no direct push, and requires the CI checks to pass before a merge.

## § 10 · TUI

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

## § 11 · Build and test

`rust-toolchain.toml` pins the channel to `1.90`, so a plain `cargo` in this
checkout is the same compiler and the same linter CI runs. No `rustup run`
prefix is needed, and `cargo clippy` cannot be clean locally and red in CI.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked                                      # dev profile, same tests CI runs
cargo deny check                                                     # advisories + duplicates + licences + sources
bun run check                                                        # strict TS + unit + governance + counts + website build
python -m pytest workers/embedding/test_main.py -q
bun run test:e2e                                                     # live engine + workers; model credentials required
```

`bun run check` chains the four Node gates individually available as
`bun run typecheck`, `bun run test:unit` (tests of the software),
`bun run test:governance` (build-evidence and documentation contracts) and
`bun run counts:check` (every published number recomputed from the tree —
`bun run counts` prints them, `bun run counts:write` fixes the numeric ones).

The Rust commands are offline only when the Rust toolchain and all locked
registry/source artifacts are already cached. `uv run` may download pytest and
the Bun command requires an installed lockfile-matching dependency tree.
The E2E command is not offline: it needs a running engine/workers stack and
model credentials for chat assertions.

## § 12 · Versioning

| | version |
|---|---|
| iii version contract | `.iii-version` contains stable `0.22.1` |
| iii engine | installers consume `.iii-version` and verify upstream checksums |
| iii-sdk (Rust) | pinned at `=0.22.1`; contract test checks every manifest |
| iii-sdk (Node) | pinned at `0.22.1`; root package manager is Bun |
| iii-sdk (Python) | pinned at `0.22.1`; worker manifest and pyproject are checked |
| agentos | `0.1.0` — stable contract on iii v0.22.1 |

## § 13 · Provenance and license

This independent repository started from [`iii-experimental/agentos@caca2b4`](https://github.com/iii-experimental/agentos/commit/caca2b439ff62499f0d4a5af30c2601302238890) and was migrated to the `iii-hq/iii` v0.22.1 engine and SDK contracts. It is not a GitHub fork and carries its own history.

Apache-2.0. Same family as `iii-sdk` and the rest of the iii ecosystem.
