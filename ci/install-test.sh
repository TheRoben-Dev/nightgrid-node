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

say "events, uptime, disk usage and alert rules answer"
for m in events.sync uptime.sync disk.sync; do
  as_user "/usr/local/bin/nightgrid-node query $m '{\"after\":0}'" >/dev/null || fail "$m"
done
as_user "/usr/local/bin/nightgrid-node query alerts.active" >/dev/null || fail "alerts.active"
as_user "/usr/local/bin/nightgrid-node query alerts.rules.get" | grep -q '"diskPercent":90' || fail "default rules"

say "live metrics are pushed (protocol 2)"
timeout 20 su -s /bin/sh "$user" -c "/usr/local/bin/nightgrid-node watch metrics --count 2" | tee /tmp/ngn-watch.txt
[ "$(grep -c '"stream":"metrics"' /tmp/ngn-watch.txt)" = 2 ] || fail "no metrics frames"

if [ "${WAIT_FOR_MINUTE:-0}" = 1 ] && command -v docker >/dev/null && docker info >/dev/null 2>&1; then
  q() { as_user "/usr/local/bin/nightgrid-node query $1"; }
  say "containers: no access until root allows it"
  q containers.list | tee /tmp/ngn-ct.json
  grep -q '"access":"denied"' /tmp/ngn-ct.json || fail "expected denied before allow"

  say "root allows container actions (before the restart that allow docker does)"
  sed -i 's/^allow = \[\]$/allow = ["container.restart", "container.start"]/' /etc/nightgrid-node/config.toml
  grep -q '^allow = \["container.restart"' /etc/nightgrid-node/config.toml || fail "could not edit [actions] allow"

  say "allow docker"
  /usr/local/bin/nightgrid-node allow docker --json | tee /tmp/ngn-allow.json
  grep -q 'root-equivalent' /tmp/ngn-allow.json || fail "no warning"
  grep -q 'SupplementaryGroups=docker' /etc/systemd/system/nightgrid-node.service.d/docker.conf || fail "drop-in"
  docker rm -f ngn-crash >/dev/null 2>&1 || true
  docker run -d --name ngn-crash alpine:3.20 sh -c 'sleep 75; exit 3' >/dev/null
  i=0; until q containers.list | grep -q '"name":"ngn-crash","state":"running"'; do
    i=$((i+1)); [ $i -lt 60 ] || { q containers.list; fail "watcher never saw ngn-crash running"; }; sleep 1; done
  q containers.list | grep -q '"access":"ok"' || fail "access not ok"

  say "a crashed container raises an alert and a timeline event"
  i=0; until q alerts.active | grep -q '"key":"docker:ngn-crash"'; do
    i=$((i+1)); [ $i -lt 180 ] || fail "no container-down alert"; sleep 1; done
  q alerts.active
  i=0; until q "events.sync '{\"after\":0}'" | grep -q '"title":"ngn-crash exited"'; do
    i=$((i+1)); [ $i -lt 300 ] || fail "no container event"; sleep 1; done
  q "events.sync '{\"after\":0}'" | tr '}' '\n' | grep ngn-crash | head -3
  docker rm -f ngn-crash >/dev/null

  say "actions: policy, dry run, a real restart, a refusal, the audit log"
  q actions.policy | grep -q '"allowed":\["container.restart","container.start"\]' || { q actions.policy; fail "policy"; }
  docker rm -f ngn-web >/dev/null 2>&1 || true
  docker run -d --init --name ngn-web alpine:3.20 sleep 3600 >/dev/null
  i=0; until q containers.list | grep -q '"name":"ngn-web","state":"running"'; do
    i=$((i+1)); [ $i -lt 60 ] || fail "watcher never saw ngn-web"; sleep 1; done
  started=$(docker inspect -f '{{.State.StartedAt}}' ngn-web)
  q "actions.run '{\"action\":\"container.restart\",\"target\":\"ngn-web\",\"dryRun\":true}'" | tee /tmp/ngn-act.json
  grep -q '"outcome":"dryRun"' /tmp/ngn-act.json || fail "dry run"
  [ "$(docker inspect -f '{{.State.StartedAt}}' ngn-web)" = "$started" ] || fail "dry run restarted it"
  q "actions.run '{\"action\":\"container.restart\",\"target\":\"ngn-web\"}'" | tee /tmp/ngn-act.json
  grep -q '"outcome":"ok"' /tmp/ngn-act.json || fail "restart"
  [ "$(docker inspect -f '{{.State.StartedAt}}' ngn-web)" != "$started" ] || fail "not restarted"
  q "actions.run '{\"action\":\"container.stop\",\"target\":\"ngn-web\"}'" | grep -q '"outcome":"refused"' || fail "stop not refused"
  docker inspect -f '{{.State.Running}}' ngn-web | grep -q true || fail "stopped anyway"
  q "actions.log '{\"after\":0}'" | tee /tmp/ngn-log.json
  grep -q '"verified":true' /tmp/ngn-log.json || fail "log chain not verified"

  say "self-healing starts a container that was killed"
  q "heal.rules.set '{\"rules\":[{\"id\":\"web-down\",\"enabled\":true,\"dryRun\":false,\"when\":\"containerDown\",\"target\":\"ngn-web\",\"forMinutes\":1,\"action\":\"container.start\"}]}'" >/dev/null
  i=0; until q containers.list | grep -q '"name":"ngn-web","state":"running"'; do
    i=$((i+1)); [ $i -lt 60 ] || fail "ngn-web not running after restart"; sleep 1; done
  sleep 5
  docker kill ngn-web >/dev/null
  i=0; until q "actions.log '{\"after\":0}'" | grep -q '"source":"rule web-down"'; do
    i=$((i+1)); [ $i -lt 300 ] || { q heal.status; q alerts.active; fail "rule never ran"; }; sleep 1; done
  q "actions.log '{\"after\":0}'" | tr '}' '\n' | grep 'rule web-down'
  q "actions.log '{\"after\":0}'" | tr '}' '\n' | grep 'rule web-down' | grep -q '"outcome":"ok"' || fail "rule run failed"
  i=0; until docker inspect -f '{{.State.Running}}' ngn-web | grep -q true; do
    i=$((i+1)); [ $i -lt 30 ] || fail "not running after the rule"; sleep 1; done
  docker rm -f ngn-web >/dev/null
  q "heal.rules.set '{\"rules\":[]}'" >/dev/null

  say "deny docker"
  /usr/local/bin/nightgrid-node deny docker >/dev/null
  [ ! -e /etc/systemd/system/nightgrid-node.service.d/docker.conf ] || fail "drop-in left"
  sleep 2
  q containers.list | grep -q '"access":"denied"' || fail "still has access after deny"
fi

if [ "${WAIT_FOR_MINUTE:-0}" = 1 ]; then
  q() { as_user "/usr/local/bin/nightgrid-node query $1"; }
  ev() { q "events.sync '{\"after\":0}'"; }

  say "a watched file change is recorded"
  cp /etc/hosts /tmp/ngn-hosts.bak
  echo "# nightgrid-node test" >> /etc/hosts
  i=0; until ev | grep -q '"title":"changed /etc/hosts"'; do
    i=$((i+1)); [ $i -lt 40 ] || fail "no file change event"; sleep 1; done
  ev | tr '}' '\n' | grep 'changed /etc/hosts' | head -1
  ev | grep -q 'nightgrid-node test' && fail "file contents leaked into an event"
  cat /tmp/ngn-hosts.bak > /etc/hosts

  say "logins: no journal access until root allows it"
  q node.info | grep -q '"journal":"denied"' || { q node.info; fail "expected journal denied"; }
  /usr/local/bin/nightgrid-node allow journal --json | tee /tmp/ngn-allow-j.json
  grep -q 'every log' /tmp/ngn-allow-j.json || fail "no journal warning"
  sleep 2
  q node.info | grep -q '"journal":"ok"' || { q node.info; fail "journal not ok after allow"; }
  sleep 3
  logger -t sshd "Failed password for invalid user ngntest from 192.0.2.10 port 4242 ssh2"
  logger -t sshd "Accepted publickey for ngntest from 192.0.2.10 port 4243 ssh2: ED25519 SHA256:test"
  i=0; until ev | grep -q '"title":"ngntest logged in from 192.0.2.10"'; do
    i=$((i+1)); [ $i -lt 40 ] || { journalctl -u nightgrid-node -n 20 --no-pager; fail "no login event"; }; sleep 1; done
  ev | grep -q '"title":"failed login for ngntest from 192.0.2.10"' || fail "no failed-login event"
  ev | tr '}' '\n' | grep ngntest | head -2
  /usr/local/bin/nightgrid-node deny journal >/dev/null
  sleep 2
  q node.info | grep -q '"journal":"denied"' || fail "journal still readable after deny"

  say "network watch: new device, a new MAC for its address, then an IP conflict"
  dev=$(ip route show default 2>/dev/null | awk '{print $5; exit}')
  wait_ev() {
    i=0; until ev | grep -q "\"title\":\"$1\""; do
      i=$((i+1)); [ $i -lt 50 ] || fail "no network event: $1"; sleep 1; done
  }
  if [ -n "$dev" ] && ip neigh replace 198.51.100.77 lladdr 02:00:5e:00:00:77 dev "$dev" nud permanent 2>/dev/null; then
    wait_ev "new device 198.51.100.77"
    q net.devices | grep -q '"ip":"198.51.100.77","lastSeen"' || { q net.devices; fail "not in net.devices"; }
    ip neigh replace 198.51.100.77 lladdr 02:00:5e:00:00:78 dev "$dev" nud permanent
    wait_ev "address 198.51.100.77 answers from a new MAC"
    ip neigh replace 198.51.100.77 lladdr 02:00:5e:00:00:77 dev "$dev" nud permanent
    wait_ev "IP conflict on 198.51.100.77"
    ip neigh del 198.51.100.77 dev "$dev"
  else
    echo "skipped: cannot add a neighbour entry here"
  fi

  say "network watch helper: off until root allows it, then DHCP servers and a rogue one"
  q node.info | grep -q '"netwatch":"denied"' || { q node.info; fail "netwatch should be denied"; }
  q "net.report '{\"arp\":[]}'" 2>&1 | grep -q 'only the network watch helper' || fail "a login user could send helper reports"
  if command -v python3 >/dev/null && ip link add ngn0 type veth peer name ngn1 2>/dev/null; then
    ip link set ngn0 up && ip link set ngn1 up
    /usr/local/bin/nightgrid-node allow netwatch --json | tee /tmp/ngn-allow-n.json
    grep -q 'CAP_NET_RAW' /tmp/ngn-allow-n.json || fail "no netwatch warning"
    systemctl is-active --quiet nightgrid-node-netwatch || { journalctl -u nightgrid-node-netwatch --no-pager -n 20; fail "helper not running"; }
    systemctl show -p CapabilityBoundingSet --value nightgrid-node-netwatch | tee /dev/stderr | grep -qx 'cap_net_raw' || fail "helper has more than CAP_NET_RAW"
    i=0; until q node.info | grep -q '"netwatch":"ok"'; do
      i=$((i+1)); [ $i -lt 90 ] || { journalctl -u nightgrid-node-netwatch --no-pager -n 20; fail "helper never reported"; }; sleep 1; done
    python3 "$(dirname "$0")/dhcp-frames.py" ngn1 first
    wait_ev "DHCP server 10.99.0.1"
    python3 "$(dirname "$0")/dhcp-frames.py" ngn1 rogue
    wait_ev "another DHCP server on the LAN: 10.99.0.66"
    q net.dhcp | tee /tmp/ngn-dhcp.json
    grep -q '"ip":"10.99.0.66"' /tmp/ngn-dhcp.json || fail "rogue server not listed"
    i=0; until q net.devices | grep -q '"mac":"02:00:5e:10:00:02","name":"ngn-phone"'; do
      i=$((i+1)); [ $i -lt 50 ] || { q net.devices; fail "DHCP client not in the inventory with its name"; }; sleep 1; done
    /usr/local/bin/nightgrid-node deny netwatch >/dev/null
    test ! -e /etc/systemd/system/nightgrid-node-netwatch.service || fail "helper unit left after deny"
    systemctl is-active --quiet nightgrid-node-netwatch && fail "helper still running after deny"
    ip link del ngn0
  else
    echo "skipped: no python3 or no veth here"
  fi

  say "root helper: unit actions only on units root listed"
  cat > /etc/systemd/system/ngn-dummy.service <<'UNIT'
[Service]
ExecStart=/bin/sleep 3600
UNIT
  systemctl daemon-reload && systemctl start ngn-dummy
  sed -i 's/^allow = .*/allow = ["container.restart", "container.start", "unit.restart", "power.reboot"]/; s/^units = .*/units = ["ngn-dummy.service"]/' /etc/nightgrid-node/config.toml
  systemctl restart nightgrid-node
  i=0; until q node.info >/dev/null 2>&1; do i=$((i+1)); [ $i -lt 30 ] || fail "not back after restart"; sleep 1; done
  q node.info | grep -q '"helper":"denied"' || { q node.info; fail "helper should be denied"; }
  q "actions.run '{\"action\":\"unit.restart\",\"target\":\"ngn-dummy.service\"}'" | grep -q 'allow helper' || fail "ran without the helper"
  /usr/local/bin/nightgrid-node allow helper --json | tee /tmp/ngn-allow-h.json
  grep -q 'actions\] allow' /tmp/ngn-allow-h.json || fail "no helper warning"
  [ "$(stat -c '%a %U' /run/nightgrid-node-helper.sock)" = "600 nightgrid-node" ] || { stat /run/nightgrid-node-helper.sock; fail "helper socket mode"; }
  as_user "python3 -c 'import socket; socket.socket(socket.AF_UNIX).connect(\"/run/nightgrid-node-helper.sock\")'" 2>/dev/null && fail "a login user reached the root helper"
  q node.info | grep -q '"helper":"ok"' || { q node.info; fail "helper not ok after allow"; }
  pid=$(systemctl show -p MainPID --value ngn-dummy)
  q "actions.run '{\"action\":\"unit.restart\",\"target\":\"ngn-dummy.service\"}'" | tee /tmp/ngn-unit.json
  grep -q '"outcome":"ok"' /tmp/ngn-unit.json || { journalctl -u 'nightgrid-node-helper@*' --no-pager -n 20; fail "unit restart"; }
  [ "$(systemctl show -p MainPID --value ngn-dummy)" != "$pid" ] || fail "ngn-dummy not restarted"
  q "actions.run '{\"action\":\"unit.restart\",\"target\":\"cron.service\"}'" | grep -q '"outcome":"refused"' || fail "unlisted unit not refused"
  say "the helper checks root's config itself, whoever asks"
  python3 - <<'PY' | tee /tmp/ngn-direct.json
import socket
s = socket.socket(socket.AF_UNIX)
s.connect("/run/nightgrid-node-helper.sock")
s.sendall(b'{"action":"unit.stop","target":"cron.service"}\n')
print(s.recv(4096).decode())
PY
  grep -q '"ok":false' /tmp/ngn-direct.json || fail "helper ran something root did not list"
  systemctl is-active --quiet ngn-dummy || fail "ngn-dummy went down"

  say "power: a reboot is scheduled a minute ahead, then cancelled"
  q "actions.run '{\"action\":\"power.reboot\"}'" | tee /tmp/ngn-power.json
  if grep -q '"outcome":"ok"' /tmp/ngn-power.json; then
    shutdown -c || fail "could not cancel the reboot"
    grep -q 'shutdown -c' /tmp/ngn-power.json || fail "no cancel hint"
  else
    shutdown -c 2>/dev/null || true
    journalctl -u 'nightgrid-node-helper@*' --no-pager -n 20
    fail "reboot not scheduled"
  fi
  q "actions.log '{\"after\":0}'" | grep -q '"verified":true' || fail "log chain"

  /usr/local/bin/nightgrid-node deny helper >/dev/null
  test ! -e /run/nightgrid-node-helper.sock || fail "helper socket left after deny"
  q node.info | grep -q '"helper":"denied"' || fail "helper still ok after deny"
  systemctl stop ngn-dummy; rm -f /etc/systemd/system/ngn-dummy.service; systemctl daemon-reload
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
