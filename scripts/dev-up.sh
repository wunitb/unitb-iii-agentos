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

    allowed_names=$'\n'
    while IFS= read -r example_line || [[ -n "$example_line" ]]; do
        case "$example_line" in
            ''|'#'*) continue ;;
            *=*) example_name="${example_line%%=*}" ;;
            *) continue ;;
        esac
        if [[ "$example_name" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
            case "$allowed_names" in
                *$'\n'"$example_name"$'\n'*) ;;
                *) allowed_names="${allowed_names}${example_name}"$'\n' ;;
            esac
        fi
    done < "$ROOT/.env.example"

    # Runtime-only memworkr settings are deliberately accepted without putting
    # production values in the distributable dotenv template.
    trusted_runtime_names=(
        III_WS_URL MEMWORKR_RUNTIME_ROOT MEMWORKR_PRODUCTION
        MEMWORKR_REQUIRE_CALLER MEMWORKR_INSTANCE_ID MEMWORKR_DB
        MEMWORKR_MAX_IN_FLIGHT MEMWORKR_EXPENSIVE_MAX_IN_FLIGHT
        MEMWORKR_MEMORY_SOFT_LIMIT_MIB MEMWORKR_REQUEST_TIMEOUT_MS
        MEMWORKR_SHUTDOWN_GRACE_MS MEMWORKR_CANDIDATE_RECONCILE_MAX
        MEMWORKR_AUTH_TRIGGER
    )
    for runtime_name in "${trusted_runtime_names[@]}"; do
        case "$allowed_names" in
            *$'\n'"$runtime_name"$'\n'*) ;;
            *) allowed_names="${allowed_names}${runtime_name}"$'\n' ;;
        esac
    done

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
for bin in "$RELEASE_DIR"/agentos-*; do
    name="$(basename "$bin")"
    case "$name" in
        agentos-tui|agentos-cli|*.d|*.dSYM) continue ;;
    esac
    [[ -x "$bin" ]] || continue
    "$bin" >> "$ROOT/.agentos-${name#agentos-}.log" 2>&1 &
    echo $! >> "$PIDFILE"
    spawned=$((spawned + 1))
done

# memworkr runs from an immutable version explicitly installed by
# scripts/memworkr-sync.sh. Never execute a development checkout directly.
MEMWORKR_RUNTIME_ROOT="${MEMWORKR_RUNTIME_ROOT:-$ROOT/.agentos-runtime/memworkr}"
memworkr_current="$MEMWORKR_RUNTIME_ROOT/current"
memworkr_version=""
if [[ -f "$memworkr_current" ]]; then
    memworkr_version="$(tr -d '\r\n' < "$memworkr_current")"
fi
if [[ "$memworkr_version" =~ ^[0-9a-f]{40}$ ]]; then
    memworkr_bin="$MEMWORKR_RUNTIME_ROOT/versions/$memworkr_version/memworkr"
else
    memworkr_bin=""
fi

if [[ -n "$memworkr_bin" && -x "$memworkr_bin" ]]; then
    if [[ "${MEMWORKR_PRODUCTION:-0}" == "1" ]] &&
       [[ "${MEMWORKR_REQUIRE_CALLER:-0}" != "1" || -z "${MEMWORKR_INSTANCE_ID:-}" ]]; then
        echo "production memworkr requires MEMWORKR_REQUIRE_CALLER=1 and MEMWORKR_INSTANCE_ID" >&2
        stop_workers
        exit 1
    fi
    if ! command -v iii >/dev/null 2>&1 || ! command -v jq >/dev/null 2>&1; then
        echo "iii CLI and jq are required for memworkr readiness checks" >&2
        stop_workers
        exit 1
    fi

    MEMWORKR_COMPAT='' \
    MEMWORKR_DB="${MEMWORKR_DB:-surrealkv://$ROOT/data/memworkr}" \
        "$memworkr_bin" >> "$ROOT/.agentos-memworkr.log" 2>&1 &
    echo $! >> "$PIDFILE"
    spawned=$((spawned + 1))

    memworkr_ready=0
    for _ in {1..30}; do
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
        echo "memworkr failed memory::health readiness check" >&2
        stop_workers
        exit 1
    fi
    echo "▸ memworkr $memworkr_version ready"
else
    echo "note: memworkr is not synced — memory::assert/as_of/provenance unavailable"
    echo "      run: bash scripts/memworkr-sync.sh sync /path/to/unitb-iii-memworkr"
fi

echo "▸ spawned $spawned workers · pids in $PIDFILE"
echo "  logs:  $ROOT/.agentos-*.log"
echo "  stop:  bash scripts/dev-up.sh stop"
