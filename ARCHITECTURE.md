# UnitB AgentOS architecture

This codebase is an independent continuation of `iii-experimental/agentos`, migrated to the stable iii version pinned in `.iii-version` (v0.23.0). AgentOS is an agent operating system built on the [iii engine](https://github.com/iii-hq/iii). The repo ships **63 narrow workers** (62 Rust workers and one Python worker), declarative config (hands, integrations, agents), and two surfaces (`crates/cli`, `crates/tui`). Everything coordinates through iii primitives — `register_function`, `register_trigger`, `iii.trigger` — over the engine's WebSocket on port 49134.

## Runtime boundary

The migrated source runs in a non-root OCI container. A private loopback engine
manager handles policy bootstrap and Compose infrastructure; a second manager
retains the native deny-by-default authentication/registration hooks for product
workers and external clients. Only the authenticated edge and API are published
on host loopback, using runtime-assigned ports. Compose controls and the raw bus
are not host-facing. `worker-compose.yaml` pins infrastructure; the CLI owns
ordered readiness and shutdown. The container marker is an operator-configuration
guard, not a sandbox against hostile processes sharing the same user identity.

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
| Reasoning | `agent-core` `llm-router` `council` `swarm` `directive` `mission` | `agent::*` `agentos::llm::*` `council::*` `swarm::*` `directive::*` `mission::*` |
| State | `realm` `memory` `ledger` `vault` `context-manager` `context-cache` `context-monitor` | `realm::*` `memory::*` `ledger::*` `vault::*` `context::*` |
| Coordination | `orchestrator` `workflow` `hierarchy` `coordination` `task-decomposer` | `orchestrator::*` `workflow::*` `hierarchy::*` `task::*` |
| Execution | `wasm-sandbox` `browser` `code-agent` `hand-runner` `lsp-tools` | `wasm::*` `browser::*` `code::*` `hand::*` `lsp::*` |
| Safety | `security` `security-headers` `security-map` `security-zeroize` `skill-security` `approval` `approval-tiers` `rate-limiter` `loop-guard` | `security::*` `approval::*` `rate::*` `loop::*` |
| Surfaces | `a2a` `a2a-cards` `mcp-client` `skillkit-bridge` `bridge` `streaming` | `a2a::*` `mcp::*` `skillkit::*` `bridge::*` `stream::*` |
| Channels | `channel-{bluesky, discord, email, linkedin, mastodon, matrix, reddit, signal, slack, teams, telegram, twitch, webex, whatsapp}` | `channel::*` |
| Telemetry | `telemetry` `pulse` `session-lifecycle` `session-replay` `feedback` `eval` `evolve` `hashline` `hooks` `cron` | `telemetry::*` `pulse::*` `session::*` `eval::*` `feedback::*` |
| Embeddings | `embedding` (Python) | `embedding::*` |

The source registers 301 literal function ids across 63 workers (62 Rust + 1 Python), resolving to 301 distinct function ids. `bun run counts` recomputes every number on this page from the tree; `bun run counts:check` fails the build when a published number drifts.

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

`config.yaml` uses the `.iii-version` stable pin, currently iii v0.23.0. It declares four engine workers — the file-backed `configuration` store, the private `iii-worker-manager#raw`, the authenticated `iii-worker-manager` edge, and `iii-stream`.

`worker-compose.yaml` separately pins registry infrastructure, including HTTP,
pubsub, state, queue, cron, provider adapters and session/context services. Their
explicit configuration IDs read the committed records under `config/`, and their
working directories preserve relative data paths. The registry LLM/context
services are not the AgentOS Rust workers with similarly named source folders.
The legacy `iii.lock` belongs to the archived native installation path; it no
longer declares the migrated Compose topology.

Inside OCI, the CLI starts its owned engine, attaches the policy worker on the
private manager, verifies all native RBAC hooks, then starts Compose. Readiness
requires both primitive identities and their critical registered functions before
product workers are started with authenticated, scoped environments. There is no
legacy bridge in this configuration and no raw host listener. The mandatory bare
manager name is retained so upstream does not inject an extra ungated default.

The host launcher publishes only the API and gated bus on loopback using dynamic
ports; `scripts/oci-stack.sh status` reports their endpoints. The raw manager,
Compose controls and internal telemetry/stream services are not host-published.
An untrusted caller cannot invoke Compose lifecycle controls. Native host
`agentos up`/`agentos start` refuses this topology before modifying operator files.

Podman/Docker run the Linux runtime on supported hosts. Native macOS archive
behavior is historical release evidence, not proof for the new OCI installation.
The gate remains a shared-operator boundary, not protection from hostile code
sharing the operator identity. `shell`, `console` and `harness` are not booted
by default. Internal workers use the authenticated `III_URL`; external clients
use the dynamically published bus endpoint and an authorized bearer.

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

## Process and environment boundary

Rust workers start from a cleared environment, a safe process baseline, their exact
identity, and the keys listed for that worker in `workers/env.allowlist`. Non-empty
dotenv values override shell exports. `AGENTOS_API_KEY` is a shared operator key,
not a distinct worker identity or an OS boundary.

Direct `mcp::connect` no longer accepts a caller-selected process. Trusted
`integration::add` calls resolve a checked-in manifest, which can still run `npx`;
that makes package-registry resolution and transitive install behavior part of the
trusted supply chain. The process bridge is default-off. Its opt-in named-binary
check does not bind an inode or sandbox arguments, and timeout/cancellation cannot
guarantee teardown of descendants.

## Sandbox primitives — two surfaces

| namespace | worker | semantics |
|---|---|---|
| `sandbox::create` / `sandbox::exec` / `sandbox::list` / `sandbox::stop` | upstream iii-sandbox namespace | Reserved for upstream infrastructure; not enabled in the shipped OCI composition. |
| `wasm::execute` / `wasm::validate` / `wasm::list_modules` | agentos `wasm-sandbox` | wasmtime, fuel-metered, sub-millisecond cold start. |

CI's `no sandbox::* clash with builtin` job greps the workspace to ensure no agentos worker registers `sandbox::*`.

## Atomic state ops

The state and stream primitives expose `state::update` / `stream::update` with `set`, `increment`, `append` and `merge` operations. Workers prefer these over `state::list + state::set` race patterns when mutating lists or counters.

The wire shape is exact, and three of its fields are easy to get wrong. This contract was characterized on 2026-09-02 and is guarded by the state-protocol tests:

```jsonc
{
  "scope": "budgets",
  "key": "alice",
  "ops": [                                       // "ops", NOT "operations"
    { "type": "set",       "path": "status", "value": "active" },
    { "type": "increment", "path": "spend",  "by": 1 },   // "by", NOT "value"
    { "type": "append",    "path": "events", "value": { "at": 0 } },  // one ELEMENT
    { "type": "merge",     "path": "meta",   "value": { "seen": true } }  // OBJECT only
  ]
}
```

- `operations` instead of `ops` fails the whole invocation: `serialization error: missing field \`ops\``.
- `increment` with `value` instead of `by` fails the same way.
- `merge` with an array value returns HTTP 200 with a non-empty `errors` array — a **silent no-op**. Growing a list is `append` with the element as `value`, not `merge` with a one-element array.

Read shapes matter too: `state::list` returns a **bare array of values** — no key, no `{key, value}` envelope, so `entry["value"]` and `entry["key"]` read nothing — while `state::list_groups` returns `{"groups": [...]}`.

`tests/state_protocol.test.ts` scans every Rust source for the three write mistakes and fails the build on any of them.

`coordination::{post,reply}` reserves a slot through an atomic `state::update`
counter before persistence, reconciles pre-counter channel history on first use,
and decrements the reservation on quota rejection or write failure. This prevents
concurrent callers from crossing the per-channel post limit.

`council::activity` retains its manual hash-chain on `state::list + state::set` because ordering and previous-hash validation are the protocol itself; replacing it requires compare-and-swap semantics, not an unguarded append.

## The chat path — one pipeline, buffered transport

`agent::chat` is the only chat pipeline. `stream::chat`, `stream::completion` and `stream::sse` in the `streaming` worker delegate to it rather than running a second implementation of their own, so an HTTP caller gets the same tool loop, prompt-injection scan, memory recall and metering the TUI gets. There is no "streaming path" with different semantics.

The transport is **buffered, not token streaming**. `stream::sse` frames an answer that is already complete, and responses label themselves `x-agentos-stream: buffered`. Incremental delivery needs a streaming provider driver in `llm-router`, which does not exist yet; until it does, no document here should describe AgentOS as streaming tokens.

## Stream joins are gated, and the gate fails open

`workers/streaming` registers `stream::authorize_join` on the engine's `stream:join` trigger. It is deny-by-default: a join is authorized only when the handshake left an authenticated context on the connection — `authenticated` literally `true` plus a non-empty `subject`, which is what `security::stream_auth` returns for a valid AgentOS bearer. No context, a context that is not an object, or a missing subject is refused before the subscription is inserted.

Two limits, stated rather than papered over:

- **The legacy engine review found it fails open around the gate.** In iii 0.22.1, when authentication or join handlers fail, joins are **allowed**, not denied. OCI therefore keeps the stream listener private rather than treating the migration as evidence of a stronger stream-authentication guarantee.
- **So the loopback bind is load-bearing.** `config/iii-stream.yaml` sets `host: 127.0.0.1`; this gate is defence in depth behind that bind, not a replacement for it. Widening the bind re-exposes the socket to anything that can reach the host, fail-open and all.

## Surfaces (cli, tui)

`crates/cli` and `crates/tui` are clients, not workers. They speak HTTP to `iii-http` on port 3111. They register no functions. HTTP routes fail closed: protected routes require a non-empty `AGENTOS_API_KEY`; only triggers explicitly declaring `auth: false` bypass authentication, and there is no process-wide disable switch. Future work moves them onto the iii client SDK so they call workers via `iii.trigger` directly.

## Hands, integrations, agents, workflows

These are **declarative config**, not workers:

- `hands/<name>/HAND.toml` — agent persona (system prompt, allowed function ids, schedule), consumed by the `hand-runner` worker.
- `integrations/<name>.toml` — MCP server connection details (transport, command, OAuth scopes), consumed by the `mcp-client` worker.
- `agents/<name>/...` — markdown templates for spawning agent personas.
- `workflows/<name>.yaml` — pre-defined workflow definitions for the `workflow` worker.

None ship as registered functions; they configure workers that do.

`pulse` accepts 5- or 6-field UTC cron expressions; 5 fields are normalized to
6 with second `0`. It admits only the newest configured due slot within a
five-second late-delivery window, at most once per slot and without overlapping
runs. Older due slots are not caught up. A known handle can shift a run within
that window; it does not prove scheduler origin. Registrations are authorized
and held in memory. A worker restart requires the caller to register them again.
This is not durable or crash-exactly-once scheduling.

The `workflow` worker auto-loads every `.yaml`/`.yml` definition from `AGENTOS_WORKFLOWS_DIR` or the bundled `workflows/` directory. Loading rejects invalid IDs, duplicate or missing dependencies, undeclared agent references, unbounded timeout/retry/loop controls, and incompatible consecutive fanout policies. Execution resolves the dependency graph, checks the selected agent's capability before every function call, and supports `sequential`, concurrently joined `parallel`, grouped `fanout`, and bounded `loop` modes with `fail`, `skip`, or retry behavior.

Workflow definitions live under the `workflows` state scope. Runs live under `workflow_runs`; each checkpoint records `status`, `results`, interpolated `vars`, and `nextStep`, so HTTP and CLI clients can inspect the last durable step boundary through `GET /api/workflow-runs/:id`. Routes also expose workflow CRUD, `POST /api/workflows/:id/run`, and paginated run history at `GET /api/workflows/:id/runs`.

## Development control plane

Development is coordinated outside this repository. Work is split into packages that run in parallel git worktrees under a file-ownership contract, integrated into a single branch, and only then measured: the complete gate set runs on the *merged* tree, because per-package green is not evidence that the combination works. Delivery is a pull request opened by the maintainer; `main` accepts no direct pushes and requires its CI checks to pass. This repository holds no scheduler, ledger, credentials, or agent sessions of its own.

## Versioning

- iii engine: **v0.23.0**, pinned and checksum-verified by `scripts/install-iii.sh`
- iii-sdk (Rust): **=0.23.0** in workspace `Cargo.toml`
- iii-sdk (Node): **0.23.0** in root `package.json` (examples and E2E clients)
- iii-sdk (Python): **=0.23.0** in `workers/embedding/pyproject.toml`
- agentos workspace: **0.2.0**, inherited by every Rust crate from `[workspace.package]`

The source uses the OCI installation contract in `docs/III-023-MIGRATION.md`.
The published native AgentOS `v0.2.0` bundles remain historical iii `v0.22.1`
artifacts; this migration does not republish them. Cross-platform and real-provider
acceptance require their own evidence, not inference from a local fixture.

## CI

`.github/workflows/ci.yml` defines twelve jobs. Ten run on every event;
`dependency-review` runs on pull requests only and `e2e-full` only on a `main`
push with `AGENTOS_FULL_E2E_ENABLED`. The workflow starts from
`permissions: {}` and each job re-grants only `contents: read`. No step carries
`continue-on-error`: every gate below can fail a pull request.

| job | gate |
|---|---|
| `rust` | `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace` (dev profile, 2,178 test attributes; 3 live-engine checks ignored by default) + `cargo build --workspace --release` + `cargo audit` + `cargo deny check` (advisories, bans, licences, sources — policy in `deny.toml`) |
| `boot-smoke` | `scripts/boot-smoke.sh` builds the OCI image and runs scratch fixture acceptance: registry, product identities, access views, a deterministic chat/protocol, worker calls, restart preservation and owned teardown; no real provider credential |
| `node-unit` | `bun run typecheck`, `bun run test:unit` (tests of the software), `bun run test:governance` (build-evidence and documentation contracts), `bun run counts:check` (every published number recomputed from the tree) |
| `dependency-review` | `actions/dependency-review-action` with `fail-on-severity: moderate`, pull requests only |
| `portable-bundle` | stages the release payload from the `rust` artifacts and asserts the extracted bundle needs no checkout-relative path |
| `python` | installs pytest and the pinned iii SDK, then `python -m pytest workers/embedding -q`, including the real SDK contract tests |
| `website` | `npm ci` + `npm run build` in `website/` |
| `scripts` | `shellcheck --severity=warning` over the seven shipped shell scripts |
| `worker-yaml` | every `workers/<name>/iii.worker.yaml` parses, matches its folder, declares `runtime.kind` of `rust` or `python`, and carries a `scripts.start` string |
| `namespace-clash` | grep ensures no agentos worker registers `sandbox::*` |
| `e2e-smoke` | typechecks/tests Node examples and startup configuration, checks native OpenTelemetry ingestion, then reuses the OCI image built by `boot-smoke` for fixture acceptance and validates its exact JSON report with `scripts/assert-oci-results.ts` |
| `e2e-full` | `scripts/oci-smoke.py --live-e2e` runs the Vitest suite against an isolated OCI stack — needs `AGENTOS_API_KEY` and `ANTHROPIC_API_KEY` secrets and the `AGENTOS_FULL_E2E_ENABLED` variable |

The toolchain is pinned in one place, `rust-toolchain.toml` (`channel = "1.90"`),
which `ci.yml`, `release.yml` and `Cargo.toml`'s `rust-version` must all match;
`tests/toolchain_pin.test.ts` fails the build when they diverge, so a local
`cargo clippy` runs the same linter the gate runs.

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
