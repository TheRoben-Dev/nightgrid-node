#!/bin/sh
# Removes Nightgrid Node without needing its binary (for when it is missing or
# broken). Same result as `nightgrid-node uninstall [--purge]`. Run as root.
#   sh uninstall.sh            keep history and config
#   sh uninstall.sh --purge    remove everything, including the account
set -u
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
# Without systemd (OpenRC, sysvinit): stop the service script and unlink it.
if [ -e /etc/init.d/nightgrid-node ]; then
  if command -v rc-service >/dev/null 2>&1; then
    rc-service nightgrid-node stop 2>/dev/null || true
    rc-update del nightgrid-node default 2>/dev/null || true
  else
    /etc/init.d/nightgrid-node stop 2>/dev/null || true
  fi
  rm -f /etc/init.d/nightgrid-node
  if command -v update-rc.d >/dev/null 2>&1; then update-rc.d nightgrid-node remove >/dev/null 2>&1 || true; fi
  if command -v chkconfig >/dev/null 2>&1; then chkconfig --del nightgrid-node 2>/dev/null || true; fi
  for l in /etc/rc[0-6].d/[SK][0-9][0-9]nightgrid-node /etc/rc.d/rc[0-6].d/[SK][0-9][0-9]nightgrid-node; do
    if [ -L "$l" ]; then rm -f "$l"; fi
  done
fi
# One at a time: systemctl gives up on the whole list if one unit is missing.
command -v systemctl >/dev/null 2>&1 || systemctl() { :; }
for u in nightgrid-node nightgrid-node-netwatch nightgrid-node-helper.socket nightgrid-node-health.socket nightgrid-node-checks \
  nightgrid-node-peer nightgrid-node-status; do
  systemctl disable --now "$u" 2>/dev/null || true
done
rm -f /etc/systemd/system/nightgrid-node.service /etc/systemd/system/nightgrid-node-netwatch.service \
  /etc/systemd/system/nightgrid-node-helper.socket /etc/systemd/system/nightgrid-node-helper@.service \
  /etc/systemd/system/nightgrid-node-health.socket /etc/systemd/system/nightgrid-node-health@.service \
  /etc/systemd/system/nightgrid-node-checks.service /etc/systemd/system/nightgrid-node-peer.service \
  /etc/systemd/system/nightgrid-node-status.service /etc/nightgrid-node/status-token
rm -rf /etc/systemd/system/nightgrid-node.service.d /etc/systemd/system/nightgrid-node-helper@.service.d \
  /etc/systemd/system/nightgrid-node-checks.service.d /etc/systemd/system/nightgrid-node-peer.service.d \
  /etc/systemd/system/nightgrid-node-status.service.d
# Blocks from firewall.block live in the node's own nftables table.
if command -v nft >/dev/null 2>&1 && nft list table inet nightgrid_node >/dev/null 2>&1; then
  nft delete table inet nightgrid_node
fi
if [ -e /etc/udev/rules.d/70-nightgrid-node-power.rules ]; then
  rm -f /etc/udev/rules.d/70-nightgrid-node-power.rules
  for f in /sys/class/powercap/intel-rapl:*/energy_uj; do [ -e "$f" ] && chgrp root "$f" && chmod 0400 "$f"; done
fi
systemctl daemon-reload 2>/dev/null || true
rm -f /usr/local/bin/nightgrid-node /usr/local/bin/nightgrid-node.prev
if [ "${1:-}" = "--purge" ]; then
  rm -rf /var/lib/nightgrid-node /var/lib/nightgrid-node-rollback /var/lib/nightgrid-node-peer /etc/nightgrid-node /run/nightgrid-node
  rm -f /etc/sysusers.d/nightgrid-node.conf
  # userdel/groupdel, or BusyBox's deluser/delgroup.
  if id nightgrid-node >/dev/null 2>&1; then
    if command -v userdel >/dev/null 2>&1; then userdel nightgrid-node; else deluser nightgrid-node; fi
  fi
  if grep -q '^nightgrid-node:' /etc/group 2>/dev/null; then
    if command -v groupdel >/dev/null 2>&1; then groupdel nightgrid-node; else delgroup nightgrid-node; fi
  fi
  echo "Nightgrid Node removed (with history, config and account)"
else
  echo "Nightgrid Node removed (history in /var/lib/nightgrid-node and config in /etc/nightgrid-node kept)"
fi
