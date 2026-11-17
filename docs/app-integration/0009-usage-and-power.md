# 0009 · Usage and power

Node milestone: **N6a** (nightgrid-node 0.13.0, `usage` 1). Status: **proposed** for the app. Builds on 0003 (containers) and 0004 (live streams).

## What the person gets

- **Which service, container or process is using the machine**, every second while a view is open, without SSH polling or `docker stats`.
- **A day of per-service and per-container history**: "what was eating CPU at 03:00" works with the app closed.
- **Power draw** from the CPU's RAPL counters (Intel, and AMD Zen on recent kernels), live and as a 24-hour graph, once root allows it.

## 1. Turning it on

`usage` is on by default (`[usage] enabled = true`). Nothing to do unless the host has the cap absent (older node or disabled), in which case keep today's behaviour exactly.

Power needs the counters to be readable, which the kernel restricts to root (CVE-2020-8694). Read `node.info.access.power`:

- `ok`: show power (section 3).
- `denied`: on the node's settings page, offer "Let the node read CPU power draw." The button runs, with the app's existing sudo confirmation:

  ```
  sudo nightgrid-node allow power
  ```

  and shows the report's `warnings[0]` (the counters become readable by the node's group, which is a minor side channel on shared machines). `deny power` turns it off again.
- `absent`: no RAPL on this host (most ARM boards, many VMs); hide the power row.

## 2. Views

When a host view opens (and on reconnect), call `usage.latest` for an immediate table, then subscribe to the `usage` stream (0004) for 1 s updates; unsubscribe when the view closes so the node drops back to 10 s sampling.

- **Services:** add CPU and Memory columns from items with `kind: "service"`, matched by unit name (`nginx.service`). Units not listed used nothing measurable.
- **Docker:** where the view now uses `docker.stats[id]` (`cpuPermille`), use `kind: "container"` items matched by name, with `cpu * 10` as permille. Network and I/O columns come from `netRx`/`netTx` and `ioRead`/`ioWrite` (bytes per second). Containers the node couldn't name show a 12-character id; match on id prefix as a fallback.
- **System:** a "Top processes" card from `kind: "process"` items (name, CPU, memory, `count` as "×9").
- **History:** clicking a row opens a graph from `usage.history` with the item's `kind` and `name`: one-minute averages for the last 24 hours, the same chart the app uses for host metrics.
- `cgroups: false` (cgroup v1 host): hide the service and container columns, keep processes.

## 3. Power

With `access.power == "ok"`, `power.watts` is the whole-platform (`psys`) or CPU package draw:

- Show it next to the GPU's `power` on the System view as "CPU 14 W" (or "Platform" when a domain named `psys` is present), with the domains in a tooltip.
- `usage.history` with `{kind: "power", name: "total"}` gives the 24-hour graph.
- Fleet totals can sum `power.watts` across hosts, labelled as CPU power, not wall power.

## 4. Checks

- On a host with node 0.13.0, the Services view shows the node's own `nightgrid-node.service` with a few MB of memory.
- `stress-ng --cpu 1` (or `yes > /dev/null`) appears at the top of the process card within 2 s while the view is open.
- After `allow power` on an Intel or AMD host, power shows a non-zero value; `deny power` returns it to the offer.
- Hosts without the node, or with an older node, behave exactly as before.
