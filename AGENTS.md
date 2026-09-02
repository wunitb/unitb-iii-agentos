# AgentOS contributor guidance

This file governs the repository root and all descendants unless a nearer `AGENTS.md` overrides it. `identity/AGENTS.md` governs work in `identity/`; `.upstream-iii/AGENTS.md`, when that ignored vendored checkout is materialized, governs only `.upstream-iii/`. Do not apply the upstream rules to AgentOS code.

## Project

AgentOS is UnitB's independent continuation of `iii-experimental/agentos`, migrated to the stable iii version pinned by `.iii-version` (**v0.22.1**) under Apache-2.0. It is a Rust workspace of narrow iii workers and three client/transport crates, plus the Python embedding worker, TypeScript examples and live-stack e2e harness, website, declarative content, and engine/runtime configuration. Workers register functions and triggers with the iii engine; they do not share in-process state.

## Repository map

- `workers/` — Rust worker binaries and `iii.worker.yaml` manifests; `workers/embedding/` is the Python worker.
- `crates/` — `cli` and `tui` user clients, plus the HTTP adapter boundary.
- `hands/` — TOML personas consumed by `hand-runner`; `agents/` — agent templates; `plugin/` — reusable agents, commands, skills, and hooks.
- `integrations/` — MCP connection TOML consumed by `mcp-client`; `workflows/` — workflow YAML consumed by `workflow`.
- `.iii-version` — canonical stable iii engine/SDK pin; `tests/iii_version.test.ts` rejects version drift and prerelease pins.
- `config.yaml` and `config/` — iii engine and built-in worker configuration.
- `tests/` — Rust integration tests; `examples/` and `scripts/*.test.ts` — local TypeScript tests; `e2e/` — Vitest tests against a live engine and workers.
- `scripts/` — installation and development-stack scripts; `website/` — independently locked Vite/React site.
- `identity/` — agent identity material, with its own guidance; `.upstream-iii/` — ignored vendored iii source when present.
- `.github/workflows/` — CI evidence for supported checks.

`target/`, root and website `node_modules/`, Python `__pycache__/`, `website/dist/`, `data/` (except committed `website/data/`), `.env*` (except `.env.example`), `.agentos-*.log`, and `.agentos-dev.pids` are generated, runtime, credential, or log state—not source. Cargo and Python lockfiles are generated; never hand-edit them. Keep changes minimal and path-scoped.

## Build and test

Run every command from the stated directory. A command below is offline only when its executable and all locked dependencies/artifacts are already installed or cached; `--offline` prevents fetching but does not create a cache.

`rust-toolchain.toml` pins the channel to **1.90**, so a plain `cargo` in this checkout is the compiler and
linter CI runs. Do not prefix commands with `rustup run`; if you change the pin, change it in
`rust-toolchain.toml`, `Cargo.toml` `rust-version`, `ci.yml` and `release.yml` together —
`tests/toolchain_pin.test.ts` fails the build otherwise.

| Purpose | Command | Offline/cache prerequisites and boundary |
| --- | --- | --- |
| Rust format check | root: `cargo fmt --all -- --check` | Rust **1.90** toolchain with `rustfmt`; no dependency download. |
| Rust lint | root: `cargo clippy --workspace --all-targets --locked -- -D warnings` | Rust 1.90 with `clippy` and the locked Cargo sources already available. This is the lint bar; CI runs exactly this command. |
| Rust metadata | root: `cargo metadata --offline --format-version 1 --locked` | Rust 1.90 plus Cargo registry/index and crate sources from `Cargo.lock` must already be cached. |
| Rust build | root: `cargo build --workspace --release --offline --locked` | Rust 1.90 and the same Cargo cache prerequisite; produces `target/` artifacts. |
| Rust tests | root: `cargo test --workspace --offline --locked` | Dev profile — the same profile CI runs; the release build exists for the shipped binaries, not for tests. Live-engine tests are ignored by default. Target one package with `cargo test -p <workspace-package> --offline --locked`; do not broaden unrelated packages. |
| Supply-chain policy | root: `cargo deny check` | **Not offline.** Needs `cargo-deny` (CI pins 0.20.2) and fetches the RustSec advisory database. Policy and every dated exception live in `deny.toml`; a new duplicate version, licence or registry fails the build. |
| Root TypeScript checks | root: `bun run check` | Bun and root `node_modules` must match `bun.lock`. Chains `typecheck`, `test:unit` (tests of the software), `test:governance` (build-evidence and documentation contracts), `counts:check` (every published number recomputed from the tree) and the npm-locked website build. `bun install --frozen-lockfile` is connected setup, not an offline guarantee. |
| Live TypeScript e2e | root: `bun run test:e2e` | **Not offline.** Requires installed root dependencies, `AGENTOS_E2E=1` (set by the script), a running iii engine and workers at `AGENTOS_BASE_URL`/`III_URL`, and model credentials for the chat assertion (`AGENTOS_API_KEY` or configured provider). The smoke name only selects health/chat tests; it still needs the live stack. |
| Website build | `website/`: `npm run build` | Node/npm and a **writable** `website/node_modules` installed from `website/package-lock.json`; produces `website/dist/`. `npm ci --no-audit --no-fund` is connected setup unless the complete npm cache is intentionally used offline. |
| Python embedding tests | `workers/embedding/`: `python -m pytest test_main.py -q` | Python >=3.11 and pytest already installed. CI installs only pytest, so the test's mocked iii and absent `sentence_transformers` use the fallback path. This is not an offline guarantee if a locally installed `sentence_transformers` tries to download its model; remove it or ensure that model is cached. |

From root, `bash scripts/install-iii.sh` reads `.iii-version`, rejects prerelease pins, and checksum-verifies the pinned iii v0.22.1 release; it is connected setup, never an offline command. From root, starting a development stack also needs a built release workspace, `iii --config config.yaml`, and usually `.env` credentials (mode 600) before `bash scripts/dev-up.sh`.

## Portability and release boundaries

Released bundles are built natively for Linux `x86_64`/`aarch64` and macOS
`aarch64` — the three targets `.github/workflows/release.yml` actually builds —
then checksum-inspected with an SPDX SBOM and verified
GitHub build-provenance attestation before publication. The bundle contains the CLI, TUI, runtime configuration,
Rust worker binaries, and embedding-worker source; it must not require the
source checkout at runtime. The installer keeps the operator-owned
`$AGENTOS_HOME/runtime/config`, `config.yaml`, `.env`, and
`runtime/data/**` paths across upgrades. Logs and transient process state stay
under `$AGENTOS_HOME` and are not source.

`AGENTOS_HOME` is the shared state root for init, onboarding, doctor, start,
reset, and config commands. A non-empty relative override is resolved against
the caller's current directory before a child process changes directory; an
empty override falls back to the platform home plus `.agentos`. A non-empty
relative `AGENTOS_CONFIG` is resolved the same way and takes precedence over
checkout or installed-runtime discovery. `scripts/dev-up.sh` resolves a
relative `CARGO_TARGET_DIR` against the repository root, so it is safe to
invoke from another directory.

The pinned iii installer and release installer are connected operations.
Offline Rust/Python checks require their toolchains and cached dependencies;
website npm, Bun, and live-engine/E2E checks have their own dependency,
network, service, or credential prerequisites. The embedding worker requires
Python >=3.11 with `venv` and `ensurepip`; SentenceTransformers is optional
for its hash fallback when the package is unavailable. Local builds validate
only the host architecture; the release CI matrix is the evidence for all
three supported targets.

## Conventions

- Rust uses edition 2024 and workspace dependency versions. Format with `cargo fmt`; treat the clippy command above, including `-D warnings`, as the lint bar — CI runs that exact command on the pinned 1.90 toolchain and there is no `continue-on-error` anywhere in `.github/workflows/`. Target a workspace package with `-p`; preserve the worker-per-process/function-registration design and the reserved `sandbox::*` namespace.
- Two workers may not register the same function id and two triggers may not bind the same HTTP route; `tests/registration_uniqueness.test.ts` enforces this against a dated allowlist that may only shrink.
- `state::update` takes `ops` (not `operations`), `increment` takes `by` (not `value`), and `merge` takes an object (appending an element is `append`); `tests/state_protocol.test.ts` scans every Rust source for these three.
- Every number this repository publishes — workers, function ids, tests, providers, CI jobs, engine workers, release targets and the worker tables — is generated by `scripts/counts.ts`. Change the tree, then run `bun run counts:write`; never hand-edit a published count.
- New dependencies must satisfy `deny.toml`. A new duplicate version needs a dated, justified `[bans] skip` entry, never a blanket allow.
- Root TypeScript is strict and NodeNext for examples and script tests; Bun is the root package manager and `bun.lock` is authoritative. The website is independently npm-locked and has its own strict TypeScript/Vite configuration.
- Python embedding code requires Python >=3.11; tests use pytest. Keep dependency changes lockfile-driven rather than editing generated locks.
- Every `workers/<name>/iii.worker.yaml` must name its containing directory, declare a `rust` or `python` runtime, and have a string `scripts.start`; CI validates this.
- Never edit generated, runtime, credential, vendor, or lock artifacts by hand. Do not change vendored `.upstream-iii/` while working on root AgentOS behavior unless that subtree's guidance and task explicitly require it.

## Delivery path

Repository changes enter as **control-room directives** and are built by the **sweafax** factory; nothing
in this repository launches an agent session of its own. `main` is protected and accepts pull requests only.
The managed UNITB OMPAX fleet was withdrawn on 2026-08-22 and no longer governs work here; if you find a
document that still describes `fleet_handoff`, `fleet_merge`, a managed Planner or a Dispatcher checkout
layout, that document is stale.
