# Changelog

## 0.17.0 · N7b (unreleased)

- Backup status with the app closed: the app pushes its backup jobs (`jobs.set`: restic, borg, rsync or rclone, a systemd unit or a path, with a stale limit). The node reads them every minute with the app's own rules (last run, result, size, what was added, the run's error line) and raises `backupFailed` and `backupStale` alerts with the app's keys and its `backupStale` switch. Paths are walked at most every 10 minutes, and their size is read again only when the newest file changes.
- The app's scheduled jobs (`nightgrid-job-*`) and backup units: each finished run is a `job` event ("job nightly sync failed (exit 2)", "backup nightly finished · snapshot 1a2b saved · 5.0 GiB"), including runs while the node was stopped. A scheduled reboot or shutdown (`nightgrid-power-*`) is recorded after the host is back, from the previous boot's journal.
- `jobs.latest`; `[jobs]` in the config (`enabled`, `interval_secs`); capability `jobs` 1; `node.info.access.jobs`. Read-only: the node schedules and runs nothing itself (N-040). Run output needs `allow journal`.

## 0.16.0 · N7a (unreleased)

- Service checks from inside the LAN: the app pushes its HTTP(S) and TCP checks (`checks.set`). After root runs `nightgrid-node allow checks`, a separate check runner (the service account, no capabilities, network access only to `[checks] networks`) runs them every minute with the app's own rules (expected status, certificate trust and expiry) and hands results to the node. `checks.latest`; `node.info.access.checks`.
- `serviceDown` and `certExpiring` alerts with the app's keys and its `serviceDown`, `certExpiring` and `certDays` rules.
- `[checks]` in the config: `enabled`, `interval_secs`, `networks`. New capability `checks` 1.

## 0.15.0 · N6c (unreleased)

- New actions `firewall.block` and `firewall.unblock` (target: an IP address), through the root helper with nftables. Blocks drop all traffic from the address (input and forwarded) for `[block] minutes`, in the node's own table with kernel timeouts. Loopback, private, link-local and CGNAT/Tailscale addresses and `[block] never` are refused, also on the helper's side.
- Optional auto-block (`[block] auto`, off by default): an address with `[block] threshold` failed SSH logins within 5 minutes is blocked, unless it logged in successfully in the last day; at most `[block] max_active` at once. Every block is in the action log and the timeline.
- `block.list`; new capability `block` 1. The root helper unit gains `CAP_NET_ADMIN` (for nft). `deny helper`, `uninstall` and `dist/uninstall.sh` remove the table.

## 0.14.0 · N6b (unreleased)

- Listening ports: every 15 s (`[ports] interval_secs`) the node reads the kernel's socket tables (no privileges) and records `port` events when a socket starts or stops listening ("new listener 6379/tcp (redis)", `warn` when reachable from the network). `ports.list` with the app's `Listener` fields, and the containers that publish each port. `containers.list` entries gain `ports`.
- Failed-login bursts as alerts: `loginBurst` (key `logins:burst`) while at least `[logins] burst` (default 20) failed SSH logins fall in 5 minutes, when the app turns `loginBurst` on in its alert rules.
- A wider file baseline (`[files] baseline`, default on): ld.so.preload, PAM, profile, cron directories and root's crontabs, sysctl, modprobe, package sources, Docker's daemon.json, enabled units, root's and every login user's `authorized_keys`, and running containers' compose files. With `allow health`, the read-only health helper takes these snapshots so homes and root-only files are covered (its unit now reads homes read-only). Cron, PAM and `authorized_keys` changes are `warn`.
- New capability `security` 1.

## 0.13.0 · N6a (unreleased)

- Usage per systemd service and per container (CPU, memory, disk read/write and, for containers with their own network, traffic) from cgroup v2, and the 15 busiest process names (CPU, resident memory, how many) from `/proc`. Every 10 s, every second while anyone subscribes to the new `usage` stream.
- Power draw from the CPU's energy counters (RAPL: package, core, uncore, dram, psys), after root runs `nightgrid-node allow power` (a udev rule that lets the service's group read `energy_uj`). `node.info.access.power`.
- `usage.latest` and `usage.history` (one-minute averages per service, container, process name and power domain, kept 24 hours).
- `[usage] enabled` in the config.

## 0.12.0 · N5b (unreleased)

- Log watching with the app's detector: every 10 minutes (`[logs] interval_secs`) the node reads what the journal logged at priority err and worse (after `allow journal`) and the last lines of up to 60 running containers (after `allow docker`), and raises `logBurst` (several times the usual error count) and `logNewError` (an error pattern not seen before) alerts, when the app turns `logAnomaly` on in its alert rules. The first round for a source only learns.
- The app's saved log searches with alerts: `logs.searches.set` (plain text or regular expression, unit, container and priority filters; at most 32), checked every round, raising `logMatch` alerts.
- `logs.status` (sources, error counts, findings and search hits of the last round) and `logs.samples` (recent error lines of a source). Lines are only kept in memory, and lines that look like they hold a secret are never kept or shown.
- `[logs]` in the config: `enabled`, `interval_secs`.

## 0.11.0 · N5a (unreleased)

- Host health, off until root runs `nightgrid-node allow health`: a read-only root helper (socket-activated per read, like the root helper) runs the app's own checks with the app's parsers: disk SMART (every hour), ZFS/mdadm/Btrfs pools (every 5 minutes), pending updates and reboot-required from the package manager's local cache (every 6 hours; the node never fetches package lists) and NUT UPS status (every 30 s, only for targets root lists in `[health] ups`).
- `health.latest` with the last reading of each, and `node.info.access.health`.
- Alerts with the app's switches: SMART warning, pool degraded, and UPS on battery / low battery / replace battery. New updates and reboot-required are info events in the timeline.
- `[health]` in the config: `enabled`, `ups`.
- Sensors (hwmon, thermal zones, GPU sysfs) are read on a helper thread with a 1 s limit, so a drive or firmware that is slow to answer a temperature read can no longer stall sampling; the last readings are kept meanwhile.
- `dist/uninstall.sh` also removes the helper units (netwatch, root helper, health) and the rollback directory.

## 0.10.0 · N4c (unreleased)

- Rollbacks with a dead-man timer: `changes.begin` snapshots files (through the root helper, only under root's `[rollback] paths`), and unless the app calls `changes.confirm` in time the node puts them back, with owner and mode, removes files the change added, and runs an optional `onRevert` unit or container action. The timer survives restarts and reboots of the node. `changes.revert`, `changes.list`.
- New action type `files.rollback` (in `[actions] allow`); `[rollback] paths` in the config, which `allow helper` turns into the only places the helper may write.
- The helper unit gains the file capabilities it needs for that and a snapshot directory, `/var/lib/nightgrid-node-rollback` (removed by `uninstall --purge`).

## 0.9.0 · N4b (unreleased)

- Root helper, off until root runs `nightgrid-node allow helper`: a socket only the service account can reach, starting one short-lived root process per request. It re-reads the root-owned config and runs only what is listed there, with fixed arguments and no shell.
- New actions: `unit.restart`, `unit.start`, `unit.stop` on units root lists in `[actions] units`, and `power.reboot` / `power.poweroff`, scheduled a minute ahead (cancel with `shutdown -c`). Each still needs `[actions] allow`; all are logged like the rest.
- `actions` capability 2; `actions.policy` gains `units` and `helper`; `node.info.access.helper`.

## 0.8.0 · N4a (unreleased)

- Typed actions, each off until root lists it in `[actions] allow`: `container.restart`, `container.start`, `container.stop` and `images.prune` (dangling images), through the engine socket. `actions.policy`, `actions.run` (with dry run), and a global `max_per_hour`.
- Hash-chained audit log of every action, dry or real, refused or done (`actions.log`, with chain verification), and `action` events in the timeline.
- Self-healing rules pushed by the app (`heal.rules.set`, `heal.status`): unhealthy or down container, or full disk, held for N minutes → action. Off and dry-run by default, capped per hour and day, waiting between runs, and giving up with a warning when the condition doesn't clear.
- Slow requests run off the socket thread, so a container restart doesn't hold up other clients.

## 0.7.0 · N3b (unreleased)

- Network watch helper, off until root runs `nightgrid-node allow netwatch`: a separate unit (`nightgrid-node-netwatch.service`) whose only privilege is `CAP_NET_RAW`. It listens for ARP and DHCP through a kernel filter, sends nothing, and reports to the service over the unix socket.
- `net.dhcp`: DHCP servers on the LAN with what they hand out. Events for the first server, another server (possible rogue DHCP, `warn`, unless listed in `[network] dhcp_servers`) and a server handing out a different router or DNS (`warn`).
- Devices heard on the wire join the inventory (also ones this host never talks to), with the host name they asked DHCP for (`name`).
- `node.info.access.netwatch`; error code `forbidden`.

## 0.6.0 · N3a (unreleased)

- Network watch: an inventory of LAN devices from the kernel's neighbour table (`net.devices`), read every 30 s with no privileges and nothing sent. `network` events for new devices, devices on a new address, addresses answering from a new MAC, IP conflicts (an address switching between MACs) and a gateway answering from a new MAC (possible ARP spoofing). The first read on a fresh install is the baseline.
- `[network]` in the config: `enabled`, `interval_secs`, `ignore_ifaces`.
- CI: jobs time out instead of hanging; the journal is followed from the start time, so the first SSH line after a start isn't skipped.

## 0.5.0 · N2b (unreleased)

- SSH logins and failed logins (summed per minute) as `login` events, from the journal, after root runs `nightgrid-node allow journal`. Resumes from a saved cursor, so logins while the node was stopped are kept.
- Watched files: changes to `/etc/ssh`, `sudoers`, `passwd`, `shadow`, cron, `hosts`, `fstab` and `/etc/systemd/system` (root can edit the list) as `file` events, with sizes, hashes, mode and owner, never contents. Changes made while the node was stopped are reported at start.
- `node.info` reports which optional access root has granted (`access`).
- Both are pushed live on the `events` stream.

## 0.4.0 · N2a (unreleased)

- Protocol 2: `subscribe` / `unsubscribe` and pushed frames on the same connection. Streams: `events` (every new event as it is recorded), `metrics` (every 2 s while watched), `containers` (on state or health changes), plus `lagged` when a slow client missed frames. Protocol 1 clients are unchanged.
- The node samples every 2 s and reads containers every 3 s only while someone subscribes.
- `nightgrid-node watch STREAM... [--count N]` prints live frames.

## 0.3.0 · N1b (unreleased)

- Container watcher over the Docker (or Podman) socket: `containers.list`, lifecycle events in the timeline (restarts merged, crashes, OOM kills, health changes) and container-down alerts with the app's rules.
- `nightgrid-node allow docker` / `deny docker`: root grants or removes access through a systemd drop-in, with a root-equivalence warning.
- Milestones re-planned for the expanded brief: real-time push, network watch and self-healing come next.
- README describes what the node does.

## 0.2.0 · N1 (unreleased)

- Events stream (`events.sync`) with a growing `seq`, 90 days, secret filter on titles and details.
- Uptime: once-a-minute heartbeat, hourly up/down/asleep seconds (`uptime.sync`) and outage events that tell a crash ("Down") from a clean reboot ("Restarted") and a suspend ("Suspended").
- Disk usage per mount per hour (`disk.sync`) and fill-up forecasts.
- Offline alerts with the app's tracker semantics: disk full and disk filling, rules pushed by the app (`alerts.rules.get/set`), active alerts (`alerts.active`) that survive restarts.

## 0.1.0 · N0 (unreleased)

- First version: `install` / `uninstall [--purge]` as a hardened systemd service with its own account and a group-gated unix socket; `bridge`, `status`, `query`.
- Metrics sampled every 10 s straight from `/proc`, `/sys` and `statvfs` (vendored Nightgrid parser), averaged per minute and rolled up to 15-minute and hourly tiers (14 / 90 / 400 days).
- Protocol 1: `hello` with version and capability negotiation, `node.info`, `metrics.latest`, `metrics.history`, `metrics.sync`.
- CI: lint and tests on x86_64 and arm64, static musl builds, real install tests on systemd (runners, Debian 12, Ubuntu 22.04, Fedora 41). Release workflow with checksums, minisign and build provenance.
