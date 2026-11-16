# Changelog

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
