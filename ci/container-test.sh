#!/bin/sh
# Boots a distro image with systemd as PID 1 and runs install-test.sh in it.
#   ci/container-test.sh IMAGE /path/to/nightgrid-node
set -eu
image=$1
bin=$2
here=$(cd "$(dirname "$0")" && pwd)
name=ngn-test-$$
case "$image" in
  fedora*) setup="dnf -y -q install systemd util-linux shadow-utils procps-ng iproute >/dev/null" ;;
  *) setup="apt-get -qq update && DEBIAN_FRONTEND=noninteractive apt-get -qq install -y systemd systemd-sysv passwd procps iproute2 >/dev/null" ;;
esac
docker run -d --name "$name" ${CONTAINER_ARGS:-} --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
  --tmpfs /run --tmpfs /run/lock "$image" sh -c "$setup && exec /sbin/init" >/dev/null
trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT
i=0
until docker exec "$name" sh -c 'systemctl is-system-running 2>/dev/null | grep -qE "running|degraded"'; do
  i=$((i + 1))
  [ "$(docker inspect -f '{{.State.Running}}' "$name")" = true ] || { docker logs "$name" | tail -n 30; echo "container exited"; exit 1; }
  [ "$i" -lt 180 ] || { docker logs "$name" | tail -n 30; echo "systemd did not boot"; exit 1; }
  sleep 1
done
docker cp "$bin" "$name":/root/nightgrid-node
docker cp "$here/install-test.sh" "$name":/root/install-test.sh
docker cp "$here/../dist/uninstall.sh" "$name":/root/uninstall.sh
docker exec "$name" useradd -m -s /bin/sh tester
docker exec -e UNINSTALL_SH=/root/uninstall.sh "$name" sh /root/install-test.sh /root/nightgrid-node tester
