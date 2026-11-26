#!/bin/bash
# install-cef-linux.sh: Build and install the CEF renderer for pleb on Linux.
#
# Usage:
#   bash scripts/install-cef-linux.sh [--force] [--prefix <dir>]
#
# Installs pleb-renderer, pleb-helper, and CEF runtime files into
# --prefix <dir> (default: ${PLEB_HOME:-$HOME/.pleb}/cef).

set -euo pipefail

FORCE=0
PREFIX=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --force)
            FORCE=1
            shift
            ;;
        --prefix)
            if [[ $# -lt 2 || -z "$2" ]]; then
                echo "Error: --prefix requires a non-empty directory argument." >&2
                exit 1
            fi
            PREFIX="$2"
            shift 2
            ;;
        --prefix=*)
            PREFIX="${1#--prefix=}"
            if [[ -z "$PREFIX" ]]; then
                echo "Error: --prefix requires a non-empty directory argument." >&2
                exit 1
            fi
            shift
            ;;
        -h|--help)
            echo "Usage: bash scripts/install-cef-linux.sh [--force] [--prefix <dir>]"
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
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
PROTOCOL_HEADER="$RENDERER_SRC/protocol.h"

if [[ ! -f "$RENDERER_SRC/renderer_linux.c" ]]; then
    echo "Error: local CEF sources not found at $RENDERER_SRC" >&2
    exit 1
fi

if [[ ! -f "$PROTOCOL_HEADER" ]]; then
    echo "Error: protocol header not found at $PROTOCOL_HEADER" >&2
    exit 1
fi

if ! command -v gcc &>/dev/null; then
    echo "Error: gcc is required to compile the CEF renderer." >&2
    exit 1
fi

RENDERER_PROTOCOL="$(sed -n 's/^#define[[:space:]]\{1,\}PLEB_RENDERER_PROTOCOL_VERSION[[:space:]]\{1,\}"\([^"]*\)"/\1/p' "$PROTOCOL_HEADER" | head -n 1)"
if [[ -z "$RENDERER_PROTOCOL" ]]; then
    echo "Error: failed to parse PLEB_RENDERER_PROTOCOL_VERSION from $PROTOCOL_HEADER" >&2
    exit 1
fi

STATE_HOME="${PLEB_HOME:-$HOME/.pleb}"
if [[ -n "$PREFIX" ]]; then
    INSTALL_DIR="$PREFIX"
else
    INSTALL_DIR="$STATE_HOME/cef"
fi

CEF_VERSION="154.0.34+g14c5a08+chromium-154.0.8037.98"
CEF_API_VERSION="15400"
ARCH="$(uname -m)"
case "$ARCH" in
    x86_64|amd64)
        CEF_PLATFORM="linux64"
        CEF_SHA1="15c91ad767097217b2e82ba8b933fe30b134be69"
        ;;
    aarch64|arm64)
        CEF_PLATFORM="linuxarm64"
        CEF_SHA1="fa85793519eef199e6b6fa8cc8d6f0eb45c6dac2"
        ;;
    *)
        echo "Error: unsupported Linux architecture: $ARCH" >&2
        exit 1
        ;;
esac
CEF_DIR="cef_binary_${CEF_VERSION}_${CEF_PLATFORM}_minimal"
CEF_URL="https://cef-builds.spotifycdn.com/${CEF_DIR}.tar.bz2"

SDK_CACHE_DIR="${HOME}/.cache/pleb/cef-sdk"
SDK_TARBALL="${SDK_CACHE_DIR}/${CEF_DIR}.tar.bz2"
SDK_DIR="${SDK_CACHE_DIR}/${CEF_DIR}"
SDK_READY="${SDK_DIR}/.extracted-ok"
SDK_LOCK_FILE="${SDK_CACHE_DIR}/.lock"
SDK_LOCK_DIR="${SDK_CACHE_DIR}/.lock.d"

RENDERER_BIN="$INSTALL_DIR/pleb-renderer"

mkdir -p "$INSTALL_DIR"
if [[ -z "$PREFIX" ]]; then
    chmod 700 "$STATE_HOME" "$INSTALL_DIR" 2>/dev/null || true
else
    chmod 700 "$INSTALL_DIR" 2>/dev/null || true
fi

if [[ "$FORCE" -eq 0 && -f "$RENDERER_BIN" && -f "$INSTALL_DIR/libcef.so" ]] && \
   grep -q "PLEB_RENDERER_PROTOCOL=${RENDERER_PROTOCOL}" "$RENDERER_BIN" 2>/dev/null; then
    echo "CEF renderer (${RENDERER_PROTOCOL}) already installed at $INSTALL_DIR (use --force to rebuild)."
    exit 0
fi

TMP_TAR=""
LOCK_HELD_DIR=0

release_sdk_lock() {
    if [[ "$LOCK_HELD_DIR" -eq 1 ]]; then
        rmdir "$SDK_LOCK_DIR" 2>/dev/null || true
        LOCK_HELD_DIR=0
    fi
}

cleanup() {
    if [[ -n "$TMP_TAR" ]]; then
        rm -f "$TMP_TAR"
    fi
    release_sdk_lock
}
trap cleanup EXIT

verify_sha1() {
    local file="$1"
    if command -v sha1sum >/dev/null 2>&1; then
        echo "$CEF_SHA1  $file" | sha1sum -c --status 2>/dev/null
    else
        echo "$CEF_SHA1  $file" | shasum -a 1 -c --status 2>/dev/null
    fi
}

prepare_sdk() {
    mkdir -p "$SDK_CACHE_DIR"
    if [[ -f "$SDK_READY" ]]; then
        return 0
    fi

    if [[ ! -f "$SDK_TARBALL" ]] || ! verify_sha1 "$SDK_TARBALL"; then
        TMP_TAR="$(mktemp "${SDK_CACHE_DIR}/cef.tar.bz2.XXXXXX")"
        echo "Downloading CEF SDK (${CEF_VERSION}) into ${SDK_CACHE_DIR}..."
        curl -fL --progress-bar -o "$TMP_TAR" "$CEF_URL"
        echo "Verifying CEF SDK SHA-1 checksum..."
        if ! verify_sha1 "$TMP_TAR"; then
            echo "Error: CEF SDK SHA-1 verification failed!" >&2
            exit 1
        fi
        mv -f "$TMP_TAR" "$SDK_TARBALL"
        TMP_TAR=""
    fi

    echo "Extracting CEF SDK into ${SDK_DIR}..."
    rm -rf "$SDK_DIR"
    tar xjf "$SDK_TARBALL" -C "$SDK_CACHE_DIR"
    touch "$SDK_READY"
}

mkdir -p "$SDK_CACHE_DIR"
if command -v flock >/dev/null 2>&1; then
    exec 9>"$SDK_LOCK_FILE"
    flock 9
    prepare_sdk
    flock -u 9
    exec 9>&-
else
    while ! mkdir "$SDK_LOCK_DIR" 2>/dev/null; do
        sleep 0.2
    done
    LOCK_HELD_DIR=1
    prepare_sdk
    release_sdk_lock
fi

cd "$INSTALL_DIR"

echo "Copying CEF runtime files..."
cp -f "$SDK_DIR/Release/libcef.so" .
cp -f "$SDK_DIR/Release/libEGL.so" . 2>/dev/null || true
cp -f "$SDK_DIR/Release/libGLESv2.so" . 2>/dev/null || true
cp -f "$SDK_DIR/Release/libvk_swiftshader.so" . 2>/dev/null || true
cp -f "$SDK_DIR/Release/libvulkan.so.1" . 2>/dev/null || true
cp -f "$SDK_DIR/Release/v8_context_snapshot.bin" .
cp -f "$SDK_DIR/Resources/icudtl.dat" .
cp -f "$SDK_DIR/Resources/resources.pak" .
cp -f "$SDK_DIR/Resources/chrome_100_percent.pak" .
cp -f "$SDK_DIR/Resources/chrome_200_percent.pak" .
rm -rf locales
cp -rf "$SDK_DIR/Resources/locales" .
strip libcef.so libEGL.so libGLESv2.so libvk_swiftshader.so libvulkan.so.1 2>/dev/null || true

echo "Compiling pleb-renderer and pleb-helper (${RENDERER_PROTOCOL})..."
gcc -o pleb-renderer "$RENDERER_SRC/renderer_linux.c" \
    -DCEF_API_VERSION="${CEF_API_VERSION}" \
    -I"$SDK_DIR" \
    -I"$RENDERER_SRC" \
    -L. -lcef \
    -Wl,-rpath,'$ORIGIN' \
    -pthread

gcc -o pleb-helper "$RENDERER_SRC/helper_linux.c" \
    -DCEF_API_VERSION="${CEF_API_VERSION}" \
    -I"$SDK_DIR" \
    -I"$RENDERER_SRC" \
    -L. -lcef \
    -Wl,-rpath,'$ORIGIN' \
    -pthread

strip pleb-renderer pleb-helper 2>/dev/null || true
chmod +x pleb-renderer pleb-helper

echo "Installed CEF renderer (${RENDERER_PROTOCOL}) at $INSTALL_DIR"
