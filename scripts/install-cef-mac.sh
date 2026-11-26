#!/bin/bash
# install-cef-mac.sh: Build and install the CEF renderer for pleb on macOS arm64.
#
# Usage:
#   bash scripts/install-cef-mac.sh [--force] [--prefix <dir>]
#
# Installs pleb-renderer.app into --prefix <dir> (default: ${PLEB_HOME:-$HOME/.pleb}/cef).

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
            echo "Usage: bash scripts/install-cef-mac.sh [--force] [--prefix <dir>]"
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

if [[ "$(uname)" != "Darwin" ]]; then
    echo "Error: install-cef-mac.sh is for macOS only." >&2
    exit 1
fi

if [[ "$(uname -m)" != "arm64" ]]; then
    echo "Error: only Apple Silicon (arm64) is supported." >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RENDERER_SRC="$(cd "$SCRIPT_DIR/../cef" && pwd)"
PROTOCOL_HEADER="$RENDERER_SRC/protocol.h"

if [[ ! -f "$RENDERER_SRC/renderer.m" ]]; then
    echo "Error: local CEF sources not found at $RENDERER_SRC" >&2
    exit 1
fi

if [[ ! -f "$PROTOCOL_HEADER" ]]; then
    echo "Error: protocol header not found at $PROTOCOL_HEADER" >&2
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
CEF_PLATFORM="macosarm64"
CEF_DIR="cef_binary_${CEF_VERSION}_${CEF_PLATFORM}_minimal"
CEF_URL="https://cef-builds.spotifycdn.com/${CEF_DIR}.tar.bz2"
CEF_SHA1="b7081f6609e5edccf07be96b5d7977329fbe1bdf"

SDK_CACHE_DIR="${HOME}/.cache/pleb/cef-sdk"
SDK_TARBALL="${SDK_CACHE_DIR}/${CEF_DIR}.tar.bz2"
SDK_DIR="${SDK_CACHE_DIR}/${CEF_DIR}"
SDK_READY="${SDK_DIR}/.extracted-ok"
SDK_LOCK_FILE="${SDK_CACHE_DIR}/.lock"
SDK_LOCK_DIR="${SDK_CACHE_DIR}/.lock.d"

APP_DIR="$INSTALL_DIR/pleb-renderer.app"
RENDERER_BIN="$APP_DIR/Contents/MacOS/pleb-renderer"

mkdir -p "$INSTALL_DIR"
if [[ -z "$PREFIX" ]]; then
    chmod 700 "$STATE_HOME" "$INSTALL_DIR" 2>/dev/null || true
else
    chmod 700 "$INSTALL_DIR" 2>/dev/null || true
fi

if [[ "$FORCE" -eq 0 && -f "$RENDERER_BIN" ]] && \
   grep -q "PLEB_RENDERER_PROTOCOL=${RENDERER_PROTOCOL}" "$RENDERER_BIN" 2>/dev/null; then
    echo "CEF renderer (${RENDERER_PROTOCOL}) already installed at $APP_DIR (use --force to rebuild)."
    exit 0
fi

TMP_BUILD_BIN="$(mktemp "${TMPDIR:-/tmp}/pleb-renderer.XXXXXX")"
TMP_TAR=""
LOCK_HELD_DIR=0

release_sdk_lock() {
    if [[ "$LOCK_HELD_DIR" -eq 1 ]]; then
        rmdir "$SDK_LOCK_DIR" 2>/dev/null || true
        LOCK_HELD_DIR=0
    fi
}

cleanup() {
    rm -f "$TMP_BUILD_BIN"
    if [[ -n "$TMP_TAR" ]]; then
        rm -f "$TMP_TAR"
    fi
    release_sdk_lock
}
trap cleanup EXIT

verify_sha1() {
    local file="$1"
    echo "$CEF_SHA1  $file" | shasum -a 1 -c --status 2>/dev/null
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
            echo "Expected: $CEF_SHA1" >&2
            echo "Got:      $(shasum -a 1 "$TMP_TAR" | cut -d' ' -f1)" >&2
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

echo "Compiling pleb-renderer (${RENDERER_PROTOCOL})..."
clang -o "$TMP_BUILD_BIN" "$RENDERER_SRC/renderer.m" \
    -DCEF_API_VERSION="${CEF_API_VERSION}" \
    -I"$RENDERER_SRC" \
    -I"$SDK_DIR" \
    -F"$SDK_DIR/Release" \
    -framework "Chromium Embedded Framework" \
    -framework Cocoa \
    -rpath "@executable_path/../Frameworks" \
    -fobjc-arc

rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Frameworks"

cat > "$APP_DIR/Contents/Info.plist" << 'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>pleb-renderer</string>
    <key>CFBundleIdentifier</key>
    <string>com.memorysynchub.pleb.renderer</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSUIElement</key>
    <true/>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
    <key>LSEnvironment</key>
    <dict>
        <key>MallocNanoZone</key>
        <string>0</string>
    </dict>
</dict>
</plist>
PLIST

echo -n "APPL????" > "$APP_DIR/Contents/PkgInfo"
mv "$TMP_BUILD_BIN" "$RENDERER_BIN"
chmod +x "$RENDERER_BIN"

echo "Copying Chromium Embedded Framework.framework..."
cp -R "$SDK_DIR/Release/Chromium Embedded Framework.framework" \
    "$APP_DIR/Contents/Frameworks/"

echo "Building CEF helper bundles..."
bash "$RENDERER_SRC/setup-helpers.sh" "$APP_DIR/Contents/Frameworks" "$SDK_DIR" "$CEF_API_VERSION"

# Each helper .app looks in its own Contents/Frameworks via @executable_path/../Frameworks.
for helper in "$APP_DIR/Contents/Frameworks"/pleb\ Helper*.app; do
    mkdir -p "$helper/Contents/Frameworks"
    ln -sf "../../../Chromium Embedded Framework.framework" \
        "$helper/Contents/Frameworks/Chromium Embedded Framework.framework"
done

echo "Installed CEF renderer (${RENDERER_PROTOCOL}) at $APP_DIR"
