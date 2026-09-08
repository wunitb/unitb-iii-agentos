#!/usr/bin/env bash
set -e

AGENTOS_REPO="wunitb/unitb-iii-agentos"
III_VERSION_OVERRIDE="${III_VERSION:-}"
III_VERSION=""
INSTALL_DIR="${BIN_DIR:-${PREFIX:-$HOME/.local}/bin}"
AGENTOS_HOME="${AGENTOS_HOME:-$HOME/.agentos}"

BOLD="\033[1m"
DIM="\033[2m"
GREEN="\033[32m"
YELLOW="\033[33m"
RED="\033[31m"
CYAN="\033[36m"
RESET="\033[0m"

info() { printf "${CYAN}>${RESET} %s\n" "$1"; }
ok() { printf "${GREEN}>${RESET} %s\n" "$1"; }
warn() { printf "${YELLOW}!${RESET} %s\n" "$1"; }
err() { printf "${RED}x${RESET} %s\n" "$1" >&2; exit 1; }

detect_os() {
  case "$(uname -s)" in
    Linux*)  echo "linux" ;;
    Darwin*) echo "darwin" ;;
    MINGW*|MSYS*|CYGWIN*) echo "windows" ;;
    *) err "Unsupported OS: $(uname -s)" ;;
  esac
}

detect_arch() {
  case "$(uname -m)" in
    x86_64|amd64)   echo "x86_64" ;;
    arm64|aarch64)   echo "aarch64" ;;
    armv7*)          echo "armv7" ;;
    *) err "Unsupported architecture: $(uname -m)" ;;
  esac
}

check_cmd() { command -v "$1" > /dev/null 2>&1; }

get_latest_release() {
  local repo="$1"
  local url="https://api.github.com/repos/${repo}/releases/latest"

  if check_cmd jq; then
    curl -fsSL "$url" | jq -r '.tag_name'
  else
    curl -fsSL "$url" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/'
  fi
}

# Operator-owned paths inside the runtime tree. Everything else is release
# payload and is replaced wholesale by an upgrade.
RUNTIME_STATE_PATHS=(config config.yaml data .env)

# Move operator-owned state from one runtime tree into another, replacing the
# release defaults. Renames keep live engine state intact and never copy it.
adopt_runtime_state() {
  local from="$1"
  local to="$2"
  local state

  for state in "${RUNTIME_STATE_PATHS[@]}"; do
    if [ -e "$from/$state" ]; then
      rm -rf "${to:?}/$state"
      mv "$from/$state" "$to/$state"
    fi
  done
}

# Security policy files are release-governed, not operator overrides: an upgrade
# must be able to close a hole on a box that was installed before the fix.
RELEASE_GOVERNED_PATHS=(config/shell.yaml config/iii-stream.yaml config/console.yaml)

# Worker entries the release stopped booting on purpose. `shell` puts
# shell::exec/coder::* on the bus; `harness` starts autonomous agent turns
# (harness::send/spawn) and drives coder::*/shell::* through the same bus;
# `console` (1.9.16) has no host key, so it binds 0.0.0.0 and proxies /ws to
# that bus. An adopted config.yaml that still lists them would carry the hole
# across the upgrade.
UNSAFE_WORKER_ENTRIES=(shell harness console)

# Drops one `- name: <worker>` list entry and the block indented under it,
# leaving every other line untouched.
strip_worker_entry() {
  awk -v worker="$1" '
    BEGIN { skip = 0; entry_indent = 0 }
    {
      if (skip) {
        if ($0 ~ /^[ \t]*$/) { print; next }
        match($0, /^[ \t]*/)
        if (RLENGTH > entry_indent) { next }
        skip = 0
      }
      if ($0 ~ "^[ \t]*-[ \t]*name:[ \t]*" worker "[ \t]*$") {
        match($0, /^[ \t]*/)
        entry_indent = RLENGTH
        skip = 1
        next
      }
      print
    }
  '
}

# The engine's own WebSocket bus. It is mandatory: when config.yaml does not
# declare it the engine appends it with the default config, whose host is
# 0.0.0.0, so the bus - which carries every AgentOS function and has no
# authentication of its own - becomes reachable from the LAN and the tailnet.
# Pinning it to loopback is what keeps the HTTP perimeter meaningful.
BUS_WORKER=iii-worker-manager
BUS_HOST=127.0.0.1

# Forces `host: <BUS_HOST>` on the bus worker entry, preserving every other key
# in its config block. Idempotent: adds the entry, the `config:` mapping, or the
# `host:` key only when that piece is missing.
ensure_bus_binding() {
  local item_indent="$1"
  local has_entry="$2"
  awk -v worker="$BUS_WORKER" -v host="$BUS_HOST" -v item_indent="$item_indent" \
      -v inserted="$has_entry" '
    function pad(width,   out) { out = ""; while (length(out) < width) out = out " "; return out }
    function close_entry(   child) {
      if (state == 1) {
        print pad(entry_indent + 2) "config:"
        print pad(entry_indent + 4) "host: " host
      } else if (state == 2 && !host_seen) {
        child = (config_child_indent > 0) ? config_child_indent : config_indent + 2
        print pad(child) "host: " host
      }
      state = 0
      host_seen = 0
      config_child_indent = 0
    }
    BEGIN { state = 0; host_seen = 0; config_child_indent = 0 }
    {
      line = $0
      match(line, /^[ \t]*/)
      indent = RLENGTH
      if (state > 0 && line !~ /^[ \t]*$/ && indent <= entry_indent) close_entry()

      if (state == 2 && line !~ /^[ \t]*$/) {
        if (indent > config_indent) {
          if (config_child_indent == 0) config_child_indent = indent
          if (line ~ /^[ \t]*host[ \t]*:/) {
            print pad(indent) "host: " host
            host_seen = 1
            next
          }
        } else {
          if (!host_seen) print pad(config_child_indent > 0 ? config_child_indent : config_indent + 2) "host: " host
          host_seen = 1
          state = 1
        }
      }

      if (state == 1 && line ~ /^[ \t]*config[ \t]*:[ \t]*$/ && indent > entry_indent) {
        state = 2
        config_indent = indent
        print line
        next
      }

      if (line ~ "^[ \t]*-[ \t]*name:[ \t]*" worker "[ \t]*$") {
        state = 1
        entry_indent = indent
        print line
        next
      }

      print line

      if (!inserted && line ~ /^[ \t]*workers[ \t]*:[ \t]*$/) {
        print pad(item_indent) "- name: " worker
        print pad(item_indent + 2) "config:"
        print pad(item_indent + 4) "host: " host
        inserted = 1
      }
      next
    }
    END { close_entry() }
  '
}

# Indentation of the first `- ` item under `workers:`, so an inserted entry
# joins the existing sequence instead of starting a second, invalid one.
workers_item_indent() {
  awk '
    BEGIN { in_workers = 0 }
    /^[ \t]*workers[ \t]*:[ \t]*$/ { in_workers = 1; next }
    in_workers && /^[ \t]*-/ { match($0, /^[ \t]*/); print RLENGTH; exit }
    in_workers && /^[^ \t]/ { exit }
  '
}

extract_worker_entry() {
  local worker="$1"
  awk -v worker="$worker" '
    BEGIN { emit = 0; entry_indent = 0 }
    {
      match($0, /^[ \t]*/); indent = RLENGTH
      if (emit && $0 !~ /^[ \t]*$/ && indent <= entry_indent) exit
      if ($0 ~ "^[ \t]*-[ \t]*name:[ \t]*" worker "[ \t]*$") {
        emit = 1; entry_indent = indent
      }
      if (emit) print
    }
  '
}

normalized_rbac() {
  awk '
    function trim(line) { sub(/^[ \t]*/, "", line); sub(/[ \t]*$/, "", line); return line }
    BEGIN { emit = 0; base = 0 }
    {
      line = $0
      if (line ~ /^[ \t]*#/ || line ~ /^[ \t]*$/) next
      match(line, /^[ \t]*/); indent = RLENGTH
      if (!emit && line ~ /^[ \t]*rbac[ \t]*:[ \t]*$/) { emit = 1; base = indent }
      else if (emit && indent <= base) exit
      if (emit) print trim(line)
    }
  '
}

normalized_entry() {
  sed -e '/^[[:space:]]*#/d' -e '/^[[:space:]]*$/d' \
      -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//'
}

EXPECTED_RBAC='rbac:
auth_function_id: agentos::bus_auth
on_function_registration_function_id: agentos::bus_on_register
on_trigger_registration_function_id: agentos::bus_on_trigger
on_trigger_type_registration_function_id: agentos::bus_on_trigger_type
expose_functions:
- match("*")'
EXPECTED_BRIDGE='- name: iii-bridge
config:
url: ws://127.0.0.1:49129
forward:
- local_function: agentos::bus_auth
remote_function: agentos::bus_auth
timeout_ms: 5000
- local_function: agentos::bus_on_register
remote_function: agentos::bus_on_register
timeout_ms: 5000
- local_function: agentos::bus_on_trigger
remote_function: agentos::bus_on_trigger
timeout_ms: 5000
- local_function: agentos::bus_on_trigger_type
remote_function: agentos::bus_on_trigger_type
timeout_ms: 5000'

# Prints armed, unarmed, or conflict. Only a completely absent topology is
# migratable; a partial/unknown topology is never overwritten by text surgery.
security_topology_state() {
  local config="$1" manager bridge rbac manager_count bridge_count
  manager_count="$(grep -cE "^[[:space:]]*-[[:space:]]*name:[[:space:]]*${BUS_WORKER}[[:space:]]*$" "$config")"
  bridge_count="$(grep -cE '^[[:space:]]*-[[:space:]]*name:[[:space:]]*iii-bridge[[:space:]]*$' "$config")"
  if [ "$manager_count" -gt 1 ] || [ "$bridge_count" -gt 1 ]; then
    printf 'conflict
'
    return
  fi
  manager="$(extract_worker_entry "$BUS_WORKER" < "$config")"
  bridge="$(extract_worker_entry iii-bridge < "$config")"
  rbac="$(printf '%s\n' "$manager" | normalized_rbac)"
  if [ -z "$rbac" ] && [ -z "$bridge" ]; then
    # Inline manager config cannot be extended safely without a YAML parser.
    if printf '%s\n' "$manager" | grep -Eq '^[[:space:]]*config[[:space:]]*:[[:space:]]*[^[:space:]]'; then
      printf 'conflict\n'
    else
      printf 'unarmed\n'
    fi
    return
  fi
  if [ "$rbac" = "$EXPECTED_RBAC" ] && \
      [ "$(printf '%s\n' "$bridge" | normalized_entry)" = "$EXPECTED_BRIDGE" ]; then
    printf 'armed\n'
  else
    printf 'conflict\n'
  fi
}

inject_release_rbac() {
  awk -v worker="$BUS_WORKER" '
    function pad(width,   out) { out = ""; while (length(out) < width) out = out " "; return out }
    function add_rbac(   child) {
      child = config_indent + 2
      print pad(child) "rbac:"
      print pad(child + 2) "auth_function_id: agentos::bus_auth"
      print pad(child + 2) "on_function_registration_function_id: agentos::bus_on_register"
      print pad(child + 2) "on_trigger_registration_function_id: agentos::bus_on_trigger"
      print pad(child + 2) "on_trigger_type_registration_function_id: agentos::bus_on_trigger_type"
      print pad(child + 2) "expose_functions:"
      print pad(child + 4) "- match(\"*\")"
    }
    BEGIN { state = 0 }
    {
      line = $0; match(line, /^[ \t]*/); indent = RLENGTH
      if (state == 2 && line !~ /^[ \t]*$/ && indent <= config_indent) { add_rbac(); state = 1 }
      if (state > 0 && line !~ /^[ \t]*$/ && indent <= entry_indent) state = 0
      if (line ~ "^[ \t]*-[ \t]*name:[ \t]*" worker "[ \t]*$") { state = 1; entry_indent = indent }
      if (state == 1 && line ~ /^[ \t]*config[ \t]*:[ \t]*$/ && indent > entry_indent) {
        state = 2; config_indent = indent
      }
      print line
    }
    END { if (state == 2) add_rbac() }
  '
}

inject_release_bridge() {
  local item_indent="$1"
  awk -v width="$item_indent" '
    function pad(n,   out) { out = ""; while (length(out) < n) out = out " "; return out }
    {
      print
      if (!done && $0 ~ /^[ \t]*workers[ \t]*:[ \t]*$/) {
        print pad(width) "- name: iii-bridge"
        print pad(width + 2) "config:"
        print pad(width + 4) "url: ws://127.0.0.1:49129"
        print pad(width + 4) "forward:"
        add("agentos::bus_auth")
        add("agentos::bus_on_register")
        add("agentos::bus_on_trigger")
        add("agentos::bus_on_trigger_type")
        done = 1
      }
    }
    function add(id) {
      print pad(width + 6) "- local_function: " id
      print pad(width + 8) "remote_function: " id
      print pad(width + 8) "timeout_ms: 5000"
    }
  '
}

preflight_release_security_defaults() {
  local release_runtime="$1" installed_runtime="$2" release_config target_config state
  release_config="$release_runtime/config.yaml"
  target_config="$installed_runtime/config.yaml"
  if [ ! -f "$release_config" ] || [ -L "$release_config" ]; then
    err "release runtime has no regular config.yaml"
  fi
  state="$(security_topology_state "$release_config")"
  [ "$state" = armed ] || err "release config.yaml does not carry the required four-hook bus security topology"
  [ -e "$target_config" ] || return 0
  if [ ! -f "$target_config" ] || [ -L "$target_config" ]; then
    err "installed config.yaml must be a regular file, not a symlink"
  fi
  grep -Eq '^[[:space:]]*workers[[:space:]]*:[[:space:]]*$' "$target_config" || return 0
  state="$(security_topology_state "$target_config")"
  [ "$state" != conflict ] || err "conflicting bus security topology in ${target_config}; refusing to overwrite operator config"
}

apply_release_security_defaults() {
    local release_runtime="$1"
    local installed_runtime="$2"
    local relative_path
    local worker
    local config="$installed_runtime/config.yaml"
    local removed=""
    local item_indent
    local topology_state
    local updated

    for relative_path in "${RELEASE_GOVERNED_PATHS[@]}"; do
        if [ -f "$release_runtime/$relative_path" ]; then
            mkdir -p "$(dirname "$installed_runtime/$relative_path")"
            cp "$release_runtime/$relative_path" "$installed_runtime/$relative_path"
        fi
    done

    [ -f "$config" ] || return 0
    # Only a file that really declares a worker roster is an engine config; an
    # unrelated operator YAML is left byte-for-byte alone.
    grep -Eq '^[[:space:]]*workers[[:space:]]*:[[:space:]]*$' "$config" || return 0
    topology_state="$(security_topology_state "$config")"
    [ "$topology_state" != conflict ] || err "conflicting bus security topology in ${config}; refusing to overwrite operator config"

    if grep -Eq "^[[:space:]]*-[[:space:]]*name:[[:space:]]*${BUS_WORKER}[[:space:]]*$" "$config" &&
        grep -Eq "^[[:space:]]*config[[:space:]]*:[[:space:]]*[^[:space:]]" "$config"; then
        warn "${config}: ${BUS_WORKER} uses an inline config mapping; leaving it untouched"
        warn "  set 'host: ${BUS_HOST}' on it by hand, or the engine bus binds 0.0.0.0"
    fi

    updated="$(cat "$config")"
    for worker in "${UNSAFE_WORKER_ENTRIES[@]}"; do
        if grep -Eq "^[[:space:]]*-[[:space:]]*name:[[:space:]]*${worker}[[:space:]]*$" "$config"; then
            removed="${removed}${removed:+, }${worker}"
        fi
        updated="$(printf '%s\n' "$updated" | strip_worker_entry "$worker")"
    done
    item_indent="$(printf '%s\n' "$updated" | workers_item_indent)"
    [ -n "$item_indent" ] || item_indent=2
    if printf '%s\n' "$updated" |
        grep -Eq "^[[:space:]]*-[[:space:]]*name:[[:space:]]*${BUS_WORKER}[[:space:]]*$"; then
        updated="$(printf '%s\n' "$updated" | ensure_bus_binding "$item_indent" 1)"
    else
        updated="$(printf '%s\n' "$updated" | ensure_bus_binding "$item_indent" 0)"
    fi
    if [ "$topology_state" = unarmed ]; then
        updated="$(printf '%s\n' "$updated" | inject_release_rbac)"
        updated="$(printf '%s\n' "$updated" | inject_release_bridge "$item_indent")"
    fi

    if [ "$updated" = "$(cat "$config")" ]; then
        return 0
    fi

    cp "$config" "$config.bak"
    printf '%s\n' "$updated" > "$config"
    if [ -n "$removed" ]; then
        warn "Removed release-governed worker entries from ${config}: ${removed}"
        warn "  they expose an arbitrary-command sink and a 0.0.0.0 web console on an unauthenticated bus"
    fi
    warn "Applied the release bus security topology and pinned ${BUS_WORKER} to ${BUS_HOST} in ${config}"
    warn "  the four RBAC hooks fail closed through the loopback iii-bridge/agentos-bus-authd gate"
    warn "  your previous file is kept at ${config}.bak; safe custom entries were preserved"
}

download_and_install() {
  local repo="$1"
  local version="$2"
  local os="$3"
  local arch="$4"
  local binary_name="$5"
  local tag="${version#v}"
  local asset="${binary_name}-${tag}-${arch}-${os}.tar.gz"
  local base_url="https://github.com/${repo}/releases/download/${version}"
  local tmp_dir archive_path runtime_dir runtime_stage runtime_retired

  info "Downloading ${binary_name} ${version} for ${os}/${arch}..."
  tmp_dir="$(mktemp -d)"
  # Capture the function-local path before it leaves scope.
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp_dir'" EXIT
  archive_path="$tmp_dir/$asset"

  curl -fsSLo "$archive_path" "$base_url/$asset" \
    || err "No AgentOS ${version} release for ${os}/${arch}"
  curl -fsSLo "$archive_path.sha256" "$base_url/$asset.sha256" \
    || err "Missing checksum for $asset"

  if check_cmd sha256sum; then
    (cd "$tmp_dir" && sha256sum --check "$asset.sha256") \
      || err "Checksum verification failed for $asset"
  elif check_cmd shasum; then
    local expected actual
    expected="$(cut -d ' ' -f 1 "$archive_path.sha256")"
    actual="$(shasum -a 256 "$archive_path" | cut -d ' ' -f 1)"
    [ "$actual" = "$expected" ] || err "Checksum verification failed for $asset"
  else
    err "sha256sum or shasum is required to verify AgentOS"
  fi

  tar -xzf "$archive_path" -C "$tmp_dir"
  runtime_dir="$AGENTOS_HOME/runtime"
  runtime_stage="$AGENTOS_HOME/runtime.new"
  runtime_retired="$AGENTOS_HOME/runtime.old"

  # Validate every release input consumed below, plus the live topology, before
  # changing any installed executable or runtime path.
  local executable relative_path
  for executable in "$binary_name" agentos-tui agentos-bus-authd; do
    if [ ! -f "$tmp_dir/bin/$executable" ] || [ -L "$tmp_dir/bin/$executable" ] || [ ! -x "$tmp_dir/bin/$executable" ]; then
      err "Could not find regular executable $executable in $asset"
    fi
  done
  if [ ! -d "$tmp_dir/runtime" ] || [ -L "$tmp_dir/runtime" ]; then
    err "Could not find regular runtime directory in $asset"
  fi
  if [ ! -f "$tmp_dir/runtime/.iii-version" ] || [ -L "$tmp_dir/runtime/.iii-version" ] || [ ! -s "$tmp_dir/runtime/.iii-version" ]; then
    err "Release runtime must contain a non-empty regular .iii-version"
  fi
  # The release archive, not a caller override, selects its compatible engine.
  # Refuse OCI payloads before touching binaries, config, or interrupted state.
  require_native_pin "$(tr -d '[:space:]' < "$tmp_dir/runtime/.iii-version")"
  if [ -n "$III_VERSION_OVERRIDE" ] && [ "$III_VERSION_OVERRIDE" != "$III_VERSION" ]; then
    err "III_VERSION must match the explicitly selected native release ($III_VERSION)"
  fi
  local existing
  for existing in "$runtime_dir" "$runtime_retired"; do
    if [ -e "$existing/.iii-version" ] || [ -L "$existing/.iii-version" ]; then
      if [ ! -f "$existing/.iii-version" ] || [ -L "$existing/.iii-version" ]; then
        err "Existing native runtime pin must be a regular file"
      fi
      require_native_pin "$(tr -d '[:space:]' < "$existing/.iii-version")"
    fi
  done
  III_VERSION="$(tr -d '[:space:]' < "$tmp_dir/runtime/.iii-version")"
  for relative_path in .env.example iii.lock workers/env.allowlist; do
    if [ ! -f "$tmp_dir/runtime/$relative_path" ] || [ -L "$tmp_dir/runtime/$relative_path" ]; then
      err "Release runtime input $relative_path must be a regular file"
    fi
  done
  for relative_path in "${RELEASE_GOVERNED_PATHS[@]}"; do
    if [ -e "$tmp_dir/runtime/$relative_path" ] && { [ ! -f "$tmp_dir/runtime/$relative_path" ] || [ -L "$tmp_dir/runtime/$relative_path" ]; }; then
      err "Release governance input $relative_path must be a regular file"
    fi
  done
  preflight_release_security_defaults "$tmp_dir/runtime" "$runtime_dir"
  if [ -d "$runtime_retired" ]; then
    preflight_release_security_defaults "$tmp_dir/runtime" "$runtime_retired"
  fi

  mkdir -p "$INSTALL_DIR" "$AGENTOS_HOME"
  for executable in "$binary_name" agentos-tui agentos-bus-authd; do
    cp "$tmp_dir/bin/$executable" "$INSTALL_DIR/$executable"
    chmod +x "$INSTALL_DIR/$executable"
  done

  # Finish an upgrade that was interrupted mid-swap, so operator state is never
  # stranded in the retired tree.
  if [ -d "$runtime_retired" ]; then
    if [ -d "$runtime_dir" ]; then
      adopt_runtime_state "$runtime_retired" "$runtime_dir"
      rm -rf "$runtime_retired"
    else
      mv "$runtime_retired" "$runtime_dir"
    fi
  fi

  # The stage only ever holds release payload, so a stage left over by an
  # interrupted run is always safe to discard.
  rm -rf "$runtime_stage"
  cp -R "$tmp_dir/runtime" "$runtime_stage"

  if [ -d "$runtime_dir" ]; then
    mv "$runtime_dir" "$runtime_retired"
  fi
  mv "$runtime_stage" "$runtime_dir"

  if [ -d "$runtime_retired" ]; then
    adopt_runtime_state "$runtime_retired" "$runtime_dir"
    rm -rf "$runtime_retired"
  fi

  apply_release_security_defaults "$tmp_dir/runtime" "$runtime_dir"

  ok "${binary_name} ${version} installed to ${INSTALL_DIR}/${binary_name}"
  ok "Runtime installed to ${AGENTOS_HOME}/runtime"
}

ensure_path() {
  case ":$PATH:" in
    *":$INSTALL_DIR:"*) return ;;
  esac

  warn "${INSTALL_DIR} is not in your PATH"

  local shell_name
  shell_name="$(basename "${SHELL:-/bin/sh}")"

  local rc_file=""
  case "$shell_name" in
    zsh)  rc_file="$HOME/.zshrc" ;;
    bash) rc_file="$HOME/.bashrc" ;;
    fish) rc_file="$HOME/.config/fish/config.fish" ;;
  esac

  if [ -n "$rc_file" ]; then
    local line="export PATH=\"${INSTALL_DIR}:\$PATH\""
    if [ "$shell_name" = "fish" ]; then
      line="set -gx PATH ${INSTALL_DIR} \$PATH"
    fi

    if [ -f "$rc_file" ] && grep -qF "$INSTALL_DIR" "$rc_file" 2>/dev/null; then
      return
    fi

    printf "\n%s\n" "$line" >> "$rc_file"
    ok "Added ${INSTALL_DIR} to PATH in ${rc_file}"
    warn "Run: source ${rc_file}  (or open a new terminal)"
  else
    warn "Add this to your shell profile: export PATH=\"${INSTALL_DIR}:\$PATH\""
  fi
}

require_native_pin() {
  local pin="$1"
  if [[ ! "$pin" =~ ^0\.22\.[0-9]+$ ]]; then
    err "Native installation supports archived iii 0.22.x releases only (found ${pin:-empty}); iii 0.23+ requires scripts/oci-stack.sh. Existing data was not migrated."
  fi
  III_VERSION="$pin"
}

resolve_iii_version() {
  local version_file="$AGENTOS_HOME/runtime/.iii-version"
  if [ -n "$III_VERSION_OVERRIDE" ]; then
    III_VERSION="$III_VERSION_OVERRIDE"
  elif [ -f "$version_file" ]; then
    III_VERSION="$(tr -d '[:space:]' < "$version_file")"
  else
    err "Installed AgentOS runtime is missing .iii-version"
  fi

  require_native_pin "$III_VERSION"
}

install_iii() {
  local current_version=""
  if check_cmd iii; then
    current_version="$(iii --version 2>/dev/null | head -1 | sed 's/[^0-9.]//g')"
    if [ "$current_version" = "$III_VERSION" ] && check_cmd iii-worker; then
      ok "iii-engine and iii-worker v${III_VERSION} already installed"
      return
    fi
    if [ "$current_version" = "$III_VERSION" ]; then
      warn "Installing missing iii-worker runtime for iii v${III_VERSION}"
    else
      warn "Replacing iii-engine v${current_version:-unknown} with pinned v${III_VERSION}"
    fi
  fi

  local os arch target ext base_url tmp_dir component asset checksum_asset binary_name found_binary extract_dir
  os="$(detect_os)"
  arch="$(detect_arch)"

  case "$os/$arch" in
    linux/armv7) target="armv7-unknown-linux-gnueabihf" ;;
    linux/*) target="${arch}-unknown-linux-gnu" ;;
    darwin/aarch64) target="aarch64-apple-darwin" ;;
    darwin/x86_64) err "iii v${III_VERSION} does not publish the required iii-worker runtime for macOS x86_64" ;;
    windows/*) err "iii v${III_VERSION} does not publish the required iii-worker runtime for Windows" ;;
    *) err "iii v${III_VERSION} has no release for ${os}/${arch}" ;;
  esac

  ext="tar.gz"
  base_url="https://github.com/iii-hq/iii/releases/download/iii/v${III_VERSION}"
  tmp_dir="$(mktemp -d)"
  # Capture the function-local path before it leaves scope.
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp_dir'" EXIT
  mkdir -p "$INSTALL_DIR"

  for component in iii iii-worker; do
    binary_name="$component"
    asset="${component}-${target}.${ext}"
    checksum_asset="${component}-${target}.sha256"
    extract_dir="$tmp_dir/$component"
    mkdir -p "$extract_dir"

    info "Downloading verified ${component} v${III_VERSION} for ${os}/${arch}..."
    curl -fsSLo "$tmp_dir/$asset" "$base_url/$asset" || err "Failed to download $asset"
    curl -fsSLo "$tmp_dir/$checksum_asset" "$base_url/$checksum_asset" || err "Failed to download $checksum_asset"

    if check_cmd sha256sum; then
      (cd "$tmp_dir" && sha256sum --check "$checksum_asset") || err "Checksum verification failed for $asset"
    elif check_cmd shasum; then
      local expected actual
      expected="$(cut -d ' ' -f 1 "$tmp_dir/$checksum_asset")"
      actual="$(shasum -a 256 "$tmp_dir/$asset" | cut -d ' ' -f 1)"
      [ "$actual" = "$expected" ] || err "Checksum verification failed for $asset"
    else
      err "sha256sum or shasum is required to verify iii runtime binaries"
    fi

    tar -xzf "$tmp_dir/$asset" -C "$extract_dir"
    found_binary="$(find "$extract_dir" -name "$binary_name" -type f | head -1)"
    [ -n "$found_binary" ] || err "Could not find $binary_name in $asset"
    cp "$found_binary" "$INSTALL_DIR/$binary_name"
    chmod +x "$INSTALL_DIR/$binary_name"
  done

  export PATH="$INSTALL_DIR:$PATH"
  ok "iii-engine and iii-worker v${III_VERSION} installed to ${INSTALL_DIR}"
}

install_agentos() {
  local os arch version

  os="$(detect_os)"
  arch="$(detect_arch)"

  if [ "$os/$arch" = "darwin/x86_64" ]; then
    err "Full-stack install is unavailable: pinned iii does not publish iii-worker for macOS x86_64"
  fi

  info "Detected platform: ${os}/${arch}"

  if [ -n "$AGENTOS_VERSION" ]; then
    version="$AGENTOS_VERSION"
  else
    info "Fetching latest AgentOS release..."
    version="$(get_latest_release "$AGENTOS_REPO")"
    if [ -z "$version" ] || [ "$version" = "null" ]; then
      err "Could not determine latest version. Set AGENTOS_VERSION=v0.1.0 to install a specific version."
    fi
  fi

  download_and_install "$AGENTOS_REPO" "$version" "$os" "$arch" "agentos"
}

install_oci() {
  local root
  root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  if [ ! -f "$root/scripts/oci-stack.sh" ] || [ ! -f "$root/Containerfile" ]; then
    err "iii 0.23+ is installed from a fresh source clone: git clone https://github.com/$AGENTOS_REPO.git; then bash scripts/install.sh --oci inside that clone. No native files were changed."
  fi
  if [ -n "${AGENTOS_VERSION:-}" ] || [ -n "$III_VERSION_OVERRIDE" ]; then
    err "OCI uses the checkout and its .iii-version pin, not AGENTOS_VERSION/III_VERSION overrides"
  fi
  check_cmd python3 || err "python3 and a running Podman or Docker runtime are required"
  info "Building the supported OCI runtime; existing native ~/.agentos and installed binaries are not migrated"
  bash "$root/scripts/oci-stack.sh" build
  ok "OCI image built. Start with: bash scripts/oci-stack.sh up"
  info "Use scripts/oci-stack.sh status|logs|doctor|exec|stop; OCI data lives in AGENTOS_OCI_HOME (default ~/.agentos-oci)"
}

main() {
  case "${1:-}" in
    --help|-h)
      printf '%s\n' 'Usage: bash scripts/install.sh [--oci]' \
        'Default: install a native release ONLY if its bundled iii pin is 0.22.x.' \
        'AGENTOS_VERSION=vX.Y.Z selects an archived native release explicitly.' \
        '--oci: build the fresh checkout OCI image, without starting it or migrating native data.'
      return ;;
    --oci)
      [ "$#" -eq 1 ] || err "Unexpected installer arguments; see --help"
      install_oci; return ;;
    '')
      [ "$#" -eq 0 ] || err "Unexpected installer arguments; see --help"
      if [ -n "$III_VERSION_OVERRIDE" ]; then require_native_pin "$III_VERSION_OVERRIDE"; fi
      info "Native release installer (iii 0.22.x only). For iii 0.23+ use scripts/install.sh --oci in a fresh clone."
      ;;
    *) err "Unknown installer argument: $1; see --help" ;;
  esac
  printf "\n"
  printf "${BOLD}  AgentOS Installer${RESET}\n"
  printf "${DIM}  Agent Operating System on iii-engine${RESET}\n"
  printf "\n"

  if ! check_cmd curl; then
    err "curl is required. Install it and try again."
  fi

  install_agentos
  resolve_iii_version
  install_iii
  ensure_path

  printf "\n"
  printf "${GREEN}${BOLD}  Installation complete!${RESET}\n"
  printf "\n"
  printf "  Get started:\n"
  printf "\n"
  printf "    ${CYAN}agentos config set-key anthropic \$ANTHROPIC_API_KEY${RESET}   Provider key\n"
  printf "    ${CYAN}agentos up${RESET}                                            Start engine, workers, TUI\n"
  printf "    ${CYAN}agentos doctor${RESET}                                        Report what is ready\n"
  printf "\n"
  printf "  ${DIM}The provider key is written to %s/runtime/.env (mode 600), which is\n" "$AGENTOS_HOME"
  printf "  the file the workers read. ${CYAN}agentos up${RESET}${DIM} generates AGENTOS_API_KEY there on\n"
  printf "  first run; without it the workers cannot register their HTTP routes.${RESET}\n"
  printf "\n"
  printf "  ${DIM}Docs: https://github.com/wunitb/unitb-iii-agentos${RESET}\n"
  printf "\n"
}

main "$@"
