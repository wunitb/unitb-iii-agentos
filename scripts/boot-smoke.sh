#!/bin/sh
# Boot the release through the same entry point a user runs, then inspect the
# live iii registry. No function is invoked, so this gate needs no provider key.
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
ENGINE_PORT=49134
BOOT_TIMEOUT_SECONDS=${AGENTOS_BOOT_SMOKE_TIMEOUT:-120}
STAGE_TIMEOUT_SECONDS=${AGENTOS_BOOT_SMOKE_STAGE_TIMEOUT:-45}
REQUIRED_FUNCTION_IDS='
agentos::llm::complete
agentos::llm::route
agent::chat
memory::recall
context::build_prompt
cron::create
'

fail() {
  printf 'boot smoke: %s\n' "$*" >&2
  exit 1
}

for command in cp iii mktemp pgrep python3 timeout; do
  command -v "$command" >/dev/null 2>&1 || fail "required command not found: $command"
done
[ -x "$REPO_ROOT/target/release/agentos" ]   || fail "release binary not found: $REPO_ROOT/target/release/agentos"

scratch=$(mktemp -d "${TMPDIR:-/tmp}/agentos-boot-smoke.XXXXXX")
runtime="$scratch/runtime"
agentos_home="$scratch/home"
registry_file="$scratch/functions.json"
expected_workers_file="$scratch/expected-workers.txt"
mkdir -p "$runtime/target/release" "$agentos_home"

port_is_open() {
  python3 - "$ENGINE_PORT" <<'PY'
import socket
import sys

with socket.socket() as probe:
    probe.settimeout(0.2)
    sys.exit(0 if probe.connect_ex(("127.0.0.1", int(sys.argv[1]))) == 0 else 1)
PY
}

cleanup() {
  status=$?
  trap - 0 1 2 15

  # Every process launched by this gate has the scratch runtime or config path
  # in its argv. Limit teardown to that path; never use a repo-wide pkill.
  pids=$(pgrep -f "$scratch" 2>/dev/null || true)
  if [ -n "$pids" ]; then
    kill $pids 2>/dev/null || true
    attempts=0
    while [ "$attempts" -lt 5 ] && pgrep -f "$scratch" >/dev/null 2>&1; do
      sleep 1
      attempts=$((attempts + 1))
    done
    pids=$(pgrep -f "$scratch" 2>/dev/null || true)
    [ -z "$pids" ] || kill -9 $pids 2>/dev/null || true
  fi

  if port_is_open; then
    printf 'boot smoke: teardown left engine port %s occupied\n' "$ENGINE_PORT" >&2
    status=1
  fi
  rm -rf "$scratch"
  exit "$status"
}
trap cleanup 0
trap 'exit 129' 1
trap 'exit 130' 2
trap 'exit 143' 15

port_is_open && fail "engine port $ENGINE_PORT is already occupied before the smoke run"

# This is the portable runtime layout produced by release.yml and validated by
# the portable-bundle CI job. Copying it keeps every engine and worker write in
# /tmp instead of changing the checkout under test.
for path in .iii-version config.yaml iii.lock config agents hands identity integrations plugin workflows workers; do
  [ -e "$REPO_ROOT/$path" ] && cp -R "$REPO_ROOT/$path" "$runtime/"
done
for binary in "$REPO_ROOT/target/release/agentos" "$REPO_ROOT"/target/release/agentos-*; do
  [ -f "$binary" ] || continue
  cp "$binary" "$runtime/target/release/"
done
chmod +x "$runtime/target/release"/agentos "$runtime/target/release"/agentos-*

# WP-A makes locally launched identities `agentos-<worker-dir>`. Resolve the
# same binary fallback as crates/cli/src/main.rs:784-792 so agent-core's
# package binary (`agentos-core`) still counts as the `agentos-agent-core`
# identity expected on the bus.
: > "$expected_workers_file"
for worker_dir in "$runtime"/workers/*; do
  [ -d "$worker_dir" ] || continue
  worker_name=${worker_dir##*/}
  packaged_binary="$runtime/target/release/agentos-$worker_name"
  package_name=''
  if [ -f "$worker_dir/Cargo.toml" ]; then
    package_name=$(awk '/^\[package\]/{p=1;next} /^\[/{p=0} p&&/^name *=/{gsub(/^name *= *"|"$/,"",$0);print;exit}' "$worker_dir/Cargo.toml")
  fi
  if [ -x "$packaged_binary" ]     || { [ -n "$package_name" ] && [ -x "$runtime/target/release/$package_name" ]; }; then
    printf 'agentos-%s\n' "$worker_name" >> "$expected_workers_file"
  fi
done
LC_ALL=C sort -u -o "$expected_workers_file" "$expected_workers_file"
[ -s "$expected_workers_file" ] || fail "scratch runtime contains no release worker binaries"

export AGENTOS_HOME="$agentos_home"
export III_URL="ws://127.0.0.1:$ENGINE_PORT"
unset AGENTOS_CONFIG

cd "$runtime"
if ! timeout "$BOOT_TIMEOUT_SECONDS"   "$runtime/target/release/agentos" up --no-tui --timeout "$STAGE_TIMEOUT_SECONDS"; then
  printf 'boot smoke: agentos up failed; scratch logs follow\n' >&2
  for log in "$agentos_home"/logs/*.log; do
    [ -f "$log" ] || continue
    printf '%s\n' "=== $log ===" >&2
    tail -100 "$log" >&2 || true
  done
  exit 1
fi

if ! iii trigger engine::functions::list --json '{}' --timeout-ms 5000 > "$registry_file"; then
  fail "engine::functions::list failed after agentos up"
fi

# Registration sites and why they are product-critical:
# - workers/llm-router/src/main.rs:1652,1666 route and complete every chat turn.
# - workers/agent-core/src/main.rs:336 exposes the main agent::chat entry point.
# - workers/memory/src/main.rs:59 provides retrieval used while building a turn.
# - workers/context-manager/src/main.rs:522 builds the model prompt.
# - workers/cron/src/main.rs:628 creates scheduled AgentOS actions.
python3 - "$registry_file" "$expected_workers_file" $REQUIRED_FUNCTION_IDS <<'PY'
import json
import sys
from pathlib import Path

registry_path = Path(sys.argv[1])
expected_path = Path(sys.argv[2])
required_ids = sys.argv[3:]
text = registry_path.read_text()
try:
    registry = json.loads(text)
except json.JSONDecodeError:
    start, end = text.find("{"), text.rfind("}")
    if start < 0 or end < start:
        raise SystemExit("boot smoke: engine registry was not JSON")
    registry = json.loads(text[start : end + 1])
if "functions" not in registry and isinstance(registry.get("result"), dict):
    registry = registry["result"]
functions = registry.get("functions")
if not isinstance(functions, list):
    raise SystemExit("boot smoke: engine registry has no functions array")

registered_ids = {
    item.get("function_id")
    for item in functions
    if isinstance(item, dict) and isinstance(item.get("function_id"), str)
}
missing_ids = [function_id for function_id in required_ids if function_id not in registered_ids]
if missing_ids:
    raise SystemExit("boot smoke: missing function id(s): " + ", ".join(missing_ids))

expected_workers = set(expected_path.read_text().splitlines())
connected_workers = {
    item.get("worker_name")
    for item in functions
    if isinstance(item, dict)
    and isinstance(item.get("worker_name"), str)
    and item["worker_name"].startswith("agentos-")
}
if len(connected_workers) != len(expected_workers) or connected_workers != expected_workers:
    missing = sorted(expected_workers - connected_workers)
    unexpected = sorted(connected_workers - expected_workers)
    details = [
        f"expected {len(expected_workers)} connected AgentOS workers, got {len(connected_workers)}"
    ]
    if missing:
        details.append("missing identities: " + ", ".join(missing))
    if unexpected:
        details.append("unexpected identities: " + ", ".join(unexpected))
    raise SystemExit("boot smoke: " + "; ".join(details))

print(
    f"boot smoke: ok: {len(required_ids)} required functions and "
    f"{len(expected_workers)} AgentOS workers are registered"
)
PY
