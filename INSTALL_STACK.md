# Install the complete UnitB stack

The deployed system uses three repositories together:

| Repository | Role |
|---|---|
| `wunitb/unitb-iii-agentos` | iii engine configuration, AgentOS workers, CLI, and TUI |
| `wunitb/unitb-iii-memworkr` | Durable tri-temporal fact memory registered on the same iii engine |
| `wunitb/Clawith` | Team web application and chat UI/backend |

AgentOS and memworkr share the iii engine at `ws://127.0.0.1:49134`. Clawith is started as its own web stack. Keep all three checkouts at approved commits and use the `wunitb` fork of Clawith because it contains the session-error fix.

## Prerequisites

- Linux `x86_64`/`aarch64` or macOS `aarch64`
- Git, curl, tar, Rust/Cargo, Python 3.11+, Node.js 20+
- Podman with `podman compose` (preferred) or Docker with Compose for Clawith
- `sha256sum` or `shasum`

## 1. Clone all repositories

```bash
mkdir -p "$HOME/unitb-stack"
cd "$HOME/unitb-stack"
git clone https://github.com/wunitb/unitb-iii-agentos.git
git clone https://github.com/wunitb/unitb-iii-memworkr.git
git clone https://github.com/wunitb/Clawith.git
```

Require clean, pinned source before installation:

```bash
git -C unitb-iii-agentos status --short
git -C unitb-iii-memworkr status --short
git -C Clawith status --short
```

## 2. Install and configure AgentOS

```bash
cd "$HOME/unitb-stack/unitb-iii-agentos"
bash scripts/install-iii.sh
install -m 600 .env.example .env
${EDITOR:-vi} .env
cargo build --workspace --release
```

Set the required model/API credentials in `.env`. `install-iii.sh` installs checksum-verified, platform-matched `iii`, `iii-worker`, and `iii-console` binaries. Linux also receives `iii-init`; macOS skips it because the upstream `iii-init-*-apple-darwin` assets are Linux ELF binaries and are not host-executable ([iii-hq/iii#2119](https://github.com/iii-hq/iii/issues/2119)). The installer verifies every installed binary's native format before accepting it. The iii engine listens on `127.0.0.1:49134`; AgentOS HTTP routes use port `3111`.

The tracked shell configuration confines `shell::fs::*`, `coder::*`, and command `cwd` values to `${III_COMPOSE_DIR:.}` (this repository checkout, with `.` as the direct-engine fallback) with `allow_unjailed: false`. Existing installations receive the same persisted value from `config/shell.yaml` after pulling this revision; do not replace it with a whole-host root.

## 3. Integrate and sync memworkr

AgentOS includes the memworkr runtime integration. Sync the clean memworkr commit into AgentOS-owned immutable runtime storage:

```bash
cd "$HOME/unitb-stack/unitb-iii-agentos"
bash scripts/memworkr-sync.sh sync ../unitb-iii-memworkr
```

Confirm that `scripts/memworkr-sync.sh status` prints the selected commit before continuing. Never run the binary directly from the memworkr development checkout.

Production configuration must use an absolute database path and a stable instance ID:

```bash
cat >> .env <<EOF
MEMWORKR_PRODUCTION=1
MEMWORKR_REQUIRE_CALLER=1
MEMWORKR_INSTANCE_ID=unitb-production
MEMWORKR_DB=surrealkv://$HOME/unitb-stack/unitb-iii-agentos/data/memworkr
MEMWORKR_MAX_IN_FLIGHT=64
MEMWORKR_EXPENSIVE_MAX_IN_FLIGHT=2
MEMWORKR_MEMORY_SOFT_LIMIT_MIB=4096
III_WS_URL=ws://127.0.0.1:49134
EOF
```

Do not set `MEMWORKR_COMPAT=1` for the normal combined deployment. AgentOS remains authoritative for episodic `memory::store`/`memory::recall`; memworkr adds fact, provenance, candidate, and re-embedding functions.

## 4. Start AgentOS and memworkr

Start the iii engine in the first terminal:

```bash
cd "$HOME/unitb-stack/unitb-iii-agentos"
iii --config config.yaml
```

Start AgentOS workers and the synced memworkr runtime in a second terminal:

```bash
cd "$HOME/unitb-stack/unitb-iii-agentos"
bash scripts/dev-up.sh
```

`dev-up.sh` starts only immutable synced memworkr binaries and waits for schema-v6 `memory::health`.

Install the canonical iii Desktop chat graph and console worker against the running engine:

```bash
bash scripts/desktop-up.sh
```

This registry `console` worker serves the chat workspace on `http://127.0.0.1:3113` and is what `iii-desktop` renders. Do not start the standalone `iii-console` binary on port 3113: that binary is the developer operations console, redirects `/` to `/workers`, and has no Chat route.

Diagnose failures with:

```bash
bash scripts/memworkr-sync.sh status
./target/release/agentos doctor
iii trigger memory::health --json '{}'
iii worker status
```

Production memory calls must traverse the authenticated AgentOS/iii route; direct `iii trigger` mutation calls are development-only.

## 5. Configure and start Clawith

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

Configure database, Redis, public URL, model providers, and secrets in Clawith's `.env`. Preserve `backend/agent_data/` and the configured database during upgrades. Compose variants without a `minio` service now default the frontend's unused `MINIO_UPSTREAM` to `127.0.0.1:9000`, so Nginx starts without a manual IP override; deployments that provide MinIO should set the real service address.

Default endpoints:

- Clawith frontend: `http://127.0.0.1:3008`
- Clawith backend: `http://127.0.0.1:8008`
- Clawith health: `http://127.0.0.1:8008/api/health`

## 6. Verify the complete stack

```bash
curl -fsS http://127.0.0.1:3111/api/health
curl -fsS http://127.0.0.1:3113/
curl -fsS http://127.0.0.1:8008/api/health
iii trigger memory::health --json '{}'
COMPOSE_RUNTIME="${COMPOSE_RUNTIME:-podman}"
"$COMPOSE_RUNTIME" compose -f "$HOME/unitb-stack/Clawith/docker-compose.yml" ps
```

Then open `http://127.0.0.1:3008`, sign in, create/select an agent, and send a chat message. Do not accept `[object Object]` or `COULD NOT CREATE THE SESSION` as a successful smoke test.

## Updates

```bash
git -C "$HOME/unitb-stack/unitb-iii-agentos" pull --ff-only
git -C "$HOME/unitb-stack/unitb-iii-memworkr" pull --ff-only
git -C "$HOME/unitb-stack/Clawith" pull --ff-only

cd "$HOME/unitb-stack/unitb-iii-agentos"
bash scripts/memworkr-sync.sh sync ../unitb-iii-memworkr
cargo build --workspace --release

cd "$HOME/unitb-stack/Clawith"
COMPOSE_RUNTIME="${COMPOSE_RUNTIME:-podman}"
if [[ "$COMPOSE_RUNTIME" == "podman" ]]; then
  export CONTAINER_SOCKET="$(podman info --format '{{.Host.RemoteSocket.Path}}')"
fi
"$COMPOSE_RUNTIME" compose up -d --build
```

Back up AgentOS `data/memworkr`, Clawith's database, and `Clawith/backend/agent_data/` before production upgrades. See the memworkr `OPERATIONS.md` for schema migration, backup verification, rollback, and memory-pressure settings.
