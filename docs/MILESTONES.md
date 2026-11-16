# Milestones

Each milestone is shippable on its own: a tagged release, tests and CI green, an app-integration spec for whatever the app has to do, and nothing in the app that breaks if the node is older, newer or absent.

## N0 · Install, offline metrics, sync — **done (0.1.0)**

- Workspace: `nightgrid-node-proto` (wire types) and `nightgrid-node` (static binary).
- `install` / `uninstall [--purge]` with `--json` reports; hardened systemd unit; dedicated account; group-gated unix socket; `bridge` for SSH exec channels; `status` / `query` for people.
- Metrics collected without a shell (vendored app parser), averaged per minute, rolled up to 15-minute and hourly tiers, pruned like the app (14 / 90 / 400 days).
- Protocol 1: `hello` with version and capability negotiation, `node.info`, `metrics.latest`, `metrics.history`, `metrics.sync`.
- CI: fmt, clippy, tests on x86_64 and arm64; static musl builds for both; real install/upgrade/uninstall/purge on the runners' systemd and in Debian 12, Ubuntu 22.04 and Fedora 41 systemd containers. Release workflow with checksums, minisign (when the key is set) and build provenance.
- App spec: [0001 install and metrics sync](app-integration/0001-install-and-metrics-sync.md).

Done when: the app can install the node on a host, close, come back a day later and show the day's graphs without a gap.

## N1 · Events, uptime, disk usage and offline alerts — **done (0.2.0)**

- `events` stream with a growing `seq` (`events.sync`), 90 days / 20 000 events, secret filter on every title and detail.
- Uptime from a once-a-minute heartbeat: hourly up/down/asleep seconds (`uptime.sync`, 400 days) and outage events. A new boot without a clean stop is "Down", with one "Restarted"; growth of `CLOCK_BOOTTIME − CLOCK_MONOTONIC` is "Suspended". Time the node alone was stopped is not guessed at.
- Disk usage per mount per hour (`disk.sync`, 90 days) and fill-up forecasts (vendored least-squares trend).
- Alert tracker vendored from the app (grace periods, clear holds), active alerts survive restarts, rules pushed by the app (`alerts.rules.get/set`). Rules evaluated so far: disk full and disk filling. Fired, changed and resolved alerts go into `events`.
- App spec: [0002 events, uptime and disk sync](app-integration/0002-events-uptime-disk.md).

Done when: the app can be closed for a day, and on reconnect shows the outages, disk alerts and forecasts it missed.

## N1b · Containers and service checks

- Container create/die/restart/OOM/health via the Docker events API, only if root adds the service account to the `docker` group (`nightgrid-node allow docker`, with the "this is root-equivalent" warning). Container-down alerts.
- Service checks from the node (HTTP(S), TCP, TLS expiry), configured by the app (`checks.set`); outbound network for this capability only, through a systemd drop-in that allows exactly the configured addresses. Service-down and cert-expiring alerts, check uptime.
- App spec 0003.

## N2 · Logs and logins

- Journal and container log watchers with the app's log rules: levels, multi-line trace grouping, HTTP 404 handling, pattern fingerprints, bursts and new-pattern detection, grace periods; saved log searches with alerts. Rules pushed by the app; secret filter on every stored line.
- Logins and failed logins (journal `sshd`/`systemd-logind`, `wtmp`).
- App spec 0003.

## N3 · Health that needs root

- Root helper for fixed reads (see N-010): SMART (`smartctl -j`), ZFS/mdadm/btrfs pools, package update counts and reboot-required (apt, dnf, rpm-ostree, pacman), backups (restic/borg/rsync job status), UPS (NUT `upsc`).
- `nightgrid-node allow <cap>` / `deny <cap>` (root) editing the policy, shown in `node.info`.
- App spec 0004.

## N4 · Live streams (protocol 2)

- Pushed frames: metrics at 1–2 s, events, journal and container log follow, container stats. The app switches from polling over SSH to subscriptions where the node has the capability.
- Backpressure and per-stream rate limits; still a single connection.
- App spec 0005.

## N5 · Actions, jobs and rollbacks

- Typed actions through the root helper, each opt-in: container start/stop/restart, unit start/stop/restart, compose up/pull/down, power (reboot/shutdown/suspend), Wake-on-LAN relay.
- Append-only, hash-chained audit log; actions synced as events.
- Scheduled jobs and power schedules that run with the app closed.
- Safe rollbacks run node-side: snapshot compose/.env/config files before a change, a dead-man timer that reverts unless the app confirms (like the app's 90 s Proxmox network rollback).
- App spec 0006.

## N6 · Upgrades and other transports

- `upgrade`: verified binary, health check after restart, automatic revert to the previous binary on failure.
- Optional tailnet listener with mutual key authentication; optional phone status page served from the node.
- App spec 0007.

## N7 · DRIFT helpers

- Daily summaries prepared on the node (what changed, what broke, what's filling up) for the app's local LLM helper; nothing leaves the host unless the app asks.
