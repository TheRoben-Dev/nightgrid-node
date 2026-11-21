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

## Windows Server · W0 (2026-11-20)

Roben asked for a Windows Server build with feature parity and, where possible, the same commands as on Linux (2026-11-20). Plan and parity table: [WINDOWS.md](WINDOWS.md).

### N-070 · One codebase and one binary name, with Windows pieces behind `cfg(windows)`
The Linux node is the reference, and most of it (the engine, the store, alerts, uptime, sync, the protocol) is platform-neutral. So Windows is the same crate with a `win` module for the service, pipe, collector, install and doctor, and a small `compat` module that stands in for the unix-only std pieces (modes, owners, the socket stream) so shared code keeps compiling unchanged. Features that aren't ported yet are switched off with `cfg!(unix)` at the watcher, never removed, and `hello` advertises only what runs on Windows (`WINDOWS_CAPS`), so the app's capability checks do the rest. The exe is `nightgrid-node.exe` with the same subcommands and flags; what isn't on Windows yet exits with a message (and `{"error":{"code":"not_on_windows"}}` under `--json`) instead of being an unknown command.

### N-071 · A named pipe reached through `bridge`, never a TCP port
Windows has AF_UNIX sockets, but Windows OpenSSH can forward neither them nor pipes, and the app already runs `bridge` on Linux, so the pipe is the natural local endpoint. `\\.\pipe\nightgrid-node` is created with `FILE_FLAG_FIRST_PIPE_INSTANCE` (so nobody can create it first and squat on it), rejects remote clients, and has a DACL of SYSTEM, Administrators, the service SID and the local group `Nightgrid Node Users`, the counterpart of the Linux group. The caller is identified by impersonating it after the first bytes arrive: its SID is the session's uid, and an elevated administrator or SYSTEM counts as root. Clients connect with identification-level QoS (the server can't act as them) and refuse a pipe whose owner isn't the service, SYSTEM, Administrators or themselves.

### N-072 · The service runs as a virtual account with a write-restricted token and its network blocked
The Linux unit's sandbox maps onto Windows service hardening: the virtual account `NT SERVICE\nightgrid-node` (no password, not an administrator), required privileges cut to `SeChangeNotifyPrivilege`, SID type *restricted* so it can only write where its own SID is granted (its data folder), a Windows Service Hardening rule that blocks every inbound and outbound packet for the service even with the firewall profile off, and a job object with the same 64 MB memory cap. Config and binary are writable only by Administrators and SYSTEM; the data folder only by the service, Administrators and SYSTEM. Logs go to a 1 MB rotated file in the data folder, since there is no journal.

### N-073 · The collector reproduces the app's Windows sampler from raw counters
The app already samples Windows hosts over SSH with PowerShell (`core/src/win/metrics.rs`) and prints the Linux-style sections its parser reads. The node does the same from PDH raw counters, `GlobalMemoryStatusEx`, page files and volumes, in-process and without PowerShell, so a sample from a Windows node and one the app takes itself have the same names and units (`diskN`, drive letters, jiffies). WMI isn't used: a COM client in a 64 MB service is heavy, and every figure the sampler needs is a counter or an API call.

### N-074 · Windows releases sit apart in the manifest
`manifest.json` gets a top-level `windows` map (and `windowsArch` for `PROCESSOR_ARCHITECTURE`) next to `programs`, not inside it, so an app that only knows `programs` can never pick a Windows file for a Linux host. The exe is static (`crt-static`), built on a Windows runner, and listed in `SHA256SUMS` with build provenance like the Linux files.

## Windows Server · W1 (2026-11-20)

### N-075 · The Event Log is polled by record id, not subscribed to
`EvtSubscribe` needs a callback thread and a bookmark to survive restarts. The node instead asks each channel for `EventRecordID > N` every few seconds and keeps one number per channel in the store (`logins_cursor`). Nothing is lost while the service is stopped, unless the log wraps past the cursor, and a first start begins at the newest record instead of reading years of history. The rendered XML is parsed by a small reader (`evtx.rs`) that is tested on every platform.

### N-076 · `allow eventlog` is the journal grant, and Event Log Readers is the group
Reading the Security log takes membership in the builtin Event Log Readers group (S-1-5-32-573), the counterpart of `systemd-journal`. `allow eventlog` (alias `allow journal`) adds the service's virtual account to it and restarts the service. `deny` and `uninstall` take it out. The node's `journal` access reads `ok` or `denied` from its own token, so the app's existing grant flow works unchanged.

### N-077 · Security log logons count only where they mean someone came in from elsewhere
4624 type 10 (Remote Desktop) is an accepted login with method `rdp`. 4625 types 3 and 10 from another address are failed logins, so SMB and RDP password guessing feed the same burst detector as SSH. Local, service, loopback and sshd's own logons are left out, because the OpenSSH log already reports SSH with the method and key.

### N-078 · Usage reads the process list without opening processes
The service account can't open other accounts' processes, and it shouldn't need to. `NtQuerySystemInformation(SystemProcessInformation)` (what Task Manager reads) gives every process's CPU time, working set and I/O counters to any caller. The SCM's process ids map services onto them. Services that share one process are one item named for all of them, so no time is counted twice. Process CPU deltas are keyed by pid and start time, because Windows reuses pids quickly.

### N-079 · The Windows file baseline watches places that run code at start-up or let someone in
These are the Windows counterparts of `/etc/ssh`, `/etc/profile.d`, cron and `authorized_keys`: the OpenSSH config and keys, the hosts file, the all-users StartUp folder, Group Policy scripts, the machine PowerShell profiles and the Tasks folder. Owner and DACL changes aren't compared yet. Reading them needs `READ_CONTROL`, which the service lacks on most of these paths, and asking the helper (W2) for them fits better than widening the service.

## Windows Server · W2a (2026-11-20)

### N-080 · The Windows admin helper is a demand-start LocalSystem service the node may only start
Linux starts one root process per request from a socket unit. Windows has no socket activation, so the helper is a service, `nightgrid-node-helper`, that the node starts through the SCM when it has a request and that stops itself after three idle minutes. Its service DACL gives the node's service SID "query status" and "start" and nothing else: the node can't stop, reconfigure or delete it. It keeps only `SeShutdownPrivilege` and `SeChangeNotifyPrivilege`, and a Service Hardening rule blocks all its network traffic.

### N-081 · The helper's pipe is checked from both ends
The pipe's DACL lets SYSTEM, Administrators and the node's service SID in; `Nightgrid Node Users` can't open it. The helper still checks every caller's token (the node's service SID, SYSTEM or an elevated administrator) and loads the Administrators-only `config.toml` for every request, exactly like the Linux helper. The node opens the pipe only when SYSTEM or Administrators own it, so a process that squatted the name before the helper started gets nothing.

### N-082 · On Windows a "unit" is a service name
`[actions] units` takes service key names such as `W32Time` or `MSSQL$SQLEXPRESS` on Windows (letters, digits, `_ . - $`), never the node's own services. The protocol is unchanged: the app sends the same `unit.restart` with the name as `target`. Restart is stop, then start, each waiting up to a minute. Blocks, rollbacks and the local fixes aren't offered on Windows until W2b: `actions.policy` leaves them out and the node refuses them before asking the helper.

## Windows Server · W2b (2026-11-20)

### N-083 · `upgrade` on Windows checks a PE copy and watches the service's pid
The same flow as N-042: the new exe is copied next to the installed one, the copy is hashed against the app's `--sha256` and must start with `MZ`, run, and say it is `nightgrid-node`. The installed exe is kept as `nightgrid-node.prev.exe`, and the new one installs itself (the running exe is renamed aside, since Windows can't overwrite it). Healthy means the pipe answers with the new version and the service keeps the same process id for the settle time: the SCM restarts a crashed service under a new pid, which is what `NRestarts` shows on Linux. Otherwise the previous exe installs itself again. `upgrade` needs an elevated prompt, like `sudo` on Linux; it doesn't go through the helper, because the app runs it over SSH as an administrator anyway.

## Windows Server · W2c (2026-11-20)

### N-084 · Windows health runs in its own read-only helper service with a write-restricted token
Reading disk reliability counters, Storage Spaces and the Windows Update search needs more than the node's virtual account has, but nothing that writes. So `allow health` installs a second demand-start service, `nightgrid-node-health`, beside the admin helper and built from the same code: LocalSystem, only `SeChangeNotifyPrivilege`, a write-restricted service SID (it can write only where its own SID is allowed, which is its log), and a Service Hardening rule that blocks all network traffic. Its pipe and service DACLs follow N-080 and N-081. It answers the same `smart`, `pools`, `updates` and `files` requests as the Linux health socket, so the watcher, the store, the alerts and the protocol are unchanged. The disk, pool and update scripts are the app's own Windows scripts, vendored with `NODE:` notes, so the app and the node read the same fields the same way. UPS readings wait for W2d.

## Windows Server · W2d (2026-11-20)

### N-085 · On Windows a UPS on USB is the `[health] ups` target `"battery"`, read as if it were `upsc`
Windows has a HID UPS driver built in and shows the UPS as a battery, so no NUT install is needed for the common case. The health helper reads `Win32_Battery` and writes the same `key: value` lines `upsc` prints (`ups.status` OL/OB/LB/CHRG/RB, `battery.charge`, `battery.runtime`, `device.model`). The reading, its alerts, `ups.guard` and `[ups] shutdown` then work exactly as on Linux, and the protocol is unchanged. Windows' "unknown" runtime (on AC) is left out rather than shown as 136 years. A NUT server on the LAN isn't read yet, because the health helper has no network. That comes with the check runner (W3). The power-off is the W2a `power.poweroff`, so a Windows host goes down one minute after the countdown ends.

## Windows Server · W2e (2026-11-20)

### N-086 · A Windows block is a firewall rule that carries its own end time
Windows Firewall has no expiring entries like an nftables set with timeouts. So each block is one inbound rule, `Nightgrid Node block <address>`, in the group `Nightgrid Node blocks`, blocking all protocols on every profile, with `until <unix time>` as its description. The admin helper removes rules past their time every time it touches the group. The node keeps a block thread on Windows whenever `firewall.block` is allowed: it sends a helper-only `firewall.sweep` at start (for rules that ran out while it was stopped) and a minute after any block is due. A block rule beats an allow rule in Windows Firewall, so a block works whatever else is open. If the firewall is off for a profile, the block still succeeds, and its detail says it does nothing there. The same safety rules apply as on Linux, checked again on the helper's side. `deny helper` and `uninstall` remove the whole group, because without the helper nothing would end the blocks.

## Windows Server · W2f (2026-11-21)

### N-087 · On Windows a "failed unit" is a listed service that stopped with an error
systemd keeps a failed state, and the SCM doesn't. What the SCM does keep is the exit code of the last stop. A service that crashed, or exited with an error, is stopped with a code other than 0, and 1077 means it never started. So `unit-failed` rules on Windows match a service root listed that is stopped with any other code. A crashed svchost leaves exit code 0, so a listed service set to start with Windows (automatic or delayed) that is stopped counts as failed too, which is the usual Windows check for services that should be running. They name it as `[actions] units` does (`W32Time`), and disk rules name volume roots the way the disk-full alert does (`D:\`). The rest of self-healing is the same code as on Linux: waits, caps, giving up, dry run, `[actions] allow` and the action log.

### N-088 · The Windows fix menu keeps the Linux names and runs PowerShell in the admin helper
Roben asked for the same commands on Windows, so `fix` keeps its names and only shows the ones Windows has: `restart`, `packages`, `ssh`, `reboot` and `run`. `packages` means the Windows Update download cache, the one large cache Windows keeps, and `ssh` means the key-file ACLs, the usual reason OpenSSH ignores a key there. There is no `systemd-run` sandbox on Windows, so the admin helper (LocalSystem, demand-started) runs fixed PowerShell scripts with the account name checked first (no quotes reach the script). Both scripts walk with .NET and skip reparse points, because Windows PowerShell 5.1 follows junctions when it recurses. Runbooks run without a shell under the helper's time limit. `journal`, `logs` and `reset` have no Windows counterpart and are left out, not faked. `start` and `prune` come with containers.

### N-089 · Windows rollbacks replace files in place and put the saved DACL back
On Linux a rollback writes a temporary file and renames it over the original, then sets mode and owner. On Windows that would give the file the folder's inherited ACL and a new identity, so the admin helper uses `ReplaceFileW`, which keeps the original's ACL, attributes and hard links, and then applies the DACL saved at snapshot time (owner, group and DACL as SDDL), so an ACL the change loosened is tightened again. Setting another account as owner takes SeRestorePrivilege, which the helper doesn't hold and shouldn't gain for this, so the owner comes back only where the helper may set it; contents and DACL always do. Snapshots go to `%ProgramData%\Nightgrid Node Rollback`, not the node's data folder, because the node's service may read that one and snapshots can hold secrets the service otherwise can't read. Anyone may create folders in `ProgramData`, so a snapshot folder not owned by SYSTEM or Administrators is refused instead of used.

## N11 · Fix menu (2026-11-18)

### N-060 · Fixes are node-local actions on the existing action path, outside the frozen protocol
Roben asked for a terminal menu that acts on issues by the user's choice. A second way to run privileged work would need its own allow list, audit and limits, so each fix is an action like the app's: checked against root's allow list, counted in `max_per_hour`, written to the hash-chained audit log, recorded as an `action` event and carried out by the root helper. The menu sends `actions.run` over the socket like any client (its `hello` name makes the log say `menu (uid N)`). The new types are `fix.*` names the node knows and the protocol doesn't: `action::ALL` and `actions.policy` stay exactly as they were (policy lists only protocol types), so nothing frozen changes. The app may see `fix.*` names and the sources `menu (uid N)` and `autofix NAME` in `actions.log`, in fields that were always free strings.

### N-061 · The menu saves root's choices in its own `fixes.toml`, never `config.toml`
With sudo the menu offers to allow a fix, list a unit or switch a preset on, like `ujust` does. Rewriting `config.toml` would lose comments and layout (the `toml` crate doesn't keep them), so these go to `/etc/nightgrid-node/fixes.toml`, root-owned 0644, which only adds to `[actions] allow` and `units`. It also holds `journal_keep`, runbooks and automation rules. The service re-reads it when it changes (checked before each action and every 15 s); the helper reads it on each request, so the root-owned files stay the only source of what may run. A file that doesn't validate adds nothing and keeps what was loaded.

### N-062 · Fixes that write outside the helper's sandbox run as their own transient unit
The helper's sandbox (read-only system, no home) is what makes it safe to keep. Shrinking the journal, rotating logs, cleaning package caches, fixing `~/.ssh` and runbooks need writes elsewhere, so the helper starts each as `systemd-run --wait --pipe` with fixed arguments, a time limit (at most 600 s) and, where it fits, its own narrow sandbox (the journal fix may write only the journal; the SSH fix only that home, with chown/chmod capabilities only). The SSH fix works through file descriptors opened with `O_NOFOLLOW`, so a user's link can't point it elsewhere. Runbooks are argv arrays root wrote, with no shell.

### N-063 · Automation is root's opt-in presets on the self-healing engine
"Act autonomously on certain events" reuses the app's self-healing engine (waits, per-hour and per-day caps per target, give up until the condition clears, audit) with root's rules from `fixes.toml` beside the app's. A new condition, `unit-failed`, watches only units root listed. Presets: restart failed services, restart unhealthy containers, start stopped containers, and on a full disk shrink the journal, clean the package cache or remove unused images. Each is off, preview (dry runs in the log) or on, and only root changes them. The app's `heal.status` doesn't show root's rules, so its view is unchanged. Units and power stay out of the app's rules (`for_rules`), as before.

### N-064 · One terminal menu for every settings-style command, in the app's colours
`nightgrid-node` with no command, `fix`, `allow`/`deny` with no name, `serve` with no arguments and the hub's equivalents all open the same menu (`tui`): grouped, numbered, arrow keys and j/k, Enter, q, the focused entry's description underneath. It uses the NIGHTGRID theme tokens (accent #F3E600, cyan #00E5FF, magenta #FF2A6D, ok #2BFF88, warn #FF9F1C, danger #FF003C), uppercase section labels and the app's ◆ ● ▲ glyphs: truecolor when `COLORTERM` says so, else 256 colours, and none with `NO_COLOR`, `TERM=dumb` or piped output. When either end isn't a terminal (or `NIGHTGRID_PLAIN=1`) it prints a plain numbered list and reads a number or name per line, so scripts and the app's SSH runs work. It's about 500 lines with no new dependency.

## N10 · Refinement (after the app's integration work started)

### N-048 · App-facing interfaces are frozen; refinements are local
From 2026-11-17 the app is being built against the socket protocol, the app-integration specs and the link protocol, so these don't change: no method, field or meaning is renamed, removed or reinterpreted. Refinements go into internals, the command line, tests and hardening. A change the app would see is written up as a proposal in `docs/app-integration/` first. `nightgrid-node doctor` follows this: it reads files, asks systemd and calls the existing `node.info`, and its output is for people at a terminal, not a new interface.

### N-049 · CI holds the sandbox and memory budget where they are
Each unit's `systemd-analyze security` score is checked offline against a limit equal to today's score (`ci/sandbox-scores.sh`), so loosening a unit needs a reviewed change to the limit. The real-systemd test also checks the service's peak resident memory (`VmHWM`) after every feature has run, not only right after start.

### N-050 · A damaged history database is set aside, not repaired
The node's history is a convenience copy. The app keeps its own, and `metrics.sync` refills what the app missed while it was closed. A power cut or a bad sector that corrupts `node.db` used to stop every start, so the node watched nothing until someone deleted the file. Now only SQLite's own verdicts count (`SQLITE_CORRUPT`, `SQLITE_NOTADB`, or a failed `PRAGMA quick_check`). The file and its WAL are renamed to `node.db.damaged`, keeping one copy for inspection, and the new database takes the old node id if that row still reads, so the app sees the same node with a gap. Repair (`.recover`) is left to a person, because a half-recovered action log would break the audit chain silently. Errors that aren't damage, such as permissions, a full disk or a schema from a newer version, still stop the start, since a fresh database would not fix them and would hide the cause.

## Hub profile (proposed, H0)

Design: [HUB.md](HUB.md). All of these are proposals; nothing is built. Roben, 2026-11-17: keep Lite and Hub separate, with the Hub recommended as one system per network.

### N-051 · The hub is a separate binary from the same workspace and release
A profile flag in `nightgrid-node` would put the hub's code (notifier, SNMP, indexer) on every Lite host and let a config edit turn a light agent into a heavy one. `nightgrid-hub` is its own static binary, account, units and sandbox, built from the same workspace (sharing `proto` and node code), released with the same version, checksums, provenance and verified upgrade (N-014, N-042). A hub host runs Lite for itself, and the hub reads that host through the local Lite socket.

### N-052 · The person designates the hub; there is no election or broadcast discovery
An election needs all-to-all links, a quorum a two-host homelab doesn't have, and split-brain handling, and it moves a choice about which machine does more work away from the person who knows which one is always on. The app already pairs nodes as the trust anchor (N-043), so it designates the hub, refuses a second one per site, and pairs members with it. No mDNS or UDP announce, so Lite still never listens on the LAN by default. A second hub is harmless (pulls are read-only) and is reported. A standby hub (N-059) is the only supported second one.

### N-053 · Hub budget: 55 to 110 MB for the core, caps per unit, a CI budget test
Targets for 25 members on a Pi 4: main unit 40 to 80 MB (`MemoryMax=200M`), collector 10 to 20 MB (64M), notifier 5 to 10 MB (32M); later features are separate opt-in units with their own caps (index up to 256M), 150 to 250 MB with everything on. Store capped by `[store] max_gb` (default 4), pruning oldest minutes first. H1 adds a test against 25 simulated members that fails over budget.

### N-054 · The hub pulls from members over the link, read-only, keeping their cursors
Pushing would give Lite a queue, retries and a dependency on the hub. Pulling keeps Lite stateless toward clients (as for the app) and unchanged when the hub is down. The hub calls a fixed read-only list of existing socket methods through a new link request (`peer.sync`), served only if root adds `sync` to `[peers] serve` and only to the peer named by `hub.member`. It stores members' own `seq` and bucket timestamps, so the app's cursors work through the hub or directly, interchangeably, with no gaps or duplicates.

### N-055 · Hub down means Lite and the app carry on as today
Members don't notice (nothing queued), the app syncs each host directly from the same cursors, and the hub catches up from its cursors when back, a page per member per round. Silence from a dead hub is covered by the app when open, an optional heartbeat URL the notifier calls, and later a standby hub.

### N-056 · A compromised hub can read telemetry, not act: the hold rule
The link answers the hub only read methods; the hub has no SSH keys or sudo passwords. The only control message it may send is `hold {kind, until, reason}`, which can only stop a member's automatic action of a kind root allowed, expires within 24 h and is logged on the member. Where each hub unit may connect is root's per-unit list; the main unit with the data stays offline, as N-039 does for Lite.

### N-057 · Notifications reuse the app's formats; the notifier holds the secrets and nothing else
ntfy, Discord and the generic webhook use the app's own formats (D-051, vendored), plus email through an SMTP submission server with TLS required. No APNs/FCM (they need a cloud relay). The main unit renders messages and hands only text to the notifier unit, which holds `systemd-creds` secrets and is the only unit that may reach the internet, and only when root allows it for the endpoints the person chose. Folding, grouping, quiet hours and rate limits follow the app. Per site, either the hub or the app delivers, never both.

### N-058 · New app-facing surface is additive, versioned and future
No existing method, field, frame, link request or spec changes. Additions: Lite capability `hub.member` 1, link requests `peer.sync` and `peer.hold` behind new `serve` entries, and a new hub socket with its own capabilities (`fleet`, `notify`, later others). The app side is future spec 0020. The Lite pieces wait for Roben's go while the app's compatibility work runs.

### N-059 · Later features each get their own unit, opt-in and consent
LAN checks with DNS (H3), SNMP v3 read-only polling (H4), deterministic baselines and forecasts with no trained model (H5), a log index in SQLite FTS5 that needs root's consent on each member before any line leaves it (H6), and per-host runbooks, backup verification and patch windows that act only through Lite's existing typed action path, with the hub limited to holds (H7). The standby hub comes with H7 unless availability matters sooner.

## N9

### N-047 · Wake-on-LAN stays with the app's relay hosts
The app's roadmap lists "a Wake-on-LAN relay for other LAN devices" among the node's uses. The app already has one that works with the app closed: any saved host can be another's wake relay, the WAKE button runs `wakeonlan` (or a Python one-liner) there over SSH, and scheduled wake-ups are `nightgrid-wake-*` systemd timers on the relay (D-095, D-098), which run without the app or the node. A node relay would have to send a UDP broadcast, which means giving network access to a unit that has none (the main service, the root helper) or a send path to one that only listens (the network watch helper, N-021), to replace something that already works. So the node adds no wake path. If a relay host lacks both `wakeonlan` and Python, a `nightgrid-node wake MAC` command for the app's timer to call is the small change to make then, as a plain command run by the timer, not through the service.

### N-046 · The daily summary is facts made on request, not a model on the node
DRIFT is a small local model in the app, and its daily summary works from plain facts the app already has (`summaryFacts`, D-108, M10p). What the app lacks is the time it was closed. So the node adds facts, not prose: `summary.daily` turns the node's own timeline and current state into the same kind of lines (each starts with what it is about, repeats are counted, at most six items a line, relative times), which keeps them short enough for a 4B model and grounded in records the person can check in the timeline. Nothing runs on a schedule: the summary is made when the app asks, from data the node keeps anyway, so it costs nothing while nobody asks and is never stale. It needs no new access and adds no network: the app reads it over the same socket as everything else, and the node never runs a model or sends text anywhere. Successful logins name users and addresses as the timeline already does; failed logins are a count. Package updates come from the health helper's cached reading only, and the summary says "from the package cache", since the node never refreshes package lists.

## N8

### N-045 · The status page is plain HTTP on the tailnet behind a secret link
The design asked for an optional read-only page for a phone, off by default, with authentication. A phone browser can't do the link's pinned mutual TLS, and a self-signed HTTPS certificate means a warning the person learns to click through. Tailscale and Headscale already encrypt and authenticate every packet on the tailnet with WireGuard, so the page is plain HTTP that listens only on the tailnet address (or loopback, for a reverse proxy such as `tailscale serve`), which config validation enforces, and systemd lets in only `[statuspage] networks`. Within the tailnet, the page sits at a secret path of 256 random bits that `allow statuspage` writes to a file only root and the service's group can read, and prints as a link the person bookmarks; anything else is a 404 after a short delay, and the comparison doesn't stop at the first difference. A path token can end up in the phone's history, so the page has no outbound links and sends `Referrer-Policy: no-referrer`, and `deny statuspage` revokes it. Running `allow statuspage` again prints the same link. It is a separate unit like the link (N-043), as the service account with no capabilities, reading the node's socket as the app does, with no scripts, forms or writes. A page that controls things (cancel a UPS countdown, restart a container) would need real sign-in and CSRF protection, so it stays in the app.

### N-044 · UPS group shutdown follows the UPS host; staged restarts and alert relay stay in the app
With the link (N-043), a host that can't read the UPS (no NUT client, or not allowed to reach the NUT server) can follow one that can. The UPS host's guard runs as before (N-041); when it starts its countdown, its link tells every paired peer at once and repeats it every minute. A peer acts only when three switches agree: root's `[peers] serve` lists "ups" and `[ups] shutdown` is on (root's consent on the follower), and the app marked that peer `followUps` (which UPS host it is on). The follower then runs its own countdown with its own `countdown_secs` and the usual action path, so it is in its action log and timeline and its own dry run applies. Like the app's group shutdown (D-112), followers should go first and the UPS host last, so the UPS host gets the longer countdown. The leader's power returning or a cancel there ends the followers' countdowns; a follower's own on-line reading doesn't, since it may be on another feed or read a stale UPS. A follower's countdown keeps running with no further word, because the UPS host may be gone already. A follower doesn't pass a shutdown on, so there are no chains or loops. The link checks root's `serve`, then the node checks it again together with the pairing and `followUps`, with the peer identified by the certificate it presented, not by anything it sends. Staged restarts across hosts were also planned for N8c, but they are something a person starts, and the app already does them over SSH with the person watching; doing them on the link would give peers a "restart this" request with little gain, so they stay in the app. Relaying alerts to a peer has nowhere to deliver while the app is closed (nodes don't send notifications, by design), so the app keeps collecting alerts from each host when it reconnects; the link's reachability events already cover a host that is gone.

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
