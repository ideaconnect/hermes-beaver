#!/usr/bin/env bash
# Installs hermes-beaver on the host. Run as root; idempotent.
#   DEPLOY_USER=<desktop user> install.sh <staging dir>
# The staging dir holds the binary, this script's companions and, optionally,
# config.toml (from the repo's gitignored secrets/).
set -euo pipefail

STAGE=${1:?usage: DEPLOY_USER=<user> install.sh <staging dir>}
USER_NAME=${DEPLOY_USER:?set DEPLOY_USER}
[ "$(id -u)" -eq 0 ] || { echo "install.sh must run as root" >&2; exit 1; }

USER_UID=$(id -u "$USER_NAME")
USER_HOME=$(getent passwd "$USER_NAME" | cut -d: -f6)
RUNTIME=/run/user/$USER_UID
step() { printf '\033[1m==> %s\033[0m\n' "$*"; }
as_user() { runuser -u "$USER_NAME" -- env XDG_RUNTIME_DIR="$RUNTIME" DBUS_SESSION_BUS_ADDRESS="unix:path=$RUNTIME/bus" "$@"; }
user_install() { install -D -o "$USER_NAME" -g "$USER_NAME" "$@"; }

step "System packages: ddcutil (monitor brightness over DDC/CI)"
if command -v ddcutil >/dev/null; then echo "already installed"
else DEBIAN_FRONTEND=noninteractive apt-get install -y -q ddcutil; fi

step "i2c access for $USER_NAME"
groupadd -f i2c
if id -nG "$USER_NAME" | tr ' ' '\n' | grep -qx i2c; then echo "already in group i2c"
else usermod -aG i2c "$USER_NAME"; echo "added to group i2c (applies from the next login; the seat ACL covers the current one)"; fi
udevadm control --reload-rules && udevadm trigger --subsystem-match=i2c-dev || true

step "Binary -> $USER_HOME/.local/bin/hermes-beaver"
user_install -m 755 "$STAGE/hermes-beaver" "$USER_HOME/.local/bin/.hermes-beaver.new"
mv -f "$USER_HOME/.local/bin/.hermes-beaver.new" "$USER_HOME/.local/bin/hermes-beaver"
"$USER_HOME/.local/bin/hermes-beaver" --version

step "systemd user unit and XFCE autostart entry"
user_install -m 644 "$STAGE/hermes-beaver.service" "$USER_HOME/.config/systemd/user/hermes-beaver.service"
user_install -m 644 "$STAGE/hermes-beaver-autostart.desktop" "$USER_HOME/.config/autostart/hermes-beaver.desktop"

step "Config"
CONFIG=$USER_HOME/.config/hermes-beaver/config.toml
if [ -f "$STAGE/config.toml" ]; then
    user_install -m 644 "$STAGE/config.toml" "$CONFIG"; echo "installed from secrets/config.toml"
elif [ ! -f "$CONFIG" ]; then
    printf '# hermes-beaver config; every key is optional. See PLAN.md.\n' > "$STAGE/default.toml"
    user_install -m 644 "$STAGE/default.toml" "$CONFIG"; echo "wrote an empty default"
else echo "kept existing $CONFIG"; fi

step "Keep the screen on (XFCE power manager, screensaver, X DPMS)"
if [ -S "$RUNTIME/bus" ]; then
    xf() { as_user xfconf-query -c "$1" -p "$2" -n -t "$3" -s "$4" 2>/dev/null && echo "  $1 $2 = $4" || true; }
    xf xfce4-power-manager /xfce4-power-manager/dpms-enabled bool false
    xf xfce4-power-manager /xfce4-power-manager/blank-on-ac int 0
    xf xfce4-power-manager /xfce4-power-manager/dpms-on-ac-sleep uint 0
    xf xfce4-power-manager /xfce4-power-manager/dpms-on-ac-off uint 0
    xf xfce4-screensaver /saver/enabled bool false
    xf xfce4-screensaver /lock/enabled bool false
else echo "no session bus for $USER_NAME (not logged in?): skipped, the autostart entry covers X DPMS"; fi
X_ENV=(DISPLAY=:0 XAUTHORITY="$USER_HOME/.Xauthority")
as_user env "${X_ENV[@]}" sh -c 'xset s off; xset s noblank; xset -dpms; xset dpms force on' 2>/dev/null || true

step "(Re)start the dashboard"
if [ -S "$RUNTIME/bus" ]; then
    as_user systemctl --user daemon-reload
    as_user systemctl --user set-environment "${X_ENV[@]}"
    as_user systemctl --user restart hermes-beaver.service
    sleep 2
    as_user systemctl --user --no-pager --lines=5 status hermes-beaver.service || true
else echo "no user session: the dashboard starts at the next login"; fi

step "Restart a running MCP server so Hermes respawns it on the new binary"
if pkill -u "$USER_NAME" -f 'hermes-beaver mcp'; then echo "stopped the old MCP server"; else echo "none running"; fi

step "Done"
