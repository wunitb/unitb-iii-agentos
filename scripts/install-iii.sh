#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION_FILE="$ROOT/.iii-version"
[[ -f "$VERSION_FILE" ]] || { echo "missing iii version file: $VERSION_FILE" >&2; exit 1; }
VERSION="${III_VERSION:-$(tr -d '[:space:]' < "$VERSION_FILE")}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "invalid stable iii version: $VERSION" >&2; exit 1; }
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

if [[ "$os" == "apple-darwin" && "$arch" == "x86_64" ]]; then
    echo "iii v${VERSION} does not publish the required iii-worker runtime for macOS x86_64" >&2
    exit 1
fi

base_url="https://github.com/iii-hq/iii/releases/download/iii/v${VERSION}"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT
mkdir -p "$INSTALL_DIR"

for binary in iii iii-worker; do
    asset="${binary}-${arch}-${os}.tar.gz"
    checksum_asset="${binary}-${arch}-${os}.sha256"

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
    [[ -f "$tmp_dir/$binary" ]] || { echo "missing $binary in $asset" >&2; exit 1; }
    install -m 0755 "$tmp_dir/$binary" "$INSTALL_DIR/$binary"
done

"$INSTALL_DIR/iii" --version
[[ -x "$INSTALL_DIR/iii-worker" ]] || { echo "iii-worker installation failed" >&2; exit 1; }
