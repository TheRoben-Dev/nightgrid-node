#!/bin/sh
# Boots a distro image with systemd as PID 1 and runs install-test.sh in it,
# then hub-test.sh when a hub binary is given. CONTAINER_ARGS can pick another
# platform (--platform linux/arm/v7 runs it under qemu).
#   ci/container-test.sh IMAGE /path/to/nightgrid-node [/path/to/nightgrid-hub]
set -eu
image=$1
bin=$2
hub=${3:-}
here=$(cd "$(dirname "$0")" && pwd)
name=ngn-test-$$
case "$image" in
  fedora*) setup="dnf -y -q install systemd util-linux shadow-utils procps-ng iproute >/dev/null" ;;
  # Ubuntu: the stock archive stalled apt-get update for minutes on the runners
  # (2026-11-19), so use the Azure mirror the runners themselves use, and main
  # only, which has everything installed here and is a fraction of the index.
  ubuntu*) setup="sed -i -E 's#http://(archive|security)[.]ubuntu[.]com/ubuntu/?#http://azure.archive.ubuntu.com/ubuntu/#; s/ (restricted|universe|multiverse)//g; /^deb(-src)? [^ ]+ [^ ]+$/d' /etc/apt/sources.list && apt-get -qq -o Acquire::Retries=3 -o Acquire::http::Timeout=30 update && DEBIAN_FRONTEND=noninteractive apt-get -qq install -y systemd systemd-sysv passwd procps iproute2 >/dev/null" ;;
  *) setup="apt-get -qq update && DEBIAN_FRONTEND=noninteractive apt-get -qq install -y systemd systemd-sysv passwd procps iproute2 >/dev/null" ;;
esac
# hub-test.sh reads JSON with python3.
[ -z "$hub" ] || setup="$setup && DEBIAN_FRONTEND=noninteractive apt-get -qq install -y python3 >/dev/null"
docker run -d --name "$name" ${CONTAINER_ARGS:-} --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
  --tmpfs /run --tmpfs /run/lock "$image" sh -c "$setup && exec /sbin/init" >/dev/null
trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT
i=0
until docker exec "$name" sh -c 'systemctl is-system-running 2>/dev/null | grep -qE "running|degraded"'; do
  i=$((i + 1))
  [ "$(docker inspect -f '{{.State.Running}}' "$name")" = true ] || { docker logs "$name" | tail -n 30; echo "container exited"; exit 1; }
  [ "$i" -lt "${BOOT_SECS:-180}" ] || {
    docker logs "$name" | tail -n 30
    # Say where it stuck: still in setup (apt), or systemd up with a job hanging.
    docker exec "$name" sh -c 'ps -eo pid,etime,args | head -n 30; systemctl is-system-running; systemctl list-jobs --no-pager | head -n 20; systemctl --failed --no-pager' 2>&1 || true
    echo "systemd did not boot"; exit 1; }
  sleep 1
done
docker cp "$bin" "$name":/root/nightgrid-node
docker cp "$here/install-test.sh" "$name":/root/install-test.sh
docker cp "$here/../dist/uninstall.sh" "$name":/root/uninstall.sh
docker exec "$name" useradd -m -s /bin/sh tester
docker exec -e UNINSTALL_SH=/root/uninstall.sh -e FAST_SU="${FAST_SU:-}" "$name" sh /root/install-test.sh /root/nightgrid-node tester
if [ -n "$hub" ]; then
  docker cp "$hub" "$name":/root/nightgrid-hub
  docker cp "$here/hub-test.sh" "$name":/root/hub-test.sh
  docker exec -e FAST_SU="${FAST_SU:-}" "$name" sh /root/hub-test.sh /root/nightgrid-node /root/nightgrid-hub tester
fi
