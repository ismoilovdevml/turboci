#!/bin/bash

# TurboCI Installation Script

set -e

VERSION="0.1.0"
REPO="turboci/turboci"

echo "⚡ Installing TurboCI v${VERSION}..."

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$ARCH" in
    x86_64)
        ARCH="x86_64"
        ;;
    aarch64|arm64)
        ARCH="aarch64"
        ;;
    *)
        echo "❌ Unsupported architecture: $ARCH"
        exit 1
        ;;
esac

case "$OS" in
    darwin)
        OS="apple-darwin"
        ;;
    linux)
        OS="unknown-linux-gnu"
        ;;
    *)
        echo "❌ Unsupported OS: $OS"
        exit 1
        ;;
esac

BINARY="turboci-${ARCH}-${OS}"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/v${VERSION}/${BINARY}"

echo "📥 Downloading TurboCI for ${ARCH}-${OS}..."

# Download binary
if command -v curl &> /dev/null; then
    curl -sSL "$DOWNLOAD_URL" -o turboci
elif command -v wget &> /dev/null; then
    wget -q "$DOWNLOAD_URL" -O turboci
else
    echo "❌ curl or wget is required"
    exit 1
fi

# Make executable
chmod +x turboci

# Move to PATH
INSTALL_DIR="/usr/local/bin"
if [ -w "$INSTALL_DIR" ]; then
    mv turboci "$INSTALL_DIR/turboci"
    echo "✅ TurboCI installed to $INSTALL_DIR/turboci"
else
    echo "🔑 Need sudo permission to install to $INSTALL_DIR"
    sudo mv turboci "$INSTALL_DIR/turboci"
    echo "✅ TurboCI installed to $INSTALL_DIR/turboci"
fi

# Verify installation
if command -v turboci &> /dev/null; then
    echo ""
    echo "🎉 TurboCI successfully installed!"
    echo ""
    echo "Quick start:"
    echo "  1. Create turboci.yml config file"
    echo "  2. Start Redis: docker run -d -p 6379:6379 redis:alpine"
    echo "  3. Run: turboci run"
    echo ""
    echo "For help: turboci --help"
else
    echo "❌ Installation failed"
    exit 1
fi
