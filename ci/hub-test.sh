#!/bin/sh
# Installs Nightgrid Node and Nightgrid Hub on one host with systemd, makes
# the node a member of the hub over a real link on loopback (mutual TLS,
# pinned certificates), and checks the hub pulls it read-only, serves it back
# with the node's own cursors, notices it going offline, and goes away
# cleanly. Run as root.
#   ci/hub-test.sh /path/to/nightgrid-node /path/to/nightgrid-hub LOGIN_USER
set -eu
node=$1
hubbin=$2
user=$3
say() { printf '\n== %s\n' "$*"; }
fail() {
  printf 'FAIL: %s\n' "$*" >&2
  for u in nightgrid-hub nightgrid-hub-collect nightgrid-node nightgrid-node-peer; do
    journalctl -u "$u" --no-pager -n 30 >&2 || true
  done
  exit 1
}
q() { /usr/local/bin/nightgrid-node query "$@"; }
h() { /usr/local/bin/nightgrid-hub query "$@"; }
# Runs a command as another account. su goes through PAM, which under
# qemu-user (CI's armv7 job) takes over a minute per call; FAST_SU=1 uses
# setpriv instead, with the same uid, gid and groups from /etc/group.
run_as() {
  if [ -n "${FAST_SU:-}" ] && command -v setpriv >/dev/null; then
    setpriv --reuid="$1" --regid="$(id -g "$1")" --init-groups \
      env HOME="$(getent passwd "$1" | cut -d: -f6)" USER="$1" LOGNAME="$1" sh -c "$2"
  else
    su -s /bin/sh "$1" -c "$2"
  fi
}
as_user() { run_as "$user" "$1"; }
# Resident kB of PID itself. Under qemu-user (CI's armv7 job) VmRSS also
# counts the emulator, so only the guest's pages are summed: a 32-bit guest
# lives below 4 GiB, qemu above it. Natively this is VmRSS.
emulated() { grep -q qemu /proc/"$1"/maps 2>/dev/null; }
rss_of() {
  if emulated "$1"; then
    awk '/^[0-9a-f]+-/ {split($1, a, "-"); lo = length(a[1]) <= 8} /^Rss:/ && lo {s += $2} END {print s + 0}' /proc/"$1"/smaps
  else
    awk '/VmRSS/ {print $2}' /proc/"$1"/status
  fi
}
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
wait_until() { # SECONDS DESCRIPTION COMMAND
  n=$1; what=$2; shift 2
  i=0; until sh -c "$*" >/dev/null 2>&1; do
    i=$((i+1)); [ $i -lt "$n" ] || fail "$what"; sleep 1; done
}

say "the hub needs the node first"
"$hubbin" install --json >/tmp/ngh-noinstall.json 2>&1 && fail "installed without the node"
grep -q 'install Nightgrid Node' /tmp/ngh-noinstall.json || { cat /tmp/ngh-noinstall.json; fail "unclear refusal"; }

say "node with its link on loopback"
"$node" install --user "$user" --json >/dev/null
sed -i '/^\[peers\]/,/^\[/ { s/^listen = "tailnet"$/listen = "127.0.0.1:17419"/; s#^networks = .*#networks = ["127.0.0.0/8"]#; s/^interval_secs = 60$/interval_secs = 30/; }' /etc/nightgrid-node/config.toml
/usr/local/bin/nightgrid-node allow peers >/dev/null
wait_until 30 "node link never listened" "/usr/local/bin/nightgrid-node query peers.latest | grep -q '\"listening\":\"127.0.0.1:17419\"'"
nid=$(q node.info | json 'd["node"]["nodeId"]')
nfp=$(q peers.latest | json 'd["fingerprint"]')

say "hub install"
"$hubbin" install --user "$user" --json | tee /tmp/ngh-install.json
grep -q '"action":"install"' /tmp/ngh-install.json || fail "no install report"
systemctl is-active --quiet nightgrid-hub || fail "hub not active"
[ "$(stat -c %a /run/nightgrid-hub/hub.sock)" = 660 ] || fail "hub socket mode"
[ "$(stat -c %a /var/lib/nightgrid-hub)" = 700 ] || fail "hub state dir mode"
as_user "/usr/local/bin/nightgrid-hub status" | grep -q 'no members yet' || fail "status for the allowed user"
if run_as nobody "/usr/local/bin/nightgrid-hub status" >/dev/null 2>&1; then fail "nobody could connect"; fi
pid=$(systemctl show -p MainPID --value nightgrid-hub)
if command -v ss >/dev/null; then ss -tulpnH | grep "pid=$pid," && fail "hub main unit has a network socket" || true; fi

say "collector on loopback"
sed -i '/^\[collect\]/,/^\[/ { s/^listen = "tailnet"$/listen = ["127.0.0.1:17420", "127.0.0.2:17420"]/; s#^networks = .*#networks = ["127.0.0.0/8"]#; s/^interval_secs = 60$/interval_secs = 30/; }' /etc/nightgrid-hub/config.toml
sed -i 's/^offline_secs = 120$/offline_secs = 60/' /etc/nightgrid-hub/config.toml
systemctl restart nightgrid-hub
/usr/local/bin/nightgrid-hub allow collect --json | tee /tmp/ngh-allow.json
grep -q '"fingerprint":"' /tmp/ngh-allow.json || fail "allow collect gave no fingerprint"
grep -q '^IPAddressAllow=127.0.0.0/8$' /etc/systemd/system/nightgrid-hub-collect.service.d/networks.conf || fail "no networks drop-in"
hid=$(json 'd["hubId"]' </tmp/ngh-allow.json)
hfp=$(json 'd["fingerprint"]' </tmp/ngh-allow.json)
[ "$(json 'd.get("listening")' </tmp/ngh-allow.json)" = 127.0.0.1:17420 ] || fail "collector not listening on loopback"
[ "$(json '",".join(d.get("listenAll", []))' </tmp/ngh-allow.json)" = 127.0.0.1:17420,127.0.0.2:17420 ] || fail "collector not on both addresses"
h fleet.overview | grep -q '"listenAll":\["127.0.0.1:17420","127.0.0.2:17420"\]' || fail "overview lacks listenAll"

say "pairing: the node names the hub but serves nothing until root adds sync"
q peers.set "{\"peers\":[{\"id\":\"$hid\",\"name\":\"hub\",\"address\":\"127.0.0.1:17420\",\"fingerprint\":\"$hfp\"}]}" >/dev/null
q hub.member.set "{\"hubId\":\"$hid\"}" | grep -q "$hid" || fail "hub.member.set"
h fleet.members.set "{\"members\":[{\"id\":\"$nid\",\"name\":\"self\",\"address\":\"127.0.0.1:17419\",\"fingerprint\":\"$nfp\"}]}" >/dev/null
wait_until 90 "no consent state without sync served" "/usr/local/bin/nightgrid-hub query fleet.overview | grep -q '\"state\":\"consent\"'"
/usr/local/bin/nightgrid-node serve add sync --json | tee /tmp/ngn-serve.json
grep -q 'sync' /etc/nightgrid-node/config.toml || fail "serve add sync"
q hub.member.get | grep -q "$hid" || fail "hub.member lost on restart"

say "the hub pulls the node and the node sees the hub"
wait_until 120 "member never ok" "/usr/local/bin/nightgrid-hub query fleet.overview | grep -q '\"state\":\"ok\"'"
wait_until 90 "the node's link never reached the hub" "/usr/local/bin/nightgrid-node query peers.latest | grep -q '\"ok\":true'"
/usr/local/bin/nightgrid-hub status

say "same events and cursors through the hub as directly"
wait_until 60 "no events pulled" "/usr/local/bin/nightgrid-hub query fleet.sync '{\"nodeId\":\"$nid\",\"method\":\"events.sync\",\"params\":{\"after\":0}}' | grep -q '\"seq\":'"
direct=$(q events.sync '{"after":0}' | json '[e["seq"] for e in d["events"]]')
via=$(h fleet.sync "{\"nodeId\":\"$nid\",\"method\":\"events.sync\",\"params\":{\"after\":0}}" | json '[e["seq"] for e in d["events"]]')
echo "direct $direct"
echo "hub    $via"
python3 -c "import sys; a=$direct; b=$via; sys.exit(0 if b and b == a[:len(b)] else 1)" || fail "hub events differ from the node's"
as_user "/usr/local/bin/nightgrid-hub query fleet.sync '{\"nodeId\":\"$nid\",\"method\":\"node.info\"}'" | grep -q "$nid" || fail "node.info through the hub"
as_user "/usr/local/bin/nightgrid-hub query fleet.sync '{\"nodeId\":\"$nid\",\"method\":\"actions.log\"}'" 2>&1 | grep -q 'not kept' || fail "hub answered a non-read"

say "hub doctor: clean with the collector on, a bad config is refused"
as_user "/usr/local/bin/nightgrid-hub doctor" | tee /tmp/ngh-doctor.txt || fail "hub doctor found a failure"
grep -Eq '^  ok  +collector +reports; answers pings on 127.0.0.1:17420, 127.0.0.2:17420$' /tmp/ngh-doctor.txt || fail "hub doctor: collector"
as_user "/usr/local/bin/nightgrid-hub doctor --json" | grep -q '"check":"store","level":"ok"' || fail "hub doctor: store"
printf '[store]\nmax_gb = 0\n' >/tmp/ngh-bad.toml
if /usr/local/bin/nightgrid-hub doctor --config /tmp/ngh-bad.toml --json >/tmp/ngh-bad.json; then fail "a bad config passed"; fi
grep -q '"check":"config","level":"fail"' /tmp/ngh-bad.json || fail "hub doctor: config"

say "only the collector may report"
as_user "/usr/local/bin/nightgrid-hub query fleet.ingest '{\"fingerprint\":\"x\",\"answers\":[]}'" 2>&1 | grep -q 'only the hub' || fail "a login user could ingest"

say "budget"
for u in nightgrid-hub nightgrid-hub-collect; do
  p=$(systemctl show -p MainPID --value "$u")
  rss=$(rss_of "$p")
  echo "$u RSS ${rss} kB"
  [ "$rss" -lt 81920 ] || fail "$u RSS ${rss} kB is over 80 MB"
done

say "offline while the node's link is down, back when it returns"
systemctl stop nightgrid-node-peer
wait_until 180 "no offline event" "/usr/local/bin/nightgrid-hub query fleet.events.sync '{\"after\":0}' | grep -q 'self offline'"
h fleet.events.sync '{"after":0}' | grep -q '"kind":"hostOffline"' || fail "offline isn't a hostOffline alert"
systemctl start nightgrid-node-peer
wait_until 120 "no back-online event" "/usr/local/bin/nightgrid-hub query fleet.events.sync '{\"after\":0}' | grep -q 'self back online'"

say "root takes sync away"
/usr/local/bin/nightgrid-node serve remove sync >/dev/null
wait_until 90 "member not back to consent" "/usr/local/bin/nightgrid-hub query fleet.overview | grep -q '\"state\":\"consent\"'"

say "uninstall the hub, the node stays"
/usr/local/bin/nightgrid-hub uninstall --purge --json >/dev/null
systemctl is-active --quiet nightgrid-hub && fail "hub still running"
for p in /usr/local/bin/nightgrid-hub /etc/systemd/system/nightgrid-hub.service /etc/systemd/system/nightgrid-hub-collect.service /var/lib/nightgrid-hub /var/lib/nightgrid-hub-collect /etc/nightgrid-hub; do
  test ! -e "$p" || fail "$p left"
done
if id nightgrid-hub >/dev/null 2>&1; then fail "hub account left"; fi
systemctl is-active --quiet nightgrid-node || fail "the node stopped with the hub"
q node.info >/dev/null || fail "node not answering"
"$node" uninstall --purge >/dev/null

say "OK"
