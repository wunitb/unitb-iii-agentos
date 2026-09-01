#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME_ROOT="${MEMWORKR_RUNTIME_ROOT:-$ROOT/.agentos-runtime/memworkr}"
VERSIONS="$RUNTIME_ROOT/versions"
CURRENT="$RUNTIME_ROOT/current"

usage() {
    echo "usage: $0 sync SOURCE_DIR | rollback COMMIT_SHA | status | list" >&2
    exit 2
}

activate() {
    local version="$1"
    [[ "$version" =~ ^[0-9a-f]{40}$ ]] || { echo "invalid memworkr commit: $version" >&2; exit 1; }
    [[ -x "$VERSIONS/$version/memworkr" ]] || { echo "memworkr version is not installed: $version" >&2; exit 1; }
    mkdir -p "$RUNTIME_ROOT"
    printf '%s\n' "$version" > "$CURRENT.tmp"
    mv -f "$CURRENT.tmp" "$CURRENT"
    echo "memworkr active: $version"
    echo "restart required; run: bash scripts/dev-up.sh stop && bash scripts/dev-up.sh"
}

case "${1:-}" in
    sync)
        [[ $# -eq 2 ]] || usage
        SOURCE="$(cd "$2" && pwd)"
        [[ -f "$SOURCE/Cargo.lock" && -x "$SOURCE/scripts/release-gate.sh" ]] || { echo "not a memworkr checkout: $SOURCE" >&2; exit 1; }
        [[ -z "$(git -C "$SOURCE" status --porcelain)" ]] || { echo "refusing to sync a dirty memworkr checkout" >&2; exit 1; }
        VERSION="$(git -C "$SOURCE" rev-parse --verify HEAD)"
        [[ "$VERSION" =~ ^[0-9a-f]{40}$ ]] || { echo "unable to resolve memworkr commit" >&2; exit 1; }
        mkdir -p "$VERSIONS"
        if [[ ! -x "$VERSIONS/$VERSION/memworkr" ]]; then
            echo "▸ verifying memworkr $VERSION"
            (cd "$SOURCE" && scripts/release-gate.sh)
            STAGE="$(mktemp -d "$RUNTIME_ROOT/.sync.XXXXXX")"
            cleanup() { rm -rf "$STAGE"; }
            trap cleanup EXIT
            install -m 0755 "$SOURCE/target/release/memworkr" "$STAGE/memworkr"
            printf '%s\n' "$VERSION" > "$STAGE/VERSION"
            mv "$STAGE" "$VERSIONS/$VERSION"
            trap - EXIT
        fi
        activate "$VERSION"
        ;;
    rollback)
        [[ $# -eq 2 ]] || usage
        activate "$2"
        ;;
    status)
        [[ $# -eq 1 ]] || usage
        if [[ -f "$CURRENT" ]]; then
            VERSION="$(tr -d '\r\n' < "$CURRENT")"
            if [[ "$VERSION" =~ ^[0-9a-f]{40}$ && -x "$VERSIONS/$VERSION/memworkr" ]]; then
                echo "memworkr active: $VERSION"
                exit 0
            fi
        fi
        echo "memworkr is not installed" >&2
        exit 1
        ;;
    list)
        [[ $# -eq 1 ]] || usage
        found=0
        for dir in "$VERSIONS"/*; do
            [[ -d "$dir" ]] || continue
            basename "$dir"
            found=1
        done
        [[ $found -eq 1 ]] || echo "no memworkr versions installed"
        ;;
    *) usage ;;
esac
