#!/usr/bin/env bash
# Run on the VPS via: sudo bash /tmp/dojjo-install.sh <install_bin>
set -euo pipefail
install_bin="${1:?install_bin}"

install -m 755 /tmp/sync-server.new "${install_bin}/sync-server"
rm -f /tmp/sync-server.new
install -m 644 /tmp/dojjo-sync-server.service /etc/systemd/system/dojjo-sync-server.service
rm -f /tmp/dojjo-sync-server.service
systemctl daemon-reload
systemctl enable dojjo-sync-server.service
systemctl restart dojjo-sync-server.service
