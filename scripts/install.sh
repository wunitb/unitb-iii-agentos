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
# shell::exec/coder::* on the bus; `console` (1.9.16) has no host key, so it
# binds 0.0.0.0 and proxies /ws to that same unauthenticated bus. An adopted
# config.yaml that still lists them would carry the hole across the upgrade.
UNSAFE_WORKER_ENTRIES=(shell console)

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

apply_release_security_defaults() {
    local release_runtime="$1"
    local installed_runtime="$2"
    local relative_path
    local worker
    local config="$installed_runtime/config.yaml"
    local removed=""
    local stripped

    for relative_path in "${RELEASE_GOVERNED_PATHS[@]}"; do
        if [ -f "$release_runtime/$relative_path" ]; then
            mkdir -p "$(dirname "$installed_runtime/$relative_path")"
            cp "$release_runtime/$relative_path" "$installed_runtime/$relative_path"
        fi
    done

    [ -f "$config" ] || return 0
    for worker in "${UNSAFE_WORKER_ENTRIES[@]}"; do
        if grep -Eq "^[[:space:]]*-[[:space:]]*name:[[:space:]]*${worker}[[:space:]]*$" "$config"; then
            removed="${removed}${removed:+, }${worker}"
        fi
    done
    [ -n "$removed" ] || return 0

    cp "$config" "$config.bak"
    for worker in "${UNSAFE_WORKER_ENTRIES[@]}"; do
        stripped="$(strip_worker_entry "$worker" < "$config")" || return 1
        printf '%s\n' "$stripped" > "$config"
    done
    warn "Removed release-governed worker entries from ${config}: ${removed}"
    warn "  they expose an arbitrary-command sink and a 0.0.0.0 web console on an unauthenticated bus"
    warn "  your previous file is kept at ${config}.bak; every other entry was left untouched"
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
  [ -x "$tmp_dir/bin/$binary_name" ] \
    || err "Could not find $binary_name in $asset"
  [ -d "$tmp_dir/runtime" ] || err "Could not find runtime in $asset"

  mkdir -p "$INSTALL_DIR" "$AGENTOS_HOME"
  cp "$tmp_dir/bin/$binary_name" "$INSTALL_DIR/$binary_name"
  chmod +x "$INSTALL_DIR/$binary_name"
  if [ -x "$tmp_dir/bin/agentos-tui" ]; then
    cp "$tmp_dir/bin/agentos-tui" "$INSTALL_DIR/agentos-tui"
    chmod +x "$INSTALL_DIR/agentos-tui"
  fi

  runtime_dir="$AGENTOS_HOME/runtime"
  runtime_stage="$AGENTOS_HOME/runtime.new"
  runtime_retired="$AGENTOS_HOME/runtime.old"

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

resolve_iii_version() {
  local version_file="$AGENTOS_HOME/runtime/.iii-version"
  if [ -n "$III_VERSION_OVERRIDE" ]; then
    III_VERSION="$III_VERSION_OVERRIDE"
  elif [ -f "$version_file" ]; then
    III_VERSION="$(tr -d '[:space:]' < "$version_file")"
  else
    err "Installed AgentOS runtime is missing .iii-version"
  fi

  case "$III_VERSION" in
    ''|*[!0-9.]*) err "Invalid stable iii version: ${III_VERSION:-empty}" ;;
    *-*) err "Refusing prerelease iii version: $III_VERSION" ;;
  esac
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

main() {
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
