#!/usr/bin/env bash
#
# Release smoke test: installs a release payload into a throwaway AgentOS home
# and drives a mock engine through the interactions a real release must support.
#
# The mock engine speaks a small request/response protocol over FIFOs so the
# smoke test observes real bidirectional interaction. Every required interaction
# records a receipt, and the run only passes when the full receipt set is
# present. A mock engine that merely exits -- with any status -- fails.
#
# Self-verification: SMOKE_FAULT injects a specific mock-engine defect so the
# negative path can be exercised on demand. Recognised values:
#   none             (default) mock engine honours the whole protocol
#   exit-early       engine exits 0 immediately, answering nothing
#   skip-health      engine starts and shuts down but never answers health
#   skip-state       engine never writes its runtime state
#   wrong-version    engine reports a version the release did not ship
#
# Usage:
#   bash scripts/release-smoke.sh
#   SMOKE_FAULT=exit-early bash scripts/release-smoke.sh   # must fail

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALLER="${INSTALLER:-$SCRIPT_DIR/install.sh}"
SMOKE_FAULT="${SMOKE_FAULT:-none}"
SMOKE_VERSION="${SMOKE_VERSION:-v9.9.9}"
SMOKE_TIMEOUT="${SMOKE_TIMEOUT:-10}"

# Interactions the release must complete. The run fails unless every one of
# these receipts exists at the end, regardless of how the engine exited.
REQUIRED_RECEIPTS=(
  installed-binary
  installed-runtime
  engine-started
  engine-read-config
  engine-wrote-state
  engine-answered-health
  engine-shutdown-clean
  runtime-state-preserved
)

WORK=""
ENGINE_PID=""
FAILURES=0

BOLD="\033[1m"
GREEN="\033[32m"
RED="\033[31m"
CYAN="\033[36m"
RESET="\033[0m"

info() { printf "${CYAN}>${RESET} %s\n" "$1"; }
ok() { printf "${GREEN}>${RESET} %s\n" "$1"; }
bad() {
  FAILURES=$((FAILURES + 1))
  printf "${RED}x${RESET} %s\n" "$1" >&2
}

cleanup() {
  if [ -n "$ENGINE_PID" ] && kill -0 "$ENGINE_PID" 2> /dev/null; then
    kill -KILL "$ENGINE_PID" 2> /dev/null
  fi
  [ -n "$WORK" ] && rm -rf "$WORK"
}
trap cleanup EXIT

receipt() { : > "$WORK/receipts/$1"; }

# --- fixture -----------------------------------------------------------------

setup_workspace() {
  WORK="$(mktemp -d)"

  HOME="$WORK/home"
  AGENTOS_HOME="$WORK/home/.agentos"
  BIN_DIR="$WORK/home/.local/bin"
  AGENTOS_TEST_FIXTURE_DIR="$WORK/fixture"
  AGENTOS_TEST_CURL_LOG="$WORK/curl.log"
  export HOME AGENTOS_HOME BIN_DIR AGENTOS_TEST_FIXTURE_DIR AGENTOS_TEST_CURL_LOG

  mkdir -p "$HOME" "$BIN_DIR" "$WORK/stub" "$WORK/fixture" "$WORK/receipts"
  : > "$AGENTOS_TEST_CURL_LOG"

  PATH="$WORK/stub:$BIN_DIR:$PATH"
  export PATH
}

write_offline_stubs() {
  cat > "$WORK/stub/curl" <<'STUB'
#!/usr/bin/env bash
# Offline stand-in for curl: serves the release fixture from disk.
set -uo pipefail

out=""
url=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o|-fsSLo) shift; out="${1:-}" ;;
    -*) : ;;
    *) url="$1" ;;
  esac
  shift
done

printf '%s\n' "$url" >> "$AGENTOS_TEST_CURL_LOG"

archive="$AGENTOS_TEST_FIXTURE_DIR/release.tar.gz"
name="${url##*/}"
[ -f "$archive" ] || exit 22

emit() {
  if [ -n "$out" ]; then
    cat > "$out"
  else
    cat
  fi
}

case "$url" in
  *.sha256)
    printf '%s  %s\n' "$(sha256sum "$archive" | cut -d ' ' -f 1)" "${name%.sha256}" | emit
    ;;
  *.tar.gz)
    emit < "$archive"
    ;;
  *)
    exit 22
    ;;
esac
STUB

  cat > "$WORK/stub/iii" <<'STUB'
#!/usr/bin/env bash
# The pinned engine is treated as already present; the smoke test drives the
# mock engine shipped inside the release payload instead.
if [ "${1:-}" = "--version" ]; then
  echo "iii 0.22.1"
fi
exit 0
STUB

  chmod +x "$WORK/stub/curl" "$WORK/stub/iii"
}

# The mock engine stands in for `iii` running against the installed runtime.
# It validates its own inputs, publishes state, then serves requests until told
# to shut down. Faults are injected only through SMOKE_FAULT.
write_mock_engine() {
  local dest="$1"

  cat > "$dest" <<'ENGINE'
#!/usr/bin/env bash
set -uo pipefail

config_path="${1:-}"
req="$SMOKE_REQ"
rep="$SMOKE_REP"
receipts="$SMOKE_RECEIPTS"
fault="${SMOKE_FAULT:-none}"
runtime_dir="$(cd "$(dirname "$config_path")" && pwd)"

receipt() { : > "$receipts/$1"; }

receipt engine-started

if [ "$fault" = "exit-early" ]; then
  exit 0
fi

[ -f "$config_path" ] || exit 64
release="$(cat "$runtime_dir/RELEASE")"
receipt engine-read-config

if [ "$fault" != "skip-state" ]; then
  mkdir -p "$runtime_dir/data"
  printf 'engine-state release=%s\n' "$release" > "$runtime_dir/data/engine.state"
  receipt engine-wrote-state
fi

if [ "$fault" = "wrong-version" ]; then
  release="0.0.0-not-shipped"
fi

: > "$receipts/engine-ready"

while IFS= read -r line < "$req"; do
  case "$line" in
    health)
      if [ "$fault" = "skip-health" ]; then
        printf 'ignored\n' > "$rep"
      else
        printf 'ok release=%s config=%s\n' "$release" "$config_path" > "$rep"
      fi
      ;;
    shutdown)
      printf 'bye\n' > "$rep"
      exit 0
      ;;
    *)
      printf 'unknown\n' > "$rep"
      ;;
  esac
done
ENGINE

  chmod +x "$dest"
}

build_release_fixture() {
  local tag="${SMOKE_VERSION#v}"
  local stage="$WORK/stage"

  mkdir -p "$stage/bin" "$stage/runtime/config" "$stage/runtime/workers/echo"

  printf '#!/bin/sh\necho "agentos %s"\n' "$tag" > "$stage/bin/agentos"
  printf '#!/bin/sh\necho "agentos-tui %s"\n' "$tag" > "$stage/bin/agentos-tui"
  chmod +x "$stage/bin/agentos" "$stage/bin/agentos-tui"

  printf 'release: "%s"\n' "$tag" > "$stage/runtime/config.yaml"
  printf 'release = "%s"\n' "$tag" > "$stage/runtime/config/default.toml"
  printf 'name: echo\nruntime: rust\n' > "$stage/runtime/workers/echo/iii.worker.yaml"
  printf '%s\n' "$tag" > "$stage/runtime/RELEASE"
  write_mock_engine "$stage/runtime/mock-engine"

  tar -czf "$WORK/fixture/release.tar.gz" -C "$stage" bin runtime
}

# --- checks ------------------------------------------------------------------

check_install() {
  info "Installing release ${SMOKE_VERSION} into ${AGENTOS_HOME}"

  if ! AGENTOS_VERSION="$SMOKE_VERSION" bash "$INSTALLER" > "$WORK/install.log" 2>&1; then
    bad "installer failed"
    sed 's/^/    | /' "$WORK/install.log" >&2
    return 1
  fi

  if [ -x "$BIN_DIR/agentos" ]; then
    receipt installed-binary
  else
    bad "release did not install an executable agentos binary"
  fi

  if [ -f "$AGENTOS_HOME/runtime/config.yaml" ] && [ -x "$AGENTOS_HOME/runtime/mock-engine" ]; then
    receipt installed-runtime
  else
    bad "release did not install a usable runtime"
  fi
}

# Seeds live engine state, reinstalls, and requires the state to survive. This
# is the release-level guard against an upgrade wiping runtime state.
check_upgrade_preserves_state() {
  local sentinel="$AGENTOS_HOME/runtime/data/smoke.sentinel"
  local payload='release-smoke-state-v1'
  local before after

  mkdir -p "$(dirname "$sentinel")"
  printf '%s' "$payload" > "$sentinel"
  before="$(sha256sum "$sentinel" | cut -d ' ' -f 1)"

  info "Re-installing over the existing runtime"
  if ! AGENTOS_VERSION="$SMOKE_VERSION" bash "$INSTALLER" > "$WORK/upgrade.log" 2>&1; then
    bad "upgrade install failed"
    sed 's/^/    | /' "$WORK/upgrade.log" >&2
    return 1
  fi

  if [ ! -f "$sentinel" ]; then
    bad "upgrade deleted runtime state: ${sentinel#"$AGENTOS_HOME"/}"
    return 1
  fi

  after="$(sha256sum "$sentinel" | cut -d ' ' -f 1)"
  if [ "$after" != "$before" ]; then
    bad "upgrade rewrote runtime state: ${sentinel#"$AGENTOS_HOME"/}"
    return 1
  fi

  if [ -e "$AGENTOS_HOME/runtime.new" ] || [ -e "$AGENTOS_HOME/runtime.old" ]; then
    bad "upgrade left staged runtime directories behind"
    return 1
  fi

  receipt runtime-state-preserved
}

start_engine() {
  local runtime="$AGENTOS_HOME/runtime"

  SMOKE_REQ="$WORK/engine.req"
  SMOKE_REP="$WORK/engine.rep"
  SMOKE_RECEIPTS="$WORK/receipts"
  export SMOKE_REQ SMOKE_REP SMOKE_RECEIPTS SMOKE_FAULT

  mkfifo "$SMOKE_REQ" "$SMOKE_REP"

  info "Starting mock engine against the installed runtime"
  (cd "$runtime" && exec ./mock-engine "$runtime/config.yaml") \
    > "$WORK/engine.log" 2>&1 &
  ENGINE_PID=$!
}

# Waits for the engine to publish a receipt, without depending on its exit.
wait_for_receipt() {
  local name="$1"
  local polls=0
  local max_polls=$((SMOKE_TIMEOUT * 5))

  while [ ! -e "$WORK/receipts/$name" ]; do
    if [ "$polls" -ge "$max_polls" ]; then
      return 1
    fi
    sleep 0.2
    polls=$((polls + 1))
  done
  return 0
}

# request <line> -> response on stdout, or empty on timeout/dead engine.
request() {
  local line="$1"
  local reply=""

  if ! kill -0 "$ENGINE_PID" 2> /dev/null; then
    return 1
  fi
  if ! timeout "$SMOKE_TIMEOUT" bash -c 'printf "%s\n" "$1" > "$2"' _ "$line" "$SMOKE_REQ"; then
    return 1
  fi
  if ! IFS= read -r -t "$SMOKE_TIMEOUT" reply < "$SMOKE_REP"; then
    return 1
  fi
  printf '%s\n' "$reply"
}

check_engine_interactions() {
  local tag="${SMOKE_VERSION#v}"
  local runtime="$AGENTOS_HOME/runtime"
  local expected reply status

  if ! wait_for_receipt engine-ready; then
    bad "mock engine never became ready"
    return 1
  fi

  expected="ok release=${tag} config=${runtime}/config.yaml"
  if ! reply="$(request health)"; then
    bad "mock engine did not answer the health request"
    return 1
  fi
  if [ "$reply" != "$expected" ]; then
    bad "unexpected health reply: [$reply] (expected [$expected])"
    return 1
  fi
  receipt engine-answered-health

  if ! reply="$(request shutdown)"; then
    bad "mock engine did not acknowledge shutdown"
    return 1
  fi
  if [ "$reply" != "bye" ]; then
    bad "unexpected shutdown reply: [$reply]"
    return 1
  fi

  wait "$ENGINE_PID"
  status=$?
  ENGINE_PID=""
  if [ "$status" -ne 0 ]; then
    bad "mock engine exited with status $status"
    return 1
  fi
  receipt engine-shutdown-clean
}

verify_receipts() {
  local name missing=()

  for name in "${REQUIRED_RECEIPTS[@]}"; do
    if [ ! -e "$WORK/receipts/$name" ]; then
      missing+=("$name")
    fi
  done

  if [ "${#missing[@]}" -gt 0 ]; then
    bad "missing required interactions: ${missing[*]}"
    return 1
  fi
  ok "all ${#REQUIRED_RECEIPTS[@]} required interactions completed"
}

main() {
  printf "\n${BOLD}  AgentOS release smoke test${RESET}\n\n"
  if [ "$SMOKE_FAULT" != "none" ]; then
    info "fault injection active: SMOKE_FAULT=${SMOKE_FAULT}"
  fi

  setup_workspace
  write_offline_stubs
  build_release_fixture

  check_install
  check_upgrade_preserves_state
  if [ -x "$AGENTOS_HOME/runtime/mock-engine" ]; then
    start_engine
    check_engine_interactions
  else
    bad "no engine to exercise"
  fi

  # A completed engine process proves nothing on its own; the receipt set does.
  verify_receipts

  printf "\n"
  if [ "$FAILURES" -eq 0 ]; then
    printf "${GREEN}${BOLD}  Release smoke test passed${RESET}\n\n"
    return 0
  fi
  printf "${RED}${BOLD}  Release smoke test failed (%d problem(s))${RESET}\n\n" "$FAILURES"
  return 1
}

main "$@"
