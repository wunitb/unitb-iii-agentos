#!/usr/bin/env bash
set -euo pipefail

ROOT=$(git -C "$(dirname "${BASH_SOURCE[0]}")/.." rev-parse --show-toplevel)
USER_SYSTEMD_DIR=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
SYSTEM_WRAPPER_DIR=/usr/local/libexec/unitb-fleet
APPARMOR_PROFILE=/etc/apparmor.d/unitb-fleet-bwrap

for command in bun herdr omp bwrap pasta nft socat sqlite3 git systemctl apparmor_parser; do
  command -v "$command" >/dev/null || {
    printf 'Missing required command: %s\n' "$command" >&2
    exit 1
  }
done

if [[ "$ROOT" == *'|'* || "$ROOT" == *$'\n'* ]]; then
  printf 'Repository path contains an unsupported character: %s\n' "$ROOT" >&2
  exit 1
fi

sudo -v
sudo install -d -o root -g root -m 0755 "$SYSTEM_WRAPPER_DIR"
sudo install -o root -g root -m 0755 "$(command -v bwrap)" "$SYSTEM_WRAPPER_DIR/bwrap"
sudo install -o root -g root -m 0755 "$(command -v pasta)" "$SYSTEM_WRAPPER_DIR/pasta"
sudo rm -f "$SYSTEM_WRAPPER_DIR/slirp4netns"
sudo install -o root -g root -m 0644 "$ROOT/orchestration/apparmor/unitb-fleet-bwrap" "$APPARMOR_PROFILE"
sudo apparmor_parser -r "$APPARMOR_PROFILE"

mkdir -p "$USER_SYSTEMD_DIR"
omp auth-broker token >/dev/null
for unit in unitb-omp-auth-broker.service unitb-herdr-supervisor.service unitb-fleet-dispatcher.service unitb-fleet-main.service; do
  sed "s|@REPO_ROOT@|$ROOT|g" "$ROOT/orchestration/systemd/$unit" > "$USER_SYSTEMD_DIR/$unit"
done

systemctl --user daemon-reload
systemctl --user enable unitb-omp-auth-broker.service unitb-herdr-supervisor.service unitb-fleet-dispatcher.service unitb-fleet-main.service
systemctl --user restart unitb-omp-auth-broker.service unitb-herdr-supervisor.service unitb-fleet-dispatcher.service
systemctl --user restart unitb-fleet-main.service
dispatcher_health=$(bun "$ROOT/orchestration/dispatcher.ts" health)
if ! bun --eval '
const health = JSON.parse(await new Response(Bun.stdin.stream()).text());
if (health.ok !== true) process.exit(1);
' <<<"$dispatcher_health" >/dev/null; then
  printf 'Fleet dispatcher health check failed: %s\n' "$dispatcher_health" >&2
  exit 1
fi
printf 'UnitB fleet installation completed.\n'
