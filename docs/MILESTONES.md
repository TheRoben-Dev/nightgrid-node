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

## N1b · Containers — **this release (0.3.0)**

- Container watcher over the engine's unix socket (Docker, or Podman's compatible socket): list (`containers.list`), lifecycle events into `events` (restarts merged, crashes, OOM, health), container-down alerts with the app's rules.
- Access only after root runs `nightgrid-node allow docker` (a drop-in adding the socket's group; root-equivalent, warned), `deny docker` takes it away. Read-only: the node only issues `GET`s.
- App spec: [0003 containers](app-integration/0003-containers.md).

## Re-plan (2026-11-16)

Roben's expanded brief sets the order after N1: **real-time push, then network watch, then self-healing, then the rest.** Node-side service checks moved into N7 (node-to-node checks), where outbound network is introduced once for both.

## N2 · Real-time push (protocol 2) — **done (0.4.0 streams, 0.5.0 logins and file changes)**

- Protocol 2: `subscribe {stream}` / `unsubscribe` on the same connection, pushed `{"stream", "data"}` frames; protocol 1 clients keep working.
- Streams: `events` (every new event as it is written: alerts, container lifecycle, outages), `metrics` (1–2 s while subscribed, nothing extra when nobody listens), `containers` (state changes).
- Container events in real time via the engine's streaming `/events` (no 2-minute hold for the push; the timeline copy still merges restarts).
- Logins and failed logins from the journal (`sshd`, `systemd-logind`) and `wtmp`, pushed and stored as events.
- File changes: inotify on a root-chosen list (default `/etc/ssh`, `authorized_keys` of login users, compose files the app registered), pushed and stored with a content hash, never contents.
- Backpressure: bounded per-subscriber queues, coalescing for metrics, a `lagged` frame instead of unbounded memory.
- App spec 0004.

## N3 · Network watch — **done (0.6.0 inventory, 0.7.0 DHCP helper)**

**N3a (0.6.0):** device inventory from `/proc/net/arp` (`net.devices`), `network` events for new devices, moves, MAC changes, IP conflicts and gateway MAC changes; `[network]` config; app spec 0006. **N3b (0.7.0):** `allow netwatch` starts a listen-only `CAP_NET_RAW` helper for ARP and DHCP: `net.dhcp`, rogue-DHCP and DHCP-change events, devices heard on the wire with their DHCP host names (N-021, N-028).


- 24/7 passive watch of the node's LAN: new or unknown devices, IP conflicts (one IP, two MACs), MAC changes, devices gone, from the kernel's neighbour table over netlink (no privileges).
- DHCP changes and rogue DHCP servers need to see DHCP broadcasts, which takes `CAP_NET_RAW` or a raw socket. This is a security choice to make before N3 (open question N-021): a separate tiny unit with only that capability, or the root helper, or neighbour-table only.
- Feeds the app's network map and device inventory (`net.devices`, `net.events`); MAC vendor lookup from a bundled OUI table, nothing online.
- App spec 0005.

## N4 · Actions and self-healing — **done (0.8.0 actions and self-healing, 0.9.0 root helper, 0.10.0 rollbacks)**

- **N4a (0.8.0):** typed container actions and image prune, root's allow-list, audit log, self-healing rules. App spec 0007.
- **N4b (0.9.0):** unit restart/start/stop and reboot/power-off through the root helper (N-010, N-032).
- **N4c (0.10.0):** file snapshots with a dead-man timer that reverts unless the app confirms (N-033).

Scope:


- Typed actions, each opt-in by root: container start/stop/restart and image prune (through the engine socket, which `allow docker` already grants), unit restart and power through the root helper (N-010). Never shell strings.
- Append-only, hash-chained audit log of every action, synced as events.
- Self-healing rules pushed by the app: condition (an alert kind and key, held for N minutes) → action, e.g. container unhealthy 5 min → restart, disk > 95 % → prune images. Each rule off by default, with a rate limit (max runs per hour/day, backoff), dry-run mode that only logs what it would do, and a stop when the condition doesn't clear.
- Safe rollbacks run node-side: snapshot compose/.env/config before a change, dead-man timer that reverts unless the app confirms.
- App spec 0007.

## N5 · Health that needs root, and logs

- **N5a (0.11.0):** read-only health helper (N-034) for SMART, ZFS/mdadm/btrfs pools, package updates and reboot-required, UPS (NUT), with the app's scripts and parsers; alerts and `health.latest`. Backup job status moves to N7 with scheduled jobs.
- **N5b (0.12.0):** journal and container log watchers (N-035) with the app's log rules (levels, trace grouping, 404 handling, fingerprints, bursts, new patterns, grace periods), secret filter on every stored line.
- App spec 0008.

## N6 · Fine-grained stats and host security

- **N6a (0.13.0):** per-service, per-container and per-process usage and RAPL power (N-036). App spec 0009.
- **N6b (0.14.0):** listening ports (`ports.list`, `port` events), login-burst alerts, wider file baseline through the health helper (N-037). App spec 0010.
- **N6c (0.15.0):** `firewall.block`/`unblock` and optional auto-block through the root helper's nftables table (N-038). App spec 0011.

- Per-process and per-container CPU, I/O and network at 1 s while the app watches, 10 s otherwise, kept 24 h and downsampled into minutes.
- Power draw from RAPL (powercap, needs read access root grants) or the UPS.
- File integrity baselines for `/etc`, compose files and `authorized_keys`; newly listening ports; failed-login bursts as alerts.
- Optional auto-block of failed-login bursts (nftables set through the root helper), off by default, time-limited, audited.
- App specs 0009, 0010, 0011.

## N7 · Node-to-node and service checks, jobs, group actions

- **N7a (0.16.0):** service checks (HTTP(S), TLS expiry, TCP) through a separate network-limited runner (N-039). App spec 0012. Node-to-node probes are TCP checks between nodes.
- Service checks from the node (HTTP(S), TCP, TLS expiry) and node-to-node probes, so the app can tell "really down" from "only unreachable over the tailnet". First outbound network: a systemd drop-in that allows exactly the configured addresses.
- Scheduled jobs that run with the app closed: backups, test restores, update windows, reboots. Results kept for the app; power jobs opt-in and audited.
- Backup job status (restic/borg/rsync, moved from N5): last run, result and staleness alerts for the jobs the app defines.
- Group actions coordinated by one node: UPS group shutdown, staged restarts across nodes.
- App specs 0012 and later.

## N8 · Upgrades, tailnet and status page

- `upgrade`: verified binary, health check after restart, automatic revert on failure.
- Optional tailnet listener with mutual key authentication.
- Optional read-only status page over the tailnet only, off by default, auth required.
- An app spec.

## N9 · DRIFT helpers

- Daily summaries prepared on the node (what changed, what broke, what's filling up) for the app's local LLM helper; nothing leaves the host unless the app asks.
