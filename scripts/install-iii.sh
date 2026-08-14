#!/usr/bin/env bash
set -euo pipefail

VERSION="${III_VERSION:-0.22.1}"
INSTALL_DIR="${III_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)" in
    Linux) os="unknown-linux-gnu" ;;
    Darwin) os="apple-darwin" ;;
    *) echo "unsupported operating system: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

asset="iii-${arch}-${os}.tar.gz"
checksum_asset="iii-${arch}-${os}.sha256"
base_url="https://github.com/iii-hq/iii/releases/download/iii/v${VERSION}"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

curl -fsSLo "$tmp_dir/$asset" "$base_url/$asset"
curl -fsSLo "$tmp_dir/$checksum_asset" "$base_url/$checksum_asset"

if command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp_dir" && sha256sum --check "$checksum_asset")
else
    expected="$(cut -d ' ' -f 1 "$tmp_dir/$checksum_asset")"
    actual="$(shasum -a 256 "$tmp_dir/$asset" | cut -d ' ' -f 1)"
    if [[ "$actual" != "$expected" ]]; then
        echo "checksum verification failed for $asset" >&2
        exit 1
    fi
fi

tar -xzf "$tmp_dir/$asset" -C "$tmp_dir"
mkdir -p "$INSTALL_DIR"
install -m 0755 "$tmp_dir/iii" "$INSTALL_DIR/iii"
"$INSTALL_DIR/iii" --version
