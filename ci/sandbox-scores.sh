#!/bin/sh
# Fails when a unit's systemd sandbox gets weaker than it is today.
# `systemd-analyze security` scores exposure from 0 (locked down) to 10;
# each limit below is today's score, so a loosened unit fails CI until the
# limit is raised on purpose, in review.
set -eu
dir=$(dirname "$0")/../dist/systemd
status=0
while read -r unit limit; do
  out=$(systemd-analyze security --offline=true --no-pager "$(realpath "$dir/$unit")" 2>&1 | grep 'Overall exposure' || true)
  max=$(echo "$limit" | awk '{printf "%.1f", $1/10}')
  echo "$unit: ${out#*: } (limit $max)"
  if ! systemd-analyze security --offline=true --threshold="$limit" --no-pager "$(realpath "$dir/$unit")" >/dev/null 2>&1; then
    echo "FAIL: $unit is more exposed than $max" >&2
    status=1
  fi
done <<'LIMITS'
nightgrid-node.service 15
nightgrid-node-netwatch.service 15
nightgrid-node-checks.service 15
nightgrid-node-peer.service 15
nightgrid-node-status.service 15
nightgrid-node-helper@.service 25
nightgrid-node-health@.service 30
nightgrid-hub.service 10
nightgrid-hub-collect.service 15
LIMITS
for f in "$dir"/*.service; do
  grep -q "^$(basename "$f") " "$0" || { echo "FAIL: $(basename "$f") has no limit here" >&2; status=1; }
done
exit $status
