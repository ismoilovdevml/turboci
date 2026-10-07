#!/bin/bash
# Installs a shell runner from a local binary without a token (configured, not
# started), checks what install.sh wrote, then removes it with uninstall.sh and
# checks that nothing is left. CI runs it on Linux and macOS.
#
#   sudo bash e2e/installer-smoke.sh target/release/turboci
set -euo pipefail

BINARY="${1:?usage: installer-smoke.sh BINARY}"
NAME=turboci-smoke
cd "$(dirname "$0")/.."

fail() { echo "FAIL: $*" >&2; exit 1; }

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
    [ "$(stat -c '%U:%G %a' "$CONFIG")" = "root:$(id -gn "$SUDO_USER") 640" ] || fail "config permissions"
    LEFT_BEHIND=("$UNIT" "$STATE")
fi
[ -d "$STATE/builds" ] || fail "no builds directory in $STATE"

bash uninstall.sh --name "$NAME"

for path in /usr/local/bin/turboci "$CONFIG" "${LEFT_BEHIND[@]}"; do
    [ ! -e "$path" ] || fail "left behind: $path"
done
id -u "$SUDO_USER" > /dev/null || fail "uninstall removed $SUDO_USER"
echo "installer smoke test passed"
