#!/bin/bash
set -e

# TurboCI Uninstaller

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

  Uninstaller
EOF
echo -e "${NC}"

# Remove binary
remove_binary() {
    if [ -f "$INSTALL_DIR/$BIN_NAME" ]; then
        echo -e "${YELLOW}🗑️  Removing TurboCI binary...${NC}"
        rm -f "$INSTALL_DIR/$BIN_NAME"
        echo -e "${GREEN}✓${NC} Removed: ${BLUE}$INSTALL_DIR/$BIN_NAME${NC}"
    else
        echo -e "${YELLOW}⚠️  Binary not found at: $INSTALL_DIR/$BIN_NAME${NC}"
    fi
}

# Clean up PATH entries
clean_path() {
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

    if [ -n "$SHELL_CONFIG" ] && [ -f "$SHELL_CONFIG" ]; then
        if grep -q "# TurboCI" "$SHELL_CONFIG" 2>/dev/null; then
            echo -e "${YELLOW}📝 Cleaning PATH entries from $SHELL_CONFIG...${NC}"
            
            # Remove TurboCI PATH entries
            sed -i.bak '/# TurboCI/d' "$SHELL_CONFIG"
            sed -i.bak "\|export PATH=\"$INSTALL_DIR:\$PATH\"|d" "$SHELL_CONFIG"
            
            # Remove backup
            rm -f "$SHELL_CONFIG.bak"
            
            echo -e "${GREEN}✓${NC} Cleaned PATH entries"
            echo -e "${YELLOW}⚠️  Run: ${BLUE}source $SHELL_CONFIG${NC}"
        fi
    fi
}

# Remove config and cache (optional)
remove_data() {
    echo -e "\n${YELLOW}📦 Do you want to remove TurboCI data? (config, cache, etc.)${NC}"
    echo -e "   This will remove:"
    echo -e "   - ~/.turboci/ (if exists)"
    echo -e "   - runner-config.toml (if exists)"
    read -p "   Remove data? [y/N]: " -n 1 -r
    echo

    if [[ $REPLY =~ ^[Yy]$ ]]; then
        if [ -d "$HOME/.turboci" ]; then
            rm -rf "$HOME/.turboci"
            echo -e "${GREEN}✓${NC} Removed: ~/.turboci/"
        fi
        
        if [ -f "$PWD/runner-config.toml" ]; then
            rm -f "$PWD/runner-config.toml"
            echo -e "${GREEN}✓${NC} Removed: runner-config.toml"
        fi
        
        if [ -f "$HOME/runner-config.toml" ]; then
            rm -f "$HOME/runner-config.toml"
            echo -e "${GREEN}✓${NC} Removed: ~/runner-config.toml"
        fi
    else
        echo -e "${BLUE}ℹ️  Keeping TurboCI data${NC}"
    fi
}

# Verify removal
verify_removal() {
    echo -e "\n${YELLOW}🔍 Verifying removal...${NC}"

    if command -v turboci &> /dev/null; then
        echo -e "${YELLOW}⚠️  TurboCI is still in PATH. Restart your terminal.${NC}"
    else
        echo -e "${GREEN}✓${NC} TurboCI command not found"
    fi

    if [ ! -f "$INSTALL_DIR/$BIN_NAME" ]; then
        echo -e "${GREEN}✓${NC} Binary removed successfully"
    else
        echo -e "${RED}❌ Binary still exists${NC}"
    fi
}

# Main uninstall
main() {
    echo -e "${BLUE}Starting TurboCI uninstallation...${NC}\n"

    remove_binary
    clean_path
    remove_data
    verify_removal

    echo -e "\n${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${GREEN}✨ TurboCI uninstalled successfully!${NC}"
    echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "\n${BLUE}💡 To reinstall:${NC}"
    echo -e "   ${YELLOW}curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/install.sh | bash${NC}"
    echo ""
}

main "$@"
