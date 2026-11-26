#!/bin/bash
# Download prebuilt libcronet.so for Linux from SagerNet releases.
set -euo pipefail

CRONET_VERSION="143.0.7499.109-2"
RELEASE_URL="https://github.com/SagerNet/cronet-go/releases/download/${CRONET_VERSION}"

ARCH=$(uname -m)
case "$ARCH" in
    x86_64)
        DIR="linux_amd64"
        LIB="libcronet-linux-amd64.so"
        SHA256="436fbbea68293ca4b844e0d24bd61dc7e7d55ef08af47ab85b6d0f99f05f2129"
        ;;
    aarch64)
        DIR="linux_arm64"
        LIB="libcronet-linux-arm64.so"
        SHA256="288341d94a086696e586002044f3167b4930867458d96741e19e8046429ae45c"
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

DEST="lib/${DIR}"
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

echo "Downloading ${LIB} for Linux ${ARCH}..."
curl -fL --progress-bar -o "$DEST/libcronet.so" "${RELEASE_URL}/${LIB}"

echo "Verifying checksum..."
if ! verify_sha256 "$DEST/libcronet.so"; then
    echo "Error: SHA256 checksum verification failed!" >&2
    rm -f "$DEST/libcronet.so"
    exit 1
fi

echo "Checksum OK."
echo "Saved to $DEST/libcronet.so"
