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
as_user "/usr/local/bin/nightgrid-node status --json" | tee /tmp/ngn-status.json
grep -q '"nodeId"' /tmp/ngn-status.json || fail "status from $user"
as_user "/usr/local/bin/nightgrid-node status" | tee /tmp/ngn-status.txt
grep -q "nightgrid-node $(/usr/local/bin/nightgrid-node --version | cut -d' ' -f2)" /tmp/ngn-status.txt || fail "readable status"
as_user "/usr/local/bin/nightgrid-node events --count 3" | grep -q . || fail "events"
as_user "/usr/local/bin/nightgrid-node events | head -1" >/dev/null 2>/tmp/ngn-pipe.err; [ ! -s /tmp/ngn-pipe.err ] || { cat /tmp/ngn-pipe.err; fail "events into a closed pipe"; }
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

  say "rollback: a change that isn't confirmed is put back by the dead-man timer"
  mkdir -p /srv/ngn-test
  printf 'v1\n' > /srv/ngn-test/app.conf
  chown "$user" /srv/ngn-test/app.conf && chmod 640 /srv/ngn-test/app.conf
  sed -i 's/^allow = .*/allow = ["unit.restart", "files.rollback"]/; s|^paths = .*|paths = ["/srv/ngn-test"]|' /etc/nightgrid-node/config.toml
  /usr/local/bin/nightgrid-node allow helper >/dev/null
  grep -q 'ReadWritePaths=-/srv/ngn-test' /etc/systemd/system/nightgrid-node-helper@.service.d/rollback.conf || fail "no rollback drop-in"
  systemctl restart nightgrid-node
  i=0; until q node.info >/dev/null 2>&1; do i=$((i+1)); [ $i -lt 30 ] || fail "not back after restart"; sleep 1; done
  q "changes.begin '{\"paths\":[\"/etc/hostname\"],\"confirmWithinSecs\":30}'" 2>&1 | grep -q 'not under \[rollback\] paths' || fail "a path outside [rollback] paths was accepted"
  pid=$(systemctl show -p MainPID --value ngn-dummy)
  q "changes.begin '{\"paths\":[\"/srv/ngn-test/app.conf\",\"/srv/ngn-test/new.conf\"],\"confirmWithinSecs\":30,\"note\":\"ci change\",\"onRevert\":{\"action\":\"unit.restart\",\"target\":\"ngn-dummy.service\"}}'" | tee /tmp/ngn-change.json
  grep -q '"state":"pending"' /tmp/ngn-change.json || { journalctl -u 'nightgrid-node-helper@*' --no-pager -n 20; fail "change not started"; }
  printf 'v2 broken\n' > /srv/ngn-test/app.conf && chmod 666 /srv/ngn-test/app.conf
  printf 'new\n' > /srv/ngn-test/new.conf
  i=0; until q changes.list | grep -q '"state":"reverted"'; do
    i=$((i+1)); [ $i -lt 90 ] || { q changes.list; journalctl -u 'nightgrid-node-helper@*' --no-pager -n 20; fail "not reverted"; }; sleep 1; done
  q changes.list
  [ "$(cat /srv/ngn-test/app.conf)" = v1 ] || fail "content not put back"
  [ "$(stat -c '%a %U' /srv/ngn-test/app.conf)" = "640 $user" ] || { stat /srv/ngn-test/app.conf; fail "mode/owner not put back"; }
  test ! -e /srv/ngn-test/new.conf || fail "new file not removed"
  i=0; until [ "$(systemctl show -p MainPID --value ngn-dummy)" != "$pid" ]; do
    i=$((i+1)); [ $i -lt 20 ] || fail "onRevert did not restart ngn-dummy"; sleep 1; done
  ev | grep -q '"title":"change reverted: \\"ci change\\""' || { ev | tr '}' '\n' | grep -i change; fail "no revert event"; }

  say "rollback: a confirmed change stays"
  id=$(q "changes.begin '{\"paths\":[\"/srv/ngn-test/app.conf\"],\"confirmWithinSecs\":30}'" | sed 's/.*"id":"\([0-9a-f]*\)".*/\1/')
  printf 'v3\n' > /srv/ngn-test/app.conf
  q "changes.confirm '{\"id\":\"$id\"}'" | grep -q '"state":"confirmed"' || fail "confirm"
  test ! -e "/var/lib/nightgrid-node-rollback/$id" || fail "snapshot left after confirm"
  sleep 40
  [ "$(cat /srv/ngn-test/app.conf)" = v3 ] || fail "confirmed change was reverted"
  q "actions.log '{\"after\":0}'" | grep -q '"verified":true' || fail "log chain"
  rm -rf /srv/ngn-test

  /usr/local/bin/nightgrid-node deny helper >/dev/null
  test ! -e /run/nightgrid-node-helper.sock || fail "helper socket left after deny"
  q node.info | grep -q '"helper":"denied"' || fail "helper still ok after deny"
  systemctl stop ngn-dummy; rm -f /etc/systemd/system/ngn-dummy.service; systemctl daemon-reload

  say "health helper: off until root allows it, then pools, SMART and updates"
  q node.info | grep -q '"health":"denied"' || { q node.info; fail "health should be denied"; }
  q health.latest | grep -q '"access":"denied"' || { q health.latest; fail "health.latest should say denied"; }
  /usr/local/bin/nightgrid-node allow health --json | tee /tmp/ngn-allow-health.json
  grep -q 'read-only' /tmp/ngn-allow-health.json || fail "no health warning"
  [ "$(stat -c '%a %U' /run/nightgrid-node-health.sock)" = "600 nightgrid-node" ] || { stat /run/nightgrid-node-health.sock; fail "health socket mode"; }
  as_user "python3 -c 'import socket; socket.socket(socket.AF_UNIX).connect(\"/run/nightgrid-node-health.sock\")'" 2>/dev/null && fail "a login user reached the health helper"
  q node.info | grep -q '"health":"ok"' || { q node.info; fail "health not ok after allow"; }
  i=0; until q health.latest | grep -q '"manager":"apt"'; do
    i=$((i+1)); [ $i -lt 90 ] || { q health.latest; journalctl -u 'nightgrid-node-health@*' --no-pager -n 30; fail "no update check"; }; sleep 1; done
  i=0; until q health.latest | grep -q '"smart":{'; do
    i=$((i+1)); [ $i -lt 90 ] || { q health.latest; journalctl -u 'nightgrid-node-health@*' --no-pager -n 30; fail "no SMART read"; }; sleep 1; done
  q health.latest | tee /tmp/ngn-health.json
  grep -q '"pools":{"at":' /tmp/ngn-health.json || fail "no pools read"
  python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); bad=[k for k in ("smart","pools","updates") if "error" in r[k]]; sys.exit(1 if bad else 0)' /tmp/ngn-health.json \
    || { journalctl -u 'nightgrid-node-health@*' --no-pager -n 30; fail "a health read failed"; }
  say "SMART reads the same through the sandbox as directly as root"
  dev=$(smartctl --scan-open 2>/dev/null | awk '$1 ~ /^\/dev\// {print $1; exit}')
  if [ -n "$dev" ]; then
    direct=$(smartctl -a -j "$dev" | python3 -c 'import json,sys; print(json.load(sys.stdin)["smartctl"]["exit_status"] & 3)')
    echo "direct smartctl on $dev: open/command bits $direct"
    if [ "$direct" = 0 ] && grep -q "couldn't open the device" /tmp/ngn-health.json; then
      journalctl -u 'nightgrid-node-health@*' --no-pager -n 30; fail "the health sandbox keeps smartctl from opening $dev"
    fi
  fi
  say "the health helper only queries UPS targets root listed"
  python3 - <<'PY' > /tmp/ngn-health-direct.json
import socket
s = socket.socket(socket.AF_UNIX)
s.connect("/run/nightgrid-node-health.sock")
s.sendall(b'{"read":"ups","target":"evil@203.0.113.9"}\n')
print(s.makefile().readline())
PY
  cat /tmp/ngn-health-direct.json
  grep -q '"ok":false' /tmp/ngn-health-direct.json || fail "health helper queried a UPS root did not list"
  /usr/local/bin/nightgrid-node deny health >/dev/null
  test ! -e /run/nightgrid-node-health.sock || fail "health socket left after deny"
  q node.info | grep -q '"health":"denied"' || fail "health still ok after deny"

  say "logs: saved-search alerts, then a new kind of error after the learning round"
  /usr/local/bin/nightgrid-node allow journal >/dev/null
  sed -i 's/^interval_secs = 600$/interval_secs = 30/' /etc/nightgrid-node/config.toml
  grep -q '^interval_secs = 30$' /etc/nightgrid-node/config.toml || fail "no [logs] interval in the config"
  systemctl restart nightgrid-node
  i=0; until q node.info >/dev/null 2>&1; do i=$((i+1)); [ $i -lt 30 ] || fail "not back after restart"; sleep 1; done
  q "alerts.rules.set '{\"logAnomaly\":true}'" | grep -q '"logAnomaly":true' || fail "logAnomaly rule"
  q "logs.searches.set '{\"searches\":[{\"id\":\"s1\",\"name\":\"ngn hits\",\"text\":\"ngn-logsearch-hit\"}]}'" | grep -q '"id":"s1"' || fail "searches.set"
  q "logs.searches.set '{\"searches\":[{\"id\":\"b\",\"name\":\"bad\",\"text\":\"(\",\"regex\":true}]}'" 2>&1 | grep -q 'not a valid regular expression' || fail "a bad regex was accepted"
  logger -p user.err -t ngn-app "ERROR ngn first failure 1"
  logger -t ngn-app "ngn-logsearch-hit one"
  i=0; until q logs.status | grep -q '"hits":1,"id":"s1"'; do
    i=$((i+1)); [ $i -lt 90 ] || { q logs.status; journalctl -u nightgrid-node --no-pager -n 20; fail "saved search never matched"; }; sleep 1; done
  q logs.status | tee /tmp/ngn-logs.json
  grep -q '"journal":"ok"' /tmp/ngn-logs.json || fail "logs.status journal access"
  grep -q '"source":"journal/ngn-app"' /tmp/ngn-logs.json || fail "journal/ngn-app not followed"
  i=0; until q alerts.active | grep -q '"key":"logsearch:s1"'; do
    i=$((i+1)); [ $i -lt 90 ] || { q alerts.active; fail "no saved-search alert"; }; sleep 1; done
  logger -p user.err -t ngn-app "fatal: a completely different breakage in module x"
  i=0; until q alerts.active | grep -q '"key":"logs:journal/ngn-app:new"'; do
    i=$((i+1)); [ $i -lt 120 ] || { q alerts.active; q logs.status; fail "no new-error alert"; }; sleep 1; done
  q "logs.samples '{\"source\":\"journal/ngn-app\"}'" | grep -q 'completely different breakage' || fail "no samples"
  q "logs.searches.set '{\"searches\":[]}'" >/dev/null
  q "alerts.rules.set '{\"logAnomaly\":false}'" >/dev/null
  sed -i 's/^interval_secs = 30$/interval_secs = 600/' /etc/nightgrid-node/config.toml
  /usr/local/bin/nightgrid-node deny journal >/dev/null

  say "usage: services from cgroups, live every second, minute history, power grant"
  i=0; until q usage.latest | grep -q '"name":"nightgrid-node.service"'; do
    i=$((i+1)); [ $i -lt 30 ] || { q usage.latest | head -c 2000; fail "the node's own service not in usage"; }; sleep 1; done
  q usage.latest | grep -q '"cgroups":true' || fail "cgroup v2 not detected"
  t0=$(date +%s)
  as_user "timeout 15 /usr/local/bin/nightgrid-node watch usage --count 4" > /tmp/ngn-usage.jsonl || fail "usage stream"
  [ "$(wc -l < /tmp/ngn-usage.jsonl)" -eq 4 ] && [ $(( $(date +%s) - t0 )) -le 8 ] || { cat /tmp/ngn-usage.jsonl | head -c 500; fail "usage stream not live"; }
  i=0; until q "usage.history '{\"kind\":\"service\",\"name\":\"nightgrid-node.service\"}'" | grep -q '"mem":'; do
    i=$((i+1)); [ $i -lt 90 ] || fail "no usage minute stored"; sleep 1; done
  q node.info | grep -oE '"power":"[a-z]+"'
  /usr/local/bin/nightgrid-node allow power --json | grep -q 'energy counters' || fail "no power warning"
  test -f /etc/udev/rules.d/70-nightgrid-node-power.rules || fail "no power udev rule"
  if ls /sys/class/powercap/intel-rapl:*/energy_uj >/dev/null 2>&1; then
    i=0; until q node.info | grep -q '"power":"ok"'; do i=$((i+1)); [ $i -lt 30 ] || fail "power not readable after allow"; sleep 1; done
  fi
  /usr/local/bin/nightgrid-node deny power >/dev/null
  test ! -e /etc/udev/rules.d/70-nightgrid-node-power.rules || fail "power rule left after deny"

  say "security: a new listening port, a login-burst alert, authorized_keys through the health helper"
  wait_for() {
    i=0; until ev | grep -qE "\"title\":\"$1"; do
      i=$((i+1)); [ $i -lt "$2" ] || { ev | tr '}' '\n' | tail -15; fail "no event: $1"; }; sleep 1; done
  }
  sed -i '/^\[ports\]/,$ s/^interval_secs = 15$/interval_secs = 5/' /etc/nightgrid-node/config.toml
  grep -A6 '^\[ports\]' /etc/nightgrid-node/config.toml | grep -q '^interval_secs = 5$' || fail "no [ports] interval in the config"
  /usr/local/bin/nightgrid-node allow journal >/dev/null
  /usr/local/bin/nightgrid-node allow health >/dev/null
  systemctl restart nightgrid-node
  i=0; until q node.info >/dev/null 2>&1; do i=$((i+1)); [ $i -lt 30 ] || fail "not back after restart"; sleep 1; done
  as_user "echo '{\"id\":1,\"method\":\"hello\",\"params\":{\"protoMin\":1,\"protoMax\":2}}' | /usr/local/bin/nightgrid-node bridge" | grep -q '"security":1' || fail "no security cap"
  python3 -m http.server 18765 --bind 0.0.0.0 >/dev/null 2>&1 &
  srv=$!
  wait_for "new listener 18765/tcp" 60
  q ports.list | grep -q '"port":18765,"process":null,"proto":"tcp","scope":"all"' || { q ports.list; fail "18765 not in ports.list"; }
  ev | tr '}' '\n' | grep 'listener 18765' | head -1 | grep -q '"level":"warn"' || fail "a port on every interface should warn"
  kill $srv
  wait_for "stopped listening 18765/tcp" 60

  q "alerts.rules.set '{\"loginBurst\":true}'" | grep -q '"loginBurst":true' || fail "loginBurst rule"
  sleep 3
  for n in $(seq 1 25); do logger -t sshd "Failed password for root from 203.0.113.50 port $n ssh2"; done
  i=0; until q alerts.active | grep -q '"key":"logins:burst"'; do
    i=$((i+1)); [ $i -lt 120 ] || { q alerts.active; journalctl -u nightgrid-node --no-pager -n 20; fail "no login-burst alert"; }; sleep 1; done
  q alerts.active | grep -q '203.0.113.50 ×25' || { q alerts.active; fail "burst alert detail"; }
  q "alerts.rules.set '{\"loginBurst\":false}'" >/dev/null

  useradd -m -s /bin/sh ngnkeys
  chmod 700 /home/ngnkeys
  mkdir -m 700 /home/ngnkeys/.ssh
  echo "ssh-ed25519 AAAAngnsecretkey ci" > /home/ngnkeys/.ssh/authorized_keys
  chown -R ngnkeys: /home/ngnkeys/.ssh && chmod 600 /home/ngnkeys/.ssh/authorized_keys
  as_user "cat /home/ngnkeys/.ssh/authorized_keys" 2>/dev/null && fail "the service can read a user's keys itself"
  wait_for "added /home/ngnkeys/.ssh/authorized_keys" 90
  ev | tr '}' '\n' | grep 'ngnkeys/.ssh/authorized_keys' | head -1
  ev | grep -q 'AAAAngnsecretkey' && fail "key contents leaked into an event"
  echo "ssh-ed25519 AAAAngnother ci2" >> /home/ngnkeys/.ssh/authorized_keys
  wait_for "changed /home/ngnkeys/.ssh/authorized_keys" 90
  ev | tr '}' '\n' | grep 'changed /home/ngnkeys' | head -1 | grep -q 'sha256' || fail "no hash in the change (read through the helper)"
  userdel -r ngnkeys 2>/dev/null || true
  /usr/local/bin/nightgrid-node deny health >/dev/null

  say "blocks: manual block and unblock, never private addresses, auto-block of a login burst"
  if command -v nft >/dev/null; then
    sed -i 's/^allow = .*/allow = ["firewall.block", "firewall.unblock"]/' /etc/nightgrid-node/config.toml
    sed -i '/^\[block\]/,$ { s/^auto = false$/auto = true/; s/^threshold = 10$/threshold = 5/; s/^minutes = 60$/minutes = 5/; }' /etc/nightgrid-node/config.toml
    grep -A3 '^\[block\]' /etc/nightgrid-node/config.toml | head -2
    /usr/local/bin/nightgrid-node allow helper >/dev/null
    systemctl restart nightgrid-node
    i=0; until q node.info >/dev/null 2>&1; do i=$((i+1)); [ $i -lt 30 ] || fail "not back after restart"; sleep 1; done
    q block.list | tee /dev/stderr | grep -q '"auto":true' || fail "auto-block not on"
    q "actions.run '{\"action\":\"firewall.block\",\"target\":\"192.168.1.9\"}'" | grep -q '"outcome":"refused"' || fail "a private address was blocked"
    q "actions.run '{\"action\":\"firewall.block\",\"target\":\"1.2.3.4 } ; flush ruleset\"}'" | grep -q '"outcome":"refused"' || fail "a non-address got through"
    q "actions.run '{\"action\":\"firewall.block\",\"target\":\"203.0.113.60\"}'" | tee /tmp/ngn-block.json
    grep -q '"outcome":"ok"' /tmp/ngn-block.json || { journalctl -u 'nightgrid-node-helper@*' --no-pager -n 20; fail "block"; }
    nft list set inet nightgrid_node v4 | tee /dev/stderr | grep -q '203.0.113.60 timeout 5m' || fail "not in the nft set with a timeout"
    nft list chain inet nightgrid_node forward | grep -q 'ip saddr @v4 drop' || fail "no forward rule"
    q block.list | grep -q '"addr":"203.0.113.60"' || fail "not in block.list"
    q "actions.run '{\"action\":\"firewall.unblock\",\"target\":\"203.0.113.60\"}'" | grep -q '"outcome":"ok"' || fail "unblock"
    nft list set inet nightgrid_node v4 | grep -q '203.0.113.60' && fail "still in the set after unblock"
    sleep 2
    logger -t sshd "Accepted password for ngnok from 198.51.100.88 port 5000 ssh2"
    for n in $(seq 1 6); do
      logger -t sshd "Failed password for root from 198.51.100.77 port $n ssh2"
      logger -t sshd "Failed password for ngnok from 198.51.100.88 port $n ssh2"
    done
    i=0; until nft list set inet nightgrid_node v4 | grep -q '198.51.100.77'; do
      i=$((i+1)); [ $i -lt 40 ] || { q block.list; journalctl -u nightgrid-node --no-pager -n 20; fail "no auto-block"; }; sleep 1; done
    q block.list | grep -q '"source":"auto-block"' || fail "auto-block source"
    ev | grep -q '"title":"blocked 198.51.100.77 for 5 minutes"' || fail "no block event"
    sleep 3
    nft list set inet nightgrid_node v4 | grep -q '198.51.100.88' && fail "an address that just logged in was blocked"
    q "actions.log '{\"after\":0}'" | grep -q '"verified":true' || fail "log chain"
    /usr/local/bin/nightgrid-node deny helper --json | grep -q 'firewall blocks' || fail "deny helper left the blocks"
    nft list table inet nightgrid_node >/dev/null 2>&1 && fail "nft table left after deny helper"
    sed -i '/^\[block\]/,$ s/^auto = true$/auto = false/' /etc/nightgrid-node/config.toml
  else
    echo "skipped: no nft here"
  fi
  /usr/local/bin/nightgrid-node deny journal >/dev/null

  say "checks: a network-limited runner checks HTTP, HTTPS (self-signed) and TCP, and a down service alerts"
  q node.info | grep -q '"checks":"denied"' || { q node.info; fail "checks should be denied before allow"; }
  sed -i '/^\[checks\]/,/^\[/ s/^interval_secs = 60$/interval_secs = 30/' /etc/nightgrid-node/config.toml
  systemctl restart nightgrid-node
  i=0; until q node.info >/dev/null 2>&1; do i=$((i+1)); [ $i -lt 30 ] || fail "not back after restart"; sleep 1; done
  mkdir -p /tmp/ngn-tls && openssl req -x509 -newkey rsa:2048 -nodes -days 10 -subj /CN=localhost \
    -keyout /tmp/ngn-tls/key.pem -out /tmp/ngn-tls/cert.pem 2>/dev/null
  python3 -m http.server 18766 --bind 127.0.0.1 >/dev/null 2>&1 &
  http_pid=$!
  python3 - <<'PY' >/dev/null 2>&1 &
import http.server, ssl
s = http.server.HTTPServer(("127.0.0.1", 18767), http.server.SimpleHTTPRequestHandler)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain("/tmp/ngn-tls/cert.pem", "/tmp/ngn-tls/key.pem")
s.socket = ctx.wrap_socket(s.socket, server_side=True)
s.serve_forever()
PY
  https_pid=$!
  q "checks.set '{\"checks\":[{\"id\":\"web\",\"name\":\"web\",\"url\":\"http://127.0.0.1:18766/\"},{\"id\":\"tls\",\"name\":\"tls\",\"url\":\"https://localhost:18767/\",\"allowUntrusted\":true},{\"id\":\"strict\",\"name\":\"strict\",\"url\":\"https://localhost:18767/\"},{\"id\":\"gone\",\"name\":\"gone\",\"url\":\"tcp://127.0.0.1:1\"},{\"id\":\"out\",\"name\":\"out\",\"url\":\"http://1.1.1.1/\"}]}'" | grep -q '"id":"web"' || fail "checks.set"
  q "checks.set '{\"checks\":[{\"id\":\"x\",\"name\":\"x\",\"url\":\"http://a/\\r\\nX: y\"}]}'" 2>&1 | grep -q 'bad characters' || fail "a header injection URL was accepted"
  /usr/local/bin/nightgrid-node allow checks --json | tee /tmp/ngn-allow-c.json
  grep -q 'no capabilities' /tmp/ngn-allow-c.json || fail "no checks warning"
  grep -q '^IPAddressAllow=192.168.0.0/16$' /etc/systemd/system/nightgrid-node-checks.service.d/networks.conf || fail "no networks drop-in"
  systemctl show -p User --value nightgrid-node-checks | grep -qx nightgrid-node || fail "runner not the service account"
  i=0; until q checks.latest | grep -q '"access":"ok"'; do
    i=$((i+1)); [ $i -lt 60 ] || { q checks.latest; journalctl -u nightgrid-node-checks --no-pager -n 20; fail "runner never reported"; }; sleep 1; done
  q checks.latest | tee /tmp/ngn-checks.json
  python3 - /tmp/ngn-checks.json <<'PY' || fail "check results"
import json, sys
r = {x["id"]: x["result"] for x in json.load(open(sys.argv[1]))["readings"]}
assert r["web"]["up"] and r["web"]["status"] == 200, r["web"]
assert r["tls"]["up"] and r["tls"]["tls"]["expires"] and not r["tls"]["tls"]["trusted"], r["tls"]
assert not r["strict"]["up"] and "not trusted" in r["strict"]["error"], r["strict"]
assert r["gone"]["error"] == "connection refused", r["gone"]
assert "outside [checks] networks" in r["out"]["error"], r["out"]
PY
  q "checks.report '{\"readings\":[]}'" 2>&1 | grep -q 'only the check runner' || fail "a login user could report checks"
  i=0; until q alerts.active | grep -q '"key":"svc:gone"'; do
    i=$((i+1)); [ $i -lt 240 ] || { q alerts.active; fail "no service-down alert"; }; sleep 1; done
  q alerts.active | grep -q '"key":"svc:tls:cert"' || { q alerts.active; fail "no certificate alert for a 10-day cert"; }
  kill $http_pid $https_pid 2>/dev/null || true
  /usr/local/bin/nightgrid-node deny checks >/dev/null
  test ! -e /etc/systemd/system/nightgrid-node-checks.service || fail "runner unit left after deny"
  systemctl is-active --quiet nightgrid-node-checks && fail "runner still running after deny"

  say "jobs: backup units and paths, the app's scheduled jobs, failed and stale backups alert"
  sed -i '/^\[jobs\]/,/^\[/ s/^interval_secs = 60$/interval_secs = 30/' /etc/nightgrid-node/config.toml
  /usr/local/bin/nightgrid-node allow journal >/dev/null
  systemctl restart nightgrid-node
  i=0; until q node.info >/dev/null 2>&1; do i=$((i+1)); [ $i -lt 30 ] || fail "not back after restart"; sleep 1; done
  q node.info | grep -q '"jobs":"ok"' || { q node.info; fail "jobs access not ok"; }
  cat > /etc/systemd/system/ngn-backup.service <<'UNIT'
[Service]
Type=oneshot
ExecStart=/bin/sh -c 'echo "Added to the repository: 12.500 MiB (3.402 MiB stored)"; echo "processed 3 files, 1.000 KiB in 0:01"; echo "snapshot 1a2b3c4d saved"'
UNIT
  cat > /etc/systemd/system/ngn-backup.timer <<'UNIT'
[Timer]
OnCalendar=*-01-01 00:00:00
UNIT
  cat > /etc/systemd/system/ngn-failing.service <<'UNIT'
[Service]
Type=oneshot
ExecStart=/bin/sh -c 'echo "Fatal: repository is locked"; exit 1'
UNIT
  cat > /etc/systemd/system/nightgrid-job-e2e.service <<'UNIT'
# nightgrid {"name":"e2e test"}
[Unit]
Description=Nightgrid job: e2e test

[Service]
Type=oneshot
ExecStart=/bin/sh -c 'echo working; echo "rsync error: e2e failure"; exit 3'
UNIT
  systemctl daemon-reload
  mkdir -p /srv/ngn-backup/a /srv/ngn-old
  head -c 20000 /dev/zero > /srv/ngn-backup/a/data
  touch -d '3 days ago' /srv/ngn-old/file /srv/ngn-old
  q "jobs.set '{\"backups\":[{\"id\":1,\"name\":\"nightly\",\"tool\":\"restic\",\"source\":\"unit\",\"target\":\"ngn-backup.timer\",\"staleHours\":26},{\"id\":2,\"name\":\"copy\",\"tool\":\"rsync\",\"source\":\"path\",\"target\":\"/srv/ngn-backup\",\"staleHours\":26},{\"id\":3,\"name\":\"old\",\"tool\":\"borg\",\"source\":\"path\",\"target\":\"/srv/ngn-old\",\"staleHours\":24},{\"id\":4,\"name\":\"locked\",\"tool\":\"restic\",\"source\":\"unit\",\"target\":\"ngn-failing.service\",\"staleHours\":26}]}'" | grep -q '"interval":30' || fail "jobs.set"
  q "jobs.set '{\"backups\":[{\"id\":9,\"name\":\"x\",\"tool\":\"restic\",\"source\":\"unit\",\"target\":\"x; rm -rf /\",\"staleHours\":26}]}'" 2>&1 | grep -q "isn't a systemd unit" || fail "a bad unit name was accepted"
  i=0; until q jobs.latest | grep -q '"jobId":4'; do
    i=$((i+1)); [ $i -lt 30 ] || { q jobs.latest; fail "jobs never read"; }; sleep 1; done
  # An active timer keeps its service loaded, so the run's state isn't
  # garbage-collected, as on a real host.
  systemctl start ngn-backup.timer
  systemctl start ngn-backup.service
  systemctl start ngn-failing.service nightgrid-job-e2e.service 2>/dev/null || true
  i=0; until ev | grep -q '"title":"job e2e test failed (exit 3)"'; do
    i=$((i+1)); [ $i -lt 90 ] || { ev | tr '}' '\n' | grep -i job | tail -5; q jobs.latest; fail "no scheduled job event"; }; sleep 1; done
  ev | grep -q '"title":"backup nightly finished"' || { q jobs.latest; fail "no backup finished event"; }
  ev | grep -q '"title":"backup locked failed"' || fail "no backup failed event"
  ev | tr '}' '\n' | grep -E 'backup nightly|job e2e' | head -2
  q jobs.latest | tee /tmp/ngn-jobs.json
  python3 - /tmp/ngn-jobs.json <<'PY' || fail "job results"
import json, sys
d = json.load(open(sys.argv[1]))
b = {x["jobId"]: x for x in d["backups"]}
assert b[1]["result"] == "ok" and b[1]["summary"] == "snapshot 1a2b3c4d saved" and b[1]["size"] == 1024, b[1]
assert b[1]["added"] == 13107200 and not b[1]["stale"], b[1]
assert b[2]["result"] == "ok" and b[2]["size"] >= 20000 and not b[2]["stale"], b[2]
assert b[3]["stale"], b[3]
assert b[4]["result"] == "failed" and b[4]["summary"] == "Fatal: repository is locked", b[4]
r = {x["unit"]: x for x in d["runs"]}["nightgrid-job-e2e"]
assert r["result"] == "failed" and r["exitCode"] == 3 and r["description"] == "e2e test", r
assert r["summary"] == "rsync error: e2e failure", r
PY
  i=0; until q alerts.active | grep -q '"key":"backup:4"'; do
    i=$((i+1)); [ $i -lt 150 ] || { q alerts.active; fail "no backup-failed alert"; }; sleep 1; done
  q alerts.active | grep -q '"key":"backup:3"' || { q alerts.active; fail "no stale-backup alert"; }
  q alerts.active | grep -q '"key":"backup:1"' && fail "a good backup alerted"
  /usr/local/bin/nightgrid-node deny journal >/dev/null
  rm -f /etc/systemd/system/ngn-backup.* /etc/systemd/system/ngn-failing.service /etc/systemd/system/nightgrid-job-e2e.service
  systemctl daemon-reload

  say "ups: on battery past root's limit counts down and requests a power-off (dry run); cancel holds it"
  q ups.guard | grep -q '"enabled":false' || { q ups.guard; fail "UPS guard should be off by default"; }
  mkdir -p /run/ngn-ups && echo "ups.status: OL" > /run/ngn-ups/state
  printf '#!/bin/sh\ncat /run/ngn-ups/state\n' > /usr/local/sbin/upsc && chmod 755 /usr/local/sbin/upsc
  sed -i 's/^allow = .*/allow = ["power.poweroff"]/; s/^ups = \[\]$/ups = ["ups@localhost"]/' /etc/nightgrid-node/config.toml
  sed -i '/^\[ups\]/,$ { s/^shutdown = false$/shutdown = true/; s/^countdown_secs = 60$/countdown_secs = 10/; s/^dry_run = false$/dry_run = true/; }' /etc/nightgrid-node/config.toml
  /usr/local/bin/nightgrid-node allow health >/dev/null
  /usr/local/bin/nightgrid-node allow helper >/dev/null
  systemctl restart nightgrid-node
  i=0; until q node.info >/dev/null 2>&1; do i=$((i+1)); [ $i -lt 30 ] || fail "not back after restart"; sleep 1; done
  q ups.guard | tee /dev/stderr | grep -q '"enabled":true' || fail "UPS guard not on"
  q ups.guard | grep -q '"problem":null' || fail "UPS guard reports a problem"
  echo "ups.status: OB DISCHRG LB" > /run/ngn-ups/state
  i=0; until ev | grep -q '"title":"UPS shutdown in 10 s"'; do
    i=$((i+1)); [ $i -lt 90 ] || { q ups.guard; q health.latest | head -c 600; fail "no UPS countdown"; }; sleep 1; done
  i=0; until q "actions.log '{\"after\":0}'" | grep -q '"source":"ups"'; do
    i=$((i+1)); [ $i -lt 60 ] || { q ups.guard; fail "no power-off request from the UPS guard"; }; sleep 1; done
  q "actions.log '{\"after\":0}'" | tr '}' '\n' | grep '"source":"ups"' | grep -q '"outcome":"dryRun"' || { q "actions.log '{\"after\":0}'"; fail "UPS power-off wasn't a dry run"; }
  q ups.guard | grep -q '"phase":"shuttingDown"' || { q ups.guard; fail "guard not shutting down"; }
  shutdown --show 2>&1 | grep -qi 'power-off' && fail "a real power-off was scheduled in dry run"
  echo "ups.status: OL CHRG" > /run/ngn-ups/state
  i=0; until q ups.guard | grep -q '"phase":"idle"'; do
    i=$((i+1)); [ $i -lt 60 ] || { q ups.guard; fail "guard not back to idle on line power"; }; sleep 1; done
  echo "ups.status: OB DISCHRG" > /run/ngn-ups/state
  i=0; until q ups.guard | grep -q '"phase":"onBattery"'; do
    i=$((i+1)); [ $i -lt 60 ] || { q ups.guard; fail "guard didn't see on-battery"; }; sleep 1; done
  q ups.cancel | grep -q '"phase":"cancelled"' || fail "ups.cancel"
  ev | grep -q '"title":"UPS shutdown cancelled"' || fail "no cancel event"
  echo "ups.status: OL" > /run/ngn-ups/state
  /usr/local/bin/nightgrid-node deny helper >/dev/null
  /usr/local/bin/nightgrid-node deny health >/dev/null
  rm -f /usr/local/sbin/upsc

  say "peers: the link unit pairs with itself over loopback, mutual TLS with a pinned fingerprint"
  q peers.latest | grep -q '"access":"off"' || { q peers.latest; fail "link should be off by default"; }
  sed -i '/^\[peers\]/,/^\[/ { s/^listen = "tailnet"$/listen = "127.0.0.1:17419"/; s#^networks = .*#networks = ["127.0.0.0/8"]#; s/^interval_secs = 60$/interval_secs = 30/; }' /etc/nightgrid-node/config.toml
  systemctl restart nightgrid-node
  i=0; until q node.info >/dev/null 2>&1; do i=$((i+1)); [ $i -lt 30 ] || fail "not back after restart"; sleep 1; done
  /usr/local/bin/nightgrid-node allow peers --json | tee /tmp/ngn-allow-p.json
  grep -q 'pinned certificates' /tmp/ngn-allow-p.json || fail "no peers warning"
  grep -q '^IPAddressAllow=127.0.0.0/8$' /etc/systemd/system/nightgrid-node-peer.service.d/networks.conf || fail "no peers networks drop-in"
  i=0; until q peers.latest | grep -q '"listening":"127.0.0.1:17419"'; do
    i=$((i+1)); [ $i -lt 30 ] || { q peers.latest; journalctl -u nightgrid-node-peer --no-pager -n 20; fail "link never reported"; }; sleep 1; done
  stat -c '%U %a' /var/lib/nightgrid-node-peer/key.pk8 | grep -qx 'nightgrid-node 600' || fail "link key not private to the service"
  fp=$(q peers.latest | python3 -c 'import json,sys; print(json.load(sys.stdin)["fingerprint"])')
  nid=$(q node.info | python3 -c 'import json,sys; print(json.load(sys.stdin)["node"]["nodeId"])')
  q "peers.set '{\"peers\":[{\"id\":\"$nid\",\"name\":\"self\",\"address\":\"127.0.0.1:17419\",\"fingerprint\":\"$fp\"}]}'" | grep -q '"serve":\["ping"\]' || fail "peers.set"
  i=0; until q peers.latest | grep -q '"ok":true'; do
    i=$((i+1)); [ $i -lt 40 ] || { q peers.latest; journalctl -u nightgrid-node-peer --no-pager -n 20; fail "the link never reached the paired node"; }; sleep 1; done
  # Pinning another certificate: refused, and an unreachable event after two misses.
  q "peers.set '{\"peers\":[{\"id\":\"$nid\",\"name\":\"self\",\"address\":\"127.0.0.1:17419\",\"fingerprint\":\"$(printf 'ab%.0s' $(seq 32))\"}]}'" >/dev/null || fail "peers.set (wrong pin)"
  i=0; until ev | grep -q '"title":"peer self unreachable"'; do
    i=$((i+1)); [ $i -lt 60 ] || { q peers.latest; fail "no unreachable event for a wrong pin"; }; sleep 1; done
  q peers.latest | grep -q 'certificate' || { q peers.latest; fail "wrong pin not explained"; }
  q "peers.report '{\"readings\":[]}'" 2>&1 | grep -q 'only the node-to-node link' || fail "a login user could report peers"
  /usr/local/bin/nightgrid-node deny peers >/dev/null
  test ! -e /etc/systemd/system/nightgrid-node-peer.service || fail "link unit left after deny"
  systemctl is-active --quiet nightgrid-node-peer && fail "link still running after deny"

  say "statuspage: a read-only page at a secret link, from [statuspage] networks only"
  sed -i '/^\[statuspage\]/,$ { s/^listen = "tailnet"$/listen = "127.0.0.1:17420"/; s#^networks = .*#networks = ["127.0.0.0/8"]#; }' /etc/nightgrid-node/config.toml
  /usr/local/bin/nightgrid-node allow statuspage --json | tee /tmp/ngn-allow-s.json
  url=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["url"])' /tmp/ngn-allow-s.json)
  case "$url" in http://127.0.0.1:17420/????????????????????????????????????????????????????????????????) ;; *) fail "bad status page link: $url";; esac
  stat -c '%U %G %a' /etc/nightgrid-node/status-token | grep -qx 'root nightgrid-node 640' || fail "token not root:nightgrid-node 0640"
  page() { python3 -c 'import sys,urllib.request,urllib.error
try:
    r = urllib.request.urlopen(sys.argv[1], timeout=10); print(r.status); print(r.read().decode())
except urllib.error.HTTPError as e:
    print(e.code)' "$1"; }
  i=0; until page "$url" | head -1 | grep -qx 200; do
    i=$((i+1)); [ $i -lt 30 ] || { journalctl -u nightgrid-node-status --no-pager -n 20; fail "status page never served"; }; sleep 1; done
  page "$url" | grep -q "Nightgrid Node" || fail "status page content"
  page "http://127.0.0.1:17420/" | head -1 | grep -qx 404 || fail "status page answered without the token"
  /usr/local/bin/nightgrid-node allow statuspage --json | grep -q "$url" || fail "allow again changed the link"
  /usr/local/bin/nightgrid-node deny statuspage >/dev/null
  test ! -e /etc/nightgrid-node/status-token || fail "token left after deny"
  systemctl is-active --quiet nightgrid-node-status && fail "status page still running after deny"

  say "doctor: a clean bill with the link on; a stopped link unit is named"
  /usr/local/bin/nightgrid-node allow peers >/dev/null
  as_user "/usr/local/bin/nightgrid-node doctor" | tee /tmp/ngn-doctor.txt || fail "doctor found a failure"
  grep -Eq '^  ok  +allow peers +on$' /tmp/ngn-doctor.txt || fail "doctor: link not on"
  grep -E '^WARN +(install|config|service|socket|version|memory) ' /tmp/ngn-doctor.txt && fail "doctor warned about the core"
  systemctl stop nightgrid-node-peer
  as_user "/usr/local/bin/nightgrid-node doctor --json" >/tmp/ngn-doctor.json || true
  grep -q '"check":"allow peers","level":"warn"' /tmp/ngn-doctor.json || { cat /tmp/ngn-doctor.json; fail "doctor missed the stopped link"; }
  /usr/local/bin/nightgrid-node deny peers >/dev/null

  say "audit verify walks the whole action log the tests filled"
  as_user "/usr/local/bin/nightgrid-node audit --count 5"
  as_user "/usr/local/bin/nightgrid-node audit verify" | tee /tmp/ngn-verify.txt || fail "audit verify"
  grep -q '^ok: ' /tmp/ngn-verify.txt || fail "audit verify found nothing to check"

  say "footprint after every feature ran: peak memory stays in budget"
  pid=$(systemctl show -p MainPID --value nightgrid-node)
  grep -E 'VmHWM|VmRSS|Threads' /proc/"$pid"/status
  hwm=$(awk '/VmHWM/ {print $2}' /proc/"$pid"/status)
  [ "$hwm" -lt 20480 ] || fail "peak RSS ${hwm} kB is over 20 MB"
fi

say "upgrade: a wrong checksum changes nothing; a good one upgrades; a new binary that won't start is reverted"
sum=$(sha256sum "$bin" | cut -d' ' -f1)
if /usr/local/bin/nightgrid-node upgrade "$bin" --sha256 "$(printf '0%.0s' $(seq 64))" >/tmp/ngn-up.txt 2>&1; then fail "a wrong checksum upgraded"; fi
grep -q 'checksum mismatch' /tmp/ngn-up.txt || { cat /tmp/ngn-up.txt; fail "no checksum message"; }
test ! -e /usr/local/bin/nightgrid-node.prev || fail "a refused upgrade kept a previous binary"
/usr/local/bin/nightgrid-node upgrade "$bin" --sha256 "$sum" --json | tee /tmp/ngn-up.json
grep -q '"ok":true' /tmp/ngn-up.json || fail "upgrade with the right checksum"
[ "$(sha256sum < /usr/local/bin/nightgrid-node.prev)" = "$(sha256sum < "$bin")" ] || fail "previous binary not kept"
as_user "/usr/local/bin/nightgrid-node query events.sync '{\"after\":0}'" | grep -q '"title":"upgraded to ' || fail "no upgrade event"
# A "new" binary that differs (trailing bytes still run) and that a drop-in keeps from starting.
cp "$bin" /run/ngn-broken && printf 'ngn-test-trailer' >> /run/ngn-broken
mkdir -p /etc/systemd/system/nightgrid-node.service.d
printf '[ "$(sha256sum < /usr/local/bin/nightgrid-node)" != "$(sha256sum < /run/ngn-broken)" ]\n' > /run/ngn-check.sh
printf '[Service]\nExecStartPre=/bin/sh /run/ngn-check.sh\n' > /etc/systemd/system/nightgrid-node.service.d/zz-test-break.conf
systemctl daemon-reload
bsum=$(sha256sum /run/ngn-broken | cut -d' ' -f1)
if /usr/local/bin/nightgrid-node upgrade /run/ngn-broken --sha256 "$bsum" --json >/tmp/ngn-up2.json 2>/tmp/ngn-up2.err; then cat /tmp/ngn-up2.json; fail "a binary that can't start was kept"; fi
cat /tmp/ngn-up2.json
grep -q '"reverted":true' /tmp/ngn-up2.json || { cat /tmp/ngn-up2.err; fail "not reverted"; }
[ "$(sha256sum < /usr/local/bin/nightgrid-node)" = "$(sha256sum < "$bin")" ] || fail "the reverted binary isn't the previous one"
systemctl is-active --quiet nightgrid-node || fail "not running after the revert"
as_user "/usr/local/bin/nightgrid-node query events.sync '{\"after\":0}'" | grep -q '"title":"upgrade to [0-9.]* failed"' || fail "no failed-upgrade event"
rm -f /etc/systemd/system/nightgrid-node.service.d/zz-test-break.conf /run/ngn-broken /run/ngn-check.sh
systemctl daemon-reload

say "reinstall (upgrade in place) keeps the node id"
before=$(as_user "/usr/local/bin/nightgrid-node query node.info" | sed 's/.*"nodeId":"\([0-9a-f]*\)".*/\1/')
cp "$bin" /tmp/ngn-upgrade && /tmp/ngn-upgrade install --json >/dev/null
after=$(as_user "/usr/local/bin/nightgrid-node query node.info" | sed 's/.*"nodeId":"\([0-9a-f]*\)".*/\1/')
[ -n "$before" ] && [ "$before" = "$after" ] || fail "node id changed: $before -> $after"

say "a damaged history database is set aside and the service starts anyway"
systemctl stop nightgrid-node
head -c 65536 /dev/urandom > /var/lib/nightgrid-node/node.db
rm -f /var/lib/nightgrid-node/node.db-wal /var/lib/nightgrid-node/node.db-shm
systemctl start nightgrid-node
i=0; until as_user "/usr/local/bin/nightgrid-node events" 2>/dev/null | grep -q "history database replaced"; do
  i=$((i+1)); [ $i -lt 20 ] || fail "no recovery after a damaged database"; sleep 1; done
test -f /var/lib/nightgrid-node/node.db.damaged || fail "damaged copy not kept"
systemctl is-active --quiet nightgrid-node || fail "service not active after recovery"

say "uninstall keeps history"
/usr/local/bin/nightgrid-node uninstall
systemctl is-active --quiet nightgrid-node && fail "still running"
test ! -e /usr/local/bin/nightgrid-node && test ! -e /usr/local/bin/nightgrid-node.prev || fail "binary left"
test ! -e /etc/systemd/system/nightgrid-node.service || fail "unit left"
test -e /var/lib/nightgrid-node/node.db || fail "history removed without --purge"

say "purge removes everything"
"$bin" uninstall --purge
for p in /var/lib/nightgrid-node /var/lib/nightgrid-node-peer /etc/nightgrid-node /etc/sysusers.d/nightgrid-node.conf; do
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
