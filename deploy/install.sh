#!/usr/bin/env bash
# Installs hermes-beaver on the host. Run as root; idempotent.
#   DEPLOY_USER=<desktop user> install.sh <staging dir>
# The staging dir holds the binary, this script's companions and, optionally,
# config.toml and sway-host.conf (from the repo's gitignored secrets/).
set -euo pipefail

STAGE=${1:?usage: DEPLOY_USER=<user> install.sh <staging dir>}
USER_NAME=${DEPLOY_USER:?set DEPLOY_USER}
[ "$(id -u)" -eq 0 ] || { echo "install.sh must run as root" >&2; exit 1; }

USER_UID=$(id -u "$USER_NAME")
USER_HOME=$(getent passwd "$USER_NAME" | cut -d: -f6)
RUNTIME=/run/user/$USER_UID
KIOSK=hermes-beaver-kiosk.service
step() { printf '\033[1m==> %s\033[0m\n' "$*"; }
as_user() { runuser -u "$USER_NAME" -- env -C "$USER_HOME" XDG_RUNTIME_DIR="$RUNTIME" DBUS_SESSION_BUS_ADDRESS="unix:path=$RUNTIME/bus" "$@"; }
user_install() { install -D -o "$USER_NAME" -g "$USER_NAME" "$@"; }
# Checksum of the files the kiosk session runs on, to restart sway only when they change.
kiosk_sum() { { cat /etc/systemd/system/$KIOSK /etc/pam.d/hermes-beaver-kiosk \
    "$USER_HOME/.config/hermes-beaver/sway.conf" "$USER_HOME"/.config/hermes-beaver/sway.d/*.conf \
    2>/dev/null || true; } | sha256sum; }

step "System packages: ddcutil (brightness over DDC/CI), sway (Wayland compositor), wlopm (monitor standby), kbd (chvt)"
PKGS=()
command -v ddcutil >/dev/null || PKGS+=(ddcutil)
command -v sway >/dev/null || PKGS+=(sway)
command -v wlopm >/dev/null || PKGS+=(wlopm)
command -v chvt >/dev/null || PKGS+=(kbd)
# No recommends: they bring Xwayland, swaybg, swayidle, swaylock and a terminal.
if [ ${#PKGS[@]} -eq 0 ]; then echo "already installed"
else DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends "${PKGS[@]}"; fi

step "i2c access for $USER_NAME"
groupadd -f i2c
if id -nG "$USER_NAME" | tr ' ' '\n' | grep -qx i2c; then echo "already in group i2c"
else usermod -aG i2c "$USER_NAME"; echo "added to group i2c (applies from the next login; the seat ACL covers the current one)"; fi
udevadm control --reload-rules && udevadm trigger --subsystem-match=i2c-dev || true

step "Binary -> $USER_HOME/.local/bin/hermes-beaver"
user_install -m 755 "$STAGE/hermes-beaver" "$USER_HOME/.local/bin/.hermes-beaver.new"
mv -f "$USER_HOME/.local/bin/.hermes-beaver.new" "$USER_HOME/.local/bin/hermes-beaver"
"$USER_HOME/.local/bin/hermes-beaver" --version

step "Config"
CONFIG=$USER_HOME/.config/hermes-beaver/config.toml
if [ -f "$STAGE/config.toml" ]; then
    user_install -m 644 "$STAGE/config.toml" "$CONFIG"; echo "installed from secrets/config.toml"
elif [ ! -f "$CONFIG" ]; then
    printf '# hermes-beaver config; every key is optional. See PLAN.md.\n' > "$STAGE/default.toml"
    user_install -m 644 "$STAGE/default.toml" "$CONFIG"; echo "wrote an empty default"
else echo "kept existing $CONFIG"; fi

step "systemd user unit and sway config"
BEFORE=$(kiosk_sum)
user_install -m 644 "$STAGE/hermes-beaver.service" "$USER_HOME/.config/systemd/user/hermes-beaver.service"
user_install -m 644 "$STAGE/sway.conf" "$USER_HOME/.config/hermes-beaver/sway.conf"
if [ -f "$STAGE/sway-host.conf" ]; then
    user_install -m 644 "$STAGE/sway-host.conf" "$USER_HOME/.config/hermes-beaver/sway.d/host.conf"
    echo "host settings from secrets/sway.conf"
fi
# Sway brings up a backend even to validate: the headless one needs no seat or GPU.
if ! as_user env WLR_BACKENDS=headless WLR_RENDERER=pixman \
    sway --validate --config "$USER_HOME/.config/hermes-beaver/sway.conf"; then
    echo "sway rejects the config: the session stays as it is" >&2; exit 1
fi
# The XFCE autostart entry from the X11 days.
rm -fv "$USER_HOME/.config/autostart/hermes-beaver.desktop"

step "Kiosk session: sway on tty7, no display manager, no X server"
install -m 644 "$STAGE/hermes-beaver-kiosk.pam" /etc/pam.d/hermes-beaver-kiosk
sed -e "s|@USER@|$USER_NAME|g" -e "s|@HOME@|$USER_HOME|g" "$STAGE/$KIOSK" > "$STAGE/kiosk.unit"
install -m 644 "$STAGE/kiosk.unit" /etc/systemd/system/$KIOSK
systemctl daemon-reload
# The display manager it replaces (LightDM and its Xorg): stopped, and masked so that
# package upgrades don't bring it back.
DM=$(basename "$(readlink /etc/systemd/system/display-manager.service 2>/dev/null)" 2>/dev/null || true)
if [ -n "$DM" ] && [ "$DM" != "$KIOSK" ]; then
    systemctl disable "$DM"
    systemctl stop "$DM" || true
    systemctl mask "$DM"
    echo "replaced $DM"
fi
systemctl enable $KIOSK
# What the X session exported into the user manager; new services mustn't inherit it.
as_user systemctl --user unset-environment DISPLAY XAUTHORITY XAUTHLOCALHOSTNAME \
    DESKTOP_SESSION XDG_MENU_PREFIX XDG_SEAT_PATH XDG_SESSION_PATH XDG_CONFIG_DIRS \
    XDG_DATA_DIRS QT_ACCESSIBILITY 2>/dev/null || true
as_user systemctl --user daemon-reload

step "(Re)start the kiosk and the dashboard"
if ! systemctl is-active -q $KIOSK; then
    systemctl start $KIOSK; echo "started sway (it starts the dashboard)"
elif [ "$BEFORE" != "$(kiosk_sum)" ]; then
    systemctl restart $KIOSK; echo "restarted sway on the new config (it restarts the dashboard)"
else
    as_user systemctl --user restart hermes-beaver.service; echo "restarted the dashboard"
fi
sleep 3
systemctl --no-pager --lines=0 status $KIOSK || true
as_user systemctl --user --no-pager --lines=5 status hermes-beaver.service || true

step "Restart a running MCP server so Hermes respawns it on the new binary"
if pkill -u "$USER_NAME" -f 'hermes-beaver mcp'; then echo "stopped the old MCP server"; else echo "none running"; fi
echo "Hermes respawns it on the next call. If this release added tools, send /reload-mcp to Hermes so the model sees them."

step "Done"
