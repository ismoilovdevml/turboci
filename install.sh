#!/bin/bash
set -e

# TurboCI Automated Installer
# Installs: TurboCI (+ Docker for the docker executor) + Systemd Service
# Supports: Ubuntu, Debian, RHEL, Rocky, AlmaLinux, Fedora
#
# One command, registered and running:
#   curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/install.sh \
#     | sudo bash -s -- --url https://gitlab.example.com --token glrt-XXXX

REPO="ismoilovdevml/turboci"
INSTALL_DIR="/usr/local/bin"
CONFIG_DIR="/etc"
BIN_NAME="turboci"
SERVICE_NAME="turboci"
SERVICE_USER="turboci"
STATE_DIR="/var/lib/turboci"
# The Docker executor currently uses this fixed host path for job workspaces.
DOCKER_BUILDS_DIR="/tmp/turboci-builds"
# Settings: command line options, or the TURBOCI_* environment variables
EXECUTOR="${TURBOCI_EXECUTOR:-docker}"
GITLAB_URL="${TURBOCI_URL:-https://gitlab.com}"
RUNNER_TOKEN="${TURBOCI_TOKEN:-}"
CONCURRENT="${TURBOCI_CONCURRENT:-4}"
VERSION="${TURBOCI_VERSION:-}"
LOCAL_BINARY=""
START_SERVICE=1

usage() {
    cat << USAGE
Usage: install.sh [options]

  --url URL          GitLab URL (default: https://gitlab.com)
  --token TOKEN      Runner authentication token (glrt-...); with it the runner
                     is configured and started, without it only installed
  --executor NAME    docker (default) or shell (no isolation: trusted projects only)
  --concurrent N     Jobs run in parallel (default: 4)
  --version vX.Y.Z   Release to install (default: latest)
  --binary PATH      Install this binary instead of downloading a release
  --no-start         Configure but do not start the service
  -h, --help         Show this help

Environment: TURBOCI_URL, TURBOCI_TOKEN, TURBOCI_EXECUTOR, TURBOCI_CONCURRENT,
TURBOCI_VERSION.
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --url) GITLAB_URL="${2:?--url needs a value}"; shift 2 ;;
        --token) RUNNER_TOKEN="${2:?--token needs a value}"; shift 2 ;;
        --executor) EXECUTOR="${2:?--executor needs a value}"; shift 2 ;;
        --concurrent) CONCURRENT="${2:?--concurrent needs a value}"; shift 2 ;;
        --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
        --binary) LOCAL_BINARY="${2:?--binary needs a value}"; shift 2 ;;
        --no-start) START_SERVICE=0; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

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

# Check option values before changing anything on the host
validate_options() {
    case "$GITLAB_URL" in
        http://*|https://*) GITLAB_URL="${GITLAB_URL%/}" ;;
        *) echo -e "${RED}❌ --url must start with https:// or http://${NC}"; exit 1 ;;
    esac
    if ! [[ "$CONCURRENT" =~ ^[1-9][0-9]*$ ]]; then
        echo -e "${RED}❌ --concurrent must be a positive number${NC}"
        exit 1
    fi
    if [ -n "$LOCAL_BINARY" ] && [ ! -f "$LOCAL_BINARY" ]; then
        echo -e "${RED}❌ --binary: $LOCAL_BINARY not found${NC}"
        exit 1
    fi
}

# Install Docker for the docker executor when it is missing
install_docker() {
    [ "$EXECUTOR" = "docker" ] || return 0
    echo -e "\n${YELLOW}🐳 Checking Docker...${NC}"

    if ! command -v docker > /dev/null 2>&1; then
        echo -e "${YELLOW}⏳ Installing Docker (get.docker.com)...${NC}"
        curl -fsSL https://get.docker.com | sh
    fi
    systemctl enable --now docker > /dev/null 2>&1 || true
    if docker info > /dev/null 2>&1; then
        echo -e "${GREEN}✓${NC} Docker: ${BLUE}$(docker version --format '{{.Server.Version}}')${NC}"
    else
        echo -e "${RED}❌ Docker is installed but not running${NC}"
        exit 1
    fi
}

# Get latest TurboCI release
get_latest_version() {
    if [ -n "$LOCAL_BINARY" ]; then
        LATEST_VERSION="local binary"
        return
    fi
    if [ -n "$VERSION" ]; then
        LATEST_VERSION="$VERSION"
        echo -e "${GREEN}✓${NC} Version: ${BLUE}$LATEST_VERSION${NC}"
        return
    fi
    echo -e "\n${YELLOW}📡 Fetching latest TurboCI release...${NC}"

    LATEST_VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | \
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
    if [ -n "$LOCAL_BINARY" ]; then
        echo -e "\n${YELLOW}📦 Installing $LOCAL_BINARY...${NC}"
        install -m 0755 "$LOCAL_BINARY" "$INSTALL_DIR/$BIN_NAME"
        verify_binary
        return
    fi
    echo -e "\n${YELLOW}📥 Downloading TurboCI $LATEST_VERSION...${NC}"

    RELEASE_URL="https://github.com/$REPO/releases/download/$LATEST_VERSION"
    ASSET="turboci-$TARGET"
    TEMP_FILE=$(mktemp)
    SUMS_FILE=$(mktemp)
    trap 'rm -f "$TEMP_FILE" "$SUMS_FILE"' EXIT

    if ! curl -fsSL "$RELEASE_URL/$ASSET" -o "$TEMP_FILE"; then
        echo -e "${RED}❌ Download failed${NC}"
        echo -e "${YELLOW}URL: $RELEASE_URL/$ASSET${NC}"
        exit 1
    fi

    # Refuse binaries that do not match the release's published checksum
    if ! curl -fsSL "$RELEASE_URL/SHA256SUMS" -o "$SUMS_FILE"; then
        echo -e "${RED}❌ Release $LATEST_VERSION has no SHA256SUMS; refusing to install an unverified binary${NC}"
        exit 1
    fi
    EXPECTED=$(awk -v asset="$ASSET" '$2 == asset || $2 == "*" asset { print $1 }' "$SUMS_FILE")
    ACTUAL=$(sha256sum "$TEMP_FILE" | awk '{ print $1 }')
    if [ -z "$EXPECTED" ] || [ "$EXPECTED" != "$ACTUAL" ]; then
        echo -e "${RED}❌ Checksum mismatch for $ASSET (expected '${EXPECTED:-none}', got '$ACTUAL')${NC}"
        exit 1
    fi

    echo -e "${GREEN}✓${NC} Downloaded and verified (sha256 $ACTUAL)"

    # Install binary
    echo -e "${YELLOW}📦 Installing to $INSTALL_DIR...${NC}"
    chmod +x "$TEMP_FILE"
    mv "$TEMP_FILE" "$INSTALL_DIR/$BIN_NAME"

    echo -e "${GREEN}✓${NC} TurboCI installed: ${BLUE}$INSTALL_DIR/$BIN_NAME${NC}"
    verify_binary
}

verify_binary() {
    if INSTALLED_VERSION=$($INSTALL_DIR/$BIN_NAME --version 2>&1); then
        echo -e "${GREEN}✓${NC} Binary verified: ${BLUE}$INSTALLED_VERSION${NC}"
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

    if [ -f "$CONFIG_FILE" ] && [ -n "$RUNNER_TOKEN" ]; then
        # A token on the command line replaces the config; keep the old one
        BACKUP="$CONFIG_FILE.bak.$(date +%Y-%m-%d-%H%M%S)"
        cp -p "$CONFIG_FILE" "$BACKUP"
        echo -e "${YELLOW}⚠️  Replacing existing config (backup: $BACKUP)${NC}"
    elif [ -f "$CONFIG_FILE" ]; then
        echo -e "${YELLOW}⚠️  Config already exists: $CONFIG_FILE${NC}"
        # stdin is the script itself under `curl | bash`: ask on the terminal
        REPLY=""
        if [ -r /dev/tty ]; then
            read -p "Overwrite? (y/N): " -n 1 -r < /dev/tty
            echo
        fi
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
concurrent = $CONCURRENT
check_interval = 3
runner_token = "$RUNNER_TOKEN"
gitlab_url = "$GITLAB_URL"
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
    if [ -z "$RUNNER_TOKEN" ]; then
        echo -e "${YELLOW}⚠️  You must edit this file and set your runner_token${NC}"
    fi
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
# SIGTERM stops running jobs and reports them; give that time before SIGKILL
TimeoutStopSec=180
StandardOutput=journal
StandardError=journal

# Security
NoNewPrivileges=true
# PrivateTmp must stay off: the Docker executor bind-mounts job workspaces
# from $DOCKER_BUILDS_DIR, and dockerd resolves that path in the host's /tmp.
# A private /tmp would hand every job container an empty workspace.
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

# Enable the service; start it when a token was given
enable_service() {
    echo -e "\n${YELLOW}🎬 Enabling service...${NC}"

    systemctl enable $SERVICE_NAME > /dev/null 2>&1
    echo -e "${GREEN}✓${NC} Service enabled (auto-start on boot)"

    if [ -z "$RUNNER_TOKEN" ] || [ "$START_SERVICE" -eq 0 ]; then
        STARTED=0
        echo -e "${YELLOW}ℹ️  Service NOT started${NC}"
        return
    fi

    STARTED_AT=$(date '+%Y-%m-%d %H:%M:%S')
    systemctl restart $SERVICE_NAME

    # Wait until the runner reports its first successful request to GitLab
    for _ in $(seq 1 20); do
        sleep 1
        if ! systemctl is-active --quiet $SERVICE_NAME; then
            echo -e "${RED}❌ Service failed to start:${NC}"
            journalctl -u $SERVICE_NAME -n 20 --no-pager
            exit 1
        fi
        LOG=$(journalctl -u $SERVICE_NAME --since "$STARTED_AT" --no-pager -o cat 2>/dev/null)
        if echo "$LOG" | grep -q "Connected to GitLab"; then
            break
        fi
        if echo "$LOG" | grep -q "Invalid runner token"; then
            echo -e "${RED}❌ GitLab rejected the runner token (check --url and --token)${NC}"
            exit 1
        fi
    done
    if ! echo "$LOG" | grep -q "Connected to GitLab"; then
        echo -e "${RED}❌ The runner could not reach $GITLAB_URL:${NC}"
        echo "$LOG" | grep -E "WARN|ERROR" | tail -5
        exit 1
    fi
    STARTED=1
    echo -e "${GREEN}✓${NC} Service running and polling ${BLUE}$GITLAB_URL${NC} for jobs"
}

# Print next steps
print_next_steps() {
    echo -e "\n${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${GREEN}✨ Installation Complete!${NC}"
    echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    if [ "$STARTED" -eq 1 ]; then
        echo -e "\n${GREEN}🚀 TurboCI is running.${NC} Jobs for this runner will start automatically."
        echo -e "   Logs: ${BLUE}journalctl -u turboci -f${NC}\n"
        return
    fi

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
    validate_options
    detect_os
    detect_arch
    install_docker
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
