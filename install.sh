#!/bin/bash
set -e

# TurboCI Automated Installer
# Installs: Redis + TurboCI + Systemd Service
# Supports: Ubuntu, Debian, RHEL, Rocky, AlmaLinux, Fedora

REPO="ismoilovdevml/turboci"
INSTALL_DIR="/usr/local/bin"
CONFIG_DIR="/etc"
BIN_NAME="turboci"
SERVICE_NAME="turboci"

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
            REDIS_PKG="redis-server"
            REDIS_SERVICE="redis-server"
            ;;
        rhel|rocky|almalinux|fedora|centos)
            PKG_MANAGER="dnf"
            REDIS_PKG="redis"
            REDIS_SERVICE="redis"
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

# Install Redis
install_redis() {
    echo -e "\n${YELLOW}📦 Installing Redis...${NC}"

    # Check if already installed
    if systemctl is-active --quiet $REDIS_SERVICE 2>/dev/null; then
        echo -e "${GREEN}✓${NC} Redis already running"
        return
    fi

    if command -v redis-server &> /dev/null; then
        echo -e "${GREEN}✓${NC} Redis already installed"
    else
        echo -e "${YELLOW}⏳ Installing $REDIS_PKG...${NC}"
        $PKG_MANAGER update -y > /dev/null 2>&1 || true
        $PKG_MANAGER install -y $REDIS_PKG
        echo -e "${GREEN}✓${NC} Redis installed"
    fi

    # Enable and start Redis
    systemctl enable $REDIS_SERVICE
    systemctl start $REDIS_SERVICE

    # Verify
    if systemctl is-active --quiet $REDIS_SERVICE; then
        echo -e "${GREEN}✓${NC} Redis service running"

        # Test connection
        if redis-cli ping > /dev/null 2>&1; then
            echo -e "${GREEN}✓${NC} Redis connection test: OK"
        else
            echo -e "${YELLOW}⚠️  Redis installed but not responding${NC}"
        fi
    else
        echo -e "${RED}❌ Failed to start Redis${NC}"
        exit 1
    fi
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
    if $INSTALL_DIR/$BIN_NAME --help > /dev/null 2>&1; then
        echo -e "${GREEN}✓${NC} Binary verified successfully"
    else
        echo -e "${RED}❌ Binary verification failed${NC}"
        exit 1
    fi
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
            return
        fi
    fi

    # Create config
    cat > "$CONFIG_FILE" << 'EOF'
# TurboCI Runner Configuration
concurrent = 4
runner_token = ""
gitlab_url = "https://gitlab.com"
redis_url = "redis://127.0.0.1:6379"
cache_enabled = true
cache_ttl_seconds = 604800

[executor]
executor_type = "shell"

[executor.shell]
work_dir = "/tmp/turboci-builds"

[executor.docker]
default_image = "alpine:latest"
EOF

    echo -e "${GREEN}✓${NC} Config created: ${BLUE}$CONFIG_FILE${NC}"
    echo -e "${YELLOW}⚠️  You must edit this file and set your runner_token${NC}"
}

# Create systemd service
create_service() {
    echo -e "\n${YELLOW}🔧 Creating systemd service...${NC}"

    SERVICE_FILE="/etc/systemd/system/$SERVICE_NAME.service"

    cat > "$SERVICE_FILE" << EOF
[Unit]
Description=TurboCI Runner
Documentation=https://github.com/$REPO
After=network.target $REDIS_SERVICE.service
Requires=$REDIS_SERVICE.service

[Service]
Type=simple
User=root
WorkingDirectory=/tmp
ExecStart=$INSTALL_DIR/$BIN_NAME runner-start -c $CONFIG_DIR/turboci-runner.toml
Restart=always
RestartSec=10
StandardOutput=journal
StandardError=journal

# Security
NoNewPrivileges=false
PrivateTmp=false

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
    echo -e "   ✓ Redis Server:  ${GREEN}Running${NC}"
    echo -e "   ✓ TurboCI:       ${GREEN}$LATEST_VERSION${NC}"
    echo -e "   ✓ Config:        ${BLUE}$CONFIG_DIR/turboci-runner.toml${NC}"
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

    detect_os
    detect_arch
    install_redis
    get_latest_version
    install_turboci
    create_config
    create_service
    enable_service
    print_next_steps
}

main "$@"
