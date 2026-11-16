# Decisions

Numbered `N-xxx` so they don't collide with the app's `D-xxx`.

## N0

### N-001 · Separate repo; the app repo is read-only reference
All node code, CI and specs live here. Anything the app must do is written up in `docs/app-integration/` for the app's own project to build. App code is reused by vendoring (with attribution and the source commit) or, later, by the app depending on this repo's `nightgrid-node-proto` crate through a pinned git dependency.

### N-002 · NDJSON over a unix socket, not HTTP or gRPC
The app already multiplexes everything over one SSH connection per host. A unix socket reached through streamlocal forwarding or a tiny stdio `bridge` adds no listener and no TLS, and line-delimited JSON is easy to debug (`nightgrid-node query …`, or `socat`). HTTP would bring a server stack and a temptation to bind to TCP; gRPC would add protobuf tooling to a Tauri app for little gain at this message rate.

### N-003 · Metrics code is vendored from the app, not depended on
`parse.rs`, `Collector`, `Point::from_sample` and `MinuteAverager` are copied from `nightgrid-core` at commit `8eedcf2` (MIT, attributed in the file headers), with the app's own fixtures and tests. Identical code means a synced point equals the one the app would have stored. A git dependency on `nightgrid-core` would pull russh, bollard, keyring, rustls and more into a static server binary. Local changes are marked `NODE:` (so far: minute points keep their sample count `n`).

### N-004 · No shell per sample
The app runs a `sh` script per sample because it can't install anything. The node reads the same files directly and prints the same `@@section` text, so the vendored parser is reused unchanged while each sample costs a handful of `read(2)` calls. `statvfs(3)` replaces `df`; network filesystems run on a helper thread with a 2 s timeout and are skipped while a call is stuck (the app uses `timeout 4 df` for the same reason). `nvidia-smi` runs at most every 30 s.

### N-005 · Static musl binaries built with cargo-zigbuild
`rusqlite` bundles SQLite's C code; zig gives a C cross-compiler for aarch64 musl without Docker or `cross`. Release profile: `opt-level = "s"`, LTO, one codegen unit, `panic = "abort"`, stripped: ~2 MB per architecture.

### N-006 · One sampling thread plus a current-thread tokio runtime
Sampling is blocking file I/O and may wait on a slow filesystem; it runs on its own thread so the socket stays responsive. The server is a single-threaded tokio runtime (connections are few: one per app instance). Measured: ~2.5 MB RSS, two threads.

### N-007 · Access is the socket's group; the account comes from systemd-sysusers
`systemd-sysusers` is present on every systemd distro (including rpm-ostree systems where `useradd` into `/usr/lib` isn't possible) and is declarative. The socket is `0660` in a `0750` runtime directory, group `nightgrid-node`; `install --user` adds the SSH login user. No setuid/setgid binaries, no polkit, no sudo for reading.

### N-008 · Binary in `/usr/local/bin`, copied not moved
Writable on Debian, Ubuntu, Fedora, Proxmox and on Fedora Atomic/Bazzite (`/usr/local` is `/var/usrlocal`). Copying into the directory and renaming gives the file the directory's SELinux type (`bin_t`) instead of a temp-file label; `restorecon` runs as a best effort where SELinux is on.

### N-009 · Sync is pull-based and the node keeps no per-client state
Cursors live in the app, per node id and tier. Points are final when served (closed buckets only), and the app's minute insert ignores duplicates, so retries and several app installs syncing the same node are all safe.

### N-010 · Privileged work runs in a root helper, not through sudo (decided 2026-11-16)
The daemon runs with `NoNewPrivileges=yes`, so it can't use sudo. Two ways to do root-only reads (SMART, pools) and actions:

- **A. Root helper (recommended).** A second, socket-activated unit (`nightgrid-node-helper.socket`, root, reachable only by the service account) runs the same binary in `helper` mode per request. It accepts only typed operations named in the root-owned policy (`smart.read`, `container.restart{name}`, …), never shell strings, audits each one, and exits. A compromised daemon can only ask for what root allowed. Works the same on every distro.
- **B. sudoers rules per capability.** `/etc/sudoers.d/nightgrid-node` lists exact commands. Familiar, but the daemon would have to drop `NoNewPrivileges`, sudoers wildcards are easy to get wrong for arguments like container names, and sudo isn't installed everywhere (minimal Debian, some Proxmox installs).

**Decided: A.** Roben chose the root helper. N3 builds it; the daemon keeps `NoNewPrivileges=yes` and never calls sudo.

### N-011 · Root-owned policy vs app-pushed preferences
`/etc/nightgrid-node/config.toml` (root) says what the node *may* do: intervals, retention, enabled capabilities, later which actions are allowed. It uses `deny_unknown_fields`, so a typo fails loudly instead of silently changing behaviour. Preferences that the app owns (alert rules, log rules, check targets) will be pushed over the socket and stored in the state directory; they can never enable a capability the policy doesn't.

### N-012 · No CLI or logging frameworks
Hand-rolled argument parsing and a 50-line stderr logger with `sd-daemon` priority prefixes (journald files them at the right level). Keeps the binary and RSS small; there are only a handful of flags.

### N-013 · 14 days of minute points by default
The app keeps 7 days of minutes by default, but the node is the copy that bridges the app's absences, so it keeps two weeks (about 20 000 rows, ~2 MB). Rollup tiers match the app (90 and 400 days).

### N-014 · Checksums and provenance now, signatures later (2026-11-16)
Roben decided checksums are enough for now. Releases ship `SHA256SUMS` and GitHub build provenance (`gh attestation verify`). The release workflow signs with minisign only if a `NODE_MINISIGN_KEY` secret is ever added; nothing else changes when it is.

## N1

### N-015 · Outages are classified from boot id, a clean-stop flag and the suspend clock
Once a minute the node writes `last_alive`, the boot id and the suspended-seconds counter (`CLOCK_BOOTTIME − CLOCK_MONOTONIC`). On start it compares:

- new boot id, no clean stop → **down** from the last heartbeat to boot time ("Down", critical);
- new boot id after a clean stop → **asleep** ("Restarted", info), so planned reboots don't read as downtime;
- same boot, suspend counter grew → that much **asleep** ("Suspended");
- same boot otherwise → the node was stopped while the host ran: nothing is recorded, since the node can't know. The app fills those hours from its own observations.

Gaps under 60 s are ignored. The app's "host offline" alert stays app-side: a host can't watch itself go offline.

### N-016 · Alert keys drop the host segment; the app adds it back
Node keys are `disk:/srv`, `fill:/srv`. The app's are `disk:<hostId>:/srv`. The node doesn't know the app's host ids, so the app maps them when it imports, which also keeps several app installs consistent.

### N-017 · Alert rules are app-owned preferences in the state directory
Per N-011, rules are pushed by the app (`alerts.rules.set`) and stored in SQLite meta, not in the root-owned config. The field names are the app's `AlertSettings`, unknown fields are ignored, so the app can send its whole settings object and older nodes evaluate what they know. A rule can never enable a capability the policy disallows.

### N-018 · Writes happen once a minute
The heartbeat, disk usage and alert evaluation run once per minute on the sampler thread, not per 10 s sample. Disk-full alerts therefore fire up to a minute later than the app would; in exchange the SD card on a Pi sees one small transaction per minute. Forecasts are recomputed and old rows pruned once an hour.

## N1b

### N-019 · Container access is a systemd drop-in adding the socket's group
`nightgrid-node allow docker` writes `nightgrid-node.service.d/docker.conf` with `SupplementaryGroups=<socket group>` instead of adding the account to the group with `usermod`. It applies to the service only (not to a login shell as that user), shows up in `systemctl cat`, and `deny docker` or uninstall removes it in one step. The group is read from the socket, so Podman's or a custom engine socket works the same. Root-owned sockets are refused, since that would mean the root group. The node talks HTTP/1.0 `GET` to the socket with a 60-line client instead of pulling in hyper or bollard.

### N-020 · Order after N1 follows the expanded brief (2026-11-16)
Roben's expanded brief puts real-time push first, then network watch, then self-healing, then the rest (root-only health and logs, fine-grained stats and host security, node-to-node and service checks with jobs and group actions, upgrades/tailnet/status page). Service checks moved from N1b to N7 so that outbound network is introduced once, for both service checks and node-to-node probes.

### N-021 · Open: how network watch sees DHCP (decide before N3)
New devices and IP conflicts come from the kernel's neighbour table over netlink, which needs no privileges. Seeing DHCP offers (to spot DHCP changes and rogue DHCP servers) needs raw packets: `CAP_NET_RAW`, which the daemon must not have. Options: a separate tiny unit holding only `CAP_NET_RAW` and a BPF filter for UDP 67/68; asking the root helper for periodic DHCP discovery; or neighbour table only. To be put to Roben when N3 starts.

## N2

### N-022 · Streams share the request connection and drop rather than buffer
Protocol 2 pushes frames on the same connection as requests, told apart by having `stream` and no `id`, so the app needs one channel per host and the bridge stays a byte pipe. Each stream is a bounded broadcast queue per connection; a slow reader loses the oldest frames and gets a `lagged` frame, so a stalled SSH channel can't grow the node's memory. Events are durable anyway (`events.sync` refills a gap); metrics and container lists are snapshots, so dropping old ones is harmless.

### N-023 · The node samples faster only while someone watches
With no subscriber the sampler keeps the configured interval (10 s). While a `metrics` subscriber is connected it samples every 2 s and the container watcher reads every 3 s, and both return to normal within a second of the last subscriber leaving. Minute averages simply include more samples. Idle cost stays where it was; the app gets live tiles without running `COLLECT` over SSH.

