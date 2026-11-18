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
  *) setup="apt-get -qq update && DEBIAN_FRONTEND=noninteractive apt-get -qq install -y systemd systemd-sysv passwd procps iproute2 python3 >/dev/null" ;;
esac
docker run -d --name "$name" ${CONTAINER_ARGS:-} --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
  --tmpfs /run --tmpfs /run/lock "$image" sh -c "$setup && exec /sbin/init" >/dev/null
trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT
i=0
until docker exec "$name" sh -c 'systemctl is-system-running 2>/dev/null | grep -qE "running|degraded"'; do
  i=$((i + 1))
  [ "$(docker inspect -f '{{.State.Running}}' "$name")" = true ] || { docker logs "$name" | tail -n 30; echo "container exited"; exit 1; }
  [ "$i" -lt "${BOOT_SECS:-180}" ] || { docker logs "$name" | tail -n 30; echo "systemd did not boot"; exit 1; }
  sleep 1
done
docker cp "$bin" "$name":/root/nightgrid-node
docker cp "$here/install-test.sh" "$name":/root/install-test.sh
docker cp "$here/../dist/uninstall.sh" "$name":/root/uninstall.sh
docker exec "$name" useradd -m -s /bin/sh tester
docker exec -e UNINSTALL_SH=/root/uninstall.sh "$name" sh /root/install-test.sh /root/nightgrid-node tester
if [ -n "$hub" ]; then
  docker cp "$hub" "$name":/root/nightgrid-hub
  docker cp "$here/hub-test.sh" "$name":/root/hub-test.sh
  docker exec "$name" sh /root/hub-test.sh /root/nightgrid-node /root/nightgrid-hub tester
fi
