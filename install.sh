#!/usr/bin/env bash
set -euo pipefail

# Vela installer — downloads the latest release binary for your platform.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/raskell-io/vela/main/install.sh | bash
#
#   # Or specify a version and install directory:
#   curl -fsSL https://raw.githubusercontent.com/raskell-io/vela/main/install.sh | bash -s -- --version v0.1.0 --to /usr/local/bin

REPO="raskell-io/vela"
INSTALL_DIR="/usr/local/bin"
VERSION=""

while [[ $# -gt 0 ]]; do
    case $1 in
        --version) VERSION="$2"; shift 2 ;;
        --to)      INSTALL_DIR="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: install.sh [--version VERSION] [--to INSTALL_DIR]"
            echo ""
            echo "Options:"
            echo "  --version VERSION   Install a specific version (e.g. v0.1.0)"
            echo "  --to DIR            Install directory (default: /usr/local/bin)"
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

detect_platform() {
    local os arch

    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)  os="linux" ;;
        Darwin) os="darwin" ;;
        *)      echo "Unsupported OS: $os"; exit 1 ;;
    esac

    case "$arch" in
        x86_64|amd64)  arch="amd64" ;;
        aarch64|arm64) arch="arm64" ;;
        *)             echo "Unsupported architecture: $arch"; exit 1 ;;
    esac

    echo "vela-${os}-${arch}"
}

get_latest_version() {
    curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' \
        | sed -E 's/.*"([^"]+)".*/\1/'
}

main() {
    local artifact version url

    artifact="$(detect_platform)"
    echo "Platform: ${artifact}"

    if [[ -z "$VERSION" ]]; then
        echo "Fetching latest version..."
        version="$(get_latest_version)"
    else
        version="$VERSION"
    fi

    if [[ -z "$version" ]]; then
        echo "Error: could not determine latest version."
        echo "Check https://github.com/${REPO}/releases"
        exit 1
    fi

    echo "Version:  ${version}"

    url="https://github.com/${REPO}/releases/download/${version}/${artifact}"
    echo "Downloading ${url}..."

    local tmpfile
    tmpfile="$(mktemp)"
    trap 'rm -f "$tmpfile"' EXIT

    if ! curl -fSL --progress-bar -o "$tmpfile" "$url"; then
        echo ""
        echo "Error: download failed."
        echo "Check that the release exists: https://github.com/${REPO}/releases/tag/${version}"
        exit 1
    fi

    chmod +x "$tmpfile"

    if [[ -w "$INSTALL_DIR" ]]; then
        mv "$tmpfile" "${INSTALL_DIR}/vela"
    else
        echo "Installing to ${INSTALL_DIR} (requires sudo)..."
        sudo mv "$tmpfile" "${INSTALL_DIR}/vela"
    fi

    echo ""
    echo "Installed vela ${version} to ${INSTALL_DIR}/vela"
    echo ""
    echo "  Server:  vela serve"
    echo "  Client:  vela init && vela deploy"
    echo ""
}

main
