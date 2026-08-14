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
RELEASE_DIR="$ROOT/target/release"

if [[ -f "$ROOT/.env" ]]; then
    __agentos_anthropic_api_key="${ANTHROPIC_API_KEY:-}"
    __agentos_openai_api_key="${OPENAI_API_KEY:-}"
    __agentos_codex_proxy_api_key="${CODEX_PROXY_API_KEY:-}"
    set -a
    # shellcheck disable=SC1091
    source "$ROOT/.env"
    set +a
    if [[ -z "${ANTHROPIC_API_KEY:-}" && -n "$__agentos_anthropic_api_key" ]]; then
        export ANTHROPIC_API_KEY="$__agentos_anthropic_api_key"
    fi
    if [[ -z "${OPENAI_API_KEY:-}" && -n "$__agentos_openai_api_key" ]]; then
        export OPENAI_API_KEY="$__agentos_openai_api_key"
    fi
    if [[ -z "${CODEX_PROXY_API_KEY:-}" && -n "$__agentos_codex_proxy_api_key" ]]; then
        export CODEX_PROXY_API_KEY="$__agentos_codex_proxy_api_key"
    fi
    unset __agentos_anthropic_api_key __agentos_openai_api_key __agentos_codex_proxy_api_key
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

if [[ -z "${CODEX_PROXY_API_KEY:-}" && -z "${ANTHROPIC_API_KEY:-}" ]]; then
    echo "warning: no model provider credential is configured"
fi

> "$PIDFILE"
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

echo "▸ spawned $spawned workers · pids in $PIDFILE"
echo "  logs:  $ROOT/.agentos-*.log"
echo "  stop:  bash scripts/dev-up.sh stop"
