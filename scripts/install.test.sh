#!/usr/bin/env bash
#
# Hermetic regression tests for scripts/install.sh.
#
# The installer under test is driven end-to-end with no network access: `curl`
# and `iii` are replaced by fixtures on PATH, and HOME/AGENTOS_HOME/BIN_DIR are
# redirected into a throwaway sandbox per test.
#
# Set INSTALLER=<path> to exercise a different copy of the installer. That is how
# the upgrade regressions below are proven red against a prior installer
# revision and green against the current one.
#
# Usage:
#   bash scripts/install.test.sh              # all tests
#   bash scripts/install.test.sh <name> ...   # named tests only

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
INSTALLER="${INSTALLER:-$SCRIPT_DIR/install.sh}"
ORIG_PATH="$PATH"
# The engine version the release bundle pins, taken from the same file the real
# bundle ships (release.yml copies .iii-version into runtime/).
III_PINNED_VERSION="$(tr -d '[:space:]' < "$REPO_ROOT/.iii-version")"

TESTS_RUN=0
TESTS_FAILED=0
TEST_FAILURES=0
SANDBOX=""
SANDBOXES=()

# Byte content of the runtime state that an upgrade must never touch.
SENTINEL_DB='agentos-sqlite-page-0;checksum=deterministic'
SENTINEL_SESSION='{"session":"s-1","turns":7}'
SENTINEL_ENV='ANTHROPIC_API_KEY=sk-do-not-lose-me'

cleanup() {
  local dir
  for dir in ${SANDBOXES+"${SANDBOXES[@]}"}; do
    [ -n "$dir" ] && rm -rf "$dir"
  done
}
trap cleanup EXIT

fail() {
  TEST_FAILURES=$((TEST_FAILURES + 1))
  printf '     x %s\n' "$1" >&2
}

sha256_of() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

# Recursive path+content fingerprint of a directory tree, for byte-for-byte
# comparison of preserved runtime state.
tree_manifest() {
  local root="$1"
  if [ ! -d "$root" ]; then
    printf '<missing:%s>\n' "$root"
    return
  fi
  (
    cd "$root" || exit 1
    find . -type f -print0 \
      | LC_ALL=C sort -z \
      | while IFS= read -r -d '' file; do
          printf '%s  %s\n' "$(sha256_of "$file")" "$file"
        done
  )
}

assert_file_content() {
  local path="$1" expected="$2" label="$3" actual
  if [ ! -f "$path" ]; then
    fail "$label: expected file is missing: $path"
    return
  fi
  actual="$(cat "$path")"
  if [ "$actual" != "$expected" ]; then
    fail "$label: content changed: $path"
  fi
}

# Substring form, for files where only one line is under test.
assert_file_contains() {
  local path="$1" expected="$2" label="$3"
  if [ ! -f "$path" ]; then
    fail "$label: expected file is missing: $path"
    return
  fi
  if ! grep -qF -e "$expected" "$path"; then
    fail "$label: $path does not contain: $expected"
  fi
}

assert_absent() {
  if [ -e "$1" ]; then
    fail "$2: expected path to be gone: $1"
  fi
}

assert_exists() {
  if [ ! -e "$1" ]; then
    fail "$2: expected path to exist: $1"
  fi
}

assert_equal() {
  if [ "$1" != "$2" ]; then
    fail "$3: expected [$2], got [$1]"
  fi
}

# --- sandbox -----------------------------------------------------------------

sandbox_new() {
  SANDBOX="$(mktemp -d)"
  SANDBOXES+=("$SANDBOX")

  HOME="$SANDBOX/home"
  AGENTOS_HOME="$SANDBOX/home/.agentos"
  BIN_DIR="$SANDBOX/home/.local/bin"
  AGENTOS_TEST_FIXTURE_DIR="$SANDBOX/fixture"
  AGENTOS_TEST_CURL_LOG="$SANDBOX/curl.log"
  export HOME AGENTOS_HOME BIN_DIR AGENTOS_TEST_FIXTURE_DIR AGENTOS_TEST_CURL_LOG

  mkdir -p "$HOME" "$BIN_DIR" "$SANDBOX/stub" "$AGENTOS_TEST_FIXTURE_DIR"
  : > "$AGENTOS_TEST_CURL_LOG"

  # BIN_DIR on PATH keeps ensure_path() from rewriting shell rc files.
  PATH="$SANDBOX/stub:$BIN_DIR:$ORIG_PATH"
  export PATH

  write_stubs
}

write_stubs() {
  cat > "$SANDBOX/stub/curl" <<'STUB'
#!/usr/bin/env bash
# Offline stand-in for curl: serves the release fixture from disk and refuses
# anything the installer is not supposed to ask for.
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

hash_of() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

emit() {
  if [ -n "$out" ]; then
    cat > "$out"
  else
    cat
  fi
}

archive="$AGENTOS_TEST_FIXTURE_DIR/release.tar.gz"
name="${url##*/}"

case "$url" in
  */releases/latest)
    printf '{"tag_name": "%s"}\n' "$(cat "$AGENTOS_TEST_FIXTURE_DIR/latest_tag")" | emit
    ;;
  *.sha256)
    [ -f "$archive" ] || exit 22
    printf '%s  %s\n' "$(hash_of "$archive")" "${name%.sha256}" | emit
    ;;
  *.tar.gz|*.zip)
    [ -f "$archive" ] || exit 22
    emit < "$archive"
    ;;
  *)
    exit 22
    ;;
esac
STUB

  cat > "$SANDBOX/stub/iii" <<'STUB'
#!/usr/bin/env bash
# Pinned engine already present, so install_iii() short-circuits offline.
if [ "${1:-}" = "--version" ]; then
  echo "iii 0.22.1"
fi
exit 0
STUB

  chmod +x "$SANDBOX/stub/curl" "$SANDBOX/stub/iii"
}

# make_release <version> [extra-relative-path ...]
# Builds the downloadable release fixture. Extra paths are stamped with the
# version so a later release can be shown to have replaced them.
make_release() {
  local version="$1"
  shift
  local stage="$SANDBOX/stage-$version"
  local tag="${version#v}"
  local extra

  rm -rf "$stage"
  mkdir -p "$stage/bin" "$stage/runtime/config" "$stage/runtime/workers/echo"

  printf '#!/bin/sh\necho "agentos %s"\n' "$tag" > "$stage/bin/agentos"
  printf '#!/bin/sh\necho "agentos-tui %s"\n' "$tag" > "$stage/bin/agentos-tui"
  chmod +x "$stage/bin/agentos" "$stage/bin/agentos-tui"

  printf 'release: "%s"\n' "$tag" > "$stage/runtime/config.yaml"
  printf 'release = "%s"\n' "$tag" > "$stage/runtime/config/default.toml"
  printf 'name: echo\nruntime: rust\n' > "$stage/runtime/workers/echo/iii.worker.yaml"
  printf '%s\n' "$tag" > "$stage/runtime/RELEASE"
  # The installed runtime pins the engine version; resolve_iii_version() refuses
  # to continue without it, so the release fixture must ship it like the real
  # bundle does (.iii-version at the repository root).
  printf '%s\n' "$III_PINNED_VERSION" > "$stage/runtime/.iii-version"

  for extra in "$@"; do
    mkdir -p "$stage/runtime/$(dirname "$extra")"
    printf 'payload from %s\n' "$tag" > "$stage/runtime/$extra"
  done

  printf '%s\n' "$version" > "$AGENTOS_TEST_FIXTURE_DIR/latest_tag"
  tar -czf "$AGENTOS_TEST_FIXTURE_DIR/release.tar.gz" -C "$stage" bin runtime
}

# run_installer [version]
# Runs the installer; empty version exercises the latest-release lookup path.
run_installer() {
  local version="${1:-}"
  local log="$SANDBOX/install-${version:-latest}.log"
  local status

  if [ -n "$version" ]; then
    AGENTOS_VERSION="$version" bash "$INSTALLER" > "$log" 2>&1
  else
    AGENTOS_VERSION="" bash "$INSTALLER" > "$log" 2>&1
  fi
  status=$?

  if [ "$status" -ne 0 ]; then
    fail "installer exited $status; log:"
    sed 's/^/       | /' "$log" >&2
  fi
  return "$status"
}

# Seeds the live runtime state that an upgrade must carry forward untouched.
seed_runtime_state() {
  mkdir -p "$AGENTOS_HOME/runtime/data/sessions/s-1"
  printf '%s' "$SENTINEL_DB" > "$AGENTOS_HOME/runtime/data/agentos.db"
  printf '%s' "$SENTINEL_SESSION" > "$AGENTOS_HOME/runtime/data/sessions/s-1/state.json"
}

# --- tests -------------------------------------------------------------------

test_fresh_install_places_binaries_and_runtime() {
  make_release v1.0.0
  run_installer v1.0.0 || return

  assert_exists "$BIN_DIR/agentos" fresh_install
  assert_exists "$BIN_DIR/agentos-tui" fresh_install
  [ -x "$BIN_DIR/agentos" ] || fail "fresh_install: agentos is not executable"
  assert_file_content "$AGENTOS_HOME/runtime/RELEASE" "1.0.0" fresh_install
  assert_exists "$AGENTOS_HOME/runtime/workers/echo/iii.worker.yaml" fresh_install
  assert_absent "$AGENTOS_HOME/runtime.new" fresh_install
  assert_absent "$AGENTOS_HOME/runtime.old" fresh_install
}

test_fresh_install_resolves_latest_release() {
  make_release v1.4.2
  run_installer "" || return

  assert_file_content "$AGENTOS_HOME/runtime/RELEASE" "1.4.2" latest_release
  if ! grep -q 'releases/latest' "$AGENTOS_TEST_CURL_LOG"; then
    fail "latest_release: installer never queried the latest-release endpoint"
  fi
}

test_upgrade_preserves_user_config() {
  make_release v1.0.0
  run_installer v1.0.0 || return

  printf 'user: true\n' > "$AGENTOS_HOME/runtime/config.yaml"
  printf 'user = true\n' > "$AGENTOS_HOME/runtime/config/local.toml"

  make_release v2.0.0
  run_installer v2.0.0 || return

  assert_file_content "$AGENTOS_HOME/runtime/config.yaml" "user: true" upgrade_config
  assert_file_content "$AGENTOS_HOME/runtime/config/local.toml" "user = true" upgrade_config
  assert_file_content "$AGENTOS_HOME/runtime/RELEASE" "2.0.0" upgrade_config
}

# Regression: the runtime state directory written by the running engine must
# survive an upgrade byte-for-byte. A prior installer replaced the whole runtime
# tree and destroyed $AGENTOS_HOME/runtime/data/**.
# Regression: an upgrade must be able to close a security hole on a box that was
# installed before the fix. Release-governed policy files are refreshed, and the
# worker entries the release stopped booting are removed from an adopted
# config.yaml with the operator's original kept beside it.
test_upgrade_applies_release_security_defaults() {
  make_release v1.0.0 config/shell.yaml config/iii-stream.yaml config/console.yaml
  run_installer v1.0.0 || return

  printf 'allow_unjailed: true\n' > "$AGENTOS_HOME/runtime/config/shell.yaml"
  printf 'host: 0.0.0.0\nauth_function: null\n' > "$AGENTOS_HOME/runtime/config/iii-stream.yaml"
  rm -f "$AGENTOS_HOME/runtime/config/console.yaml"

  make_release v2.0.0 config/shell.yaml config/iii-stream.yaml config/console.yaml
  run_installer v2.0.0 || return

  assert_file_contains "$AGENTOS_HOME/runtime/config/shell.yaml" "payload from 2.0.0" security_defaults
  assert_file_contains "$AGENTOS_HOME/runtime/config/iii-stream.yaml" "payload from 2.0.0" security_defaults
  assert_file_contains "$AGENTOS_HOME/runtime/config/console.yaml" "payload from 2.0.0" security_defaults
}

test_upgrade_removes_unsafe_worker_entries_from_adopted_config() {
  make_release v1.0.0
  run_installer v1.0.0 || return

  cat > "$AGENTOS_HOME/runtime/config.yaml" <<'OPERATOR'
workers:
  - name: state
  - name: shell
    config:
      host_roots:
        - ${III_COMPOSE_DIR:.}
      allow_unjailed: true
  - name: console
  - name: operator-worker
user: true
OPERATOR

  make_release v2.0.0
  run_installer v2.0.0 || return

  local config="$AGENTOS_HOME/runtime/config.yaml"
  if grep -Eq '^[[:space:]]*-[[:space:]]*name:[[:space:]]*shell[[:space:]]*$' "$config"; then
    fail "unsafe_entries: the shell worker entry survived the upgrade"
  fi
  if grep -Eq '^[[:space:]]*-[[:space:]]*name:[[:space:]]*console[[:space:]]*$' "$config"; then
    fail "unsafe_entries: the console worker entry survived the upgrade"
  fi
  if grep -q 'allow_unjailed: true' "$config"; then
    fail "unsafe_entries: the shell entry's inline block survived the upgrade"
  fi
  assert_file_contains "$config" "operator-worker" unsafe_entries
  assert_file_contains "$config" "user: true" unsafe_entries
  assert_file_contains "$config" "- name: state" unsafe_entries
  assert_file_contains "$config.bak" "allow_unjailed: true" unsafe_entries
}

test_upgrade_keeps_a_clean_config_untouched() {
  make_release v1.0.0
  run_installer v1.0.0 || return

  printf 'workers:\n  - name: state\nuser: true\n' > "$AGENTOS_HOME/runtime/config.yaml"

  make_release v2.0.0
  run_installer v2.0.0 || return

  assert_file_contains "$AGENTOS_HOME/runtime/config.yaml" "user: true" clean_config
  assert_absent "$AGENTOS_HOME/runtime/config.yaml.bak" clean_config
}

test_upgrade_preserves_runtime_data() {
  make_release v1.0.0
  run_installer v1.0.0 || return

  seed_runtime_state
  local before after
  before="$(tree_manifest "$AGENTOS_HOME/runtime/data")"

  make_release v2.0.0
  run_installer v2.0.0 || return

  assert_exists "$AGENTOS_HOME/runtime/data/agentos.db" upgrade_data
  assert_exists "$AGENTOS_HOME/runtime/data/sessions/s-1/state.json" upgrade_data
  assert_file_content "$AGENTOS_HOME/runtime/data/agentos.db" "$SENTINEL_DB" upgrade_data
  assert_file_content "$AGENTOS_HOME/runtime/data/sessions/s-1/state.json" \
    "$SENTINEL_SESSION" upgrade_data

  after="$(tree_manifest "$AGENTOS_HOME/runtime/data")"
  assert_equal "$after" "$before" upgrade_data_manifest

  # The upgrade must still have landed, otherwise "preserved" is meaningless.
  assert_file_content "$AGENTOS_HOME/runtime/RELEASE" "2.0.0" upgrade_data
}

# Regression: credentials the operator placed in the runtime working directory
# are state, not release payload.
test_upgrade_preserves_runtime_dotenv() {
  make_release v1.0.0
  run_installer v1.0.0 || return

  printf '%s' "$SENTINEL_ENV" > "$AGENTOS_HOME/runtime/.env"
  chmod 600 "$AGENTOS_HOME/runtime/.env"

  make_release v2.0.0
  run_installer v2.0.0 || return

  assert_file_content "$AGENTOS_HOME/runtime/.env" "$SENTINEL_ENV" upgrade_dotenv
}

# Preserving state must not degrade into merging trees: files that the new
# release dropped have to disappear.
test_upgrade_drops_stale_release_payload() {
  make_release v1.0.0 workers/legacy/iii.worker.yaml
  run_installer v1.0.0 || return
  assert_exists "$AGENTOS_HOME/runtime/workers/legacy/iii.worker.yaml" stale_payload

  make_release v2.0.0
  run_installer v2.0.0 || return

  assert_absent "$AGENTOS_HOME/runtime/workers/legacy" stale_payload
  assert_file_content "$AGENTOS_HOME/runtime/RELEASE" "2.0.0" stale_payload
}

# A staged directory left behind by an interrupted run must not leak into the
# installed runtime, and no staging directory may survive a successful run.
test_upgrade_leaves_no_staged_directories() {
  make_release v1.0.0
  run_installer v1.0.0 || return

  seed_runtime_state
  mkdir -p "$AGENTOS_HOME/runtime.new/workers/ghost"
  printf 'interrupted run\n' > "$AGENTOS_HOME/runtime.new/GHOST"

  make_release v2.0.0
  run_installer v2.0.0 || return

  assert_absent "$AGENTOS_HOME/runtime.new" staged
  assert_absent "$AGENTOS_HOME/runtime.old" staged
  assert_absent "$AGENTOS_HOME/runtime/GHOST" staged
  assert_absent "$AGENTOS_HOME/runtime/workers/ghost" staged
  assert_file_content "$AGENTOS_HOME/runtime/data/agentos.db" "$SENTINEL_DB" staged
}

# An upgrade interrupted after the directory swap leaves state behind in the
# retired tree; the next run must adopt it instead of discarding it.
test_interrupted_upgrade_adopts_retired_state() {
  make_release v1.0.0
  run_installer v1.0.0 || return

  seed_runtime_state
  mv "$AGENTOS_HOME/runtime" "$AGENTOS_HOME/runtime.old"
  mkdir -p "$AGENTOS_HOME/runtime"
  printf 'half-swapped\n' > "$AGENTOS_HOME/runtime/RELEASE"

  make_release v2.0.0
  run_installer v2.0.0 || return

  assert_absent "$AGENTOS_HOME/runtime.old" interrupted_swap
  assert_file_content "$AGENTOS_HOME/runtime/data/agentos.db" "$SENTINEL_DB" interrupted_swap
  assert_file_content "$AGENTOS_HOME/runtime/data/sessions/s-1/state.json" \
    "$SENTINEL_SESSION" interrupted_swap
  assert_file_content "$AGENTOS_HOME/runtime/RELEASE" "2.0.0" interrupted_swap
}

# An upgrade interrupted between retiring the old tree and installing the new
# one leaves no live runtime at all; state still has to come back.
test_interrupted_upgrade_restores_missing_runtime() {
  make_release v1.0.0
  run_installer v1.0.0 || return

  seed_runtime_state
  mv "$AGENTOS_HOME/runtime" "$AGENTOS_HOME/runtime.old"

  make_release v2.0.0
  run_installer v2.0.0 || return

  assert_absent "$AGENTOS_HOME/runtime.old" interrupted_retire
  assert_file_content "$AGENTOS_HOME/runtime/data/agentos.db" "$SENTINEL_DB" interrupted_retire
  assert_file_content "$AGENTOS_HOME/runtime/RELEASE" "2.0.0" interrupted_retire
}

# The published installer is a byte copy of the tested one.
test_published_installer_is_identical() {
  if ! cmp -s "$REPO_ROOT/scripts/install.sh" "$REPO_ROOT/website/public/install.sh"; then
    fail "installer_sync: scripts/install.sh and website/public/install.sh differ"
  fi
}

ALL_TESTS=(
  test_fresh_install_places_binaries_and_runtime
  test_fresh_install_resolves_latest_release
  test_upgrade_preserves_user_config
  test_upgrade_applies_release_security_defaults
  test_upgrade_removes_unsafe_worker_entries_from_adopted_config
  test_upgrade_keeps_a_clean_config_untouched
  test_upgrade_preserves_runtime_data
  test_upgrade_preserves_runtime_dotenv
  test_upgrade_drops_stale_release_payload
  test_upgrade_leaves_no_staged_directories
  test_interrupted_upgrade_adopts_retired_state
  test_interrupted_upgrade_restores_missing_runtime
  test_published_installer_is_identical
)

run_test() {
  local name="$1"
  TESTS_RUN=$((TESTS_RUN + 1))
  TEST_FAILURES=0
  sandbox_new
  "$name"
  if [ "$TEST_FAILURES" -eq 0 ]; then
    printf '  ok   %s\n' "$name"
  else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    printf '  FAIL %s\n' "$name"
  fi
}

main() {
  local requested=("$@")
  local name

  printf 'install.sh regression suite (installer: %s)\n' "$INSTALLER"
  if [ ! -f "$INSTALLER" ]; then
    printf 'error: installer not found: %s\n' "$INSTALLER" >&2
    exit 2
  fi

  if [ "${#requested[@]}" -gt 0 ]; then
    for name in "${requested[@]}"; do
      if ! declare -F "$name" > /dev/null; then
        printf 'error: no such test: %s\n' "$name" >&2
        exit 2
      fi
      run_test "$name"
    done
  else
    for name in "${ALL_TESTS[@]}"; do
      run_test "$name"
    done
  fi

  printf '\n%d test(s), %d failed\n' "$TESTS_RUN" "$TESTS_FAILED"
  [ "$TESTS_FAILED" -eq 0 ]
}

main "$@"
