#!/bin/bash
# install-cef-linux.sh: Build and install the CEF renderer for mesh-wick on Linux.
#
# Usage:
#   bash scripts/install-cef-linux.sh [--force]
#
# Installs wick-renderer, wick-helper, and CEF runtime files into
# ${MESH_WICK_HOME:-$HOME/.mesh-wick}/cef.

set -euo pipefail

FORCE=0
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        -h|--help)
            echo "Usage: bash scripts/install-cef-linux.sh [--force]"
            exit 0
            ;;
        *)
            echo "Unknown argument: $arg" >&2
            exit 1
            ;;
    esac
done

if [[ "$(uname)" != "Linux" ]]; then
    echo "Error: install-cef-linux.sh is for Linux only." >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RENDERER_SRC="$(cd "$SCRIPT_DIR/../cef" && pwd)"

if [[ ! -f "$RENDERER_SRC/renderer_linux.c" ]]; then
    echo "Error: local CEF sources not found at $RENDERER_SRC" >&2
    exit 1
fi

if ! command -v gcc &>/dev/null; then
    echo "Error: gcc is required to compile the CEF renderer." >&2
    exit 1
fi

STATE_HOME="${MESH_WICK_HOME:-$HOME/.mesh-wick}"
INSTALL_DIR="$STATE_HOME/cef"
CEF_VERSION="144.0.18+gc5b2ec2+chromium-144.0.7559.246"
CEF_PLATFORM="linux64"
CEF_DIR="cef_binary_${CEF_VERSION}_${CEF_PLATFORM}_minimal"
CEF_URL="https://cef-builds.spotifycdn.com/${CEF_DIR}.tar.bz2"
RENDERER_BIN="$INSTALL_DIR/wick-renderer"
RENDERER_PROTOCOL="v3-cf-aware"

mkdir -p "$INSTALL_DIR"

if [[ "$FORCE" -eq 0 && -f "$RENDERER_BIN" && -f "$INSTALL_DIR/libcef.so" ]] && \
   grep -q "WICK_RENDERER_PROTOCOL=${RENDERER_PROTOCOL}" "$RENDERER_BIN" 2>/dev/null; then
    echo "CEF renderer already installed at $INSTALL_DIR (use --force to rebuild)."
    exit 0
fi

TMP_TAR="$INSTALL_DIR/cef.tar.bz2"
cleanup() {
    rm -f "$TMP_TAR"
}
trap cleanup EXIT

cd "$INSTALL_DIR"

if [[ ! -d "$INSTALL_DIR/$CEF_DIR" ]]; then
    echo "Downloading CEF SDK (${CEF_VERSION})..."
    curl -fL --progress-bar -o "$TMP_TAR" "$CEF_URL"
    tar xjf "$TMP_TAR" -C "$INSTALL_DIR"
    rm -f "$TMP_TAR"
fi

echo "Copying CEF runtime files..."
cp -f "$CEF_DIR/Release/libcef.so" .
cp -f "$CEF_DIR/Release/libEGL.so" . 2>/dev/null || true
cp -f "$CEF_DIR/Release/libGLESv2.so" . 2>/dev/null || true
cp -f "$CEF_DIR/Release/libvk_swiftshader.so" . 2>/dev/null || true
cp -f "$CEF_DIR/Release/libvulkan.so.1" . 2>/dev/null || true
cp -f "$CEF_DIR/Release/v8_context_snapshot.bin" .
cp -f "$CEF_DIR/Resources/icudtl.dat" .
cp -f "$CEF_DIR/Resources/resources.pak" .
cp -f "$CEF_DIR/Resources/chrome_100_percent.pak" .
cp -f "$CEF_DIR/Resources/chrome_200_percent.pak" .
rm -rf locales
cp -rf "$CEF_DIR/Resources/locales" .
strip libcef.so libEGL.so libGLESv2.so libvk_swiftshader.so libvulkan.so.1 2>/dev/null || true

echo "Compiling wick-renderer and wick-helper..."
gcc -o wick-renderer "$RENDERER_SRC/renderer_linux.c" \
    -DCEF_API_VERSION=14400 \
    -I"$CEF_DIR" \
    -I"$RENDERER_SRC" \
    -L. -lcef \
    -Wl,-rpath,'$ORIGIN' \
    -pthread

gcc -o wick-helper "$RENDERER_SRC/helper_linux.c" \
    -DCEF_API_VERSION=14400 \
    -I"$CEF_DIR" \
    -I"$RENDERER_SRC" \
    -L. -lcef \
    -Wl,-rpath,'$ORIGIN' \
    -pthread

strip wick-renderer wick-helper 2>/dev/null || true
chmod +x wick-renderer wick-helper

echo "Cleaning up unpacked CEF SDK..."
rm -rf "$INSTALL_DIR/$CEF_DIR"

echo "Installed CEF renderer at $INSTALL_DIR"
