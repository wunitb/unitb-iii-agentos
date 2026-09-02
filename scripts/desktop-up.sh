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

# The console worker is opt-in. iii console 1.9.16 has no host key: it binds
# 0.0.0.0 and proxies /ws to the bus, which has no authentication of its own, so
# the tracked config.yaml deliberately does not boot it. Say that immediately
# instead of installing artifacts and then polling a port for 60s that nothing
# will ever answer.
CONFIG_PATH="${AGENTOS_CONFIG:-$ROOT/config.yaml}"
if [[ -f "$CONFIG_PATH" ]] &&
    ! grep -Eq '^[[:space:]]*-[[:space:]]*name:[[:space:]]*console[[:space:]]*$' "$CONFIG_PATH"; then
    echo "iii desktop chat console is not enabled in $CONFIG_PATH" >&2
    echo "  the console worker binds 0.0.0.0:3113 and proxies /ws to the unauthenticated bus," >&2
    echo "  so it is opt-in. To accept that exposure, add this entry under 'workers:' in" >&2
    echo "  $CONFIG_PATH, block 3113 at the host firewall, then re-run this script:" >&2
    echo "" >&2
    echo "    - name: console" >&2
    echo "" >&2
    exit 1
fi

# `verify --strict` checks config.yaml and iii.lock agree for this platform.
# The install step must be plain `sync`: it installs the registry workers
# exactly as iii.lock pins them, and (unlike `iii worker update`) it does not
# rewrite config.yaml or iii.lock. `sync --frozen` only *verifies* the lockfile
# "without mutating local files" (iii 0.22.1 `worker sync --help`), so on a host
# without the console artifacts it can never install anything.
iii worker verify --strict
iii worker sync

for _ in {1..60}; do
    if [[ "$(probe_console)" == 200\ * ]]; then
        echo "iii desktop chat console ready at $CONSOLE_URL"
        exit 0
    fi
    sleep 1
done

echo "iii desktop chat console failed readiness at $CONSOLE_URL" >&2
exit 1
