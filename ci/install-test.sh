#!/bin/sh
# Installs, exercises and removes Nightgrid Node on a host with systemd.
# Run as root. Used by CI on the runner itself and inside systemd containers.
#   ci/install-test.sh /path/to/nightgrid-node LOGIN_USER
set -eu
bin=$1
user=$2
say() { printf '\n== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; journalctl -u nightgrid-node --no-pager -n 50 >&2 || true; exit 1; }
as_user() { su -s /bin/sh "$user" -c "$1"; }

say "install"
"$bin" install --user "$user" --json | tee /tmp/ngn-install.json
grep -q '"action":"install"' /tmp/ngn-install.json || fail "no install report"
systemctl is-active --quiet nightgrid-node || fail "service not active"
test -x /usr/local/bin/nightgrid-node || fail "binary missing"
id nightgrid-node >/dev/null || fail "service account missing"

say "permissions"
stat -c '%a %U:%G %n' /run/nightgrid-node /run/nightgrid-node/node.sock /var/lib/nightgrid-node
[ "$(stat -c %a /run/nightgrid-node/node.sock)" = 660 ] || fail "socket mode"
[ "$(stat -c %a /var/lib/nightgrid-node)" = 700 ] || fail "state dir mode"
[ "$(stat -c %U /var/lib/nightgrid-node/node.db)" = nightgrid-node ] || fail "db owner"

say "allowed user can talk to it (new login picks up the group)"
as_user "/usr/local/bin/nightgrid-node status" | tee /tmp/ngn-status.json
grep -q '"nodeId"' /tmp/ngn-status.json || fail "status from $user"
as_user "echo '{\"id\":1,\"method\":\"hello\",\"params\":{\"protoMin\":1,\"protoMax\":1}}' | /usr/local/bin/nightgrid-node bridge" | grep -q '"proto":1' || fail "bridge"

say "other users cannot"
if su -s /bin/sh nobody -c "/usr/local/bin/nightgrid-node status" >/dev/null 2>&1; then fail "nobody could connect"; fi

say "no network sockets"
if command -v ss >/dev/null; then
  pid=$(systemctl show -p MainPID --value nightgrid-node)
  ss -tulpnH | grep "pid=$pid," && fail "node has a network socket" || true
fi

say "footprint"
pid=$(systemctl show -p MainPID --value nightgrid-node)
grep -E 'VmRSS|Threads' /proc/"$pid"/status
rss=$(awk '/VmRSS/ {print $2}' /proc/"$pid"/status)
[ "$rss" -lt 20480 ] || fail "RSS ${rss} kB is over 20 MB"
systemd-analyze security nightgrid-node --no-pager 2>/dev/null | tail -n 1 || true

if [ "${WAIT_FOR_MINUTE:-0}" = 1 ]; then
  say "a minute point is written and synced"
  sleep 90
  as_user "/usr/local/bin/nightgrid-node query metrics.sync '{\"res\":60,\"after\":0}'" | tee /tmp/ngn-sync.json
  grep -q '"cpu":' /tmp/ngn-sync.json || fail "no minute point"
fi

say "reinstall (upgrade in place) keeps the node id"
before=$(as_user "/usr/local/bin/nightgrid-node query node.info" | sed 's/.*"nodeId":"\([0-9a-f]*\)".*/\1/')
cp "$bin" /tmp/ngn-upgrade && /tmp/ngn-upgrade install --json >/dev/null
after=$(as_user "/usr/local/bin/nightgrid-node query node.info" | sed 's/.*"nodeId":"\([0-9a-f]*\)".*/\1/')
[ -n "$before" ] && [ "$before" = "$after" ] || fail "node id changed: $before -> $after"

say "uninstall keeps history"
/usr/local/bin/nightgrid-node uninstall
systemctl is-active --quiet nightgrid-node && fail "still running"
test ! -e /usr/local/bin/nightgrid-node || fail "binary left"
test ! -e /etc/systemd/system/nightgrid-node.service || fail "unit left"
test -e /var/lib/nightgrid-node/node.db || fail "history removed without --purge"

say "purge removes everything"
"$bin" uninstall --purge
for p in /var/lib/nightgrid-node /etc/nightgrid-node /etc/sysusers.d/nightgrid-node.conf; do
  test ! -e "$p" || fail "$p left"
done
if id nightgrid-node >/dev/null 2>&1; then fail "account left"; fi
if getent group nightgrid-node >/dev/null; then fail "group left"; fi
"$bin" uninstall --purge >/dev/null || fail "second purge failed"

if [ -n "${UNINSTALL_SH:-}" ]; then
  say "fallback uninstall.sh works without the binary"
  "$bin" install --json >/dev/null
  rm -f /usr/local/bin/nightgrid-node
  sh "$UNINSTALL_SH" --purge
  systemctl is-active --quiet nightgrid-node && fail "still running after uninstall.sh"
  for p in /etc/systemd/system/nightgrid-node.service /var/lib/nightgrid-node /etc/nightgrid-node; do
    test ! -e "$p" || fail "$p left by uninstall.sh"
  done
  if id nightgrid-node >/dev/null 2>&1; then fail "account left by uninstall.sh"; fi
fi

say "OK"
