#!/bin/bash
set -e

# TurboCI Uninstaller
# Removes: TurboCI + Systemd Service + Config
# Option: Remove Redis (if not used by other apps)

SERVICE_NAME="turboci"
BIN_NAME="turboci"
INSTALL_DIR="/usr/local/bin"
CONFIG_FILE="/etc/turboci-runner.toml"
SERVICE_FILE="/etc/systemd/system/$SERVICE_NAME.service"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# Check if running as root
if [ "$EUID" -ne 0 ]; then
    echo -e "${RED}❌ Please run as root: sudo $0${NC}"
    exit 1
fi

echo -e "${RED}"
cat << "EOF"
  _____           _          ____ ___
 |_   _|   _ _ __| |__   ___/ ___|_ _|
   | || | | | '__| '_ \ / _ \___ \| |
   | || |_| | |  | |_) | (_) |__) | |
   |_| \__,_|_|  |_.__/ \___/____/___|

  🗑️  TurboCI Uninstaller
EOF
echo -e "${NC}"

# Stop and disable service
remove_service() {
    echo -e "${YELLOW}🛑 Stopping service...${NC}"

    if systemctl is-active --quiet $SERVICE_NAME 2>/dev/null; then
        systemctl stop $SERVICE_NAME
        echo -e "${GREEN}✓${NC} Service stopped"
    else
        echo -e "${BLUE}ℹ️  Service not running${NC}"
    fi

    if systemctl is-enabled --quiet $SERVICE_NAME 2>/dev/null; then
        systemctl disable $SERVICE_NAME
        echo -e "${GREEN}✓${NC} Service disabled"
    fi

    if [ -f "$SERVICE_FILE" ]; then
        rm -f "$SERVICE_FILE"
        systemctl daemon-reload
        echo -e "${GREEN}✓${NC} Service file removed"
    fi
}

# Remove binary
remove_binary() {
    echo -e "\n${YELLOW}🗑️  Removing binary...${NC}"

    if [ -f "$INSTALL_DIR/$BIN_NAME" ]; then
        rm -f "$INSTALL_DIR/$BIN_NAME"
        echo -e "${GREEN}✓${NC} Binary removed: $INSTALL_DIR/$BIN_NAME"
    else
        echo -e "${BLUE}ℹ️  Binary not found${NC}"
    fi
}

# Remove config
remove_config() {
    echo -e "\n${YELLOW}📝 Removing configuration...${NC}"

    if [ -f "$CONFIG_FILE" ]; then
        # Backup config before removing
        BACKUP_FILE="${CONFIG_FILE}.backup.$(date +%Y%m%d-%H%M%S)"
        cp "$CONFIG_FILE" "$BACKUP_FILE"
        echo -e "${BLUE}ℹ️  Config backed up to: $BACKUP_FILE${NC}"

        rm -f "$CONFIG_FILE"
        echo -e "${GREEN}✓${NC} Config removed: $CONFIG_FILE"
    else
        echo -e "${BLUE}ℹ️  Config not found${NC}"
    fi
}

# Remove work directory
remove_workdir() {
    echo -e "\n${YELLOW}📁 Removing work directory...${NC}"

    WORK_DIR="/tmp/turboci-builds"
    if [ -d "$WORK_DIR" ]; then
        rm -rf "$WORK_DIR"
        echo -e "${GREEN}✓${NC} Work directory removed: $WORK_DIR"
    else
        echo -e "${BLUE}ℹ️  Work directory not found${NC}"
    fi
}

# Ask about Redis
ask_remove_redis() {
    echo -e "\n${YELLOW}❓ Remove Redis?${NC}"
    echo -e "${BLUE}Redis might be used by other applications.${NC}"
    read -p "Remove Redis? (y/N): " -n 1 -r
    echo

    if [[ $REPLY =~ ^[Yy]$ ]]; then
        remove_redis
    else
        echo -e "${BLUE}ℹ️  Keeping Redis${NC}"
    fi
}

# Remove Redis
remove_redis() {
    echo -e "\n${YELLOW}🗑️  Removing Redis...${NC}"

    # Detect OS
    if [ -f /etc/os-release ]; then
        . /etc/os-release
        OS_ID=$ID
    else
        echo -e "${YELLOW}⚠️  Cannot detect OS, skipping Redis removal${NC}"
        return
    fi

    case "$OS_ID" in
        ubuntu|debian)
            REDIS_SERVICE="redis-server"
            systemctl stop $REDIS_SERVICE 2>/dev/null || true
            systemctl disable $REDIS_SERVICE 2>/dev/null || true
            apt-get remove -y redis-server redis-tools 2>/dev/null || true
            apt-get autoremove -y 2>/dev/null || true
            ;;
        rhel|rocky|almalinux|fedora|centos)
            REDIS_SERVICE="redis"
            systemctl stop $REDIS_SERVICE 2>/dev/null || true
            systemctl disable $REDIS_SERVICE 2>/dev/null || true
            if command -v dnf &> /dev/null; then
                dnf remove -y redis 2>/dev/null || true
            else
                yum remove -y redis 2>/dev/null || true
            fi
            ;;
        *)
            echo -e "${YELLOW}⚠️  Unsupported OS for Redis removal: $OS_ID${NC}"
            return
            ;;
    esac

    echo -e "${GREEN}✓${NC} Redis removed"
}

# Print summary
print_summary() {
    echo -e "\n${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${GREEN}✨ TurboCI Uninstalled Successfully!${NC}"
    echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    echo -e "\n${BLUE}📋 Removed:${NC}"
    echo -e "   ✓ TurboCI service"
    echo -e "   ✓ TurboCI binary"
    echo -e "   ✓ Configuration (backed up)"
    echo -e "   ✓ Work directory"

    echo -e "\n${BLUE}📚 To reinstall:${NC}"
    echo -e "   ${BLUE}curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/install.sh | sudo bash${NC}"

    echo ""
}

# Main
main() {
    echo -e "${BLUE}Starting uninstallation...${NC}\n"

    remove_service
    remove_binary
    remove_config
    remove_workdir
    ask_remove_redis
    print_summary
}

main "$@"
