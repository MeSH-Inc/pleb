#!/bin/bash
# Download prebuilt libcronet.a for macOS arm64 from the sagernet/cronet-go 'go' branch.
# This is a ~48MB static library built from Chromium 154.0.8037.49.

set -euo pipefail

ARCH=$(uname -m)
# Pinned commit on the 'go' branch for reproducibility (Chromium 154.0.8037.49)
COMMIT="2767273ce7ab"
SHA256="d25cd5f048901cc16abbf875a9ca79ad542656e34c44d184acf4ef8823733be9"

case "$ARCH" in
    arm64)  DIR="darwin_arm64" ;;
    *)      echo "Error: static libcronet.a is only supported for macOS arm64 (Apple Silicon)."
            echo "Build without --features cronet to use the reqwest fallback."
            exit 1 ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
DEST="${REPO_ROOT}/lib/${DIR}"
mkdir -p "$DEST"

if [ -f "$DEST/libcronet.a" ]; then
    echo "libcronet.a already exists at $DEST/libcronet.a"
    echo "Verifying checksum..."
    if echo "$SHA256  $DEST/libcronet.a" | shasum -a 256 -c --status 2>/dev/null; then
        echo "Checksum OK."
        exit 0
    else
        echo "Checksum mismatch — re-downloading."
        rm -f "$DEST/libcronet.a"
    fi
fi

echo "Downloading libcronet.a for macOS arm64 (~40MB)..."
curl -fL --progress-bar -o "$DEST/libcronet.a" \
    "https://raw.githubusercontent.com/SagerNet/cronet-go/${COMMIT}/lib/${DIR}/libcronet.a"

echo "Verifying checksum..."
if ! echo "$SHA256  $DEST/libcronet.a" | shasum -a 256 -c --status 2>/dev/null; then
    echo "Error: SHA256 checksum verification failed!"
    echo "Expected: $SHA256"
    echo "Got:      $(shasum -a 256 "$DEST/libcronet.a" | cut -d' ' -f1)"
    rm -f "$DEST/libcronet.a"
    exit 1
fi

echo "Checksum OK."
echo "Saved to $DEST/libcronet.a"
echo ""
echo "Install with: cargo install --path . --features cronet --locked"
