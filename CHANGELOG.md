# Changelog

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
