#!/bin/sh
# Runs rc-install-test.sh in a container without systemd: Alpine with OpenRC
# (BusyBox userland, musl) or Debian with sysvinit's start-stop-daemon.
#   ci/rc-container-test.sh openrc|sysvinit /path/to/nightgrid-node [PLATFORM]
set -eu
init=$1
bin=$2
platform=${3:-}
here=$(cd "$(dirname "$0")" && pwd)
name=ngn-rc-$$
case "$init" in
  openrc)
    image=alpine:3.20
    # OpenRC as after a boot: a container system (rc_sys), then the boot and
    # default runlevels once, so /run/openrc has its state as on a real host.
    setup="apk add -q openrc procps-ng >/dev/null && sed -i 's/^#*rc_sys=.*/rc_sys=\"docker\"/' /etc/rc.conf && mkdir -p /run/openrc && touch /run/openrc/softlevel && (openrc boot && openrc default) >/dev/null 2>&1; adduser -D -s /bin/sh tester"
    ;;
  sysvinit)
    image=debian:12
    # apt-get update succeeds even when DNS fails, so retry the install.
    setup="for i in 1 2 3; do apt-get -qq update; DEBIAN_FRONTEND=noninteractive apt-get -qq install -y procps sysv-rc >/dev/null && break; [ \$i = 3 ] && exit 1; sleep 15; done; useradd -m -s /bin/sh tester"
    ;;
  *) echo "openrc or sysvinit"; exit 2 ;;
esac
args=""
[ -z "$platform" ] || args="--platform $platform"
# shellcheck disable=SC2086
# --init reaps the stopped daemon (sleep as PID 1 would leave a zombie that
# start-stop-daemon counts as still running), as init does on a real host.
docker run -d --init --name "$name" $args "$image" sleep 7200 >/dev/null
trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT
docker exec "$name" sh -c "$setup"
docker cp "$bin" "$name":/root/nightgrid-node
docker cp "$here/rc-install-test.sh" "$name":/root/rc-install-test.sh
docker exec "$name" sh /root/rc-install-test.sh /root/nightgrid-node tester "$init"
