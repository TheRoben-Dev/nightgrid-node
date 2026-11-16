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
