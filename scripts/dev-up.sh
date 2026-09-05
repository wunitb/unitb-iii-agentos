#!/usr/bin/env bash
# Boot the AgentOS dev stack in one enforced order: bus-auth (when armed),
# iii-engine, then scoped release workers. Do not start iii by hand first.
#
# Usage:
#   bash scripts/dev-up.sh           # spawn all release workers in background
#   bash scripts/dev-up.sh --build   # cargo build --workspace --release first
#   bash scripts/dev-up.sh stop      # kill anything launched here
#
# Env:
#   III_URL                          (default: ws://localhost:49134)
#   CODEX_PROXY_API_KEY              local proxy credential (preferred)
#   ANTHROPIC_API_KEY                optional cloud credential

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PIDFILE="$ROOT/.agentos-dev.pids"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
if [[ "$TARGET_DIR" != /* ]]; then
    TARGET_DIR="$ROOT/$TARGET_DIR"
fi
RELEASE_DIR="$TARGET_DIR/release"

env_file="$ROOT/.env"
if [[ -e "$env_file" || -L "$env_file" ]]; then
    if [[ -L "$env_file" || ! -f "$env_file" ]]; then
        echo "error: $env_file must be a regular file owned by the current user with mode 600" >&2
        exit 1
    fi
    if [[ ! -f "$ROOT/.env.example" ]]; then
        echo "error: trusted dotenv allowlist is missing: $ROOT/.env.example" >&2
        exit 1
    fi

    current_uid="$(id -u)"
    if env_uid="$(stat -c '%u' "$env_file" 2>/dev/null)" &&
        env_mode="$(stat -c '%a' "$env_file" 2>/dev/null)"; then
        :
    elif env_uid="$(stat -f '%u' "$env_file" 2>/dev/null)" &&
        env_mode="$(stat -f '%Lp' "$env_file" 2>/dev/null)"; then
        :
    else
        echo "error: unable to inspect ownership and mode for $env_file" >&2
        exit 1
    fi
    if [[ "$env_uid" != "$current_uid" || "$env_mode" != "600" ]]; then
        echo "error: $env_file must be owned by the current user and have mode 600" >&2
        exit 1
    fi

    # The allowlist is derived, never hand-maintained: every name declared in
    # `.env.example` plus every env key declared by an integration manifest.
    # A credential the workers read is therefore accepted the moment it is
    # documented, and a name nobody declares is still refused.
    allowed_names=$'\n'
    allow_name() {
        local candidate="$1"
        [[ "$candidate" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || return 0
        case "$allowed_names" in
            *$'\n'"$candidate"$'\n'*) ;;
            *) allowed_names="${allowed_names}${candidate}"$'\n' ;;
        esac
    }

    while IFS= read -r example_line || [[ -n "$example_line" ]]; do
        case "$example_line" in
            ''|'#'*) continue ;;
            *=*) allow_name "${example_line%%=*}" ;;
            *) continue ;;
        esac
    done < "$ROOT/.env.example"

    # `integrations/*.toml` declares its own required env keys under
    # `[integration.env]`; workers/mcp-client passes them to the integration.
    if [[ -d "$ROOT/integrations" ]]; then
        for manifest in "$ROOT/integrations"/*.toml; do
            [[ -f "$manifest" ]] || continue
            in_env_section=0
            while IFS= read -r manifest_line || [[ -n "$manifest_line" ]]; do
                case "$manifest_line" in
                    '[integration.env]'*) in_env_section=1; continue ;;
                    '['*) in_env_section=0; continue ;;
                esac
                [[ $in_env_section -eq 1 ]] || continue
                case "$manifest_line" in
                    *=*) allow_name "$(printf '%s' "${manifest_line%%=*}" | tr -d '[:space:]')" ;;
                esac
            done < "$manifest"
        done
    fi

    seen_names=$'\n'
    line_number=0
    while IFS= read -r env_line || [[ -n "$env_line" ]]; do
        line_number=$((line_number + 1))
        case "$env_line" in
            ''|'#'*) continue ;;
            *=*) ;;
            *)
                echo "error: malformed dotenv entry on line $line_number" >&2
                exit 1
                ;;
        esac
        name="${env_line%%=*}"
        value="${env_line#*=}"
        value="${value#"${value%%[![:space:]]*}"}"
        value="${value%"${value##*[![:space:]]}"}"
        case "$value" in
            '"'*'"' | "'"*"'") value="${value:1:${#value}-2}" ;;
        esac
        if [[ ! "$name" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
            echo "error: invalid dotenv variable name on line $line_number" >&2
            exit 1
        fi
        case "$allowed_names" in
            *$'\n'"$name"$'\n'*) ;;
            *)
                echo "error: unknown dotenv variable '$name' on line $line_number" >&2
                exit 1
                ;;
        esac
        case "$seen_names" in
            *$'\n'"$name"$'\n'*)
                echo "error: duplicate dotenv variable '$name' on line $line_number" >&2
                exit 1
                ;;
        esac
        seen_names="${seen_names}${name}"$'\n'

        inherited_value="${!name:-}"
        if [[ -n "$value" ]]; then
            export "$name=$value"
        elif [[ -n "$inherited_value" ]]; then
            export "$name=$inherited_value"
        else
            unset "$name"
        fi
    done < "$env_file"
fi

export III_URL="${III_URL:-ws://localhost:49134}"

stop_workers() {
    if [[ ! -f "$PIDFILE" ]]; then
        echo "no PID file at $PIDFILE — nothing to stop"
        return 0
    fi
    while read -r pid; do
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
        fi
    done < "$PIDFILE"
    rm -f "$PIDFILE"
    echo "stopped."
}

if [[ "${1:-}" == "stop" ]]; then
    stop_workers
    exit 0
fi

if [[ "${1:-}" == "--build" ]]; then
    echo "▸ cargo build --workspace --release"
    (cd "$ROOT" && cargo build --workspace --release)
fi

if [[ ! -d "$RELEASE_DIR" ]]; then
    echo "no release binaries at $RELEASE_DIR"
    echo "  run: bash scripts/dev-up.sh --build"
    exit 1
fi

if [[ -z "${CODEX_PROXY_API_KEY:-}" &&
    ( -n "${AGENTOS_DEFAULT_PROVIDER:-}" || -n "${AGENTOS_DEFAULT_MODEL:-}" ) ]]; then
    echo "warning: configured default provider '${AGENTOS_DEFAULT_PROVIDER:-codex}' disabled because CODEX_PROXY_API_KEY is empty; unqualified requests can fall back to the Anthropic cloud API" >&2
fi

if [[ -z "${CODEX_PROXY_API_KEY:-}" && -z "${ANTHROPIC_API_KEY:-}" ]]; then
    echo "warning: no model provider credential is configured"
fi

spawned=0

CONFIG="$ROOT/config.yaml"
POLICY="$ROOT/workers/env.allowlist"
[[ -f "$CONFIG" ]] || { echo "error: missing engine config: $CONFIG" >&2; exit 1; }
[[ -f "$POLICY" ]] || { echo "error: missing worker env policy: $POLICY" >&2; exit 1; }

# Parse and validate the same strict policy as the Rust launcher. The format is
# deliberately only `worker=KEY,KEY`: no shell evaluation, quoting, or spaces.
rust_workers=$'\n'
for manifest in "$ROOT"/workers/*/iii.worker.yaml; do
    [[ -f "$manifest" ]] || continue
    if grep -Eq '^[[:space:]]*kind:[[:space:]]*python[[:space:]]*$|^runtime:[[:space:]]*python[[:space:]]*$' "$manifest"; then
        continue
    fi
    worker="$(basename "$(dirname "$manifest")")"
    [[ "$worker" =~ ^[a-z0-9]+(-[a-z0-9]+)*$ ]] || {
        echo "error: invalid discovered Rust worker name '$worker'" >&2
        exit 1
    }
    rust_workers="${rust_workers}${worker}"$'\n'
done

policy_workers=$'\n'
policy_line=0
while IFS= read -r declaration || [[ -n "$declaration" ]]; do
    policy_line=$((policy_line + 1))
    [[ -z "$declaration" || "$declaration" == \#* ]] && continue
    if [[ ! "$declaration" =~ ^([a-z0-9]+(-[a-z0-9]+)*)=([A-Z][A-Z0-9_]*(,[A-Z][A-Z0-9_]*)*)$ ]]; then
        echo "error: malformed worker env policy on line $policy_line" >&2
        exit 1
    fi
    worker="${declaration%%=*}"
    keys="${declaration#*=}"
    case "$rust_workers" in
        *$'\n'"$worker"$'\n'*) ;;
        *) echo "error: unknown worker '$worker' in env policy on line $policy_line" >&2; exit 1 ;;
    esac
    case "$policy_workers" in
        *$'\n'"$worker"$'\n'*) echo "error: duplicate worker '$worker' in env policy on line $policy_line" >&2; exit 1 ;;
    esac
    [[ "$keys" == III_URL,AGENTOS_API_KEY || "$keys" == III_URL,AGENTOS_API_KEY,* ]] || {
        echo "error: worker '$worker' must declare III_URL,AGENTOS_API_KEY first" >&2
        exit 1
    }
    seen_keys=$'\n'
    IFS=',' read -r -a key_list <<< "$keys"
    for key in "${key_list[@]}"; do
        case "$allowed_names" in
            *$'\n'"$key"$'\n'*) ;;
            *) echo "error: unknown env key '$key' for worker '$worker' on policy line $policy_line" >&2; exit 1 ;;
        esac
        case "$seen_keys" in
            *$'\n'"$key"$'\n'*) echo "error: duplicate env key '$key' for worker '$worker' on policy line $policy_line" >&2; exit 1 ;;
        esac
        seen_keys="${seen_keys}${key}"$'\n'
    done
    policy_workers="${policy_workers}${worker}"$'\n'
done < "$POLICY"

while IFS= read -r worker; do
    [[ -n "$worker" ]] || continue
    case "$policy_workers" in
        *$'\n'"$worker"$'\n'*) ;;
        *) echo "error: worker env policy has no declaration for '$worker'" >&2; exit 1 ;;
    esac
done <<< "$rust_workers"

disabled_workers=$'\n'
if [[ -n "${AGENTOS_DISABLED_WORKERS:-}" ]]; then
    if [[ ! "$AGENTOS_DISABLED_WORKERS" =~ ^[a-z0-9]+(-[a-z0-9]+)*(,[a-z0-9]+(-[a-z0-9]+)*)*$ ]]; then
        echo "error: invalid AGENTOS_DISABLED_WORKERS value '$AGENTOS_DISABLED_WORKERS'" >&2
        exit 1
    fi
    IFS=',' read -r -a disabled_list <<< "$AGENTOS_DISABLED_WORKERS"
    for worker in "${disabled_list[@]}"; do
        [[ "$worker" =~ ^[a-z0-9]+(-[a-z0-9]+)*$ ]] || {
            echo "error: invalid AGENTOS_DISABLED_WORKERS entry '$worker'" >&2; exit 1;
        }
        case "$rust_workers" in
            *$'\n'"$worker"$'\n'*) ;;
            *) echo "error: unknown AGENTOS_DISABLED_WORKERS worker '$worker'" >&2; exit 1 ;;
        esac
        case "$disabled_workers" in
            *$'\n'"$worker"$'\n'*) echo "error: duplicate AGENTOS_DISABLED_WORKERS worker '$worker'" >&2; exit 1 ;;
        esac
        disabled_workers="${disabled_workers}${worker}"$'\n'
    done
fi

engine_authority="${III_URL#*://}"
engine_authority="${engine_authority%%/*}"
ENGINE_HOST="${engine_authority%:*}"
ENGINE_PORT="${engine_authority##*:}"
if [[ -z "$ENGINE_HOST" || ! "$ENGINE_PORT" =~ ^[0-9]+$ ]]; then
    echo "error: III_URL must name a host:port WebSocket endpoint: $III_URL" >&2
    exit 1
fi
engine_listening() {
    (exec 3<>"/dev/tcp/$ENGINE_HOST/$ENGINE_PORT") 2>/dev/null
}
config_armed=0
grep -q 'auth_function_id' "$CONFIG" && config_armed=1

# A listener that predates this invocation has no trustworthy config identity.
# Never kill it, but never claim an armed config protects it either.
if engine_listening; then
    if [[ $config_armed -eq 1 ]]; then
        echo "error: an engine already listens on $ENGINE_HOST:$ENGINE_PORT, but live bus RBAC cannot be verified" >&2
        echo "       stop that stack yourself, then rerun scripts/dev-up.sh so bus-auth starts before the engine" >&2
        exit 1
    fi
    : > "$PIDFILE"
    echo "▸ reusing unarmed engine on $ENGINE_HOST:$ENGINE_PORT"
else
    : > "$PIDFILE"
    BUS_AUTH_BIN="$RELEASE_DIR/agentos-bus-authd"
    BUS_AUTH_ADDR="${AGENTOS_BUS_AUTH_ADDR:-127.0.0.1:49129}"
    bus_auth_listening() {
        local host="${BUS_AUTH_ADDR%:*}" port="${BUS_AUTH_ADDR##*:}"
        (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null
    }
    if [[ $config_armed -eq 1 ]]; then
        if bus_auth_listening; then
            echo "▸ bus-auth daemon already listening on $BUS_AUTH_ADDR"
        elif [[ -x "$BUS_AUTH_BIN" ]]; then
            "$BUS_AUTH_BIN" "--listen=$BUS_AUTH_ADDR" >> "$ROOT/.agentos-bus-authd.log" 2>&1 &
            echo $! >> "$PIDFILE"
            spawned=$((spawned + 1))
            for _ in {1..20}; do
                bus_auth_listening && break
                sleep 0.2
            done
            if ! bus_auth_listening; then
                echo "error: agentos-bus-authd did not listen on $BUS_AUTH_ADDR; see $ROOT/.agentos-bus-authd.log" >&2
                stop_workers >/dev/null
                exit 1
            fi
            echo "▸ bus-auth daemon listening on $BUS_AUTH_ADDR"
        else
            echo "error: $CONFIG arms bus RBAC but $BUS_AUTH_BIN is not built" >&2
            echo "       run: cargo build --workspace --release" >&2
            exit 1
        fi
    fi

    command -v iii >/dev/null 2>&1 || { echo "error: iii is not on PATH" >&2; stop_workers >/dev/null; exit 1; }
    IIIWORKER_DISABLE_BUILTIN_DAEMONS=1 iii --config "$CONFIG" >> "$ROOT/.agentos-engine.log" 2>&1 &
    echo $! >> "$PIDFILE"
    spawned=$((spawned + 1))
    for _ in {1..20}; do
        engine_listening && break
        sleep 0.2
    done
    if ! engine_listening; then
        echo "error: iii-engine did not listen on $ENGINE_HOST:$ENGINE_PORT; see $ROOT/.agentos-engine.log" >&2
        stop_workers >/dev/null
        exit 1
    fi
    echo "▸ iii-engine listening on $ENGINE_HOST:$ENGINE_PORT"
fi

# Each worker receives only a small process baseline plus its declared policy
# values. `env -i` makes parent-shell canaries and other workers' credentials
# absent by construction. A non-empty dotenv assignment already overwrote the
# shell above; an empty assignment preserved the shell value.
while IFS= read -r worker; do
    [[ -n "$worker" ]] || continue
    case "$disabled_workers" in *$'\n'"$worker"$'\n'*) continue ;; esac
    bin="$RELEASE_DIR/agentos-$worker"
    [[ -x "$bin" ]] || continue
    declaration="$(grep -E "^${worker}=" "$POLICY")"
    keys="${declaration#*=}"
    worker_env=(env -i)
    for key in PATH HOME USER LOGNAME SHELL TERM AGENTOS_HOME TMPDIR TMP TEMP LANG LANGUAGE SSL_CERT_FILE SSL_CERT_DIR NIX_SSL_CERT_FILE RUST_BACKTRACE RUST_LOG; do
        if [[ ${!key+x} ]]; then worker_env+=("$key=${!key}"); fi
    done
    while IFS= read -r key; do
        [[ "$key" == LC_* ]] || continue
        worker_env+=("$key=${!key}")
    done < <(compgen -e)
    IFS=',' read -r -a key_list <<< "$keys"
    for key in "${key_list[@]}"; do
        if [[ ${!key+x} ]]; then worker_env+=("$key=${!key}"); fi
    done
    worker_env+=("III_WORKER_NAME=agentos-$worker")
    "${worker_env[@]}" "$bin" >> "$ROOT/.agentos-${worker}.log" 2>&1 &
    echo $! >> "$PIDFILE"
    spawned=$((spawned + 1))
done <<< "$rust_workers"

# memworkr runs from an immutable version explicitly installed by
# scripts/memworkr-sync.sh. Never execute a development checkout directly.
#
# memworkr is an OPTIONAL capability: no AgentOS code path calls
# memory::assert/as_of/provenance today, so every failure below degrades this
# one process and leaves the rest of the stack running. Only an explicit sync
# makes it a participant, and only a matching digest makes it executable.
MEMWORKR_RUNTIME_ROOT="${MEMWORKR_RUNTIME_ROOT:-$ROOT/.agentos-runtime/memworkr}"
memworkr_current="$MEMWORKR_RUNTIME_ROOT/current"
memworkr_version=""
if [[ -f "$memworkr_current" ]]; then
    memworkr_version="$(tr -d '\r\n' < "$memworkr_current")"
fi
if [[ "$memworkr_version" =~ ^[0-9a-f]{40}$ ]]; then
    memworkr_dir="$MEMWORKR_RUNTIME_ROOT/versions/$memworkr_version"
    memworkr_bin="$memworkr_dir/memworkr"
else
    memworkr_dir=""
    memworkr_bin=""
fi

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        return 1
    fi
}

memworkr_degraded() {
    echo "warning: memworkr disabled: $1" >&2
    echo "         the rest of the stack keeps running; no AgentOS code path calls memory::assert/as_of/provenance" >&2
}

memworkr_started=0
if [[ -n "$memworkr_bin" && -x "$memworkr_bin" ]]; then
    memworkr_ok=1
    # The runtime root sits inside the shell worker's jail, so a confined write
    # could replace this binary. Refuse to execute anything that does not match
    # the digest recorded by scripts/memworkr-sync.sh at sync time.
    if [[ ! -f "$memworkr_dir/SHA256" ]]; then
        memworkr_degraded "no recorded digest in $memworkr_dir/SHA256; re-run scripts/memworkr-sync.sh sync"
        memworkr_ok=0
    elif ! memworkr_actual="$(sha256_of "$memworkr_bin")"; then
        memworkr_degraded "no sha256sum or shasum on PATH to verify $memworkr_bin"
        memworkr_ok=0
    else
        memworkr_expected="$(tr -d '\r\n' < "$memworkr_dir/SHA256")"
        if [[ "$memworkr_actual" != "$memworkr_expected" ]]; then
            memworkr_degraded "digest mismatch for $memworkr_bin (recorded $memworkr_expected, found $memworkr_actual)"
            memworkr_ok=0
        fi
    fi

    if [[ $memworkr_ok -eq 1 ]] && [[ "${MEMWORKR_PRODUCTION:-0}" == "1" ]] &&
       [[ "${MEMWORKR_REQUIRE_CALLER:-0}" != "1" || -z "${MEMWORKR_INSTANCE_ID:-}" ]]; then
        memworkr_degraded "production memworkr requires MEMWORKR_REQUIRE_CALLER=1 and MEMWORKR_INSTANCE_ID"
        memworkr_ok=0
    fi
    if [[ $memworkr_ok -eq 1 ]] &&
       { ! command -v iii >/dev/null 2>&1 || ! command -v jq >/dev/null 2>&1; }; then
        memworkr_degraded "iii CLI and jq are required for memworkr readiness checks"
        memworkr_ok=0
    fi

    if [[ $memworkr_ok -eq 1 ]]; then
        MEMWORKR_COMPAT='' \
        MEMWORKR_DB="${MEMWORKR_DB:-surrealkv://$ROOT/data/memworkr}" \
            "$memworkr_bin" >> "$ROOT/.agentos-memworkr.log" 2>&1 &
        memworkr_pid=$!
        echo $memworkr_pid >> "$PIDFILE"
        spawned=$((spawned + 1))
        memworkr_started=1

        # Attempts are fixed at 30 in normal use; the override is a test seam,
        # read from the process environment only (never from the dotenv gate).
        memworkr_attempts="${MEMWORKR_READY_ATTEMPTS:-30}"
        [[ "$memworkr_attempts" =~ ^[0-9]+$ ]] || memworkr_attempts=30
        memworkr_ready=0
        for ((memworkr_attempt = 0; memworkr_attempt < memworkr_attempts; memworkr_attempt++)); do
            if health="$(iii trigger memory::health --json '{}' 2>/dev/null)" &&
               jq -e --arg production "${MEMWORKR_PRODUCTION:-0}" \
                 '.status == "ok" and .schemaVersion == 6 and
                  ($production != "1" or
                   (.callerEnforced == true and .instanceClaimed == true))' \
                 >/dev/null <<< "$health"; then
                memworkr_ready=1
                break
            fi
            sleep 1
        done
        if [[ $memworkr_ready -ne 1 ]]; then
            memworkr_degraded "memworkr failed the memory::health readiness check; see $ROOT/.agentos-memworkr.log"
            kill "$memworkr_pid" 2>/dev/null || true
            spawned=$((spawned - 1))
            memworkr_started=0
        else
            echo "▸ memworkr $memworkr_version ready"
        fi
    fi
elif [[ -n "$memworkr_version" && -z "$memworkr_bin" ]]; then
    memworkr_degraded "current points at '$memworkr_version', which is not a 40-character commit"
elif [[ -n "$memworkr_bin" ]]; then
    memworkr_degraded "no executable at $memworkr_bin"
fi

if [[ $memworkr_started -eq 0 && -z "$memworkr_version" ]]; then
    echo "note: memworkr is not synced — memory::assert/as_of/provenance unavailable"
    echo "      run: bash scripts/memworkr-sync.sh sync /path/to/unitb-iii-memworkr"
fi

echo "▸ spawned $spawned workers · pids in $PIDFILE"
echo "  logs:  $ROOT/.agentos-*.log"
echo "  stop:  bash scripts/dev-up.sh stop"
