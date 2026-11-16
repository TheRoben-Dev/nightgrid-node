# Nightgrid Node: design

Nightgrid is agentless: the app does everything over SSH while it is open. Nightgrid Node is an **optional** agent for Linux servers that keeps collecting while the app is closed or the computer is offline, and hands everything over when the app reconnects. It is an accelerator, never a requirement: every app feature keeps working over plain SSH on hosts without it.

Status: N0 to N3 are built (metrics, events, uptime, disk usage, offline alerts, containers, live push, logins, watched files, LAN device inventory, DHCP watch); this document describes the whole plan and marks what exists with its milestone in bold. The order after N1 follows the expanded brief of 2026-11-16: real-time push, network watch, self-healing, then the rest. Milestones are in [MILESTONES.md](MILESTONES.md), the wire format in [PROTOCOL.md](PROTOCOL.md), choices and their reasons in [DECISIONS.md](DECISIONS.md), and the app-side work in [app-integration/](app-integration/).

## 1. Shape

```
 Nightgrid app (Mac / Windows / Linux)                        Linux server
┌──────────────────────────────────┐   existing SSH connection   ┌────────────────────────────────────────────┐
│ core::ssh pool                   │ ══════════════════════════> │ sshd                                       │
│   ├ shell, SFTP, Docker (today)  │                             │  └ exec "nightgrid-node bridge"  (stdio)   │
│   └ node client (NDJSON)  ───────┼── or direct-streamlocal ──> │      or streamlocal to the socket ─┐       │
│       hello → caps → sync/stream │                             │                                    ▼       │
│ falls back to plain SSH when the │                             │  /run/nightgrid-node/node.sock (0660,      │
│ node is absent or lacks the cap  │                             │   group nightgrid-node)                    │
└──────────────────────────────────┘                             │  nightgrid-node run  (user nightgrid-node) │
                                                                 │   ├ sampler thread: /proc, /sys, statvfs   │
                                                                 │   ├ engine: uptime, disk usage, alerts     │
                                                                 │   ├ containers thread (opt-in docker grant)│
                                                                 │   ├ SQLite: /var/lib/nightgrid-node/node.db│
                                                                 │   └ socket server (tokio, current thread)  │
                                                                 └────────────────────────────────────────────┘
```

- **One static binary** (`nightgrid-node`, musl, x86_64 and aarch64, ~2 MB) with subcommands: `run` (the service), `install`, `uninstall`, `bridge`, `status`, `query`, `version`. **N0**
- **One systemd unit**, heavily sandboxed, running as the unprivileged `nightgrid-node` account. **N0**
- **No open ports, no outbound connections.** The unit sets `IPAddressDeny=any` and `RestrictAddressFamilies=AF_UNIX`, so this is enforced by the kernel, not just by the code. **N0**
- Footprint, measured on N0: ~2.5 MB RSS, two threads, no measurable CPU at the default 10 s interval. The unit caps it at `MemoryMax=64M` with low CPU and IO weights.

## 2. Transport

The app already holds one multiplexed russh connection per host. It reaches the node through that connection in one of two ways, tried in order:

1. **`direct-streamlocal@openssh.com`** to `/run/nightgrid-node/node.sock` (the same mechanism the app uses for the Docker socket, D-025). No process on the host per connection.
2. **Exec channel running `nightgrid-node bridge`**, which copies stdin/stdout to the socket. Works where sshd has `AllowStreamLocalForwarding no`. **N0**

Who may connect is decided by file permissions: the socket is `0660` and its directory `0750`, both group `nightgrid-node`. `install --user NAME` (default: the user who ran sudo) adds the SSH login user to that group. Group membership applies from the next login, so the app reconnects after installing (or uses `sg nightgrid-node -c 'nightgrid-node bridge'` for the current session). The node logs the peer uid of every `hello`.

Tailnet transport (N8, off by default): a listener bound only to the host's tailnet address, mutually authenticated with keys exchanged over SSH at install time. It exists for the phone status page and for the app reaching the node without an SSH session; it is never on unless chosen.

## 3. Protocol

NDJSON over the stream: one JSON request per line, one response per request, matched by `id`. The first request must be `hello`, which negotiates a protocol version (newest both sides speak) and returns the node's identity and **capabilities** (`name → version`). The app calls only methods whose capability is listed and otherwise falls back to SSH, so an old node, a new node, or a node with a capability switched off all work. Details and every method: [PROTOCOL.md](PROTOCOL.md).

Versioning rules:

- The **protocol version** changes only for framing or session semantics (e.g. version 2 adds server-pushed stream frames).
- A **capability version** bumps when a method's meaning changes or a client must rely on a new field. Adding optional fields doesn't bump anything; unknown fields are ignored on both sides.
- The node keeps accepting the previous protocol version for at least one minor release after a bump, so the app and node can be upgraded in either order.

## 4. Storage

SQLite (rusqlite, bundled, WAL, incremental auto-vacuum) at `/var/lib/nightgrid-node/node.db`, mode 0700 directory owned by the service account. Forward-only migrations via `rusqlite_migration` (same as the app, D-001).

**Metrics (N0)** use the app's own tiers so synced history is indistinguishable from what the app would have recorded:

| Tier | Bucket | Kept | Serves |
|---|---|---|---|
| minute | 60 s | 14 days (configurable 1–365) | ranges ≤ 2 days |
| rollup | 900 s | 90 days | ranges ≤ 31 days |
| hourly | 3600 s | 400 days | longer ranges, SLA reports |

The collector reads `/proc`, `/sys` and `statvfs(3)` directly and prints the same `@@section` text as the app's `COLLECT` script, which then goes through the app's own parser and `Collector` (vendored, see N-003). Samples are averaged into minute points and folded into rollups as running averages, exactly like `Store::insert_metric_point`. Network filesystems are measured on a helper thread with a timeout so a hung NFS mount can't stall sampling.

**Later tiers** (N1+): an append-only `events` table (alerts fired/resolved, container restarts, boots, OOM kills, failed units, logins, package transactions, backup results, UPS changes, log findings) with a monotonically increasing `seq`, and `uptime_hours` / `outages` mirroring the app's tables. Size is bounded by row caps and age, like the app's timeline (90 days / 5000 rows) and uptime (400 days).

## 5. Sync

Goal: graphs, uptime bars, timeline and SLA reports have no gaps for time the node was running.

- **Pull-based, app-driven, idempotent.** The node keeps no per-client state; each app install keeps its own cursors per node (`node_id` + tier/stream). A Mac, a Windows desktop and a handheld can all sync the same node independently.
- **Metrics (N0):** `metrics.sync {res, after}` returns *closed* buckets with `ts > after`, oldest first, in pages of up to 5000. The app inserts minute points through its existing `insert_metric_point` (minute rows are `ON CONFLICT DO NOTHING`, so overlap with what it sampled itself is harmless) and advances the cursor. If the app was away longer than the node's minute retention, it first fills the older span from the hourly and 15-minute tiers (`oldest` in the answer tells it where minutes start). See [app-integration/0001](app-integration/0001-install-and-metrics-sync.md).
- **Events and alerts (N1):** `events.sync {after: seq}`; the same cursor rule. A changed `node_id` (purged reinstall) resets cursors.
- **Uptime (N1):** the node records the host as up for each minute it was running and closes an outage at boot when the gap since the last heartbeat is longer than a clean shutdown explains; services checked from the node record their own up/down time. Unknown time stays unknown, as in the app (D-113).
- **Live (N2):** after sync, the app subscribes to streams (metrics at 1–2 s, events, log tails, container stats) instead of polling over SSH.

## 6. Alerts while offline (N1)

Shipped in 0.2.0: disk full and disk filling, plus outages and uptime (see N-015 to N-018 in [DECISIONS](DECISIONS.md)). The rest of the list below arrives with the milestone that collects its data.


The node evaluates the same conditions as the app's alerts loop (host resources, disks and forecasts, containers, services and certificates, SMART and pools, backups, UPS, log anomalies) with the same pure `Tracker` semantics: per-kind grace periods (host offline 120 s, service down 90 s), clear holds (disk 5 min, forecasts 3 h) and scoped reports. Rules come from the app (`alerts.rules.set`, stored in the node's state directory), so the app stays the single place to edit them. Fired and resolved alerts go into the `events` stream; the app pulls them on reconnect and shows them as "while you were away", feeding its own alert history, timeline and notifications. There are no external channels (ntfy, email, webhooks) for now, and the node can't make outbound connections anyway.

## 7. Security model

| Concern | Measure |
|---|---|
| Privilege | Dedicated system account (`systemd-sysusers`, no shell, no password). `NoNewPrivileges`, empty capability set, `ProtectSystem=strict`, `ProtectHome=read-only`, syscall filter, no device access except NVIDIA's for `nvidia-smi`. **N0** |
| Network | None. `IPAddressDeny=any`, `RestrictAddressFamilies=AF_UNIX`. Tailnet listener is a later, opt-in capability. **N0** |
| Who can talk to it | Unix socket `0660`, group `nightgrid-node`; members are chosen at install. Peer uid logged. **N0** |
| What it may do | Read-only until N4 (actions). Policy lives in root-owned `/etc/nightgrid-node/config.toml`; nothing on the socket can widen it. `deny_unknown_fields` so a typo can't silently enable something. **N0** |
| Privileged reads and actions (N4, N5) | Never run by the daemon itself. Each capability is opted into by root (`nightgrid-node allow <cap>`), and the work is done by a separate, short-lived, root helper that accepts only fixed, typed operations listed in the policy (no shell strings). See DECISIONS.md N-010. |
| Audit | (N4) Append-only, hash-chained action log in the state directory: who (uid, client), what, target, result, before/after for rollbacks. Synced to the app with the events stream. |
| Secrets | N0 stores none. Later ones (e.g. a service check's token) use `systemd-creds` (encrypted at rest, TPM-bound where available) or are pushed by the app per session and held in memory only. Logs and events go through the same secret filter as the app (`redact::line_has_secret`). |
| Releases | Static binaries with SHA256SUMS and GitHub build provenance; the app verifies the checksum before uploading, and `install` is only run on a verified file. Minisign signing is wired in the release workflow but off until a key is configured (N-014). |
| Container engine | Off until root runs `nightgrid-node allow docker`, which writes a drop-in adding the socket's group (root-equivalent, warned) and is undone by `deny docker`. The node only sends `GET`s. **N1b** |
| Automatic actions | See section 7a. |
| Public repo | No secrets, host names or personal details anywhere; fixtures are synthetic (`server01`, `pi`). |

## 7a. Automatic actions (N4 onwards)

Self-healing rules, auto-block, scheduled jobs and power schedules, and group actions all follow the same rules:

- **Off by default, per rule.** Root's policy says which action types may run at all; the app pushes the rules; each rule is enabled on its own.
- **Typed, not scripted.** An action is a named operation with checked arguments (`container.restart {name}`, `images.prune`), carried out through the engine socket or the root helper, never a shell string.
- **Bounded.** Every rule has a rate limit (runs per hour and per day) with backoff, and stops after N attempts when the condition doesn't clear, raising an alert instead.
- **Dry-run first.** A rule can run in dry-run mode, which records what it would have done without doing it.
- **Audited.** Every run, dry or real, goes into the hash-chained action log and the events stream, so the app shows it on reconnect.

Status: N4a (0.8.0) implements this for container actions and image prune (decisions N-029 to N-031, [PROTOCOL](PROTOCOL.md#actionsrun-actions-1)). Unit restart and power need the root helper (N4b).

## 8. Install, upgrade, uninstall

All three are one command run as root, idempotent, with a `--json` report for the app. **N0**

- **Install:** the app uploads the verified binary to a private temp dir over SFTP and runs `sudo <tmp>/nightgrid-node install --json`. It copies itself to `/usr/local/bin` (a new file, so it gets the right SELinux label; writable on rpm-ostree/Bazzite too), creates the account with `systemd-sysusers`, writes the config (only if missing) and the unit, adds the login user to the group, enables and (re)starts the service and waits until it answers.
- **Upgrade:** the same command with a newer binary. History and node id are kept; the running service keeps its old inode until the restart. N8 adds `upgrade` with a health check and automatic revert to the previous binary.
- **Uninstall:** `sudo nightgrid-node uninstall` stops and removes the service and binary but keeps history and config, so reinstalling resumes. `--purge` also removes history, config and the account. If the binary is gone or broken, [dist/uninstall.sh](../dist/uninstall.sh) does the same with plain shell commands.
- **Downgrade:** a binary refuses to open a database migrated by a newer version; `uninstall --purge` then install is the way back.

Portability: needs systemd ≥ 247 and `usermod`. Tested in CI on Ubuntu (x86_64 and arm64 runners with real systemd) and in systemd containers for Debian 12, Ubuntu 22.04 and Fedora 41. Proxmox VE is Debian; Bazzite is Fedora Atomic (`/usr/local` → `/var/usrlocal`, `/etc` writable).

## 9. App features → node capabilities

| App feature (today over SSH) | Node capability | Milestone | Without the node |
|---|---|---|---|
| Live host metrics, overview tiles, GPU | `metrics` (latest), `metrics` stream | **N0** / N2 | `COLLECT` script over SSH |
| Metric history, long-range charts, forecasts | `metrics.sync` (minute, 15 min, hourly) | **N0** | only while the app runs |
| Uptime bars, SLA reports | `uptime.sync` | **N1** | only while the app runs |
| Disk usage trend, "full in N days" | `disk.sync` | **N1** | only while the app runs |
| Alerts while away (disk, filling, containers) | `alerts`, `events.sync` | **N1** / **N1b** | only while the app runs |
| Container states, restarts, health | `containers` (opt-in docker grant) | **N1b** | Docker API over SSH |
| Real-time events (alerts, containers, logins, file changes) | protocol 2 streams, `logins`, `files` | **N2** | polling over SSH |
| Network map, device inventory, IP conflicts, rogue DHCP | `net` | **N3a** (inventory, conflicts), **N3b** (DHCP helper) | scans from the app |
| Container/unit/power actions, self-healing | `act.*`, `heal` (opt-in per rule) | N4 | SSH + sudo prompt |
| Snapshot and rollback (compose, config, network) | `rollback` with dead-man timer | N4 | app-driven |
| SMART, pools, updates, backups, UPS | root helper reads | N5 | sudo over SSH |
| Log anomalies, saved log searches with alerts | `logs` (rules pushed by the app) | N5 | only while the app runs |
| Per-process/container stats at 1 s, power draw | `procs`, `power` | N6 | top/docker stats over SSH |
| File integrity, new ports, failed-login bursts, auto-block | `security` | N6 | on demand |
| Service checks, TLS expiry, node-to-node probes | `checks` (from inside the LAN) | N7 | from the app's computer |
| Scheduled jobs, power schedules, group shutdown | `jobs`, `group` | N7 | only while the app runs |
| Node self-update, tailnet, status page | `upgrade`, `tailnet`, `statuspage` | N8 | n/a |
| DRIFT daily summaries | `summaries` | N9 | n/a |

## 10. Reuse of app code

App logic that must give identical results (metrics parsing, rates, minute averaging, rollups; later the alert tracker, log patterns and secret filter) is **vendored** with attribution and the source commit, and marked where it differs (`NODE:`). Depending on `nightgrid-core` by git would pull russh, bollard, keyring, Tauri-adjacent crates and their C dependencies into a static server binary; vendoring keeps the node at ~2 MB. When the app later depends on this repo's `nightgrid-node-proto` crate (pinned git dependency), the wire types are shared instead of copied.
