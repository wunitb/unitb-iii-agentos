# Install the UnitB stack

## Current source: iii v0.23.0 through OCI

Follow [README quickstart](README.md#-03--quickstart) for the current checkout.
Install a working Podman or Docker runtime, Git, Bash and Python 3.11+, then run
`bash scripts/oci-stack.sh build` followed by `bash scripts/oci-stack.sh up`.
Use a separate private `AGENTOS_OCI_HOME` (default `~/.agentos-oci`), not an
existing `~/.agentos` native home. `status` reports dynamically published
host-loopback API/bus endpoints; `doctor` reports credential and capability gaps.
Provider/agent setup is separate from credential-free boot. Container loopback
does not refer to services on the host. No raw engine endpoint is host-published.

The private memworkr integration and optional safety/channel features require
their own acceptance; a successful OCI boot does not certify them. Do not attach
an existing native memworkr database or copy real credentials into a smoke test.

## Archived native guide — v0.2.0 / iii 0.22.1 only

The remaining sections document the previously published native stack. They are
retained for existing operators and **must not be followed to start the current
OCI-only source on the host**. Native state is not automatically migrated.

This guide covers two repositories, and it is explicit about which is which:

| Repository | Access | Needed for | Role |
|---|---|---|---|
| `wunitb/unitb-iii-agentos` | **public** | everything | iii engine configuration, AgentOS workers, CLI, and TUI |
| `wunitb/unitb-iii-memworkr` | **private (UnitB only)** | optional | durable tri-temporal fact memory on the same iii engine |

**AgentOS runs without memworkr.** No AgentOS code path calls `memory::assert`, `memory::as_of` or
`memory::provenance` today (`rg 'memory::(assert|as_of|provenance)' workers crates` finds no call site), and
`scripts/dev-up.sh` treats memworkr as optional: absent, unverifiable, misconfigured or unhealthy, it prints
a warning and leaves the rest of the stack running. Sections 1, 2, 4 and 5 below are the complete public
path; section 3 is the only addition.

If you do not have access to `wunitb/unitb-iii-memworkr`, `git clone` fails with an authentication error.
That is expected: skip section 3 entirely.

## Prerequisites

- Linux `x86_64`/`aarch64` or macOS `aarch64` release artifacts. The owned detached `agentos up`/`agentos stop` lifecycle is Linux-only; macOS uses foreground `agentos start`.
- Git, curl, tar, Rust/Cargo, Python 3.11+, Node.js 20+
- `sha256sum` or `shasum` — the installer and `scripts/memworkr-sync.sh` verify digests with them
- `file` — `scripts/install-iii.sh:64` exits without it
- `jq` — only for the memworkr readiness check in `scripts/dev-up.sh`; without it memworkr is skipped
- For section 3 only: `cargo-audit` **exactly 0.22.2**, required by the memworkr release gate

## 1. Clone

```bash
mkdir -p "$HOME/unitb-stack"
cd "$HOME/unitb-stack"
git clone https://github.com/wunitb/unitb-iii-agentos.git
git clone https://github.com/wunitb/unitb-iii-memworkr.git   # optional, PRIVATE
```

Require clean, pinned source before installation:

```bash
git -C unitb-iii-agentos status --short
```

## 2. Install and configure AgentOS

```bash
cd "$HOME/unitb-stack/unitb-iii-agentos"
bash scripts/install-iii.sh
install -m 600 .env.example .env
${EDITOR:-vi} .env
cargo build --workspace --release
```

`.env.example` is the dotenv schema. `workers/env.allowlist` assigns the permitted
subset to each shipped Rust worker. Both guarded launchers clear inherited worker
environments, add a small safe baseline and exact identity, then add only that
worker's declared keys. Non-empty dotenv assignments override shell exports. Set at
least one model credential (`ANTHROPIC_API_KEY`, or `CODEX_PROXY_API_KEY` for a local
OpenAI-compatible proxy).

Leave `AGENTOS_API_KEY` and `AUDIT_HMAC_KEY` empty. Linux `agentos up`,
Linux/macOS `agentos start`, and `agentos onboard` generate distinct 32-byte keys into the active `.env` with
mode 0600 on first run and never overwrite existing values. `scripts/dev-up.sh`
consumes that initialized file; it does not invent keys. Every protected
HTTP route needs it (`crates/http-adapter/src/lib.rs`): without it almost every worker exits while
registering its routes.

`install-iii.sh` installs checksum-verified, platform-matched `iii`, `iii-worker` and `iii-console`
binaries. Linux also receives `iii-init`; macOS skips it because the upstream `iii-init-*-apple-darwin`
assets are Linux ELF binaries and are not host-executable
([iii-hq/iii#2119](https://github.com/iii-hq/iii/issues/2119)). The installer verifies every installed
binary's native format before accepting it. The iii engine listens on `127.0.0.1:49134`; AgentOS HTTP
routes use port `3111`.

The tracked shell configuration confines `shell::fs::*`, `coder::*`, and command `cwd` values to
`${III_COMPOSE_DIR:.}` (this repository checkout, with `.` as the direct-engine fallback) with
`allow_unjailed: false`. Existing installations receive the same persisted value from `config/shell.yaml`
after pulling this revision; do not replace it with a whole-host root.

## 3. Sync memworkr (optional, private repository)

Skip this section unless you have access to `wunitb/unitb-iii-memworkr`. Sync the clean memworkr commit
into AgentOS-owned immutable runtime storage:

```bash
cd "$HOME/unitb-stack/unitb-iii-agentos"
bash scripts/memworkr-sync.sh sync ../unitb-iii-memworkr
```

The sync runs the memworkr release gate, installs the built binary under a commit-named directory, and
records its sha256. `scripts/dev-up.sh` re-checks that digest before every start and refuses to execute a
binary that does not match. Confirm `scripts/memworkr-sync.sh status` prints the selected commit before
continuing; `status` also fails when the recorded digest no longer matches. Never run the binary directly
from the memworkr development checkout.

Production configuration must use an absolute database path and a stable instance ID. `.env.example`
already declares these names with empty values, so **edit the existing lines in `.env`** — appending a
second assignment makes `scripts/dev-up.sh` fail with `duplicate dotenv variable`:

```
MEMWORKR_PRODUCTION=1
MEMWORKR_REQUIRE_CALLER=1
MEMWORKR_INSTANCE_ID=unitb-production
MEMWORKR_DB=surrealkv:///home/you/unitb-stack/unitb-iii-agentos/data/memworkr
MEMWORKR_MAX_IN_FLIGHT=64
MEMWORKR_EXPENSIVE_MAX_IN_FLIGHT=2
MEMWORKR_MEMORY_SOFT_LIMIT_MIB=4096
III_WS_URL=ws://127.0.0.1:49134
```

`MEMWORKR_DB` must be an absolute path: the dotenv parser does not expand `$HOME`, and `dev-up.sh` keeps
shell syntax inert on purpose.

`.env.example` also declares four optional settings that AgentOS never reads and passes straight through to
the memworkr process — `MEMWORKR_REQUEST_TIMEOUT_MS`, `MEMWORKR_SHUTDOWN_GRACE_MS`,
`MEMWORKR_CANDIDATE_RECONCILE_MAX` and `MEMWORKR_AUTH_TRIGGER`. They are accepted by the dotenv gate;
memworkr's own `API.md`, `ISA.md` and `OPERATIONS.md` define what they do.

Do not set `MEMWORKR_COMPAT=1` for the normal combined deployment. AgentOS remains authoritative for
episodic `memory::store`/`memory::recall`; memworkr adds fact, provenance, candidate, and re-embedding
functions.

## 4. Start AgentOS

Use a guarded launcher. Do not start `iii` first and do not loop over
`target/release/agentos-*`: that bypasses authd ordering, engine verification, the
worker env policy, and the worker identity checks.

On Linux, use the owned detached lifecycle:

```bash
cd "$HOME/unitb-stack/unitb-iii-agentos"
./target/release/agentos up            # add --no-tui to stay headless
./target/release/agentos stop --grace-seconds 5  # later; range 0..60, default 5
```

On macOS `aarch64`, keep the stack in the foreground. Open the TUI in a second
terminal:

```bash
./target/release/agentos start
# another terminal
./target/release/agentos tui
```

For the Linux development or optional memworkr helper, initialize the keys once
through the CLI before using it:

```bash
./target/release/agentos onboard
bash scripts/dev-up.sh
```

The default `config.yaml` arms bus RBAC. The CLI generates missing machine keys;
these launch paths start `agentos-bus-authd` before an engine they start, force
builtin mutation daemons off, and apply `workers/env.allowlist`. If an engine is
already listening but its live gate cannot be verified, they refuse it rather than
silently reusing or killing an unrelated process.

`dev-up.sh` starts memworkr only when a synced version is active and its recorded
digest matches, then waits for schema-v6 `memory::health`. A memworkr problem
degrades to a warning; the AgentOS workers keep running.

Only the Linux owned detached lifecycle has runtime proof. The release workflow
building a macOS archive proves compilation and package shape, not detached process
ownership or stop behavior on macOS. This limit does not remove the macOS artifact
or the supported foreground `start` path.

### Desktop chat console (opt-in)

The tracked `config.yaml` does **not** boot the `console` worker. iii console 1.9.16 has no `host` key: it
binds `0.0.0.0:3113` and proxies `/ws` to the iii bus, which has no authentication of its own, so on a host
with a tailnet or LAN address that is a remotely reachable chat UI in front of the bus. Enable it only when
you accept that, and block 3113 at the host firewall:

```bash
# add under `workers:` in config.yaml
#   - name: console
bash scripts/desktop-up.sh
```

`desktop-up.sh` refuses immediately, naming the exposure, when the entry is absent — it does not install
artifacts and then poll a port nothing will answer. With the entry present it runs `iii worker verify
--strict` (config.yaml and iii.lock must agree for this platform) and then `iii worker sync`, which installs
the registry workers exactly as `iii.lock` pins them. It does not run `iii worker update`, so `iii.lock` and
`config.yaml` are not rewritten. The registry `console` worker serves the chat workspace on port 3113 and is
what `iii-desktop` renders. Do not start the standalone `iii-console` binary on that port: it is the
developer operations console, redirects `/` to `/workers`, and has no Chat route.

Diagnose failures with:

```bash
./target/release/agentos doctor            # API key, provider, default route, workers, capabilities
iii worker status console --no-watch       # <WORKER> is required; --no-watch prints once and exits
bash scripts/memworkr-sync.sh status       # only when section 3 was used
iii trigger memory::health --json '{}'     # only when section 3 was used
```

Production memory calls must traverse the authenticated AgentOS/iii route; direct `iii trigger` mutation
calls are development-only.

## 5. Verify

```bash
curl -fsS http://127.0.0.1:3111/api/health
./target/release/agentos doctor
curl -fsS http://127.0.0.1:3113/    # only when you opted into the console worker
iii trigger memory::health --json '{}'   # only when you installed section 3
```

`/api/health` is the only route registered with `auth: false`
(`workers/agent-core/src/main.rs:172`), so it answers without a bearer token; every other route needs
`AGENTOS_API_KEY`. `agentos doctor` is the real acceptance check: it reports the engine, the connected
worker identities, the machine key, which provider credential is present, the resulting default route, the
bus-auth daemon, and which agents have a capability document. A check it prints red is a stack that will
fail at runtime, whatever the health endpoint says.

Linux `agentos up` ends in the TUI by itself. With macOS foreground `agentos start`, open the TUI in the second terminal and send a message:

```bash
./target/release/agentos tui
```

## Upgrading an existing install

`bash scripts/install.sh` (or the `curl … | bash` one-liner) adopts your `config/`, `config.yaml`, `data/`
and `.env`, and then re-applies the parts of the configuration the release governs, so a security fix
reaches a box that was installed before it:

- `config/shell.yaml`, `config/iii-stream.yaml` and `config/console.yaml` are replaced with the release
  copies. An operator edit to those three files does not survive an upgrade, by design.
- `- name: shell`, `- name: harness` and `- name: console` are removed from your `config.yaml` if present,
  with their indented blocks. The installer prints what it removed and keeps your original at
  `config.yaml.bak`. Every other entry, including workers you added, is left untouched.
- `- name: iii-worker-manager` is added, or its `host` forced to `127.0.0.1`, keeping any other keys you set
  on it. Without that entry the engine appends the worker itself with a `0.0.0.0` bind, which exposes the
  bus to the LAN and the tailnet.
- Nothing is rewritten when your `config.yaml` already satisfies all of the above, and a `config.yaml` with
  no `workers:` roster is not touched at all.

Fresh release installs use the default-on `rbac:` and `iii-bridge` topology from
the release. An older operator-owned `config.yaml` is not silently rewritten with a
nested RBAC block during upgrade. Such an installation keeps its earlier unarmed
policy until the operator reviews and copies both blocks from the release
`config.yaml`. `agentos doctor` reports that state; do not claim an upgraded host is
armed until the live check says so.

## Security and compatibility limits

The bus is a loopback, single-operator boundary, not a hostile local-user sandbox.
The shared `AGENTOS_API_KEY` gives trusted processes operator authority, and generic
registry/state compatibility remains available below the sensitive-call denies.
The opt-in process bridge gives bearer holders executable authority without inode or
argument sandboxing; direct-child teardown does not guarantee descendant teardown.
Trusted integration manifests may execute `npx`, so review package-registry and
transitive dependency risk. See [`SECURITY.md`](SECURITY.md).

Credential-free tests use a local fake provider and do not prove real-provider
credentials, billing, limits, or egress. iii `v0.23` is the latest stable upstream
line, but AgentOS remains on `v0.22.1` until compatibility is validated across RBAC,
SDK wire shapes, registry assets, every supported platform, boot, and the full suite.

## Updates

```bash
git -C "$HOME/unitb-stack/unitb-iii-agentos" pull --ff-only
cd "$HOME/unitb-stack/unitb-iii-agentos"
cargo build --workspace --release

# optional addition
git -C "$HOME/unitb-stack/unitb-iii-memworkr" pull --ff-only
bash scripts/memworkr-sync.sh sync ../unitb-iii-memworkr
```

An upgrade from a release tarball is `bash scripts/install.sh` again; see "Upgrading an existing install"
above for what it governs and what it deliberately leaves alone.

Back up AgentOS `data/memworkr` before production upgrades. See the memworkr `OPERATIONS.md` for schema
migration, backup verification, rollback, and memory-pressure settings.
