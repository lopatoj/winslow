#!/bin/bash
# Build winslowd and install it on the PocketBeagle over SSH.
#
#   scripts/deploy.sh [user@host] [--config]
#
# The host defaults to debian@192.168.7.2 (USB networking). The config is
# only installed if the board has none yet; pass --config to replace it (the
# old one is kept as winslow.toml.bak).
set -euo pipefail

HOST=debian@192.168.7.2
CONFIG=0
for arg in "$@"; do
    case $arg in
        --config) CONFIG=1 ;;
        *) HOST=$arg ;;
    esac
done

cd "$(dirname "$0")/.."
cross build --release -p winslowd

# The install steps run on the board as a script file, so that ssh's
# terminal stays free for sudo's password prompt.
INSTALL=$(mktemp)
trap 'rm -f "$INSTALL"' EXIT
cat > "$INSTALL" <<'EOF'
set -euo pipefail
. /etc/os-release
if [ "${VERSION_ID%%.*}" -lt 12 ]; then
    echo "This board runs $PRETTY_NAME; winslowd needs Debian 12 or newer." >&2
    exit 1
fi
for pkg in libiio0 libasound2 libsystemd0; do
    dpkg -s $pkg >/dev/null 2>&1 || echo "warning: $pkg is not installed (sudo apt install $pkg)" >&2
done

# The first prototype's gadget service would fight over the USB port.
sudo systemctl disable --now winslow-usb-midi.service 2>/dev/null || true

sudo install -m 755 /tmp/winslowd /usr/local/bin/winslowd
sudo install -m 644 /tmp/winslowd.service /etc/systemd/system/winslowd.service
sudo install -d /etc/winslow
if [ ! -e /etc/winslow/winslow.toml ]; then
    sudo install -m 644 /tmp/winslow.toml /etc/winslow/winslow.toml
elif [ "$CONFIG" = 1 ]; then
    sudo cp /etc/winslow/winslow.toml /etc/winslow/winslow.toml.bak
    sudo install -m 644 /tmp/winslow.toml /etc/winslow/winslow.toml
else
    echo "Keeping the existing /etc/winslow/winslow.toml (--config replaces it)."
fi
rm -f /tmp/winslowd /tmp/winslowd.service /tmp/winslow.toml /tmp/winslow-install.sh

sudo systemctl daemon-reload
sudo systemctl enable winslowd.service
# Don't wait: the first start adds MIDI to the USB gadget, which briefly
# drops the USB network this SSH session runs over.
sudo systemctl restart --no-block winslowd.service
echo
echo "Started. Give it a few seconds, reconnect, and check with:"
echo "  ssh $USER@192.168.7.2 journalctl -u winslowd -b"
EOF

# One connection for everything, so the password is asked for once at most.
CONTROL=$(mktemp -u)
SSH_OPTS=(-o ControlMaster=auto -o ControlPath="$CONTROL" -o ControlPersist=60)
scp "${SSH_OPTS[@]}" target/armv7-unknown-linux-gnueabihf/release/winslowd \
    systemd/winslowd.service systemd/winslow.toml "$HOST:/tmp/"
scp "${SSH_OPTS[@]}" "$INSTALL" "$HOST:/tmp/winslow-install.sh"
ssh "${SSH_OPTS[@]}" -t "$HOST" CONFIG=$CONFIG bash /tmp/winslow-install.sh
ssh "${SSH_OPTS[@]}" -O exit "$HOST" 2>/dev/null || true
