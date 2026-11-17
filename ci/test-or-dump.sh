#!/bin/sh
# Runs `cargo test`; if it hangs, prints where every test thread is blocked
# (kernel stacks and wait channels) and the mounts, then fails, instead of
# letting the job time out with nothing to go on.
set -u
limit=${TEST_LIMIT_SECS:-300}
cargo test --locked "$@" &
pid=$!
i=0
while kill -0 "$pid" 2>/dev/null; do
  i=$((i+1))
  if [ "$i" -ge "$limit" ]; then
    echo "::error::tests still running after ${limit}s; dumping test threads"
    for p in $(pgrep -f '/target/debug/.*-[0-9a-f]{16}( |$)'); do
      echo "== pid $p: $(tr '\0' ' ' < /proc/$p/cmdline)"
      for t in /proc/$p/task/*; do
        echo "-- $(cat $t/comm) state=$(awk '/^State/ {print $2}' $t/status) wchan=$(cat $t/wchan 2>/dev/null)"
        sudo cat $t/stack 2>/dev/null | head -12
      done
      ls -l /proc/$p/fd 2>/dev/null | tail -n +2 | head -20
      for c in $(pgrep -P "$p"); do echo "child $c: $(tr '\0' ' ' < /proc/$c/cmdline) wchan=$(cat /proc/$c/wchan)"; done
    done
    echo "== mounts"; cat /proc/mounts
    kill "$pid"; exit 1
  fi
  sleep 1
done
wait "$pid"
