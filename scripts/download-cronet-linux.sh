#!/bin/bash
# Download prebuilt libcronet.so for Linux from the sagernet/cronet-go 'go' branch.
# Built from Chromium 154.0.8037.49 (matching macOS libcronet.a and src/transport/mod.rs).
set -euo pipefail

COMMIT="2767273ce7ab"
BASE_URL="https://raw.githubusercontent.com/SagerNet/cronet-go/${COMMIT}/lib"

ARCH=$(uname -m)
case "$ARCH" in
    x86_64)
        DIR="linux_amd64"
        SHA256="fcc7c263c15e4276a315dc094f584f8737874387d06830fd9c29e3acf7a44aa9"
        ;;
    aarch64)
        DIR="linux_arm64"
        SHA256="9633c2d0e1e4a41d9ba5da72810d87d6211e12cb6ce126ed5043f288023196b9"
        ;;
    *)
        echo "Error: unsupported architecture: $ARCH" >&2
        exit 1
        ;;
esac

verify_sha256() {
    local file="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        echo "$SHA256  $file" | sha256sum -c --status 2>/dev/null
    else
        echo "$SHA256  $file" | shasum -a 256 -c --status 2>/dev/null
    fi
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
DEST="${REPO_ROOT}/lib/${DIR}"
mkdir -p "$DEST"

if [ -f "$DEST/libcronet.so" ]; then
    echo "libcronet.so already exists at $DEST/libcronet.so"
    echo "Verifying checksum..."
    if verify_sha256 "$DEST/libcronet.so"; then
        echo "Checksum OK."
        exit 0
    else
        echo "Checksum mismatch — re-downloading."
        rm -f "$DEST/libcronet.so"
    fi
fi

echo "Downloading libcronet.so for Linux ${ARCH} (Chromium 154.0.8037.49)..."
curl -fL --progress-bar -o "$DEST/libcronet.so" "${BASE_URL}/${DIR}/libcronet.so"

echo "Verifying checksum..."
if ! verify_sha256 "$DEST/libcronet.so"; then
    echo "Error: SHA256 checksum verification failed!" >&2
    rm -f "$DEST/libcronet.so"
    exit 1
fi

echo "Checksum OK."
echo "Saved to $DEST/libcronet.so"
echo ""
echo "Install with: cargo install --path . --features cronet --locked"
echo "Note: Linux binaries embed rpath \$ORIGIN and $DEST; if moving pleb without keeping the checkout, copy libcronet.so beside the binary or set LD_LIBRARY_PATH=$DEST."
