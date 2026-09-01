#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONSOLE_URL="${III_CONSOLE_URL:-http://127.0.0.1:3113}"

command -v iii >/dev/null 2>&1 || {
    echo "iii is required; run bash scripts/install-iii.sh first" >&2
    exit 1
}
command -v curl >/dev/null 2>&1 || {
    echo "curl is required for console readiness checks" >&2
    exit 1
}

probe_console() {
    curl -sS --max-redirs 0 -o /dev/null -w '%{http_code} %{redirect_url}' \
        "$CONSOLE_URL/" 2>/dev/null || true
}

# The standalone iii-console binary redirects / to /workers. Fail before sync so
# that process cannot masquerade as the registry console worker's chat workspace.
initial_probe="$(probe_console)"
if [[ "$initial_probe" =~ ^30[1278]\ .*/workers/?$ ]]; then
    echo "standalone iii-console is occupying 3113; stop it before starting Desktop chat" >&2
    exit 1
fi

cd "$ROOT"
iii worker verify --strict
iii worker sync --frozen

for _ in {1..60}; do
    if [[ "$(probe_console)" == 200\ * ]]; then
        echo "iii desktop chat console ready at $CONSOLE_URL"
        exit 0
    fi
    sleep 1
done

echo "iii desktop chat console failed readiness at $CONSOLE_URL" >&2
exit 1
