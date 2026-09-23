#!/bin/bash
set -e

# TurboCI Automated Installer
# Installs: TurboCI + Systemd Service
# Supports: Ubuntu, Debian, RHEL, Rocky, AlmaLinux, Fedora

REPO="ismoilovdevml/turboci"
INSTALL_DIR="/usr/local/bin"
CONFIG_DIR="/etc"
BIN_NAME="turboci"
SERVICE_NAME="turboci"
SERVICE_USER="turboci"
STATE_DIR="/var/lib/turboci"
# The Docker executor currently uses this fixed host path for job workspaces.
DOCKER_BUILDS_DIR="/tmp/turboci-builds"
# Executor for the generated config: "docker" (default) or "shell".
# Select shell explicitly with: sudo TURBOCI_EXECUTOR=shell bash install.sh
EXECUTOR="${TURBOCI_EXECUTOR:-docker}"

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

echo -e "${BLUE}"
cat << "EOF"
  _____           _          ____ ___
 |_   _|   _ _ __| |__   ___/ ___|_ _|
   | || | | | '__| '_ \ / _ \___ \| |
   | || |_| | |  | |_) | (_) |__) | |
   |_| \__,_|_|  |_.__/ \___/____/___|

  ⚡ TurboCI Installer
EOF
echo -e "${NC}"

# Detect OS
detect_os() {
    echo -e "${YELLOW}🔍 Detecting OS...${NC}"

    if [ -f /etc/os-release ]; then
        . /etc/os-release
        OS_ID=$ID
        OS_VERSION=$VERSION_ID
    else
        echo -e "${RED}❌ Cannot detect OS${NC}"
        exit 1
    fi

    case "$OS_ID" in
        ubuntu|debian)
            PKG_MANAGER="apt-get"
            ;;
        rhel|rocky|almalinux|fedora|centos)
            PKG_MANAGER="dnf"
            # Fallback to yum if dnf not available
            if ! command -v dnf &> /dev/null; then
                PKG_MANAGER="yum"
            fi
            ;;
        *)
            echo -e "${RED}❌ Unsupported OS: $OS_ID${NC}"
            echo -e "${YELLOW}Supported: Ubuntu, Debian, RHEL, Rocky, AlmaLinux, Fedora${NC}"
            exit 1
            ;;
    esac

    echo -e "${GREEN}✓${NC} OS: ${BLUE}$NAME $VERSION_ID${NC}"
    echo -e "${GREEN}✓${NC} Package manager: ${BLUE}$PKG_MANAGER${NC}"
}

# Detect architecture
detect_arch() {
    ARCH=$(uname -m)

    case "$ARCH" in
        x86_64|amd64)
            TARGET="x86_64-unknown-linux-musl"
            DISPLAY_ARCH="x86_64"
            ;;
        *)
            echo -e "${RED}❌ Unsupported architecture: $ARCH${NC}"
            echo -e "${YELLOW}Supported: x86_64${NC}"
            exit 1
            ;;
    esac

    echo -e "${GREEN}✓${NC} Architecture: ${BLUE}$DISPLAY_ARCH${NC}"
}

# Get latest TurboCI release
get_latest_version() {
    echo -e "\n${YELLOW}📡 Fetching latest TurboCI release...${NC}"

    LATEST_VERSION=$(curl -s "https://api.github.com/repos/$REPO/releases/latest" | \
                     grep '"tag_name":' | \
                     sed -E 's/.*"([^"]+)".*/\1/')

    if [ -z "$LATEST_VERSION" ]; then
        echo -e "${RED}❌ Failed to fetch latest release${NC}"
        exit 1
    fi

    echo -e "${GREEN}✓${NC} Latest version: ${BLUE}$LATEST_VERSION${NC}"
}

# Download and install TurboCI
install_turboci() {
    echo -e "\n${YELLOW}📥 Downloading TurboCI $LATEST_VERSION...${NC}"

    DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_VERSION/turboci-$TARGET"
    TEMP_FILE="/tmp/turboci-download-$$"

    if ! curl -fsSL "$DOWNLOAD_URL" -o "$TEMP_FILE"; then
        echo -e "${RED}❌ Download failed${NC}"
        echo -e "${YELLOW}URL: $DOWNLOAD_URL${NC}"
        rm -f "$TEMP_FILE"
        exit 1
    fi

    echo -e "${GREEN}✓${NC} Downloaded successfully"

    # Install binary
    echo -e "${YELLOW}📦 Installing to $INSTALL_DIR...${NC}"
    chmod +x "$TEMP_FILE"
    mv "$TEMP_FILE" "$INSTALL_DIR/$BIN_NAME"

    echo -e "${GREEN}✓${NC} TurboCI installed: ${BLUE}$INSTALL_DIR/$BIN_NAME${NC}"

    # Verify
    if VERSION=$($INSTALL_DIR/$BIN_NAME --version 2>&1); then
        echo -e "${GREEN}✓${NC} Binary verified: ${BLUE}$VERSION${NC}"
    else
        echo -e "${RED}❌ Binary verification failed${NC}"
        exit 1
    fi
}

# Validate the executor choice; shell must be an explicit, warned-about choice
validate_executor() {
    case "$EXECUTOR" in
        docker)
            echo -e "${GREEN}✓${NC} Executor: ${BLUE}docker${NC} (jobs isolated in containers)"
            ;;
        shell)
            echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
            echo -e "${YELLOW}⚠️  WARNING: shell executor selected (TURBOCI_EXECUTOR=shell)${NC}"
            echo -e "${YELLOW}   Job scripts will run directly on this host as the${NC}"
            echo -e "${YELLOW}   '$SERVICE_USER' user, with no container isolation.${NC}"
            echo -e "${YELLOW}   Any project that can run CI jobs on this runner can read${NC}"
            echo -e "${YELLOW}   the runner token and other jobs' workspaces.${NC}"
            echo -e "${YELLOW}   Use it only when every project on this runner is trusted.${NC}"
            echo -e "${YELLOW}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
            ;;
        *)
            echo -e "${RED}❌ Unknown TURBOCI_EXECUTOR: '$EXECUTOR' (use 'docker' or 'shell')${NC}"
            exit 1
            ;;
    esac
}

# Create the unprivileged system user the service runs as (idempotent)
create_service_user() {
    echo -e "\n${YELLOW}👤 Creating service user...${NC}"

    if id -u "$SERVICE_USER" > /dev/null 2>&1; then
        echo -e "${GREEN}✓${NC} User already exists: ${BLUE}$SERVICE_USER${NC}"
    else
        useradd --system --no-create-home --home-dir "$STATE_DIR" \
            --shell /usr/sbin/nologin "$SERVICE_USER"
        echo -e "${GREEN}✓${NC} User created: ${BLUE}$SERVICE_USER${NC}"
    fi

    # Membership in the docker group is required to use the Docker socket.
    # Note: docker group access is equivalent to root on this host.
    if getent group docker > /dev/null 2>&1; then
        usermod -aG docker "$SERVICE_USER"
        echo -e "${GREEN}✓${NC} Added $SERVICE_USER to the docker group"
    elif [ "$EXECUTOR" = "docker" ]; then
        echo -e "${YELLOW}⚠️  Docker group not found - is Docker installed?${NC}"
        echo -e "${YELLOW}   After installing Docker run: usermod -aG docker $SERVICE_USER${NC}"
    fi
}

# Create directories owned by the service user
create_directories() {
    echo -e "\n${YELLOW}📁 Creating directories...${NC}"

    install -d -o "$SERVICE_USER" -g "$SERVICE_USER" -m 0750 "$STATE_DIR" "$STATE_DIR/builds"
    echo -e "${GREEN}✓${NC} State directory: ${BLUE}$STATE_DIR${NC}"

    # Job containers run as root, so files they create in the workspace can
    # not always be removed by the service user. Let systemd-tmpfiles create
    # the workspace root with the right owner and age out leftovers.
    if [ -d /etc/tmpfiles.d ]; then
        echo "d $DOCKER_BUILDS_DIR 0750 $SERVICE_USER $SERVICE_USER 1d" > /etc/tmpfiles.d/turboci.conf
        if command -v systemd-tmpfiles > /dev/null 2>&1; then
            systemd-tmpfiles --create /etc/tmpfiles.d/turboci.conf || true
        fi
        echo -e "${GREEN}✓${NC} tmpfiles rule: ${BLUE}/etc/tmpfiles.d/turboci.conf${NC}"
    fi

    # Workspaces left behind by an earlier root-run install cannot be
    # written or cleaned up by the unprivileged service user.
    for dir in "$DOCKER_BUILDS_DIR" /tmp/turboci; do
        if [ -e "$dir" ] && [ "$(stat -c %U "$dir" 2>/dev/null)" != "$SERVICE_USER" ]; then
            echo -e "${YELLOW}⚠️  $dir exists and is not owned by $SERVICE_USER${NC}"
            echo -e "${YELLOW}   Remove it before starting the service: rm -rf $dir${NC}"
        fi
    done
}

# Create TurboCI config
create_config() {
    echo -e "\n${YELLOW}📝 Creating configuration...${NC}"

    CONFIG_FILE="$CONFIG_DIR/turboci-runner.toml"

    if [ -f "$CONFIG_FILE" ]; then
        echo -e "${YELLOW}⚠️  Config already exists: $CONFIG_FILE${NC}"
        read -p "Overwrite? (y/N): " -n 1 -r
        echo
        if [[ ! $REPLY =~ ^[Yy]$ ]]; then
            echo -e "${BLUE}ℹ️  Keeping existing config${NC}"
            secure_config
            if grep -Eq '^[[:space:]]*executor_type[[:space:]]*=[[:space:]]*"shell"' "$CONFIG_FILE"; then
                echo -e "${YELLOW}⚠️  Existing config uses the shell executor (no job isolation)${NC}"
            fi
            return
        fi
    fi

    # Create the file with restrictive permissions before writing the token field
    install -m 0640 -o root -g "$SERVICE_USER" /dev/null "$CONFIG_FILE"

    cat > "$CONFIG_FILE" << EOF
# TurboCI Runner Configuration
concurrent = 4
check_interval = 3
runner_token = ""
gitlab_url = "https://gitlab.com"
cache_enabled = true
cache_dir = "$STATE_DIR/cache"

[executor]
# "docker" isolates each job in a container. "shell" runs job scripts
# directly on this host as the $SERVICE_USER user - only for trusted projects.
executor_type = "$EXECUTOR"

[executor.shell]
work_dir = "$STATE_DIR/builds"

[executor.docker]
default_image = "alpine:latest"
privileged = false
volumes = []
network_mode = "bridge"
EOF

    secure_config

    echo -e "${GREEN}✓${NC} Config created: ${BLUE}$CONFIG_FILE${NC} (executor: $EXECUTOR)"
    echo -e "${YELLOW}⚠️  You must edit this file and set your runner_token${NC}"
}

# The config holds the runner token: readable by the service group only
secure_config() {
    chown "root:$SERVICE_USER" "$CONFIG_FILE"
    chmod 0640 "$CONFIG_FILE"
    echo -e "${GREEN}✓${NC} Config permissions: root:$SERVICE_USER 0640"
}

# Create systemd service
create_service() {
    echo -e "\n${YELLOW}🔧 Creating systemd service...${NC}"

    SERVICE_FILE="/etc/systemd/system/$SERVICE_NAME.service"

    cat > "$SERVICE_FILE" << EOF
[Unit]
Description=TurboCI Runner
Documentation=https://github.com/$REPO
After=network.target docker.service

[Service]
Type=simple
User=$SERVICE_USER
Group=$SERVICE_USER
WorkingDirectory=$STATE_DIR
ExecStart=$INSTALL_DIR/$BIN_NAME runner-start -c $CONFIG_DIR/turboci-runner.toml
Restart=always
RestartSec=10
StandardOutput=journal
StandardError=journal

# Security
NoNewPrivileges=true
# PrivateTmp must stay off: the Docker executor bind-mounts job workspaces
# from $DOCKER_BUILDS_DIR, and dockerd resolves that path in the host's /tmp.
# A private /tmp would hand every job container an empty workspace.
# Revisit once workspaces move out of /tmp (issue #21).
PrivateTmp=false
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=$STATE_DIR /tmp
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
LockPersonality=true
RestrictRealtime=true

[Install]
WantedBy=multi-user.target
EOF

    echo -e "${GREEN}✓${NC} Service created: ${BLUE}$SERVICE_FILE${NC}"

    # Reload systemd
    systemctl daemon-reload
    echo -e "${GREEN}✓${NC} Systemd reloaded"
}

# Enable service (but don't start yet - needs config)
enable_service() {
    echo -e "\n${YELLOW}🎬 Enabling service...${NC}"

    systemctl enable $SERVICE_NAME
    echo -e "${GREEN}✓${NC} Service enabled (auto-start on boot)"
    echo -e "${YELLOW}ℹ️  Service NOT started yet - configure token first${NC}"
}

# Print next steps
print_next_steps() {
    echo -e "\n${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${GREEN}✨ Installation Complete!${NC}"
    echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    echo -e "\n${BLUE}📋 Next Steps:${NC}"
    echo -e "\n${YELLOW}1. Get GitLab Runner Token:${NC}"
    echo -e "   • Open your GitLab project"
    echo -e "   • Go to Settings → CI/CD → Runners"
    echo -e "   • Click 'New project runner'"
    echo -e "   • Add tag: ${BLUE}turboci${NC}"
    echo -e "   • Copy the token (starts with glrt-)"

    echo -e "\n${YELLOW}2. Configure TurboCI:${NC}"
    echo -e "   ${BLUE}nano $CONFIG_DIR/turboci-runner.toml${NC}"
    echo -e "   Set: ${BLUE}runner_token = \"glrt-YOUR-TOKEN-HERE\"${NC}"

    echo -e "\n${YELLOW}3. Start TurboCI:${NC}"
    echo -e "   ${BLUE}systemctl start turboci${NC}"
    echo -e "   ${BLUE}systemctl status turboci${NC}"

    echo -e "\n${YELLOW}4. Check Logs:${NC}"
    echo -e "   ${BLUE}journalctl -u turboci -f${NC}"

    echo -e "\n${BLUE}📊 Installed Components:${NC}"
    echo -e "   ✓ TurboCI:       ${GREEN}$LATEST_VERSION${NC}"
    echo -e "   ✓ Config:        ${BLUE}$CONFIG_DIR/turboci-runner.toml${NC} (executor: $EXECUTOR)"
    echo -e "   ✓ Runs as:       ${BLUE}$SERVICE_USER${NC} (work dir $STATE_DIR)"
    echo -e "   ✓ Service:       ${GREEN}Enabled${NC} (not started)"

    echo -e "\n${BLUE}🔧 Management Commands:${NC}"
    echo -e "   ${BLUE}systemctl status turboci${NC}   # Check status"
    echo -e "   ${BLUE}systemctl restart turboci${NC}  # Restart"
    echo -e "   ${BLUE}systemctl stop turboci${NC}     # Stop"
    echo -e "   ${BLUE}journalctl -u turboci -f${NC}   # View logs"

    echo -e "\n${BLUE}📚 Documentation:${NC}"
    echo -e "   https://github.com/$REPO"

    echo -e "\n${BLUE}🗑️  Uninstall:${NC}"
    echo -e "   ${BLUE}curl -sSL https://raw.githubusercontent.com/$REPO/main/uninstall.sh | sudo bash${NC}"

    echo ""
}

# Main installation
main() {
    echo -e "${BLUE}Starting automated installation...${NC}\n"

    validate_executor
    detect_os
    detect_arch
    get_latest_version
    install_turboci
    create_service_user
    create_directories
    create_config
    create_service
    enable_service
    print_next_steps
}

main "$@"
