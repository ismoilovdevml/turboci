#!/bin/bash
# Installs a shell runner from a local binary without a token (configured, not
# started), checks what install.sh wrote, then removes it with uninstall.sh and
# checks that nothing is left. Where systemd runs, it also checks that an
# upgrade's drain and `systemctl stop` signal the runner, not its jobs. CI
# runs it on Linux and macOS.
#
#   sudo bash e2e/installer-smoke.sh target/release/turboci
set -euo pipefail

BINARY="${1:?usage: installer-smoke.sh BINARY}"
NAME=turboci-smoke
cd "$(dirname "$0")/.."

fail() { echo "FAIL: $*" >&2; exit 1; }

# An upgrade from a version up to 0.9 removes its tmpfiles rule for /tmp
TMPFILES="/etc/tmpfiles.d/$NAME.conf"
if [ -d /etc/tmpfiles.d ]; then
    echo "d /tmp/turboci-builds 0750 $SUDO_USER $SUDO_USER 1d" > "$TMPFILES"
fi

bash install.sh --binary "$BINARY" --executor shell --name "$NAME" --user "$SUDO_USER"

CONFIG="/etc/$NAME-runner.toml"
/usr/local/bin/turboci --version
grep -q 'executor_type = "shell"' "$CONFIG" || fail "config has no shell executor"
if [ "$(uname -s)" = "Darwin" ]; then
    HOME_DIR=$(dscl . -read "/Users/$SUDO_USER" NFSHomeDirectory | awk '{ print $2 }')
    PLIST="$HOME_DIR/Library/LaunchAgents/io.github.ismoilovdevml.$NAME.plist"
    STATE="$HOME_DIR/Library/TurboCI/$NAME"
    plutil -lint "$PLIST"
    [ "$(stat -f '%Su %Lp' "$CONFIG")" = "$SUDO_USER 600" ] || fail "config is not owner-only"
    LEFT_BEHIND=("$PLIST" "$STATE" "$HOME_DIR/Library/TurboCI")
else
    UNIT="/etc/systemd/system/$NAME.service"
    STATE="/var/lib/$NAME"
    grep -q "^User=$SUDO_USER$" "$UNIT" || fail "unit does not run as $SUDO_USER"
    grep -qx 'PrivateTmp=true' "$UNIT" || fail "unit has no private /tmp"
    ! grep -q '/tmp' "$UNIT" || fail "unit still uses the host's /tmp"
    # shellcheck disable=SC2016 # $MAINPID is systemd's, not the shell's
    grep -qx 'ExecReload=/bin/kill -HUP $MAINPID' "$UNIT" || fail "unit cannot reload"
    grep -qx 'KillMode=mixed' "$UNIT" || fail "systemctl stop signals the jobs too"
    # Every signal the installer sends goes to the runner only
    ! grep -Eq 'systemctl kill (-s|--signal)' install.sh || fail "install.sh signals the whole unit"
    [ ! -e "$TMPFILES" ] || fail "old tmpfiles rule kept"
    if command -v systemd-analyze > /dev/null 2>&1; then
        systemd-analyze verify "$UNIT" || fail "systemd rejects the unit"
    fi
    [ "$(stat -c '%U:%G %a' "$CONFIG")" = "root:$(id -gn "$SUDO_USER") 640" ] || fail "config permissions"
    LEFT_BEHIND=("$UNIT" "$STATE" "$TMPFILES")
fi
[ -d "$STATE/builds" ] || fail "no builds directory in $STATE"

# With systemd running: a stand-in for the runner with one job, started from
# the installed unit. The drain the installer sends on an upgrade must reach
# the runner only, and `systemctl stop` must not SIGTERM the job.
if [ "$(uname -s)" = "Linux" ] && [ -d /run/systemd/system ]; then
    DROPIN_DIR="/etc/systemd/system/$NAME.service.d"
    # Like a shell job: a process group of its own in the service's cgroup
    cat > "$STATE/fake-runner.sh" << 'EOF'
#!/bin/bash
setsid bash -c 'trap "echo QUIT >> job.signals" QUIT; trap "echo TERM >> job.signals" TERM
    echo $$ > job.pid; while :; do sleep 1 & wait $!; done' &
trap 'echo QUIT >> runner.signals' QUIT
trap 'echo TERM >> runner.signals; exit 0' TERM
while :; do sleep 1 & wait $!; done
EOF
    chown "$SUDO_USER" "$STATE/fake-runner.sh"
    mkdir -p "$DROPIN_DIR"
    printf '[Service]\nExecStart=\nExecStart=/bin/bash %s/fake-runner.sh\n' "$STATE" > "$DROPIN_DIR/smoke.conf"
    systemctl daemon-reload
    systemctl start "$NAME"
    for _ in $(seq 1 20); do [ -s "$STATE/job.pid" ] && break; sleep 0.5; done
    JOB_PID=$(cat "$STATE/job.pid" 2> /dev/null) || fail "the stand-in job did not start"

    # An upgrade of a running runner (no token: the config is kept) drains it.
    # No terminal, so the installer does not ask to overwrite the config.
    setsid -w bash install.sh --binary "$BINARY" --executor shell --name "$NAME" --user "$SUDO_USER" < /dev/null
    sleep 1
    grep -qx QUIT "$STATE/runner.signals" || fail "the drain did not reach the runner"
    [ ! -e "$STATE/job.signals" ] || fail "the drain signalled the job: $(cat "$STATE/job.signals")"
    kill -0 "$JOB_PID" || fail "the drain killed the job"
    grep -qx 'KillMode=mixed' "$UNIT" || fail "the upgrade dropped KillMode=mixed"

    systemctl stop "$NAME"
    grep -qx TERM "$STATE/runner.signals" || fail "systemctl stop did not SIGTERM the runner"
    [ ! -e "$STATE/job.signals" ] || fail "systemctl stop signalled the job: $(cat "$STATE/job.signals")"
    ! kill -0 "$JOB_PID" 2> /dev/null || fail "the job outlived the stopped service"

    rm -rf "$DROPIN_DIR"
    systemctl daemon-reload
    echo "drain and stop reach the runner only"
fi

bash uninstall.sh --name "$NAME"

for path in /usr/local/bin/turboci "$CONFIG" "${LEFT_BEHIND[@]}"; do
    [ ! -e "$path" ] || fail "left behind: $path"
done
id -u "$SUDO_USER" > /dev/null || fail "uninstall removed $SUDO_USER"
echo "installer smoke test passed"
