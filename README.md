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
  <img src="https://img.shields.io/badge/rust_tests-2161_total-0c0b0a?style=flat-square&labelColor=f2ede1" alt="2,161 Rust tests">
  <img src="https://img.shields.io/badge/iii--sdk-0.23.0-d96e2e?style=flat-square&labelColor=f2ede1" alt="iii-sdk 0.23.0">
</p>

<p align="center">
  <a href="https://www.agentsos.sh">website</a> ·
  <a href="ARCHITECTURE.md">architecture</a> ·
  <a href="INSTALL_STACK.md">complete stack install</a> ·
  <a href="SECURITY.md">security</a> ·
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

See [migration scope and verification boundaries](docs/III-023-MIGRATION.md).

The current source targets stable iii **v0.23.0**. Its supported startup path is
a non-root OCI container, using **Podman or Docker**. Install and start one of
those runtimes first; on macOS it must run Linux containers. The host needs Git,
Bash and Python 3.11+; Rust and the pinned iii binaries are built/installed inside
the image. First build needs network access and enough space for a Rust workspace.

```bash
git clone https://github.com/wunitb/unitb-iii-agentos.git
cd unitb-iii-agentos

# A separate persistent home; never point this at an existing native installation.
export AGENTOS_OCI_HOME="$HOME/.agentos-oci"
bash scripts/oci-stack.sh build
bash scripts/oci-stack.sh up
bash scripts/oci-stack.sh status
bash scripts/oci-stack.sh doctor
```

`up` is headless. It creates the private home with mode `0700`, generates distinct
`AGENTOS_API_KEY` and `AUDIT_HMAC_KEY` values, and keeps
`$AGENTOS_OCI_HOME/runtime/.env` mode `0600`. These machine keys are **not** model
credentials. Generated machine keys are independent 32-byte random values; existing
non-empty keys are never overwritten. Empty declarations are replaced in place,
not appended: duplicate assignments fail with `Duplicate dotenv variable`. Commands
print file paths and status, not generated values. Provider credentials are never
fabricated. Inside OCI, `agentos doctor` reports the default route and names a
missing bearer as the cause instead of only reporting missing identities.

A credential-free boot can be healthy while `doctor` still names
Provider, Route or Capabilities as missing; that is not ready-to-chat acceptance.

Configure your provider through the interactive setup, then restart the owned
container so worker environments pick up the change:

```bash
bash scripts/oci-stack.sh exec agentos onboard
bash scripts/oci-stack.sh stop
bash scripts/oci-stack.sh up
bash scripts/oci-stack.sh doctor
bash scripts/oci-stack.sh exec agentos agent new assistant
# Use the returned agent ID with: bash scripts/oci-stack.sh exec agentos agent chat AGENT_ID
bash scripts/oci-stack.sh exec agentos tui
```

Onboarding configures credentials/model preferences; it does not manufacture a
provider account. `agentos agent new assistant` creates an agent through the
authenticated API and initializes its canonical capability document. Use the
returned ID with `agentos agent chat AGENT_ID`, or select that agent in the TUI.
Grant only the capabilities needed before invoking tools. Never put credentials in
Git, shell arguments, screenshots or issue reports. For providers other than the
interactive Anthropic option, edit the private runtime `.env`, not checkout `.env`.
Non-empty active `.env` values take precedence over process exports.

The default provider is the first configured credential in this order:
`anthropic`, `openai`, `google`, `codex`, `groq`, `deepseek`, `mistral`, `together`,
`fireworks`, `openrouter`. Explicit provider/model selection takes precedence.
With no credential the router returns `provider_credential_missing`. Container
loopback is **not host loopback**: a host-only model proxy needs an explicitly
reachable endpoint; the quickstart never assumes a service on a fixed host port.

### Lifecycle, ports and persistent data

```bash
bash scripts/oci-stack.sh logs
bash scripts/oci-stack.sh status
bash scripts/oci-stack.sh stop
# Restart the same home without deleting configuration or data:
bash scripts/oci-stack.sh up
```

The launcher publishes only the API and authenticated bus on **host loopback**,
with dynamically assigned host ports reported by `status`. Do not assume host
ports `3111` or `49134`. The raw engine bus stays inside the container; host
`agentos up`/`agentos start` must not be used with this OCI-only config.
Within the container, the CLI coordinates policy registration, engine readiness,
Compose infrastructure, and authenticated product workers. Stop is tied to the
recorded immutable container identity, not a sweep of matching process names.

The private home retains operator `runtime/config.yaml`, `runtime/config/`,
`runtime/.env` and `runtime/data/` across stop/up. A home with a different engine
pin is refused rather than silently translated. `AGENTOS_OCI_RUNTIME` explicitly
selects a Podman/Docker executable; `AGENTOS_OCI_IMAGE` selects an image whose
engine label must match the checkout. Neither option bypasses the ownership or
loopback-only publication checks.

**Upgrade policy:** same-engine image rebuilds support `stop`, `build`, `up`
against the same home, preserving operator configuration and data. Engine-version
changes and native-to-OCI conversion are **not automatic data migrations**. Stop
the old deployment, retain its complete home and exact old image/release, and use
a new `AGENTOS_OCI_HOME` for the new engine. Configure the new home independently;
import application records only after validating the storage/schema contract.
Never change `.iii-version` in an old home to bypass the guard or copy a native
`config.yaml` into the OCI topology. Existing data stays in the original home;
no reset, deletion or in-place schema conversion is performed by the launcher.

`runtime/config.yaml`, `runtime/config/`, `runtime/.env`, `runtime/data/` and
home-level state remain operator-owned. Binaries, manifests, Compose declarations,
and bundled personas/integrations are image-managed and may be refreshed on
startup; keep custom material outside those managed paths.

Inside the container, engine boots on port 49134. Linux `agentos up` and foreground `agentos start` start the 62 Rust workers; the Python embedding worker is packaged separately and needs its Python `>=3.11` venv setup before it can connect. The source registers 301 literal function ids, which resolve to 301 distinct function ids (`bun run counts`). The TUI opens on Chat; `/help` shows the keymap and `Ctrl+W` browses the worker catalog.

HTTP routes are authenticated except explicit `auth: false` routes such as
`/api/health`; there is no global authentication-disable switch. Use the API
endpoint from `status` and the private bearer credential for protected routes.

### Archived native v0.2.0 releases (iii 0.22.1 only)

This subsection describes the already-published native release, **not** how to
run the migrated source checkout. Use the OCI quickstart above for iii v0.23.0.

The full-stack release installer supports Linux `x86_64` and `aarch64`, and
macOS `aarch64`. Upstream iii `v0.22.1` does not publish the required
`iii-worker` runtime for macOS `x86_64`. Only the Linux owned detached
`up`/`stop` lifecycle has runtime proof; building a macOS artifact is not proof
that detached lifecycle works there. macOS `aarch64` uses foreground `start`:

```bash
curl -fsSL https://raw.githubusercontent.com/wunitb/unitb-iii-agentos/main/scripts/install.sh | bash
agentos init --quick
agentos up      # Linux detached lifecycle
# macOS instead: agentos start; then run `agentos tui` in another terminal
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

For an archived native bundle, the engine and `iii-worker` runtime must match its stable version in
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
config.yaml      iii v0.23.0 native managers and private runtime topology
worker-compose.yaml  pinned registry infrastructure and scoped environments
config/           committed values for ten iii worker configurations
website/         agentsos.sh — design.md aesthetic, three themes
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full primitive flow and worker manifest spec.


### Security boundary

The default `config.yaml` arms tiered bus RBAC on the authenticated edge. It narrows
what a credential-less caller may invoke or register; generic registry/state
compatibility remains, and product workers share the operator `AGENTOS_API_KEY`.
The raw bootstrap bus is private container loopback and is never host-published.
This topology is not a hostile same-UID sandbox: use the non-root OCI launcher,
retain host-loopback publication, and trust the account that owns the runtime.

The process bridge is off by default. Enabling
`AGENTOS_ENABLE_PROCESS_BRIDGE=1` grants executable authority to trusted bearer
holders; executable basename checks and a cleared child environment are not an
inode or argument sandbox, and descendant processes can outlive direct-child
teardown. Integration catalog entries are trusted manifests and may invoke `npx`,
so their registry and transitive package supply chain must be reviewed. See
[`SECURITY.md`](SECURITY.md) for the supported threat model and reporting path.

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
bun run check                                                        # strict TS + unit + governance + script tests + counts + website build
python -m pytest workers/embedding -q                               # pytest and pinned iii-sdk required
bun run test:oci --report /tmp/agentos-oci-acceptance.json             # scratch OCI build + fake-provider acceptance
bun scripts/assert-oci-results.ts /tmp/agentos-oci-acceptance.json
bun run test:e2e                                                     # separately configured live stack; explicit provider authorization required
```

`bun run check` chains `typecheck`, `test:unit` (tests of the software),
`test:governance` (build-evidence and documentation contracts), `test:scripts`
(the Vitest script suite), `counts:check` and `build:website`.
Every published number is recomputed from the tree: `bun run counts` prints them,
`bun run counts:write` fixes the numeric ones.

The Rust commands are offline only when the Rust toolchain and all locked
registry/source artifacts are already cached. `uv run` may download pytest and
the Bun command requires an installed lockfile-matching dependency tree.
The credential-free OCI fixture proves one real chat turn and its provider request
shape against container loopback, plus registry, worker calls, restart preservation
and owned teardown. Artifact setup requires network access; the chat uses no real
provider. It does not prove multi-turn history, real provider accounts, billing,
rate limits, or production egress. The separate live E2E command needs a running
stack, credentials, egress, and explicit authorization.

## § 12 · Versioning

| | version |
|---|---|
| iii version contract | `.iii-version` contains stable `0.23.0` |
| iii engine | installers consume `.iii-version` and verify upstream checksums |
| iii-sdk (Rust) | pinned at `=0.23.0`; contract test checks every manifest |
| iii-sdk (Node) | pinned at `0.23.0`; root package manager is Bun |
| iii-sdk (Python) | pinned at `0.23.0`; worker manifest and pyproject are checked |
| agentos | `0.2.0` — source package version, not a new release tag |

The current source migrates to iii `v0.23.0` through OCI. The already-published
native AgentOS `v0.2.0` bundles still use iii `v0.22.1`; they are not migration
artifacts and are not retagged. Local checks validate only the host platform.
See [migration boundaries and checks](docs/III-023-MIGRATION.md).

## § 13 · Provenance and license

This independent repository started from [`iii-experimental/agentos@caca2b4`](https://github.com/iii-experimental/agentos/commit/caca2b439ff62499f0d4a5af30c2601302238890) and now targets the `iii-hq/iii` v0.23.0 engine and SDK contracts through OCI. It is not a GitHub fork and carries its own history.

Apache-2.0. Same family as `iii-sdk` and the rest of the iii ecosystem.
