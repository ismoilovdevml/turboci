#!/bin/bash
set -e

# TurboCI Uninstaller
# Removes: service, binary, config (a root-only backup is kept), work and state
# directories, TurboCI's copy of registry CAs, the turboci user, and leftover
# job containers/networks. Docker and /etc/docker/certs.d are left alone.
# On macOS: the launchd agent, binary, config and state directory; the user stays.

BIN_NAME="turboci"
INSTALL_DIR="/usr/local/bin"
# --name NAME removes a runner installed with install.sh --name NAME
INSTANCE="${TURBOCI_NAME:-turboci}"
while [ $# -gt 0 ]; do
    case "$1" in
        --name) INSTANCE="${2:?--name needs a value}"; shift 2 ;;
        -h|--help) echo "Usage: uninstall.sh [--name NAME]"; exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done
if ! [[ "$INSTANCE" =~ ^[a-z][a-z0-9-]{0,30}$ ]]; then
    echo "--name must be lowercase letters, digits and dashes" >&2
    exit 1
fi
SERVICE_NAME="$INSTANCE"
CONFIG_FILE="/etc/$INSTANCE-runner.toml"
SERVICE_FILE="/etc/systemd/system/$SERVICE_NAME.service"
TMPFILES_FILE="/etc/tmpfiles.d/$INSTANCE.conf"
STATE_DIR="/var/lib/$INSTANCE"
# install.sh --registry-ca copies; shared by every runner on the host
REGISTRY_CA_DIR="/etc/turboci-registry-ca"
# The user the service ran as; it is only removed if install.sh created it
# (no unit on macOS, or one already removed: set -e must not stop here)
SERVICE_USER=$(sed -n 's/^User=//p' "$SERVICE_FILE" 2>/dev/null || true)
SERVICE_USER="${SERVICE_USER:-$INSTANCE}"
ROOT_GROUP="root"
MACOS=0
if [ "$(uname -s)" = "Darwin" ]; then
    MACOS=1
    ROOT_GROUP="wheel"
    # install.sh puts the agent in the LaunchAgents of the user it runs as
    LAUNCHD_LABEL="io.github.ismoilovdevml.$INSTANCE"
    PLIST_FILE=""
    OTHER_RUNNERS=""
    for plist in /Users/*/Library/LaunchAgents/io.github.ismoilovdevml.*.plist; do
        [ -e "$plist" ] || continue
        if [ -z "$PLIST_FILE" ] && [ "$(basename "$plist")" = "$LAUNCHD_LABEL.plist" ]; then
            PLIST_FILE="$plist"
        else
            OTHER_RUNNERS="$OTHER_RUNNERS $plist"
        fi
    done
    STATE_DIR=""
    SERVICE_USER=""
    if [ -n "$PLIST_FILE" ]; then
        SERVICE_USER=$(stat -f %Su "$PLIST_FILE")
        STATE_DIR=$(plutil -extract WorkingDirectory raw "$PLIST_FILE" 2>/dev/null || true)
    fi
else
    # Containers, networks and volumes carry the runner's system ID
    OWNER=$(cat "$STATE_DIR/.runner_system_id" 2>/dev/null || true)
    # Other TurboCI runners on this host keep the binary and shared directories
    OTHER_RUNNERS=$(grep -l "$INSTALL_DIR/$BIN_NAME runner-start" /etc/systemd/system/*.service 2>/dev/null \
        | grep -vx "$SERVICE_FILE" || true)
fi

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

# Stop and remove the launchd agent (macOS)
remove_launch_agent() {
    echo -e "${YELLOW}🛑 Stopping agent...${NC}"
    if [ -z "$PLIST_FILE" ]; then
        echo -e "${BLUE}ℹ️  Agent not found${NC}"
        return
    fi

    # bootout stops it with SIGTERM: running jobs are stopped and reported
    TARGET="gui/$(id -u "$SERVICE_USER")/$LAUNCHD_LABEL"
    if launchctl print "$TARGET" > /dev/null 2>&1; then
        launchctl bootout "$TARGET"
        echo -e "${GREEN}✓${NC} Agent stopped"
    else
        echo -e "${BLUE}ℹ️  Agent not loaded${NC}"
    fi
    rm -f "$PLIST_FILE"
    echo -e "${GREEN}✓${NC} Agent removed: $PLIST_FILE"
}

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
    if [ -n "$OTHER_RUNNERS" ]; then
        echo -e "${BLUE}ℹ️  Kept: other TurboCI runners use it ($(echo $OTHER_RUNNERS | xargs -n1 basename | tr '\n' ' '))${NC}"
        return
    fi

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
        install -m 0600 -o root -g "$ROOT_GROUP" "$CONFIG_FILE" "$BACKUP_FILE"
        echo -e "${BLUE}ℹ️  Config backed up to: $BACKUP_FILE (root only)${NC}"

        rm -f "$CONFIG_FILE"
        echo -e "${GREEN}✓${NC} Config removed: $CONFIG_FILE"
    else
        echo -e "${BLUE}ℹ️  Config not found${NC}"
    fi
    # Backups made by install.sh when replacing the config also hold tokens
    for old in "$CONFIG_FILE".bak.*; do
        [ -e "$old" ] || continue
        chown "root:$ROOT_GROUP" "$old" && chmod 0600 "$old"
    done
}

# Remove job containers and networks left by a crash
remove_job_containers() {
    command -v docker > /dev/null 2>&1 || return 0
    echo -e "\n${YELLOW}🐳 Removing leftover job containers...${NC}"

    if [ -n "$OWNER" ]; then
        FILTER="label=turboci.runner=$OWNER"
    elif [ -z "$OTHER_RUNNERS" ]; then
        FILTER="name=^turboci-job-"
    else
        echo -e "${YELLOW}⚠️  No system ID in $STATE_DIR; containers left alone (other runners are installed)${NC}"
        return
    fi
    CONTAINERS=$(docker ps -aq --filter "$FILTER" 2>/dev/null || true)
    if [ -n "$CONTAINERS" ]; then
        echo "$CONTAINERS" | xargs docker rm -f > /dev/null
        echo -e "${GREEN}✓${NC} Removed $(echo "$CONTAINERS" | wc -l) container(s)"
    fi
    NETWORKS=$(docker network ls -q --filter "$FILTER" 2>/dev/null || true)
    if [ -n "$NETWORKS" ]; then
        echo "$NETWORKS" | xargs docker network rm > /dev/null
        echo -e "${GREEN}✓${NC} Removed $(echo "$NETWORKS" | wc -l) network(s)"
    fi
    # Persistent cache volumes (executor.docker.volumes) of this runner
    if [ -n "$OWNER" ]; then
        VOLUMES=$(docker volume ls -q --filter "$FILTER" 2>/dev/null || true)
        if [ -n "$VOLUMES" ]; then
            echo "$VOLUMES" | xargs docker volume rm > /dev/null
            echo -e "${GREEN}✓${NC} Removed $(echo "$VOLUMES" | wc -l) cache volume(s)"
        fi
    fi
}

# Remove work and state directories (job workspaces and the local cache)
remove_workdir() {
    echo -e "\n${YELLOW}📁 Removing work directories...${NC}"

    DIRS="$STATE_DIR"
    [ -n "$OTHER_RUNNERS" ] || [ "$MACOS" -eq 1 ] || DIRS="$DIRS /tmp/turboci-builds /tmp/turboci"
    for dir in $DIRS; do
        if [ -d "$dir" ]; then
            rm -rf "$dir"
            echo -e "${GREEN}✓${NC} Removed: $dir"
        fi
    done
    # ~/Library/TurboCI goes with the user's last runner
    if [ "$MACOS" -eq 1 ] && [ -n "$STATE_DIR" ]; then
        rmdir "$(dirname "$STATE_DIR")" 2> /dev/null || true
    fi
    if [ -f "$TMPFILES_FILE" ]; then
        rm -f "$TMPFILES_FILE"
        echo -e "${GREEN}✓${NC} Removed: $TMPFILES_FILE"
    fi
}

# Remove TurboCI's copy of registry CAs. The directory is shared by all
# runners, so it goes with the last one. The copies in /etc/docker/certs.d
# stay: dockerd (and other tools) read them and may still pull from the hosts.
remove_registry_ca() {
    [ -d "$REGISTRY_CA_DIR" ] || return 0
    echo -e "\n${YELLOW}🔐 Removing registry CAs...${NC}"
    if [ -n "$OTHER_RUNNERS" ]; then
        echo -e "${BLUE}ℹ️  Kept: other TurboCI runners use $REGISTRY_CA_DIR${NC}"
        return
    fi

    HOSTS=""
    for pem in "$REGISTRY_CA_DIR"/*.pem; do
        [ -e "$pem" ] || continue
        HOSTS="$HOSTS $(basename "$pem" .pem)"
    done
    if [ -n "$HOSTS" ]; then
        echo -e "${BLUE}ℹ️  Left for dockerd: the CAs of${HOSTS} in /etc/docker/certs.d/<host>/ca.crt${NC}"
    fi
    rm -rf "$REGISTRY_CA_DIR"
    echo -e "${GREEN}✓${NC} Removed: $REGISTRY_CA_DIR"
}

# Remove the service user
remove_user() {
    id -u "$SERVICE_USER" > /dev/null 2>&1 || return 0
    # Only the system user install.sh created: named after the runner, with
    # the runner's state directory as its home. A --user is never removed.
    if [ "$SERVICE_USER" = "$INSTANCE" ] && \
       [ "$(getent passwd "$SERVICE_USER" | cut -d: -f6)" = "$STATE_DIR" ]; then
        userdel "$SERVICE_USER"
        echo -e "\n${GREEN}✓${NC} User removed: $SERVICE_USER"
    else
        echo -e "\n${BLUE}ℹ️  User $SERVICE_USER kept (not created by the installer)${NC}"
    fi
}

# Print summary
print_summary() {
    echo -e "\n${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${GREEN}✨ TurboCI Uninstalled Successfully!${NC}"
    echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    echo -e "\n${BLUE}📋 Removed:${NC}"
    if [ "$MACOS" -eq 1 ]; then
        echo -e "   ✓ TurboCI launchd agent"
        echo -e "   ✓ TurboCI binary, if no other runner uses it"
        echo -e "   ✓ Configuration (root-only backup kept)"
        echo -e "   ✓ State directory: builds, local cache and logs"
    else
        echo -e "   ✓ TurboCI service"
        echo -e "   ✓ TurboCI binary"
        echo -e "   ✓ Configuration (root-only backup kept)"
        echo -e "   ✓ Work directories, local cache and leftover job containers"
        echo -e "   ✓ TurboCI's copy of registry CAs, if no other runner uses it"
        echo -e "   ✓ Service user, if the installer created it"
        echo -e "\n${BLUE}ℹ️  Docker was left installed.${NC}"
    fi

    echo -e "\n${BLUE}📚 To reinstall:${NC}"
    echo -e "   ${BLUE}curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/install.sh | sudo bash${NC}"

    echo ""
}

# Main
main() {
    echo -e "${BLUE}Starting uninstallation...${NC}\n"

    if [ "$MACOS" -eq 1 ]; then
        remove_launch_agent
        remove_binary
        remove_config
        remove_workdir
        [ -z "$SERVICE_USER" ] || echo -e "\n${BLUE}ℹ️  User $SERVICE_USER kept${NC}"
    else
        remove_service
        remove_binary
        remove_config
        remove_job_containers
        remove_workdir
        remove_registry_ca
        remove_user
    fi
    print_summary
}

main "$@"
