#!/bin/bash
# install-cef-mac.sh: Build and install the CEF renderer for mesh-wick on macOS arm64.
#
# Usage:
#   bash scripts/install-cef-mac.sh [--force]
#
# Installs wick-renderer.app into ${MESH_WICK_HOME:-$HOME/.mesh-wick}/cef.

set -euo pipefail

FORCE=0
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        -h|--help)
            echo "Usage: bash scripts/install-cef-mac.sh [--force]"
            exit 0
            ;;
        *)
            echo "Unknown argument: $arg" >&2
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

if [[ ! -f "$RENDERER_SRC/renderer.m" ]]; then
    echo "Error: local CEF sources not found at $RENDERER_SRC" >&2
    exit 1
fi

STATE_HOME="${MESH_WICK_HOME:-$HOME/.mesh-wick}"
INSTALL_DIR="$STATE_HOME/cef"
CEF_VERSION="144.0.18+gc5b2ec2+chromium-144.0.7559.246"
CEF_PLATFORM="macosarm64"
CEF_DIR="cef_binary_${CEF_VERSION}_${CEF_PLATFORM}_minimal"
CEF_URL="https://cef-builds.spotifycdn.com/${CEF_DIR}.tar.bz2"
APP_DIR="$INSTALL_DIR/wick-renderer.app"
RENDERER_BIN="$APP_DIR/Contents/MacOS/wick-renderer"
RENDERER_PROTOCOL="v3-cf-aware"

mkdir -p "$INSTALL_DIR"

if [[ "$FORCE" -eq 0 && -f "$RENDERER_BIN" ]] && \
   grep -q "WICK_RENDERER_PROTOCOL=${RENDERER_PROTOCOL}" "$RENDERER_BIN" 2>/dev/null; then
    echo "CEF renderer already installed at $APP_DIR (use --force to rebuild)."
    exit 0
fi

TMP_BUILD_BIN="/tmp/mesh-wick-renderer-$$"
TMP_SDK_LINK="$RENDERER_SRC/$CEF_DIR"
TMP_TAR="$INSTALL_DIR/cef.tar.bz2"

cleanup() {
    rm -f "$TMP_BUILD_BIN" "$TMP_SDK_LINK" "$TMP_TAR"
}
trap cleanup EXIT

cd "$INSTALL_DIR"

if [[ ! -d "$INSTALL_DIR/$CEF_DIR" ]]; then
    echo "Downloading CEF SDK (${CEF_VERSION})..."
    curl -fL --progress-bar -o "$TMP_TAR" "$CEF_URL"
    tar xjf "$TMP_TAR" -C "$INSTALL_DIR"
    rm -f "$TMP_TAR"
fi

echo "Compiling wick-renderer..."
clang -o "$TMP_BUILD_BIN" "$RENDERER_SRC/renderer.m" \
    -DCEF_API_VERSION=14400 \
    -I"$INSTALL_DIR/$CEF_DIR" \
    -F"$INSTALL_DIR/$CEF_DIR/Release" \
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
    <string>wick-renderer</string>
    <key>CFBundleIdentifier</key>
    <string>io.mesh-wick.renderer</string>
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

echo "Copying Chromium Embedded Framework.framework..."
cp -R "$INSTALL_DIR/$CEF_DIR/Release/Chromium Embedded Framework.framework" \
    "$APP_DIR/Contents/Frameworks/"

echo "Building CEF helper bundles..."
ln -sfn "$INSTALL_DIR/$CEF_DIR" "$TMP_SDK_LINK"
bash "$RENDERER_SRC/setup-helpers.sh" "$APP_DIR/Contents/Frameworks"
rm -f "$TMP_SDK_LINK"

# Each helper .app looks in its own Contents/Frameworks via @executable_path/../Frameworks.
for helper in "$APP_DIR/Contents/Frameworks"/wick\ Helper*.app; do
    mkdir -p "$helper/Contents/Frameworks"
    ln -sf "../../../Chromium Embedded Framework.framework" \
        "$helper/Contents/Frameworks/Chromium Embedded Framework.framework"
done

echo "Cleaning up unpacked CEF SDK..."
rm -rf "$INSTALL_DIR/$CEF_DIR"

echo "Installed CEF renderer at $APP_DIR"
