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
    # OpenRC outside of boot: mark the runlevel as started.
    setup="apk add -q openrc procps-ng >/dev/null && mkdir -p /run/openrc && touch /run/openrc/softlevel && adduser -D -s /bin/sh tester"
    ;;
  sysvinit)
    image=debian:12
    setup="apt-get -qq update && DEBIAN_FRONTEND=noninteractive apt-get -qq install -y procps sysv-rc >/dev/null && useradd -m -s /bin/sh tester"
    ;;
  *) echo "openrc or sysvinit"; exit 2 ;;
esac
args=""
[ -z "$platform" ] || args="--platform $platform"
# shellcheck disable=SC2086
docker run -d --name "$name" $args "$image" sleep 7200 >/dev/null
trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT
docker exec "$name" sh -c "$setup"
docker cp "$bin" "$name":/root/nightgrid-node
docker cp "$here/rc-install-test.sh" "$name":/root/rc-install-test.sh
docker exec "$name" sh /root/rc-install-test.sh /root/nightgrid-node tester "$init"
