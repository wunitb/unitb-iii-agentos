#!/usr/bin/env bash
# Boot the agentos dev stack: every release worker binary connects to the
# local iii engine on ws://localhost:49134. Run this in a second terminal
# after `iii --config config.yaml` is up.
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

: > "$PIDFILE"
spawned=0

# Bus RBAC gate. iii 0.22.1 calls the RBAC auth function for EVERY bus
# connection, so this daemon has to answer before a worker connects; it is not a
# worker itself and must not be started by the loop below. Started here as a
# best effort: in this flow the engine is already running, and its iii-bridge
# retries, so a late daemon costs the connections made in that window.
BUS_AUTH_BIN="$RELEASE_DIR/agentos-bus-authd"
BUS_AUTH_ADDR="${AGENTOS_BUS_AUTH_ADDR:-127.0.0.1:49129}"
bus_auth_listening() {
    local host="${BUS_AUTH_ADDR%:*}" port="${BUS_AUTH_ADDR##*:}"
    (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null
}
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
    if bus_auth_listening; then
        echo "▸ bus-auth daemon listening on $BUS_AUTH_ADDR"
    else
        echo "warning: agentos-bus-authd did not listen on $BUS_AUTH_ADDR; see $ROOT/.agentos-bus-authd.log" >&2
        echo "         it refuses to start without AGENTOS_API_KEY; with bus RBAC armed the engine refuses every worker" >&2
    fi
elif grep -q 'auth_function_id' "$ROOT/config.yaml" 2>/dev/null; then
    echo "warning: $ROOT/config.yaml arms bus RBAC but $BUS_AUTH_BIN is not built" >&2
    echo "         the engine will refuse every worker connection; run: cargo build --workspace --release" >&2
fi

for bin in "$RELEASE_DIR"/agentos-*; do
    name="$(basename "$bin")"
    case "$name" in
        agentos-tui|agentos-cli|agentos-bus-authd|*.d|*.dSYM) continue ;;
    esac
    [[ -x "$bin" ]] || continue
    "$bin" >> "$ROOT/.agentos-${name#agentos-}.log" 2>&1 &
    echo $! >> "$PIDFILE"
    spawned=$((spawned + 1))
done

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
