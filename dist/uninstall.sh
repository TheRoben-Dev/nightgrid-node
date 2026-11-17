#!/bin/sh
# Removes Nightgrid Node without needing its binary (for when it is missing or
# broken). Same result as `nightgrid-node uninstall [--purge]`. Run as root.
#   sh uninstall.sh            keep history and config
#   sh uninstall.sh --purge    remove everything, including the account
set -u
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
# One at a time: systemctl gives up on the whole list if one unit is missing.
for u in nightgrid-node nightgrid-node-netwatch nightgrid-node-helper.socket nightgrid-node-health.socket; do
  systemctl disable --now "$u" 2>/dev/null || true
done
rm -f /etc/systemd/system/nightgrid-node.service /etc/systemd/system/nightgrid-node-netwatch.service \
  /etc/systemd/system/nightgrid-node-helper.socket /etc/systemd/system/nightgrid-node-helper@.service \
  /etc/systemd/system/nightgrid-node-health.socket /etc/systemd/system/nightgrid-node-health@.service
rm -rf /etc/systemd/system/nightgrid-node.service.d /etc/systemd/system/nightgrid-node-helper@.service.d
systemctl daemon-reload 2>/dev/null || true
rm -f /usr/local/bin/nightgrid-node
if [ "${1:-}" = "--purge" ]; then
  rm -rf /var/lib/nightgrid-node /var/lib/nightgrid-node-rollback /etc/nightgrid-node /run/nightgrid-node
  rm -f /etc/sysusers.d/nightgrid-node.conf
  if id nightgrid-node >/dev/null 2>&1; then userdel nightgrid-node; fi
  if getent group nightgrid-node >/dev/null 2>&1; then groupdel nightgrid-node; fi
  echo "Nightgrid Node removed (with history, config and account)"
else
  echo "Nightgrid Node removed (history in /var/lib/nightgrid-node and config in /etc/nightgrid-node kept)"
fi
