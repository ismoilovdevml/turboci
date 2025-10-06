#!/bin/bash
set -e

# TurboCI Installer
# Supports macOS (Intel/Apple Silicon) and Linux (x86_64/ARM64)

REPO="ismoilovdevml/turboci"
INSTALL_DIR="${TURBOCI_INSTALL_DIR:-$HOME/.local/bin}"
BIN_NAME="turboci"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${BLUE}"
cat << "EOF"
  _____           _          ____ ___
 |_   _|   _ _ __| |__   ___/ ___|_ _|
   | || | | | '__| '_ \ / _ \___ \| |
   | || |_| | |  | |_) | (_) |__) | |
   |_| \__,_|_|  |_.__/ \___/____/___|

  ⚡ Super Fast CI/CD Runner
EOF
echo -e "${NC}"

# Detect OS and architecture
detect_platform() {
    local OS=$(uname -s | tr '[:upper:]' '[:lower:]')
    local ARCH=$(uname -m)

    case "$OS" in
        linux*)
            OS="linux"
            ;;
        darwin*)
            OS="darwin"
            ;;
        *)
            echo -e "${RED}❌ Unsupported OS: $OS${NC}"
            exit 1
            ;;
    esac

    case "$ARCH" in
        x86_64|amd64)
            ARCH="x86_64"
            ;;
        aarch64|arm64)
            ARCH="aarch64"
            ;;
        *)
            echo -e "${RED}❌ Unsupported architecture: $ARCH${NC}"
            exit 1
            ;;
    esac

    # Determine target triple
    if [ "$OS" = "linux" ]; then
        TARGET="${ARCH}-unknown-linux-musl"
    elif [ "$OS" = "darwin" ]; then
        if [ "$ARCH" = "aarch64" ]; then
            TARGET="aarch64-apple-darwin"
        else
            TARGET="x86_64-apple-darwin"
        fi
    fi

    echo -e "${GREEN}✓${NC} Detected platform: ${BLUE}$TARGET${NC}"
}

# Get latest release version
get_latest_version() {
    echo -e "${YELLOW}📡 Fetching latest release...${NC}"

    LATEST_RELEASE=$(curl -s "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')

    if [ -z "$LATEST_RELEASE" ]; then
        echo -e "${RED}❌ Failed to fetch latest release${NC}"
        exit 1
    fi

    echo -e "${GREEN}✓${NC} Latest version: ${BLUE}$LATEST_RELEASE${NC}"
}

# Download and install
install_turboci() {
    local DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_RELEASE/turboci-$TARGET.tar.gz"
    local TEMP_DIR=$(mktemp -d)
    local ARCHIVE="$TEMP_DIR/turboci.tar.gz"

    echo -e "${YELLOW}📥 Downloading TurboCI...${NC}"
    echo -e "   URL: $DOWNLOAD_URL"

    if ! curl -fsSL "$DOWNLOAD_URL" -o "$ARCHIVE"; then
        echo -e "${RED}❌ Download failed${NC}"
        rm -rf "$TEMP_DIR"
        exit 1
    fi

    echo -e "${GREEN}✓${NC} Downloaded successfully"

    # Create install directory
    mkdir -p "$INSTALL_DIR"

    # Extract and install
    echo -e "${YELLOW}📦 Installing to $INSTALL_DIR...${NC}"
    tar -xzf "$ARCHIVE" -C "$TEMP_DIR"

    # Make executable and move
    chmod +x "$TEMP_DIR/$BIN_NAME"
    mv "$TEMP_DIR/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"

    # Cleanup
    rm -rf "$TEMP_DIR"

    echo -e "${GREEN}✓${NC} Installed to: ${BLUE}$INSTALL_DIR/$BIN_NAME${NC}"
}

# Update PATH
update_path() {
    local SHELL_CONFIG=""

    # Detect shell
    case "$SHELL" in
        */bash)
            SHELL_CONFIG="$HOME/.bashrc"
            ;;
        */zsh)
            SHELL_CONFIG="$HOME/.zshrc"
            ;;
        */fish)
            SHELL_CONFIG="$HOME/.config/fish/config.fish"
            ;;
    esac

    # Check if already in PATH
    if echo "$PATH" | grep -q "$INSTALL_DIR"; then
        echo -e "${GREEN}✓${NC} $INSTALL_DIR is already in PATH"
        return
    fi

    if [ -n "$SHELL_CONFIG" ] && [ -f "$SHELL_CONFIG" ]; then
        if ! grep -q "export PATH=\"\$INSTALL_DIR:\$PATH\"" "$SHELL_CONFIG" 2>/dev/null; then
            echo -e "${YELLOW}📝 Adding $INSTALL_DIR to PATH in $SHELL_CONFIG${NC}"
            echo "" >> "$SHELL_CONFIG"
            echo "# TurboCI" >> "$SHELL_CONFIG"
            echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$SHELL_CONFIG"
            echo -e "${GREEN}✓${NC} Added to PATH. Run: ${BLUE}source $SHELL_CONFIG${NC}"
        fi
    else
        echo -e "${YELLOW}⚠️  Please manually add $INSTALL_DIR to your PATH${NC}"
        echo -e "   Add this to your shell config:"
        echo -e "   ${BLUE}export PATH=\"$INSTALL_DIR:\$PATH\"${NC}"
    fi
}

# Verify installation
verify_installation() {
    echo -e "\n${YELLOW}🔍 Verifying installation...${NC}"

    if [ -x "$INSTALL_DIR/$BIN_NAME" ]; then
        echo -e "${GREEN}✓${NC} TurboCI installed successfully!"

        # Try to run version check (if already in PATH)
        if command -v turboci &> /dev/null; then
            VERSION=$(turboci --version 2>/dev/null || echo "unknown")
            echo -e "${GREEN}✓${NC} Version: ${BLUE}$VERSION${NC}"
        else
            echo -e "${YELLOW}⚠️  Restart your terminal or run: ${BLUE}source ~/.bashrc${NC} (or ~/.zshrc)"
        fi
    else
        echo -e "${RED}❌ Installation verification failed${NC}"
        exit 1
    fi
}

# Main installation
main() {
    echo -e "${BLUE}Starting TurboCI installation...${NC}\n"

    detect_platform
    get_latest_version
    install_turboci
    update_path
    verify_installation

    echo -e "\n${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${GREEN}✨ TurboCI $LATEST_RELEASE installed successfully!${NC}"
    echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "\n${BLUE}🚀 Quick Start:${NC}"
    echo -e "   ${YELLOW}turboci --help${NC}              # Show help"
    echo -e "   ${YELLOW}turboci hash${NC}                # Compute content hash"
    echo -e "   ${YELLOW}turboci init-cache${NC}          # Initialize cache"
    echo -e "   ${YELLOW}turboci init-runner${NC}         # Create runner config"
    echo -e "\n${BLUE}📚 Documentation:${NC}"
    echo -e "   https://github.com/$REPO"
    echo -e "\n${BLUE}🗑️  Uninstall:${NC}"
    echo -e "   ${YELLOW}curl -sSL https://raw.githubusercontent.com/$REPO/main/uninstall.sh | bash${NC}"
    echo ""
}

main "$@"
