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
- **N7b (0.17.0):** backup status and alerts for the app's backup jobs; `job` events for every run of backup units and the app's scheduled jobs, scheduled reboots included. The node watches systemd's timers instead of scheduling (N-040). App spec 0013.
- **N7c (0.18.0):** UPS shutdown per host with root's limits and a cancellable countdown, no node-to-node link (N-041). Staged restarts across hosts stay in the app. App spec 0014.
- Service checks from the node (HTTP(S), TCP, TLS expiry) and node-to-node probes, so the app can tell "really down" from "only unreachable over the tailnet". First outbound network: a systemd drop-in that allows exactly the configured addresses.
- Scheduled jobs that run with the app closed: backups, test restores, update windows, reboots. Results kept for the app; power jobs opt-in and audited.
- Backup job status (restic/borg/rsync, moved from N5): last run, result and staleness alerts for the jobs the app defines.
- Group actions coordinated by one node: UPS group shutdown, staged restarts across nodes.
- App specs 0012 and later.

## N8 · Upgrades, node-to-node link and status page

- **N8a (0.19.0):** `upgrade` with a verified binary, a health check after restart and automatic revert (N-042). App spec 0015.
- **N8b (0.20.0):** an opt-in node-to-node link (Roben, 2026-11-17: "nodes should be able to communicate in general over Nightgrid Node, but default to each node acting alone"), N-043. App spec 0016.
  - off by default, turned on per host by root (`allow peers`);
  - a separate unit that listens only on the tailnet address (or an address root names), never on every interface;
  - mutual TLS 1.3 with certificates pinned per peer, paired through the app, so no trust-on-first-use;
  - every request typed and checked against root's `[peers] serve` on the receiving side;
  - peer reachability: pings, round trip, version, and "unreachable" / "reachable again" events.
- **N8c (0.21.0):** UPS group shutdown over the link: hosts the app marks as following a UPS host start their own countdown when it starts its own, and stop when its power returns (N-044). App spec 0017. Peer reachability came with N8b. Staged restarts and alert relay stay in the app (N-044).
- **N8d (0.22.0):** an optional read-only status page over the tailnet only, off by default, at a secret link (N-045). App spec 0018.
- App specs for each part.

## N9 · DRIFT helpers

- **N9 (0.23.0):** `summary.daily`: what happened on the host over the last day (or up to a week) as plain facts for the app's DRIFT helper, made on request from the node's own history (N-046). App spec 0019.
- Wake-on-LAN relay: stays with the app's relay hosts and their systemd timers, which already work with the app closed (N-047).

## N10 · Refinement

App-facing interfaces are frozen (N-048); this milestone polishes the node from the inside.

- **0.24.0:** `nightgrid-node doctor`, a read-only self-check of the install; CI gates for the sandbox scores and peak memory; every method fed malformed input in tests (N-049).
- **0.25.0:** a readable `status`; `events`, `audit` and `audit verify` (the whole SHA-256 chain); shell completions and a manual page.

## N11 · Fix menu and opt-in automation — **done (0.27.0)**

Roben, 2026-11-18: act on issues on the server through the user's choice, in a menu like Bazzite's `ujust`, with the node optionally acting by itself on chosen events; every settings-style command uses the same menu, themed after Nightgrid. Shell-side only; the frozen protocol is unchanged (N-060 to N-064).

- `nightgrid-node fix`: detected issues, fixes grouped by kind, automation presets; `fix list [--json]`, `fix NAME [TARGET] [-y]`, `fix auto [NAME on|preview|off]`.
- Node-local actions `fix.unit-reset`, `fix.journal-vacuum`, `fix.logrotate`, `fix.package-cache`, `fix.ssh-permissions`, `fix.runbook` through the root helper, run as transient units.
- Root's `fixes.toml`: extra allow and units, `journal_keep`, runbooks, automation rules, re-read by the service when it changes.
- A shared Nightgrid-themed terminal menu (`tui`), used by `fix`, `allow`/`deny`, `serve`, the bare `nightgrid-node` and `nightgrid-hub`.

## Hub profile

A separate, heavier `nightgrid-hub` for one always-on host per network, next to the unchanged Lite agent. Design in [HUB.md](HUB.md), decisions N-051 to N-059, app spec [0020](app-integration/0020-hub-fleet-sync.md) (future). Nothing here changes a frozen interface; the Lite additions are additive.

- **H0:** design (this document set). Docs only.
- **H1 — done (0.26.0):** `nightgrid-hub` binary, install/uninstall/upgrade, store, collector pulling members over the link (`peer.sync`, `hub.member`), `fleet.*` on the hub socket, host-offline alerts, CI budget test with 25 simulated members.
- **H2:** notifier: ntfy, Discord, webhook, email; folding, grouping, quiet hours, rate limits, heartbeat URL.
- **H3:** fleet status page; hub checks (HTTP, TLS, TCP, DNS).
- **H4:** SNMP devices (v3 read-only).
- **H5:** baselines, anomalies and forecasts.
- **H6:** log index and search (opt-in per member).
- **H7:** runbooks, backup verification and restore tests, patch windows with rollback, holds, standby hub.
