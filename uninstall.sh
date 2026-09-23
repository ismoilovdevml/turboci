#!/bin/bash
set -e

# TurboCI Uninstaller
# Removes: service, binary, config (a root-only backup is kept), work and state
# directories, the turboci user, and leftover job containers/networks.
# Docker is left installed.

SERVICE_NAME="turboci"
BIN_NAME="turboci"
INSTALL_DIR="/usr/local/bin"
CONFIG_FILE="/etc/turboci-runner.toml"
SERVICE_FILE="/etc/systemd/system/$SERVICE_NAME.service"
TMPFILES_FILE="/etc/tmpfiles.d/turboci.conf"
SERVICE_USER="turboci"
STATE_DIR="/var/lib/turboci"

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

# Remove config, keeping a root-only backup (it contains the runner token)
remove_config() {
    echo -e "\n${YELLOW}📝 Removing configuration...${NC}"

    if [ -f "$CONFIG_FILE" ]; then
        BACKUP_FILE="${CONFIG_FILE}.backup.$(date +%Y%m%d-%H%M%S)"
        install -m 0600 -o root -g root "$CONFIG_FILE" "$BACKUP_FILE"
        echo -e "${BLUE}ℹ️  Config backed up to: $BACKUP_FILE (root only)${NC}"

        rm -f "$CONFIG_FILE"
        echo -e "${GREEN}✓${NC} Config removed: $CONFIG_FILE"
    else
        echo -e "${BLUE}ℹ️  Config not found${NC}"
    fi
    # Backups made by install.sh when replacing the config also hold tokens
    for old in "$CONFIG_FILE".bak.*; do
        [ -e "$old" ] || continue
        chown root:root "$old" && chmod 0600 "$old"
    done
}

# Remove job containers and networks left by a crash
remove_job_containers() {
    command -v docker > /dev/null 2>&1 || return 0
    echo -e "\n${YELLOW}🐳 Removing leftover job containers...${NC}"

    CONTAINERS=$(docker ps -aq --filter "name=^turboci-job-" 2>/dev/null || true)
    if [ -n "$CONTAINERS" ]; then
        echo "$CONTAINERS" | xargs docker rm -f > /dev/null
        echo -e "${GREEN}✓${NC} Removed $(echo "$CONTAINERS" | wc -l) container(s)"
    fi
    NETWORKS=$(docker network ls -q --filter "name=^turboci-job-" 2>/dev/null || true)
    if [ -n "$NETWORKS" ]; then
        echo "$NETWORKS" | xargs docker network rm > /dev/null
        echo -e "${GREEN}✓${NC} Removed $(echo "$NETWORKS" | wc -l) network(s)"
    fi
}

# Remove work and state directories (job workspaces and the local cache)
remove_workdir() {
    echo -e "\n${YELLOW}📁 Removing work directories...${NC}"

    for dir in /tmp/turboci-builds /tmp/turboci "$STATE_DIR"; do
        if [ -d "$dir" ]; then
            rm -rf "$dir"
            echo -e "${GREEN}✓${NC} Removed: $dir"
        fi
    done
    if [ -f "$TMPFILES_FILE" ]; then
        rm -f "$TMPFILES_FILE"
        echo -e "${GREEN}✓${NC} Removed: $TMPFILES_FILE"
    fi
}

# Remove the service user
remove_user() {
    if id -u "$SERVICE_USER" > /dev/null 2>&1; then
        userdel "$SERVICE_USER"
        echo -e "\n${GREEN}✓${NC} User removed: $SERVICE_USER"
    fi
}

# Print summary
print_summary() {
    echo -e "\n${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${GREEN}✨ TurboCI Uninstalled Successfully!${NC}"
    echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    echo -e "\n${BLUE}📋 Removed:${NC}"
    echo -e "   ✓ TurboCI service"
    echo -e "   ✓ TurboCI binary"
    echo -e "   ✓ Configuration (root-only backup kept)"
    echo -e "   ✓ Work directories, local cache and leftover job containers"
    echo -e "   ✓ User $SERVICE_USER"
    echo -e "\n${BLUE}ℹ️  Docker was left installed.${NC}"

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
    remove_job_containers
    remove_workdir
    remove_user
    print_summary
}

main "$@"
