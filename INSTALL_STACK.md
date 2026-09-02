# Install the UnitB stack

This guide covers two things, and it is explicit about which is which:

| Repository | Access | Needed for | Role |
|---|---|---|---|
| `wunitb/unitb-iii-agentos` | **public** | everything | iii engine configuration, AgentOS workers, CLI, and TUI |
| `wunitb/Clawith` | **public** | optional | team web application and chat UI/backend |
| `wunitb/unitb-iii-memworkr` | **private (UnitB only)** | optional | durable tri-temporal fact memory on the same iii engine |

**AgentOS runs without memworkr.** No AgentOS code path calls `memory::assert`, `memory::as_of` or
`memory::provenance` today (`rg 'memory::(assert|as_of|provenance)' workers crates` finds no call site), and
`scripts/dev-up.sh` treats memworkr as optional: absent, unverifiable, misconfigured or unhealthy, it prints
a warning and leaves the rest of the stack running. Sections 1, 2, 4 and 6 below are the complete public
path. Section 3 (memworkr) and section 5 (Clawith) are additions.

If you do not have access to `wunitb/unitb-iii-memworkr`, `git clone` fails with an authentication error.
That is expected: skip section 3 entirely.

## Prerequisites

- Linux `x86_64`/`aarch64` or macOS `aarch64`
- Git, curl, tar, Rust/Cargo, Python 3.11+, Node.js 20+
- `sha256sum` or `shasum` — the installer and `scripts/memworkr-sync.sh` verify digests with them
- `file` — `scripts/install-iii.sh:64` exits without it
- `jq` — only for the memworkr readiness check in `scripts/dev-up.sh`; without it memworkr is skipped
- Podman with `podman compose` (preferred) or Docker with Compose, for Clawith only
- For section 3 only: `cargo-audit` **exactly 0.22.2**, required by the memworkr release gate

## 1. Clone

```bash
mkdir -p "$HOME/unitb-stack"
cd "$HOME/unitb-stack"
git clone https://github.com/wunitb/unitb-iii-agentos.git
git clone https://github.com/wunitb/Clawith.git          # optional, public
git clone https://github.com/wunitb/unitb-iii-memworkr.git   # optional, PRIVATE
```

Use the `wunitb` fork of Clawith: it contains the session-error fix. Require clean, pinned source before
installation:

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

`.env.example` is the dotenv template and the allowlist `scripts/dev-up.sh` enforces: it declares every
variable the workers read, with empty values. Set at least one model credential
(`ANTHROPIC_API_KEY`, or `CODEX_PROXY_API_KEY` for a local OpenAI-compatible proxy).

Leave `AGENTOS_API_KEY` empty. `agentos up`, `agentos start` and `agentos onboard` generate a 32-byte key
into the active `.env` with mode 0600 on first run and never overwrite an existing value. Every protected
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

Either start the whole stack with the CLI:

```bash
cd "$HOME/unitb-stack/unitb-iii-agentos"
./target/release/agentos up            # engine, workers, TUI (add --no-tui to stay headless)
```

or drive the engine and the workers separately, which is what the memworkr path needs. First terminal:

```bash
cd "$HOME/unitb-stack/unitb-iii-agentos"
iii --config config.yaml
```

Second terminal:

```bash
cd "$HOME/unitb-stack/unitb-iii-agentos"
bash scripts/dev-up.sh
```

If `config.yaml` arms bus RBAC (`rbac.auth_function_id`), `agentos-bus-authd` must be listening **before**
the engine starts: iii 0.22.1 calls the auth function for every bus connection, so with the gate armed and
no daemon the engine refuses every worker. `agentos up` and `agentos start` start it first and stop it with
the stack. In the two-terminal flow above, start it before the engine:

```bash
./target/release/agentos-bus-authd --listen=127.0.0.1:49129 &   # must match iii-bridge url in config.yaml
```

`scripts/dev-up.sh` also starts it when it is not already listening; the engine's `iii-bridge` retries, so a
late daemon only costs the connections made in that window. It refuses to start without `AGENTOS_API_KEY`.

`dev-up.sh` starts every release worker binary, then starts memworkr only when a synced version is active
and its recorded digest matches, and waits for schema-v6 `memory::health`. Any memworkr problem degrades to
a warning; the AgentOS workers keep running.

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

## 5. Configure and start Clawith (optional)

```bash
cd "$HOME/unitb-stack/Clawith"
cp .env.example .env
${EDITOR:-vi} .env
touch ss-nodes.json

# Linux: systemctl --user enable --now podman.socket
# macOS: podman machine start
COMPOSE_RUNTIME="${COMPOSE_RUNTIME:-podman}"
if [[ "$COMPOSE_RUNTIME" == "podman" ]]; then
  export CONTAINER_SOCKET="$(podman info --format '{{.Host.RemoteSocket.Path}}')"
fi
"$COMPOSE_RUNTIME" compose up -d --build
```

Configure database, Redis, public URL, model providers, and secrets in Clawith's `.env`. Preserve
`backend/agent_data/` and the configured database during upgrades. Compose variants without a `minio`
service now default the frontend's unused `MINIO_UPSTREAM` to `127.0.0.1:9000`, so Nginx starts without a
manual IP override; deployments that provide MinIO should set the real service address.

Default endpoints:

- Clawith frontend: `http://127.0.0.1:3008`
- Clawith backend: `http://127.0.0.1:8008`
- Clawith health: `http://127.0.0.1:8008/api/health`

## 6. Verify

Public path:

```bash
curl -fsS http://127.0.0.1:3111/api/health
./target/release/agentos doctor
curl -fsS http://127.0.0.1:3113/    # only when you opted into the console worker
```

Additions, only when you installed them:

```bash
iii trigger memory::health --json '{}'                   # section 3
curl -fsS http://127.0.0.1:8008/api/health               # section 5
COMPOSE_RUNTIME="${COMPOSE_RUNTIME:-podman}"
"$COMPOSE_RUNTIME" compose -f "$HOME/unitb-stack/Clawith/docker-compose.yml" ps
```

With Clawith running, open `http://127.0.0.1:3008`, sign in, create/select an agent, and send a chat
message. Do not accept `[object Object]` or `COULD NOT CREATE THE SESSION` as a successful smoke test.

## Updates

```bash
git -C "$HOME/unitb-stack/unitb-iii-agentos" pull --ff-only
cd "$HOME/unitb-stack/unitb-iii-agentos"
cargo build --workspace --release

# optional additions
git -C "$HOME/unitb-stack/unitb-iii-memworkr" pull --ff-only
bash scripts/memworkr-sync.sh sync ../unitb-iii-memworkr

git -C "$HOME/unitb-stack/Clawith" pull --ff-only
cd "$HOME/unitb-stack/Clawith"
COMPOSE_RUNTIME="${COMPOSE_RUNTIME:-podman}"
if [[ "$COMPOSE_RUNTIME" == "podman" ]]; then
  export CONTAINER_SOCKET="$(podman info --format '{{.Host.RemoteSocket.Path}}')"
fi
"$COMPOSE_RUNTIME" compose up -d --build
```

Back up AgentOS `data/memworkr`, Clawith's database, and `Clawith/backend/agent_data/` before production
upgrades. See the memworkr `OPERATIONS.md` for schema migration, backup verification, rollback, and
memory-pressure settings.
