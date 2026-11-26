#!/bin/bash
# Creates the CEF helper app bundles for macOS multi-process mode.
# All helpers use the same binary; only the bundle ID differs.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DEST="${1:-$SCRIPT_DIR/Frameworks}"
CEF_DIR="${2:-$(ls -d "$SCRIPT_DIR"/cef_binary_144* 2>/dev/null | head -1 || true)}"

if [ -z "$CEF_DIR" ] || [ ! -d "$CEF_DIR" ]; then
    echo "Error: CEF distribution not found (checked '$CEF_DIR')" >&2
    exit 1
fi

TMP_HELPER="$(mktemp "${TMPDIR:-/tmp}/mesh-wick-helper.XXXXXX")"
cleanup() {
    rm -f "$TMP_HELPER"
}
trap cleanup EXIT

echo "Setting up CEF helper bundles in $DEST..."

# Compile helper binary
echo "Compiling wick Helper..."
clang -DCEF_API_VERSION=14400 \
    -o "$TMP_HELPER" \
    "$SCRIPT_DIR/helper.m" \
    -I"$SCRIPT_DIR" \
    -I"$CEF_DIR" \
    -F"$CEF_DIR/Release" \
    -framework "Chromium Embedded Framework" \
    -framework Cocoa \
    -fobjc-arc

# Create Frameworks directory and copy the CEF framework if not already copied
mkdir -p "$DEST"
if [ ! -e "$DEST/Chromium Embedded Framework.framework" ]; then
    cp -R "$CEF_DIR/Release/Chromium Embedded Framework.framework" "$DEST/"
fi

# Helper variants: "suffix:bundle_id_suffix"
HELPERS=(
    "::"
    " (GPU):.gpu"
    " (Renderer):.renderer"
    " (Alerts):.alerts"
    " (Plugin):.plugin"
)

for entry in "${HELPERS[@]}"; do
    IFS=: read -r suffix bid_suffix <<< "$entry"
    name="wick Helper${suffix}"
    bundle_id="io.mesh-wick.renderer.helper${bid_suffix}"
    bundle_dir="$DEST/${name}.app/Contents"

    echo "  Creating ${name}.app (${bundle_id})"
    mkdir -p "$bundle_dir/MacOS"

    # Use the same binary for all helpers (copy for first, hardlink for rest)
    if [ -z "$suffix" ]; then
        cp "$TMP_HELPER" "$bundle_dir/MacOS/$name"
        chmod +x "$bundle_dir/MacOS/$name"
    else
        ln -f "$DEST/wick Helper.app/Contents/MacOS/wick Helper" "$bundle_dir/MacOS/$name" 2>/dev/null \
            || { cp "$TMP_HELPER" "$bundle_dir/MacOS/$name" && chmod +x "$bundle_dir/MacOS/$name"; }
    fi

    # Info.plist
    cat > "$bundle_dir/Info.plist" << PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>${name}</string>
    <key>CFBundleIdentifier</key>
    <string>${bundle_id}</string>
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

    # PkgInfo
    echo -n "APPL????" > "$bundle_dir/PkgInfo"
done

echo "Done. Helper bundles created in $DEST"
echo ""
echo "Bundles:"
ls -d "$DEST"/*.app 2>/dev/null | while read -r d; do basename "$d"; done
