#!/usr/bin/env bash
# Installs nao-eph on a NAO6 that runs HULKs-OS: ./install.sh <robot address> [nao-eph-robot.tar.gz]
# The robot's password is "nao". Run it again to update.
set -euo pipefail
ROBOT="${1:?usage: ./install.sh <robot address> [nao-eph-robot.tar.gz]}"
BUNDLE="${2:-$(dirname "$0")/nao-eph-robot.tar.gz}"
[ -f "$BUNDLE" ] || { echo "Missing $BUNDLE: download nao-eph-robot.tar.gz from the latest release." >&2; exit 1; }
SSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null nao@$ROBOT"

echo "==> Copying nao-eph to $ROBOT (password: nao)"
cat "$BUNDLE" | $SSH 'set -e
  systemctl stop nao-eph-voice 2>/dev/null || true
  systemctl stop hulk || true
  rm -rf /home/nao/hulk.new && mkdir -p /home/nao/hulk.new
  tar -xz -C /home/nao/hulk.new
  mkdir -p /home/nao/hulk.new/logs
  rm -rf /home/nao/hulk && mv /home/nao/hulk.new /home/nao/hulk
  sudo cp /home/nao/hulk/nao-eph-voice.service /etc/systemd/system/
  sudo systemctl daemon-reload
  sudo systemctl enable nao-eph-voice
  systemctl start hulk
  sudo systemctl start nao-eph-voice'
echo "==> Done. Press the chest button once: the NAO stands up and you can talk to it."
