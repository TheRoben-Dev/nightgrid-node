#!/bin/sh
# Installs, checks and removes Nightgrid Node on a host WITHOUT systemd
# (OpenRC or sysvinit with start-stop-daemon). Run as root inside the
# container rc-container-test.sh starts. POSIX sh, BusyBox-safe.
#   ci/rc-install-test.sh /path/to/nightgrid-node LOGIN_USER INIT
set -eu
bin=$1
user=$2
init=$3
say() { printf '\n== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
as_user() { su -s /bin/sh "$user" -c "$1"; }
has() { grep -q "$1" "$2" || { cat "$2"; fail "$2 lacks $1"; }; }

say "install ($init)"
"$bin" install --user "$user" --json | tee /tmp/ngn-install.json
has '"action":"install"' /tmp/ngn-install.json
has "\"init\":\"$init\"" /tmp/ngn-install.json
has '"kernel":"' /tmp/ngn-install.json
has '"libc":"' /tmp/ngn-install.json
has '"arch":"' /tmp/ngn-install.json
test -x /etc/init.d/nightgrid-node || fail "no service script"
test ! -e /etc/systemd/system/nightgrid-node.service || fail "wrote a systemd unit"
id nightgrid-node >/dev/null || fail "no service account"

say "it runs as its account and answers on the socket"
i=0; until [ -S /run/nightgrid-node/node.sock ]; do i=$((i+1)); [ $i -lt 60 ] || fail "no socket"; sleep 1; done
/etc/init.d/nightgrid-node status || fail "status says not running"
pid=$(pgrep -f '/usr/local/bin/nightgrid-node run' | head -n 1)
[ -n "$pid" ] || fail "no process"
owner=$(awk '/^Uid:/ {print $2}' /proc/"$pid"/status)
[ "$owner" = "$(id -u nightgrid-node)" ] || fail "runs as uid $owner"
[ "$(stat -c %a /run/nightgrid-node/node.sock)" = 660 ] || fail "socket mode"
[ "$(stat -c %a /var/lib/nightgrid-node)" = 700 ] || fail "state dir mode"
[ "$(stat -c %a /etc/nightgrid-node)" = 755 ] || fail "config dir mode"
as_user "/usr/local/bin/nightgrid-node status --json" | grep -q '"nodeId"' || fail "status for $user"
as_user "echo '{\"id\":1,\"method\":\"hello\",\"params\":{\"protoMin\":1,\"protoMax\":1}}' | /usr/local/bin/nightgrid-node bridge" | grep -q '"proto":1' || fail "bridge"
if su -s /bin/sh nobody -c "/usr/local/bin/nightgrid-node status" >/dev/null 2>&1; then fail "nobody could connect"; fi
su -s /bin/sh nobody -c "/usr/local/bin/nightgrid-node bridge </dev/null" >/tmp/ngn-bridge.json 2>/dev/null && fail "bridge as nobody succeeded"
has '"code":"no_access"' /tmp/ngn-bridge.json

say "doctor"
as_user "/usr/local/bin/nightgrid-node doctor" | tee /tmp/ngn-doctor.txt || fail "doctor found a failure"
grep -Eq '^  ok  +service +nightgrid-node is running' /tmp/ngn-doctor.txt || fail "doctor: service"
as_user "/usr/local/bin/nightgrid-node doctor --json" >/tmp/ngn-doctor.json
has "\"init\":\"$init\"" /tmp/ngn-doctor.json
has '"check":"sandbox"' /tmp/ngn-doctor.json

say "what needs systemd says so"
/usr/local/bin/nightgrid-node allow peers --json >/tmp/ngn-allow.json 2>/dev/null && fail "allow peers worked without systemd"
has '"code":"no_systemd"' /tmp/ngn-allow.json

say "reinstall keeps the node id; a stopped service is named"
before=$(as_user "/usr/local/bin/nightgrid-node status --json" | sed 's/.*"nodeId":"\([^"]*\)".*/\1/')
"$bin" install --json >/dev/null
i=0; until as_user "/usr/local/bin/nightgrid-node status --json" >/tmp/ngn-st.json 2>/dev/null; do i=$((i+1)); [ $i -lt 30 ] || fail "not back"; sleep 1; done
grep -q "$before" /tmp/ngn-st.json || fail "node id changed"
/etc/init.d/nightgrid-node stop
as_user "/usr/local/bin/nightgrid-node bridge </dev/null" >/tmp/ngn-bridge2.json 2>/dev/null && fail "bridge to a stopped node succeeded"
has '"code":"not_running"' /tmp/ngn-bridge2.json
as_user "/usr/local/bin/nightgrid-node doctor --json" >/tmp/ngn-doctor2.json && fail "doctor passed with the service stopped"
has '"check":"service","level":"fail"' /tmp/ngn-doctor2.json

say "uninstall --purge"
/usr/local/bin/nightgrid-node uninstall --purge --json | tee /tmp/ngn-uninstall.json
has '"action":"uninstall"' /tmp/ngn-uninstall.json
for p in /usr/local/bin/nightgrid-node /etc/init.d/nightgrid-node /var/lib/nightgrid-node /etc/nightgrid-node; do
  test ! -e "$p" || fail "$p left"
done
if id nightgrid-node >/dev/null 2>&1; then fail "account left"; fi
if pgrep -f '/usr/local/bin/nightgrid-node run' >/dev/null; then fail "still running"; fi
say "OK"
