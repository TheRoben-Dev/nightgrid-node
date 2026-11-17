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

### N-021 · DHCP is watched by a separate unit holding only `CAP_NET_RAW` (decided 2026-11-16)
New devices and IP conflicts come from the kernel's neighbour table over netlink, which needs no privileges. Seeing DHCP offers (to spot DHCP changes and rogue DHCP servers) needs raw packets: `CAP_NET_RAW`, which the daemon must not have. Options: a separate tiny unit holding only `CAP_NET_RAW` and a BPF filter for UDP 67/68; asking the root helper for periodic DHCP discovery; or neighbour table only. **Decision (Roben, 2026-11-16): a separate tiny unit with only `CAP_NET_RAW`**, opt-in through `nightgrid-node allow netwatch`, that passes what it sees to the main service over the socket. The main service never gains a capability or a network address family, and root never parses LAN packets. Built in N3b (N-028).

## N2

### N-022 · Streams share the request connection and drop rather than buffer
Protocol 2 pushes frames on the same connection as requests, told apart by having `stream` and no `id`, so the app needs one channel per host and the bridge stays a byte pipe. Each stream is a bounded broadcast queue per connection; a slow reader loses the oldest frames and gets a `lagged` frame, so a stalled SSH channel can't grow the node's memory. Events are durable anyway (`events.sync` refills a gap); metrics and container lists are snapshots, so dropping old ones is harmless.

### N-023 · The node samples faster only while someone watches
With no subscriber the sampler keeps the configured interval (10 s). While a `metrics` subscriber is connected it samples every 2 s and the container watcher reads every 3 s, and both return to normal within a second of the last subscriber leaving. Minute averages simply include more samples. Idle cost stays where it was; the app gets live tiles without running `COLLECT` over SSH.

### N-024 · Logins come from the journal through one `journalctl` child, after `allow journal`
Failed SSH logins are only in the journal (`btmp` is root-only), and reading the system journal needs its group. `nightgrid-node allow journal` adds that group with a drop-in, like `allow docker` (N-019), and warns that it exposes every log. The node runs one long-lived `journalctl --follow -o json -t sshd -t sshd-session` rather than parsing journal files itself (no libsystemd in a static binary), resumes from a saved cursor, keeps only parsed login lines, and sums failed logins per minute. The child adds a few MB of memory only on hosts where root granted it.

### N-025 · Watched files are polled, and contents are never stored
Root lists files and directories (`[files] watch`). Every 10 s the node `stat`s them (and the direct entries of directories) and hashes readable files under 4 MiB when their metadata changed. Polling instead of inotify handles editors that replace files by rename and paths that appear later, at the cost of a few dozen `stat` calls. Events carry sizes, a 16-digit SHA-256 prefix, mode and owner, never contents. Unreadable files (`/etc/shadow`) still report that they changed. The baseline is saved, so changes made while the node was stopped are reported at the next start.

## N3

### N-026 · The device inventory reads `/proc/net/arp`, not netlink, and ships no vendor table
The kernel's IPv4 neighbour table and default route are in `/proc/net/arp` and `/proc/net/route`, readable by any user. Netlink would add IPv6 neighbours but needs `AF_NETLINK` in the unit's `RestrictAddressFamilies`, which is `AF_UNIX` only today; IPv4 covers the devices that matter for a homelab map, so the unit stays as tight as it was. Devices are keyed by MAC with their latest address. The app already bundles the IEEE OUI list (about 1 MB) and `mac_vendor`, so the node reports MACs and the app names the maker; the node binary stays about 2 MB.

### N-027 · IP conflicts are an address that keeps switching MACs
An ARP table holds one MAC per address, so two devices on one address show up as the address switching back and forth. One switch while the old owner still claims the address is logged ("answers from a new MAC"); a second within ten minutes is an IP conflict (`warn`, at most hourly per address). If the old owner has since been seen at another address, the switch is a reused lease and stays quiet. Any switch on the default gateway is a `warn` at once, because that is what ARP spoofing looks like. The first read on a fresh install is the baseline, and more than 20 new devices in one read (the host moved to another network) become one summary event.

### N-028 · The netwatch helper only listens, and the service trusts it by uid
The helper opens one `AF_PACKET` socket with a classic BPF filter for ARP and unfragmented UDP 67/68, so other traffic never reaches user space. It sends nothing: no DHCP discover, no ARP probes. That means it sees what clients broadcast (discover, request with the chosen server and host name, ARP) and servers' broadcast replies, but can miss unicast offers on a switched LAN. A rogue server still shows up as soon as a client accepts it, because the client's broadcast request names the server. An active probe would need the helper to send, which the brief's "no outbound" rule argues against; it can be added later as its own opt-in. Reports go over the existing socket as `net.report`, which the service accepts only from its own uid (the helper runs as the same account), so a member of the node's group can't inject fake devices or servers. The main service decides what is news; the helper keeps no state.

## N4

### N-029 · Root allows action types; the app and its rules choose when
`[actions] allow` in the root-owned config lists the action types that may run at all (none by default), with a global `max_per_hour` for real runs. Anyone who may use the socket may run an allowed action or push rules, the same trust as `alerts.rules.set`: root decides *what* can happen, the app decides *when*. Container actions use the engine socket that `allow docker` already grants (root-equivalent, as SECURITY.md says), so N4a adds no privilege. Unit restarts and power need the root helper and come in N4b.

### N-030 · The audit log is a SHA-256 chain in the node's database
Each `action_log` row stores the previous row's hash and SHA-256 over it and its own fields; `actions.log` re-checks the chain for the page it returns. This catches edits and deletions made with the database file but not a rewrite of the whole chain by someone who can write the file (the service account or root). An external anchor (the app storing the latest hash it saw, or signing) can tighten that later; the app spec asks it to keep the last hash. Refusals are logged too, so a rule repeatedly hitting a missing permission is visible.

### N-031 · Self-healing waits between runs and gives up per target
A rule acts once its condition held `forMinutes`, then waits `forMinutes` again before the next run, so a restart has time to work and health checks time to pass. Per rule and target, runs are capped per hour and day and the caps survive the condition clearing, so a flapping container can't be restarted in a loop. After `giveUpAfter` runs without the condition clearing, the rule stops for that target and warns; a cleared condition resets the count. Rules evaluate every 15 s in their own thread from the alert engine's active alerts and the container list, so they add no extra polling.

### N-032 · The root helper is socket-activated per request and trusts only root's config
`allow helper` installs `nightgrid-node-helper.socket` (owned by the service account, mode 0600, `Accept=yes`) and a template service, so there is no long-running root process: systemd starts `nightgrid-node helper` as root for one connection and it exits after one answer. The helper checks the peer is the service account or root, re-reads `/etc/nightgrid-node/config.toml` itself rather than trusting what the service says, and runs `systemctl restart|start|stop -- <unit>` only for units in `[actions] units` (full names, never the node's own) or `shutdown -r|-P +1`, never a shell. Power is scheduled a minute ahead so the service can log it and the app can show it before the host goes down, and root can cancel it. The unit keeps `NoNewPrivileges`, only `AF_UNIX`, a read-only system and only `CAP_SYS_BOOT` (so logind accepts the shutdown without polkit). Self-healing rules may not use unit or power actions until there are unit conditions to pair them with; a reboot loop from a rule is the failure we least want.

### N-033 · Rollbacks snapshot named files, and the timer lives in the service
A change from the app (a compose file, `sshd_config`, a network config) can cut the app off from the host, so the safety net has to work without it. Before the change the app names the files; the root helper copies them (with owner and mode) into its own state directory, and the service stores the change and its deadline in its database. If the app doesn't confirm in time, the service's timer, which ticks every 5 s and survives restarts because it is in the database, asks the helper to put the files back atomically (temp file and rename), removes files the change added, and runs an optional allowed unit or container action, such as restarting `ssh.service`. Root lists where this may happen in `[rollback] paths`; the helper re-checks each path after resolving its directory (so a symlinked directory can't lead outside), refuses symlinks and the node's own files, and `allow helper` turns the list into the unit's only `ReadWritePaths`. Whole-directory snapshots, and changes the app makes through `actions.run` itself, can come later; naming files keeps snapshots small and the restore exact.

## N5

### N-034 · Host health uses a separate read-only root helper and the app's own checks
SMART needs raw disk commands (`CAP_SYS_RAWIO`, and `CAP_SYS_ADMIN` for NVMe), so it can't run as the service. Rather than widen the root helper (which may restart units, reboot and write files), `allow health` installs its own socket-activated helper whose unit has only `CAP_SYS_RAWIO`, `CAP_SYS_ADMIN` and `CAP_DAC_READ_SEARCH`, a read-only file system, and network only to localhost and private LAN ranges (for `upsc` to reach a NUT server). It runs exactly the scan scripts the app runs over SSH (vendored with their parsers from app commit 8eedcf2), so readings and verdicts match the app's. Requests name a read (`smart`, `pools`, `updates`, `ups`), never a command, and `ups` takes only a target root listed in `[health] ups`. Package checks are offline by design: the node never refreshes package lists (that would be outbound network); the app's manual check over SSH stays the way to refresh. Backup status (restic/borg/rsync jobs) needs the job definitions the app keeps, so it moves to N7 with scheduled jobs, where the node will own those definitions.

### N-035 · Log watching reuses the app's detector and keeps lines in memory only
The node runs the app's `logwatch` detector (vendored) over the same inputs the app reads over SSH: journal lines of priority err and worse, and the last 500 lines of each running container since the previous round, every 10 minutes. Same patterns, same burst rule, same first-round learning, so an alert means the same thing whether the app or the node raised it. Saved searches with alerts move to the node too, so they keep firing with the app closed. Journal reads use `journalctl -o json` with a field list (no shell) and need `allow journal`; container logs go through the engine socket and need `allow docker`, so this adds no privilege. Nothing from logs is written to disk: samples live in memory, lines that look like secrets are never kept, and a saved search's alert shows a placeholder instead of such a line. To stay within the memory budget the node follows at most 64 sources and remembers 500 patterns per source (the app: 2000); the learned patterns are not persisted, so the first round after a restart learns again. Regular expressions use `regex-lite`, which matches in linear time, so a pushed pattern can't stall a round.

## N6

### N-036 · Usage comes from cgroups and /proc; RAPL needs a udev rule
Per-service and per-container numbers come from cgroup v2 (`cpu.stat`, `memory.current`, `io.stat`), which any user can read and which already add up every process in the unit or container. This needs no Docker API calls and works for Podman the same way. Container traffic comes from `/proc/<pid>/net/dev` of the container's first process, which shows its own network namespace, and is left out when it shows the host's interfaces. Per-process I/O (`/proc/<pid>/io`) is root-only for other users' processes, so processes report CPU and memory only, grouped by name, top 15. RAPL's `energy_uj` has been root-only since the PLATYPUS side channel (CVE-2020-8694); `allow power` installs a udev rule that gives the node's group read access on every boot and applies it at once, and `deny power` puts it back. The node reads it at most once a second, so it can't serve as a fine-grained side channel through the node. One-minute averages are kept 24 hours in `usage_minute`; finer data only exists live, on the stream. UPS power will come with the UPS reading once the vendored parser reads `ups.realpower`.

### N-037 · Host security watching adds no new privilege
Listening sockets come from `/proc/net/{tcp,tcp6,udp,udp6}`, which every user can read, instead of `ss` (which would also only show other users' process names to root). The cost is that `ports.list` has no process names; the app's audit over SSH still shows them. A socket counts as opened or closed after two reads agree, and unconnected UDP sockets in the ephemeral range are listed but not reported, so resolvers and short-lived clients don't fill the timeline. Login bursts reuse the journal watcher (`allow journal`) and become an alert kind the app doesn't have yet, so the rule `loginBurst` defaults to off and the app turns it on once it knows the kind; that keeps older apps from receiving an alert they can't parse. The wider file baseline (more of `/etc`, every login user's `authorized_keys`, compose files) mostly lives where the service can't read: rather than widen the service, the existing read-only health helper takes those snapshots when root has allowed it. The helper computes the paths from root's config and `/etc/passwd` itself, so the service can't use it to probe other files, and it returns metadata and a short hash only. When the helper comes or goes, the baseline is taken again silently instead of reporting every file only one of them can see as added or removed.

### N-038 · Auto-block uses a node-owned nftables table with kernel timeouts
Blocks are elements with a timeout in two sets (`v4`, `v6`) of a table only the node uses, matched by drop rules in its own `input` and `forward` chains (priority -5, so before firewalld, ufw or Docker's chains, and forwarded traffic to published container ports is covered). The kernel expires each block by itself, so a block can't outlive its duration if the node crashes, and removing the table undoes everything. That avoids editing iptables/ufw/firewalld rules the person manages, and works the same on Debian, Fedora, Bazzite and Proxmox (all nftables-based now). The root helper runs `nft` with a fixed command built from a parsed IP address and root's duration, after checking root's `[actions] allow`, the protected ranges and `[block] never` itself. Auto-block is a second opt-in (`[block] auto`) on top of allowing the action. It never touches addresses that logged in successfully in the last day, and it is capped at `[block] max_active` blocks. Blocks don't use `max_per_hour`, because a distributed attack could otherwise exhaust the budget other actions rely on.

## N8

### N-043 · The node-to-node link is a separate unit with pinned mutual TLS
Roben asked that nodes be able to talk to each other, with each node acting alone by default (2026-11-17). Opening a port is the largest change to the node's exposure so far, so it follows N-021 and N-039: the main service stays offline, and a separate unit (`nightgrid-node peer`, the service account, no capabilities) holds the key, listens and connects, with `IPAddressAllow` from root's `[peers] networks` (tailnet ranges by default). It listens on the tailnet address only, found from the interfaces at start and retried until it appears; a LAN address has to be named, and an unspecified address (`0.0.0.0`, `::`) is refused by config validation. Trust comes from the app, which already reaches every host over SSH as an admin: it reads each node's certificate fingerprint and gives it to the nodes it pairs. Both sides check the other's certificate against that list (SHA-256 of the DER), so there is no CA to run, no trust on first use and no shared secret; unpairing is removing the entry. TLS 1.3 only, rustls with ring (already in the binary for service checks), Ed25519 keys made with rcgen. Requests on the link are single JSON lines (at most 4 KiB) answered only if root's `[peers] serve` lists them, so even a paired node can't ask for more than root allows. The first request is `ping` (id, host name, version, uptime); actions across hosts come in N8c, each a new `serve` entry that root adds. Results reach the main service as `peers.report`, accepted only from the service's own account, and become events after two misses in a row so a single dropped packet doesn't page anyone.

### N-042 · Upgrades are verified by the app's checksum and revert themselves
The node has no network, so it can't fetch releases or check their provenance itself. The app downloads the release, checks its build provenance, copies the binary over SSH and runs `sudo nightgrid-node upgrade FILE --sha256 HEX` with the value from `SHA256SUMS` (signatures stay out for now, per N-014). The upgrade copies the file next to the installed binary first and hashes and runs that copy, so nothing can swap the file between the check and the install. The new binary installs itself, so its own units and drop-ins are what get written, and the old one is kept as `nightgrid-node.prev`. "Healthy" means the socket answers as the new version and the service stays active for 10 s with no restart. Otherwise the previous binary's own `install` runs, which also restores its units. A downgrade needs `--allow-downgrade`, so an old, possibly vulnerable build can't be reinstalled by mistake.

### N-041 · UPS group shutdown without a node-to-node link
Group actions were planned as "coordinated by one node". That needs nodes to reach each other, but the node opens no ports and only the optional tailnet listener (N8) would add one. For a UPS, coordination isn't needed: every host on the UPS can read it (NUT over the LAN, through the health helper), so each node follows the UPS itself and decides alone with root's limits (minutes on battery, charge, the UPS's low-battery flag). The order of a group shutdown comes from the limits (storage and the NUT server get larger ones), as with `upsmon`. A countdown is announced first, power coming back cancels it, and `ups.cancel` holds it for the rest of the outage, matching the app's group shutdown (D-112). The power-off is the ordinary `power.poweroff` action through the root helper, so it is in the action log and needs root's allow. An unreadable UPS keeps the current state: it never starts a countdown, and a running one still ends, since the NUT server may have lost power too. Staged restarts across hosts need real coordination, so they stay in the app (which already has the SSH connections) until an authenticated node-to-node transport exists.

### N-040 · Scheduled jobs stay systemd timers; the node watches them
The plan said "scheduled jobs that run with the app closed". The app already installs its schedules as systemd timers on the host (`nightgrid-job-*` for commands, M10v; `nightgrid-power-*` for power, M10k), and systemd runs them whether or not the app or the node is up, with persistence, randomized delays and its own logging. A second scheduler in the node would need root (or sudo rules per command) to run the same things, duplicate state the app edits over SSH, and stop when the node stops. So the node schedules nothing: it watches those timers and the backup jobs the app defines, read-only, as the service account. Each finished run becomes a `job` event (remembered across restarts, so runs while the node was down are reported once), backups get the app's `backupFailed`/`backupStale` alerts and status logic (vendored from `core/src/backup_jobs.rs`), and a scheduled reboot or shutdown, which takes the node down with it, is recorded at the next start from the previous boot's journal. Paths are read in Rust instead of `find`/`du` and at most every 10 minutes, with the size walk only when the newest file changes, so a large repository costs nothing most of the time. Test restores stay on request in the app (they need the repository password from the unit's environment); an app user who wants them scheduled can make a `nightgrid-job-*` of it, which the node then reports like any other job.

### N-039 · Service checks run in a separate network-limited unit
Checks are the first thing that needs outbound connections. The milestone plan said "a drop-in that allows exactly the configured addresses" on the main service, but that would give the process holding the history, the socket and the journal grant network access too. Following N-021, a separate unit (`nightgrid-node checker`, the service account, no capabilities) runs them instead, with `IPAddressAllow` built from root's `[checks] networks` (LAN, loopback and tailnet ranges by default). The app supplies what to check, root decides where checks may go. It reports over the node's socket like the network watch helper, and the main service stays offline. The check code is the app's (URL parsing, status rules, certificate capture with rustls and the platform verifier, alert conditions), vendored from 8eedcf2. The one change is a minimal HTTP/1.1 GET that reads only the status line instead of hyper. This keeps the binary at about 4.7 MB static. Alerts use the app's own keys and rules, so the app can drop its own result for a service the node already checks from inside the LAN. When the runner goes quiet, its alerts lapse instead of turning into "down".
