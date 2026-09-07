#!/usr/bin/env bash
# Compatibility command for the supported OCI development runtime.
# Never read checkout .env, adopt native ~/.agentos, or signal old PID files.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
launcher="$ROOT/scripts/oci-stack.sh"
if [[ ! -f "$launcher" ]]; then
    echo "error: OCI launcher missing; use a complete iii 0.23+ source checkout" >&2
    exit 1
fi

case "${1:-}" in
    '') set -- up ;;
    --build)
        [[ $# -eq 1 ]] || { echo 'error: --build takes no arguments' >&2; exit 2; }
        bash "$launcher" build
        set -- up ;;
    --stop) shift; set -- stop "$@" ;;
    build|up|stop|status|logs|doctor|exec|--help|-h) ;;
    *) echo "error: unknown argument: $1; use build|up|stop|status|logs|doctor|exec" >&2; exit 2 ;;
esac
exec bash "$launcher" "$@"
