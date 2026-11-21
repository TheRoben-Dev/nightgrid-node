# Changelog

## 0.37.1 · Arrow keys in menus (2026-11-21)

Fix only: no frozen method, field, frame or link request changed.

- **Arrow keys no longer quit the menus.** Every full-screen menu (`nightgrid-node fix`, hub settings and the rest of `tui::pick`) closed on the first ↑ or ↓ as if Esc had been pressed, which left a blank screen in some terminals. Rust's buffered stdin read the whole `ESC [ B` at once, so the check for "more bytes after Esc" asked an empty fd 0 and saw a lone Esc. The menu now reads the terminal (fd 0, or the console handle on Windows) without that buffer and parses escape sequences from its own 16-byte buffer, waiting only when an Esc is the last byte it has. A lone Esc still backs out.
- New tests cover an arrow whose bytes are already read, several keys in one read (a paste or key repeat, PgDn), and a sequence split across reads.

## 0.37.0 · W2g: the fix menu on Windows (2026-11-21)

Additive only: no frozen method, field, frame or link request changed, and no new cap. App spec [0032](docs/app-integration/0032-windows-node-fixes.md). Decision N-088.

- **`nightgrid-node fix` on Windows**: the same menu, `fix list [--json]`, `fix NAME [TARGET] [-y]` and `fix auto`, run through `actions.run` and the admin helper, so `[actions] allow`, the rate limit and the action log apply. An elevated session can allow a fix or switch a preset on, saved in `fixes.toml`.
- **Windows fixes under the Linux names**: `restart` (a listed service), `packages` (empties the Windows Update download cache with Windows Update and BITS stopped, skipping links), `ssh` (resets the ACLs of `administrators_authorized_keys` and the user's `.ssh\authorized_keys` to what OpenSSH accepts), `reboot` (`shutdown /r`, cancel with `shutdown /a`) and `run` (a runbook).
- **Runbooks on Windows** name a `C:\...` program, run with no shell and their time limit.
- **Detection on Windows**: failed listed services, volumes over 90 %, and a pending reboot.
- **`fix auto` on Windows** offers the presets that apply: restart failed services, and clean the update cache when a disk fills.
- The Windows CI end-to-end test lists the fixes, runs a runbook and the SSH fix from the CLI (checking the key file's ACL), and cleans the update cache through `actions.run`.

## 0.36.0 · W2f: self-healing on Windows (2026-11-21)

Additive only: no frozen method, field, frame or link request changed. A Windows node now also advertises `heal`. App spec [0031](docs/app-integration/0031-windows-node-heal.md). Decision N-087.

- **Self-healing on Windows**: the app's rules (`heal.rules.set`) and `fixes.toml` autofix rules run as on Linux, through `[actions] allow` and the action log.
- **`unit-failed` on Windows** matches a listed service that stopped with an error (a non-zero exit), or one set to start automatically that isn't running (a crash), and restarts it with `unit.restart`.
- `fixes.toml` rules name Windows services (`W32Time`) and volume roots (`D:\`) on Windows.
- A health read that gets no answer at all (the helper was just starting or stopping) is asked once more instead of leaving that reading empty until the next round.
- The Windows CI end-to-end test kills a listed service (where it has its own process) and checks that an autofix rule brings it back.

## 0.35.0 · W2e: blocking addresses on Windows (2026-11-20)

Additive only: no frozen method, field, frame or link request changed. A Windows node now also advertises `block`. App spec [0030](docs/app-integration/0030-windows-node-blocks.md). Decision N-086.

- **`firewall.block` / `firewall.unblock` on Windows**: an inbound Windows Firewall rule per address in the group `Nightgrid Node blocks`, for root's `[block] minutes`. The same refusals as on Linux apply (private, local and VPN addresses, `[block] never`).
- **Auto-block after failed logins on Windows**: failed SSH, RDP and network logons from one address past `[block] threshold` block it, with the same rules as on Linux.
- Expired rules are removed by the admin helper: the node asks once a minute after a block is due, and at start.
- `deny helper` and `uninstall` remove every block rule.
- The Windows CI end-to-end test blocks and unblocks a documentation address, checks the rule, sees a private address refused and an expired rule removed, and checks no rule is left after `uninstall`.

## 0.34.0 · W2d: UPS on USB and the UPS shutdown guard on Windows (2026-11-20)

Additive only: no frozen method, field, frame or link request changed. A Windows node now also advertises `ups`. App spec [0029](docs/app-integration/0029-windows-node-ups.md). Decision N-085.

- **`[health] ups = ["battery"]` on Windows** reads a UPS on the host's USB port (Windows shows it as a battery) through the health helper: on battery, low battery, replace battery, charge, runtime and model, in the same reading as NUT's.
- **`[ups] shutdown` on Windows**: the guard follows that reading and powers the host off through the admin helper, like on Linux.
- A NUT target on Windows says it isn't read yet; NUT servers come with the check runner.
- The Windows config template has a `[health]` section with `ups = []`.
- `deny` and `uninstall` now mark a Windows helper for deletion before stopping it, so a running node can't start it again in between, and a helper asked to stop during a long read (a Windows Update search can take minutes) ends that read at once.
- The Windows CI end-to-end test now reads `"battery"` and checks `ups.guard`.

## 0.33.0 · W2c: Windows health (disks, Storage Spaces, Windows Update) (2026-11-20)

Additive only: no frozen method, field, frame or link request changed. A Windows node now also advertises `health`. App spec [0028](docs/app-integration/0028-windows-node-health.md). Decision N-084.

- **`allow health` / `deny health` on Windows**: the service `nightgrid-node-health` (LocalSystem, demand start, write-restricted, no privileges beyond traversal, no network). The node may start it and nothing more; it stops itself after three idle minutes. `uninstall` removes it and `doctor` has a `health` finding.
- **Disks**: physical disk health, operational status, temperature, wear, power-on hours and read/write errors from the Storage module, reported as `smart.report` like on Linux.
- **Pools**: Storage Spaces pools and their virtual disks as `pools.report`, with the new pool kind `storageSpaces`.
- **Updates**: pending Windows updates (security ones marked) and pending restarts as `updates.report`, with the new manager `windowsUpdate`. Hidden updates are left out.
- Refusals for actions that aren't allowed now name the Windows config path on Windows.
- The Windows CI end-to-end test now allows health, checks the service's account, token, privileges and hardening rule, waits for disks and pools in `health.latest`, and checks the service is gone after `uninstall`.

## 0.32.0 · W2b: `upgrade` on Windows (2026-11-20)

Additive only: no frozen method, field, frame or link request changed. App spec [0027](docs/app-integration/0027-windows-node-upgrade.md). Decision N-083.

- **`nightgrid-node upgrade FILE --sha256 HEX` on Windows**, with the same report as on Linux. It hashes a staged copy, checks that it is a Windows exe that runs and says it is `nightgrid-node`, and refuses downgrades without `--allow-downgrade`. It keeps the installed exe as `nightgrid-node.prev.exe` and lets the new one install itself. If the service doesn't answer with the new version, or doesn't keep the same process for 10 seconds, the previous exe installs itself again.
- `uninstall` also removes the kept and staged exes.
- The Windows CI end-to-end test now checks that a wrong checksum is refused and leaves nothing behind, and that a verified upgrade keeps the node id.

## 0.31.0 · W2a: Windows admin helper, service and power actions (2026-11-20)

Additive only: no frozen method, field, frame or link request changed (the frozen protocol test passes as it was). A Windows node now also advertises `actions`. App spec [0026](docs/app-integration/0026-windows-node-w2a.md). Decisions N-080 to N-082.

- **`allow helper` / `deny helper` on Windows**: the service `nightgrid-node-helper` (LocalSystem, demand start, only the shutdown privilege, no network). The node may start it and nothing more; it stops itself after three idle minutes. `uninstall` removes it, `install` stops it before replacing the binary, and `doctor` has a `helper` finding.
- **Service actions on Windows**: `unit.restart`, `unit.start` and `unit.stop` on services listed in `[actions] units` (service names such as `W32Time`), through the Service Control Manager.
- **Power actions on Windows**: `power.reboot` and `power.poweroff` run `shutdown.exe` a minute ahead with a planned reason code; an administrator cancels with `shutdown /a`.
- Blocks, rollbacks and the local fixes are left out of `actions.policy` on Windows, and refused, until W2b.
- The pipe client now honours read timeouts on Windows, so a helper or node that stops answering can't hold a request forever.
- The Windows CI end-to-end test now allows the helper, checks its account, privileges, DACLs and hardening rule, restarts `W32Time` through `actions.run`, sees an unlisted service refused, and denies the helper again.

## 0.30.0 · W1: Windows logins, logs, ports, LAN, usage and files (2026-11-20)

Additive only: no frozen method, field, frame or link request changed (the frozen protocol test passes as it was). A Windows node now also advertises `logins`, `logs`, `security`, `net`, `usage` and `files`. App spec [0025](docs/app-integration/0025-windows-node-w1.md). Decisions N-075 to N-079.

- **Event Log reader** (`wevtapi`): channels are polled by record id, with one cursor per channel kept in the store, so nothing is missed while the service is stopped.
- **`allow eventlog`** (alias `allow journal`) and `deny eventlog`: add the service to Event Log Readers, or take it out, and restart the service. `doctor` has an `eventlog` finding, and `node.info` access `journal` reads `ok` or `denied`.
- **Logins on Windows**: SSH logins and failures from `OpenSSH/Operational`, Remote Desktop logons (method `rdp`), and failed network logons from other addresses (RDP and SMB guessing) from the Security log. All of them feed the same burst alerts as on Linux.
- **Logs on Windows**: the System and Application logs stand in for the journal. Providers are the identifiers, messages are formatted by the provider, and levels map to journal priorities. Service crashes and unexpected shutdowns show up as errors there.
- **Ports on Windows**: TCP and UDP listeners for v4 and v6, each with its owning process's name. Ephemeral UDP is skipped from 49152 up.
- **LAN devices on Windows**: the neighbour table and the default gateway from IP Helper, with interfaces named by their alias. The wire watch (rogue DHCP) stays off, because it would need Npcap.
- **Usage on Windows**: CPU, working set and I/O per service and for the busiest process names, read from the system process list without opening any process. Services that share a process are named together.
- **Watched files on Windows**: the defaults are the OpenSSH server's config, administrators' keys and the hosts file. The baseline adds start-up folders, Group Policy scripts, the machine PowerShell profiles, scheduled tasks and users' `authorized_keys`.
- The Windows CI end-to-end test now also checks failed network logons becoming login events, ports with processes, the gateway, usage per service, and a hosts-file change becoming a file event.

## 0.29.1 · Memory: answers written straight to text (2026-11-20)

- **Lower memory with a full history.** The node built every answer as an in-memory JSON tree before sending it, about eight times the size of the text: a 5 000-point `metrics.sync` page (1.6 MB) briefly took about 13 MB. The release builds' musl allocator keeps such freed memory fragmented, so RSS climbed with each big sync, to 37 to 55 MB on hosts in use and 59 MB in a test with 260 full syncs. It was not a leak (valgrind finds nothing lost, and RSS falls back between peaks). Answers are now written straight to text; the same test ends at about 12 MB with a 14.5 MB peak (11 to 18 MB on 32-bit).
- **Hub:** `fleet.sync` and `fleet.events.sync` pass the stored rows through as text instead of parsing them into trees first.
- **Wire:** the same fields and values. Two textual differences: keys come in declaration order instead of alphabetical, and 32-bit float values use their shortest form (`18.097475` instead of `18.097475051879883`). JSON parsers, including the app's, read both the same way. The frozen protocol test passes as it was.
- **CI:** a new e2e test builds 14 days of history and 20 000 events, runs the app's full sync 31 times and fails if the peak RSS passes 24 MB (48 MB in debug builds) or RSS grows by more than 10 MB after the first sync. It runs natively and on the 32-bit musl targets.
- **Docs:** the memory figures in DESIGN.md and HUB.md are measured again (5 to 8 MB fresh, 10 to 18 MB with a full history).

## 0.29.0 · W0: Windows Server node (2026-11-20)

Additive only: no frozen method, field, frame or link request changed (the frozen protocol test passes as it was). Plan and parity table: [docs/WINDOWS.md](docs/WINDOWS.md). App spec [0024](docs/app-integration/0024-windows-node.md). Decisions N-070 to N-074.

- **`nightgrid-node-vX.Y.Z-x86_64-windows.exe`**: a static 64-bit build for Windows Server, tested in CI on 2022 and 2025 (2019 and Windows 10/11 use the same APIs), in `SHA256SUMS` and in `manifest.json` under the new top-level `windows` and `windowsArch` keys.
- **Windows service** `nightgrid-node` as the virtual account `NT SERVICE\nightgrid-node`: write-restricted token, only `SeChangeNotifyPrivilege`, all network traffic blocked by a Windows Service Hardening rule, a 64 MB job-object memory cap, restart on failure, logs in a rotated file in the data folder.
- **Named pipe** `\\.\pipe\nightgrid-node` instead of the socket: first instance only, remote clients rejected, readable by Administrators and the local group `Nightgrid Node Users`. Callers are identified by their token; clients check the pipe's owner. `bridge` works over Windows OpenSSH exactly as on Linux, including the `no_access` and `not_running` errors.
- **Native collector**: CPU, cores, memory, page files, volumes, physical disks, network interfaces, thermal zones and NVIDIA GPUs from PDH raw counters and Win32 calls, in the same shape as the app's own Windows sampler. History, uptime with outages, disk usage and forecasts, alerts, events and `summary.daily` work as on Linux.
- **Same commands**: `install [--user NAME] [--no-start] [--json]` (to `%ProgramFiles%\Nightgrid Node`, on the machine `PATH`, config and data under `%ProgramData%\Nightgrid Node`), `uninstall [--purge]`, `doctor`, `status`, `events`, `audit`, `query`, `watch`, `bridge`, `version`, `man` (plain text on Windows) and `completions`. Commands that come in later Windows milestones exit with a message, and `{"error":{"code":"not_on_windows"}}` under `--json`.
- **`completions powershell`** on every platform.
- CI builds, lints and tests on real Windows Server 2022 and 2025 runners and runs an end-to-end install, bridge, sandbox, outsider-refused, crash-restart, upgrade-in-place and uninstall test (`ci/windows-e2e.ps1`). The release workflow builds the exe on Windows.

## 0.28.0 · Old and 32-bit hosts, LAN links, hub doctor (2026-11-18)

Additive only: no frozen method, field, frame or link request changed (the frozen protocol test passes as it was). App specs [0022](docs/app-integration/0022-legacy-hardware-and-releases.md) and [0023](docs/app-integration/0023-lan-links-and-hub-doctor.md).

- **New release arches** for the node and the hub: `armv7` (armv7l, armv8l), `armv6` (Raspberry Pi 1 and Zero) and `i686`, all static musl, in `SHA256SUMS`. CI runs the unit and e2e tests on i686 and on armv7 under qemu, and runs install, doctor and a hub pairing in a Debian 12 armhf systemd container under qemu.
- **`manifest.json`** in every release: version, protocol range, and per program and arch the file, size and SHA-256, plus the `uname -m` map. Listed in `SHA256SUMS`, with build provenance; not signed (checksums and provenance, as decided).
- **Old systemd** (215 and later, such as ReadyNAS OS 6): units are fitted to the running version. Unknown settings are commented out with the version they need, older spellings are used where they exist (`MemoryLimit=`, `ProtectSystem=full`), and state directories are made by `install` below 235. `install --json` lists what was left out (`unsupported`), and `doctor` shows it (`sandbox`).
- **Hosts without systemd**: `install` and `uninstall` work on OpenRC and on sysvinit with `start-stop-daemon` (BusyBox too), with the same account, socket, modes and history. The account is made with `useradd` or BusyBox `adduser`. `allow`, `deny`, `serve`, `upgrade` and the hub refuse with `{"error":{"code":"no_systemd",…}}` under `--json`. `dist/uninstall.sh` handles these hosts too.
- **Host facts**: `install --json` and `doctor --json` carry `arch`, `assetArch`, `libc`, `init`, `initVersion`, `kernel`, `busybox` and `limits` (no cgroup v2, no RAPL, no systemd). `doctor` adds the `host`, `sandbox` and `limits` findings, and checks the service through its init script on non-systemd hosts.
- **JSON errors**: with `--json`, failed `install`, `uninstall`, `allow`, `deny`, `serve` and `upgrade` (node and hub) print `{"error":{"code":"no_systemd"|"not_root"|"failed","message"}}` on stdout.
- **`bridge`** (node and hub) prints `{"error":{"code":"no_access"|"not_running","message"}}` when it can't reach the socket, so the app can run the bridge first and probe only on failure.
- **32-bit audit**: statvfs sizes are computed in `u64` (disks over 4 GiB were already right on 64-bit, now on 32-bit too), local times after 2038 fall back to UTC on 32-bit musl instead of truncating, and new tests cover counters across 2^32, disks over 4 GiB and dates past 2038. The Docker client sends a request in one write (a partial read showed up under qemu).
- **`[peers] listen = "lan"`**: the link listens on this host's LAN address (the route to the first non-tailnet `[peers] networks` entry, or the default route), never on every interface. It is looked up every 30 s and follows a new DHCP lease. `peers.latest.listening` shows it. No `ip` command needed.
- **Hub: `[collect] listen` takes a list** (up to 8 entries: `tailnet`, `lan`, `ip:port`). `fleet.overview` and `allow collect --json` add `listenAll`; `listening` stays the first.
- **`nightgrid-hub doctor [--config PATH] [--json]`**: host, install, node, init, config, both units, socket, collector (on every `listen` entry), members and store, in the node doctor's shape.
- **`events.sync` takes `kinds`** (node and the hub's `fleet.sync`); `next` moves past skipped events. PROTOCOL.md now also states how `subscribe`/`unsubscribe` combine, that an empty page has `next == after`, and that alert kinds stay camelCase.
- Releases are tagged and published by CI from `main` when `Cargo.toml` has a new version (`auto-release.yml`).

## 0.27.0 · N11: fix menu and opt-in automation (2026-11-18)

- `nightgrid-node fix`: a ujust-style menu that lists what looks wrong on the host (failed units, unhealthy or stopped containers, disks at 90% or more, a journal over 2 GB, a pending reboot, loose `~/.ssh` permissions), every fix grouped by kind with whether root allows it, and automation. Each fix explains what it runs and asks first; a reboot needs the word typed.
  - Short commands: `fix list [--json]`, `fix restart|reset|start|journal|logs|packages|prune|ssh|reboot|run [TARGET] [-y]`, `fix auto [NAME on|preview|off]`.
  - Node-local action types `fix.unit-reset`, `fix.journal-vacuum`, `fix.logrotate`, `fix.package-cache`, `fix.ssh-permissions`, `fix.runbook`, all through the root helper, which runs the ones that write outside its sandbox as short transient units (N-060, N-062). The helper unit's `RuntimeMaxSec` is now 630 s; run `sudo nightgrid-node allow helper` again after upgrading to pick it up.
  - Root's `/etc/nightgrid-node/fixes.toml`, written by `sudo nightgrid-node fix` and safe to edit: extra allow and units, `journal_keep`, runbooks and automation rules; `config.toml` is never rewritten (N-061).
  - Automation presets on the self-healing engine, off by default, root only: `services` (a new `unit-failed` condition, listed units only), `containers`, `stopped`, `journal`, `packages`, `images`, each off, preview or on (N-063).
- One terminal menu for every settings-style command, in the Nightgrid app's colours and glyphs: `nightgrid-node` and `nightgrid-hub` on their own, `fix`, `allow`/`deny` with no name, `serve` with no arguments. Plain numbered lists with `NO_COLOR`, `NIGHTGRID_PLAIN=1` or piped output (N-064).
- Protocol: unchanged (the frozen protocol test passes as it was). `actions.policy` lists only protocol action types; `actions.log` may now carry `fix.*` actions and the sources `menu (uid N)` and `autofix NAME`.

## 0.26.1 · install fix (2026-11-18)

- `install` (node and hub) now creates its directories as 0755 whatever the umask of the shell that ran sudo. Under a strict umask (0077 or 0027) 0.26.0 created `/etc/nightgrid-node` (or `/etc/nightgrid-hub`) closed to the service account, so the service exited with `config.toml: Permission denied` and restarted in a loop.
- `nightgrid-node serve add|remove` no longer fails with `Unit nightgrid-node-peer.service not found` when the node-to-node link is off. It restarts only the node and says to run `sudo nightgrid-node allow peers` (the config change itself was already saved).
- App spec 0021: the app shows "installed, not running" right after install, because its probe can't see the socket through the 0750 runtime directory until the SSH login has the new group. App-side fix proposed; no node change.
- README: the hub steps now include `allow peers` on each member, the tailnet requirement, and how to pair a hub and its members by hand until the app does it.
- Running `install` again on a host installed with 0.26.0 opens that directory (0755) and says so. By hand: `sudo chmod 755 /etc/nightgrid-node && sudo systemctl restart nightgrid-node`.

## 0.26.0 · H1: the hub (2026-11-18)

First published release. It includes everything below; 0.1.0 to 0.25.1 were never published on their own.

- New, optional binary `nightgrid-hub` for one always-on host per network ([docs/HUB.md](docs/HUB.md)). It needs Nightgrid Node on the same host and changes nothing on hosts that don't opt in.
  - `install`, `uninstall [--purge]`, `allow collect`, `deny collect`, `status`, `query`, `bridge`. Two units: `nightgrid-hub` holds the store and the socket `/run/nightgrid-hub/hub.sock` and has no network at all; `nightgrid-hub-collect` holds the link key and may reach only `[collect] networks` (tailnet ranges by default).
  - The collector pulls each member read-only over the existing mutual-TLS link: metrics (minute, quarter-hour and hour), events, uptime, disk usage, active alerts and `node.info`, keeping the member's own `seq` numbers and timestamps, so `fleet.sync` answers exactly as the member would and the app's cursors work both ways.
  - Socket capability `fleet` 1: `fleet.members.get` / `fleet.members.set` (set only by the app), `fleet.overview`, `fleet.sync`, `fleet.events.sync` (the hub's own events: "host offline" as a `hostOffline` alert, "back online", store at its cap).
  - Store limits: 14 days of minutes, 90 days of quarter-hours, 400 days of hours, a 4 GB cap. Memory limits: 200 MB for the main unit, 64 MB for the collector. A CI test pulls 25 simulated members with two days of minutes each over real TLS in under a second, growing memory by about 6 MB.
- Lite, additive only (the frozen protocol test shows new lines, no changed ones):
  - `hub.member.get` / `hub.member.set {hubId}` (capability `hub.member` 1): which paired peer is this node's hub. Only a paired peer can be named.
  - The link answers `peer.sync {method, params}` from that hub only, only if root lists `sync` in `[peers] serve`, and only for `node.info`, `metrics.sync`, `events.sync`, `uptime.sync`, `disk.sync` and `alerts.active`. Answers are capped at 1 MiB.
  - `nightgrid-node serve add|remove ping|ups|sync` (root) edits `[peers] serve` and restarts the node and its link, so the app can do it through its sudo flow.
- Release assets now include `nightgrid-hub` for both architectures, in the same `SHA256SUMS` and build provenance.

## 0.25.1 · N10 refinement

- The minimum supported Rust is 1.89, which the locked `rusqlite` 0.41 requires. The old 1.80 was never checked, and a CI job now builds with 1.89.
- CI also runs `shellcheck` on the install, test and uninstall scripts, and `cargo audit` against the RustSec advisory database for the locked dependencies.

## 0.25.0 · N10 refinement

- `nightgrid-node status` is now a readable summary: host, version, uptime, CPU, memory, swap, disks, active alerts and which access is on. `status --json` prints the raw `node.info` as before.
- `nightgrid-node events [--count N]`: the last N events (30 by default), oldest first, in local time. `--json` gives one event per line.
- `nightgrid-node audit [--count N]`: the last N action-log entries. `audit verify` walks the whole log, checks every SHA-256 link and exits 1 with the broken range if a row was edited or removed.
- `nightgrid-node completions bash|zsh|fish` and `nightgrid-node man` (a manual page made from the help: `nightgrid-node man | man -l -`).
- The command-line tools no longer panic when their output pipe closes early (`nightgrid-node events | head`).
- These use only methods the app already uses (N-048).
- A damaged history database no longer keeps the service in a restart loop. If SQLite says `node.db` is not a database or fails its quick check at start, it is moved to `node.db.damaged` (replacing an older one), a new one starts with the same node id when that row still reads, and a "history database replaced" warning event says so. Other open errors, such as permissions, a full disk or a newer schema, still stop the start as before (N-050).
- A test pins the app-facing protocol: method names, capabilities, streams, error codes and the fields each method answers with (`crates/node/tests/frozen-protocol.txt`). Any change to it fails CI until the file is regenerated on purpose and the diff reviewed (N-048).
- The database's write-ahead log is trimmed back to 4 MiB after checkpoints instead of keeping its largest size.

## 0.24.0 · N10 refinement

- `nightgrid-node doctor` (`--json` too): a read-only self-check any user can run. It checks the installed binary, that `/etc/nightgrid-node/config.toml` parses and validates, that the service is active, that the socket answers (with a hint when the user isn't in the node's group), that the running version matches this binary, resident memory against the 20 MB budget, history size, and for each `allow` grant whether it is on, its unit runs and the node sees the access. It exits 1 on a failure. Nothing in the app protocol changed (N-048).
- CI: every unit's systemd sandbox score is held at today's level (`ci/sandbox-scores.sh`), the real-systemd test checks peak memory after every feature ran and runs `doctor`, and a test feeds every protocol method wrong-shaped params (never a panic or an internal error) (N-049).

## 0.23.0 · N9

- `summary.daily` (`{hours}`, 1–168, default 24): what happened on this host, as short plain facts for the app's DRIFT helper: outages and restarts, active and resolved alerts, container trouble, failed and finished jobs, actions run, logins and failed logins, changed files, new listeners and LAN changes, upgrades and other warnings, disks filling up (with the forecast), and pending updates from the package cache. Each line starts with what it is about, repeats are counted, lists are capped, and times are relative. `level` is the worst thing in it, and `complete` is false (with a "Coverage" line first) when the node hasn't watched the whole window.
- Made on request from the node's own history; nothing is sent anywhere. Capability `summary` 1 (N-046).

## 0.22.0 · N8d

- An optional read-only status page for a phone on the tailnet. `sudo nightgrid-node allow statuspage` installs `nightgrid-node-status.service` (the service account, no capabilities, reachable only from `[statuspage] networks`) and prints the page's secret link, `http://<tailnet address>:7420/<token>`. Running it again prints the same link; `deny statuspage` removes the page and the token, so the link stops working.
- The page shows the host's state (CPU, memory, load, uptime, disks), active alerts, the UPS guard and linked hosts, refreshes every 30 s, has no scripts or forms and changes nothing. Light and dark follow the phone. It reads the node at most every 10 s.
- Plain HTTP, so it listens only on the tailnet address (WireGuard encrypts it) or loopback; config validation refuses anything else. Any other path is a 404, and the token (256 random bits, in `/etc/nightgrid-node/status-token`, root and the service's group) is compared in constant time (N-045).

## 0.21.0 · N8c

- UPS group shutdown over the node-to-node link (opt-in on each follower): when the host that reads the UPS starts its shutdown countdown, it tells its paired peers right away (and again every minute while it lasts). A peer the app marked `followUps`, whose root lists `"ups"` in `[peers] serve` and turned on `[ups] shutdown`, starts its own countdown with its own `countdown_secs`, so followers go first and the UPS host last, as in the app's group shutdown. Power coming back or a cancel on the UPS host ends it on the followers too; a cancel on a follower holds only that follower.
- A follower needs no NUT client or `[health] ups` of its own, only the root helper and `power.poweroff` in `[actions] allow`. Its countdown ends on time even if the UPS host is already gone. A follower's own "on line power" reading doesn't end a shutdown it follows.
- `ups.guard` gains `ledBy` (the peer followed); `peers.set` entries gain `followUps`. Requests on the link that need the node (`peer.ups`) go to it as internal `peers.inbound`, which checks root's `serve`, the pairing and `followUps` again (N-044).

## 0.20.0 · N8b

- An opt-in node-to-node link (each node still acts alone by default). `sudo nightgrid-node allow peers` installs `nightgrid-node-peer.service`: the service account, no capabilities, network access only to `[peers] networks` (tailnet ranges by default). It makes an Ed25519 key and self-signed certificate on first start (mode 0600, in `/var/lib/nightgrid-node-peer`) and listens on this host's tailnet address (`[peers] listen`, port 7419), never on every interface.
- Pairing goes through the app: it reads each node's `peers.latest.fingerprint` and gives it to the other with `peers.set` (id, name, address, fingerprint). Connections are mutual TLS 1.3 and both sides accept only the certificates the app paired; there is no CA and no trust on first use. A node answers only what root lists in `[peers] serve` (`ping` for now).
- Peer reachability: every `interval_secs` (60) the link pings each peer (version, host name, round trip) and reports to the node; `peers.latest` shows the readings. A peer that misses two pings in a row is a "peer nas unreachable" event (`warn`), and "reachable again" when it answers. A changed list from the app takes effect within 10 s, and a peer that missed is tried again after 15 s.
- `peers.set`, `peers.get`, `peers.latest`, internal `peers.report` (the link's account only); capability `peers` 1; `node.info.access.peers`. `deny peers`, `uninstall --purge` and `dist/uninstall.sh` remove it (N-043).

## 0.19.0 · N8a

- `sudo nightgrid-node upgrade FILE --sha256 HEX`: installs a new binary only when its SHA-256 matches (the app passes the value from the release's `SHA256SUMS`). The check runs on a staged copy next to the installed binary, so the file that runs is the file that was hashed. The new binary must run here, say it is nightgrid-node, and not be older (`--allow-downgrade` to go back).
- After the new binary installs itself (its own units and drop-ins), the upgrade waits for the socket to answer with the new version and for the service to stay active for 10 s without a restart. Otherwise it reinstalls the previous binary, kept as `/usr/local/bin/nightgrid-node.prev`, and checks that one too. `--json` reports `ok`, `reverted`, the steps and the error.
- Fix: with a sensor read that hangs (some NVMe drives), a sample can take longer than the interval, and the service then missed its stop request and only stopped when systemd killed it.
- Fix: the service's task limit (`TasksMax`) goes from 16 to 64. With most watchers on, short-lived reads could hit the limit, and a watcher thread that failed to start a helper thread stopped (seen as log alerts and saved searches that never fired).
- The timeline records "upgraded to 0.19.0" or "upgrade to 0.19.0 failed" (internal `upgrade.note`, root only). `uninstall` also removes the previous binary.

## 0.18.0 · N7c

- UPS shutdown with the app closed (opt-in, `[ups] shutdown`): when the UPS this host follows (`[health] ups`) has been on battery for `on_battery_minutes` (5), drops below `below_percent`, or reports low battery, the node announces a countdown (`countdown_secs`, 60) in the timeline and then requests `power.poweroff` through the root helper, in the action log as source `ups`. Power coming back cancels the countdown, and `ups.cancel` holds it for the rest of the outage. `dry_run` only logs the power-off.
- There is no node-to-node link: each host decides alone with its own limits, so a group shutdown is ordered by giving the hosts that must stay up longest larger limits (N-041). `ups.guard` shows settings, phase and any problem. Capability `ups` 1.

## 0.17.0 · N7b

- Backup status with the app closed: the app pushes its backup jobs (`jobs.set`: restic, borg, rsync or rclone, a systemd unit or a path, with a stale limit). The node reads them every minute with the app's own rules (last run, result, size, what was added, the run's error line) and raises `backupFailed` and `backupStale` alerts with the app's keys and its `backupStale` switch. Paths are walked at most every 10 minutes, and their size is read again only when the newest file changes.
- The app's scheduled jobs (`nightgrid-job-*`) and backup units: each finished run is a `job` event ("job nightly sync failed (exit 2)", "backup nightly finished · snapshot 1a2b saved · 5.0 GiB"), including runs while the node was stopped. A scheduled reboot or shutdown (`nightgrid-power-*`) is recorded after the host is back, from the previous boot's journal.
- `jobs.latest`; `[jobs]` in the config (`enabled`, `interval_secs`); capability `jobs` 1; `node.info.access.jobs`. Read-only: the node schedules and runs nothing itself (N-040). Run output needs `allow journal`.

## 0.16.0 · N7a

- Service checks from inside the LAN: the app pushes its HTTP(S) and TCP checks (`checks.set`). After root runs `nightgrid-node allow checks`, a separate check runner (the service account, no capabilities, network access only to `[checks] networks`) runs them every minute with the app's own rules (expected status, certificate trust and expiry) and hands results to the node. `checks.latest`; `node.info.access.checks`.
- `serviceDown` and `certExpiring` alerts with the app's keys and its `serviceDown`, `certExpiring` and `certDays` rules.
- `[checks]` in the config: `enabled`, `interval_secs`, `networks`. New capability `checks` 1.

## 0.15.0 · N6c

- New actions `firewall.block` and `firewall.unblock` (target: an IP address), through the root helper with nftables. Blocks drop all traffic from the address (input and forwarded) for `[block] minutes`, in the node's own table with kernel timeouts. Loopback, private, link-local and CGNAT/Tailscale addresses and `[block] never` are refused, also on the helper's side.
- Optional auto-block (`[block] auto`, off by default): an address with `[block] threshold` failed SSH logins within 5 minutes is blocked, unless it logged in successfully in the last day; at most `[block] max_active` at once. Every block is in the action log and the timeline.
- `block.list`; new capability `block` 1. The root helper unit gains `CAP_NET_ADMIN` (for nft). `deny helper`, `uninstall` and `dist/uninstall.sh` remove the table.

## 0.14.0 · N6b

- Listening ports: every 15 s (`[ports] interval_secs`) the node reads the kernel's socket tables (no privileges) and records `port` events when a socket starts or stops listening ("new listener 6379/tcp (redis)", `warn` when reachable from the network). `ports.list` with the app's `Listener` fields, and the containers that publish each port. `containers.list` entries gain `ports`.
- Failed-login bursts as alerts: `loginBurst` (key `logins:burst`) while at least `[logins] burst` (default 20) failed SSH logins fall in 5 minutes, when the app turns `loginBurst` on in its alert rules.
- A wider file baseline (`[files] baseline`, default on): ld.so.preload, PAM, profile, cron directories and root's crontabs, sysctl, modprobe, package sources, Docker's daemon.json, enabled units, root's and every login user's `authorized_keys`, and running containers' compose files. With `allow health`, the read-only health helper takes these snapshots so homes and root-only files are covered (its unit now reads homes read-only). Cron, PAM and `authorized_keys` changes are `warn`.
- New capability `security` 1.

## 0.13.0 · N6a

- Usage per systemd service and per container (CPU, memory, disk read/write and, for containers with their own network, traffic) from cgroup v2, and the 15 busiest process names (CPU, resident memory, how many) from `/proc`. Every 10 s, every second while anyone subscribes to the new `usage` stream.
- Power draw from the CPU's energy counters (RAPL: package, core, uncore, dram, psys), after root runs `nightgrid-node allow power` (a udev rule that lets the service's group read `energy_uj`). `node.info.access.power`.
- `usage.latest` and `usage.history` (one-minute averages per service, container, process name and power domain, kept 24 hours).
- `[usage] enabled` in the config.

## 0.12.0 · N5b

- Log watching with the app's detector: every 10 minutes (`[logs] interval_secs`) the node reads what the journal logged at priority err and worse (after `allow journal`) and the last lines of up to 60 running containers (after `allow docker`), and raises `logBurst` (several times the usual error count) and `logNewError` (an error pattern not seen before) alerts, when the app turns `logAnomaly` on in its alert rules. The first round for a source only learns.
- The app's saved log searches with alerts: `logs.searches.set` (plain text or regular expression, unit, container and priority filters; at most 32), checked every round, raising `logMatch` alerts.
- `logs.status` (sources, error counts, findings and search hits of the last round) and `logs.samples` (recent error lines of a source). Lines are only kept in memory, and lines that look like they hold a secret are never kept or shown.
- `[logs]` in the config: `enabled`, `interval_secs`.

## 0.11.0 · N5a

- Host health, off until root runs `nightgrid-node allow health`: a read-only root helper (socket-activated per read, like the root helper) runs the app's own checks with the app's parsers: disk SMART (every hour), ZFS/mdadm/Btrfs pools (every 5 minutes), pending updates and reboot-required from the package manager's local cache (every 6 hours; the node never fetches package lists) and NUT UPS status (every 30 s, only for targets root lists in `[health] ups`).
- `health.latest` with the last reading of each, and `node.info.access.health`.
- Alerts with the app's switches: SMART warning, pool degraded, and UPS on battery / low battery / replace battery. New updates and reboot-required are info events in the timeline.
- `[health]` in the config: `enabled`, `ups`.
- Sensors (hwmon, thermal zones, GPU sysfs) are read on a helper thread with a 1 s limit, so a drive or firmware that is slow to answer a temperature read can no longer stall sampling; the last readings are kept meanwhile.
- `dist/uninstall.sh` also removes the helper units (netwatch, root helper, health) and the rollback directory.

## 0.10.0 · N4c

- Rollbacks with a dead-man timer: `changes.begin` snapshots files (through the root helper, only under root's `[rollback] paths`), and unless the app calls `changes.confirm` in time the node puts them back, with owner and mode, removes files the change added, and runs an optional `onRevert` unit or container action. The timer survives restarts and reboots of the node. `changes.revert`, `changes.list`.
- New action type `files.rollback` (in `[actions] allow`); `[rollback] paths` in the config, which `allow helper` turns into the only places the helper may write.
- The helper unit gains the file capabilities it needs for that and a snapshot directory, `/var/lib/nightgrid-node-rollback` (removed by `uninstall --purge`).

## 0.9.0 · N4b

- Root helper, off until root runs `nightgrid-node allow helper`: a socket only the service account can reach, starting one short-lived root process per request. It re-reads the root-owned config and runs only what is listed there, with fixed arguments and no shell.
- New actions: `unit.restart`, `unit.start`, `unit.stop` on units root lists in `[actions] units`, and `power.reboot` / `power.poweroff`, scheduled a minute ahead (cancel with `shutdown -c`). Each still needs `[actions] allow`; all are logged like the rest.
- `actions` capability 2; `actions.policy` gains `units` and `helper`; `node.info.access.helper`.

## 0.8.0 · N4a

- Typed actions, each off until root lists it in `[actions] allow`: `container.restart`, `container.start`, `container.stop` and `images.prune` (dangling images), through the engine socket. `actions.policy`, `actions.run` (with dry run), and a global `max_per_hour`.
- Hash-chained audit log of every action, dry or real, refused or done (`actions.log`, with chain verification), and `action` events in the timeline.
- Self-healing rules pushed by the app (`heal.rules.set`, `heal.status`): unhealthy or down container, or full disk, held for N minutes → action. Off and dry-run by default, capped per hour and day, waiting between runs, and giving up with a warning when the condition doesn't clear.
- Slow requests run off the socket thread, so a container restart doesn't hold up other clients.

## 0.7.0 · N3b

- Network watch helper, off until root runs `nightgrid-node allow netwatch`: a separate unit (`nightgrid-node-netwatch.service`) whose only privilege is `CAP_NET_RAW`. It listens for ARP and DHCP through a kernel filter, sends nothing, and reports to the service over the unix socket.
- `net.dhcp`: DHCP servers on the LAN with what they hand out. Events for the first server, another server (possible rogue DHCP, `warn`, unless listed in `[network] dhcp_servers`) and a server handing out a different router or DNS (`warn`).
- Devices heard on the wire join the inventory (also ones this host never talks to), with the host name they asked DHCP for (`name`).
- `node.info.access.netwatch`; error code `forbidden`.

## 0.6.0 · N3a

- Network watch: an inventory of LAN devices from the kernel's neighbour table (`net.devices`), read every 30 s with no privileges and nothing sent. `network` events for new devices, devices on a new address, addresses answering from a new MAC, IP conflicts (an address switching between MACs) and a gateway answering from a new MAC (possible ARP spoofing). The first read on a fresh install is the baseline.
- `[network]` in the config: `enabled`, `interval_secs`, `ignore_ifaces`.
- CI: jobs time out instead of hanging; the journal is followed from the start time, so the first SSH line after a start isn't skipped.

## 0.5.0 · N2b

- SSH logins and failed logins (summed per minute) as `login` events, from the journal, after root runs `nightgrid-node allow journal`. Resumes from a saved cursor, so logins while the node was stopped are kept.
- Watched files: changes to `/etc/ssh`, `sudoers`, `passwd`, `shadow`, cron, `hosts`, `fstab` and `/etc/systemd/system` (root can edit the list) as `file` events, with sizes, hashes, mode and owner, never contents. Changes made while the node was stopped are reported at start.
- `node.info` reports which optional access root has granted (`access`).
- Both are pushed live on the `events` stream.

## 0.4.0 · N2a

- Protocol 2: `subscribe` / `unsubscribe` and pushed frames on the same connection. Streams: `events` (every new event as it is recorded), `metrics` (every 2 s while watched), `containers` (on state or health changes), plus `lagged` when a slow client missed frames. Protocol 1 clients are unchanged.
- The node samples every 2 s and reads containers every 3 s only while someone subscribes.
- `nightgrid-node watch STREAM... [--count N]` prints live frames.

## 0.3.0 · N1b

- Container watcher over the Docker (or Podman) socket: `containers.list`, lifecycle events in the timeline (restarts merged, crashes, OOM kills, health changes) and container-down alerts with the app's rules.
- `nightgrid-node allow docker` / `deny docker`: root grants or removes access through a systemd drop-in, with a root-equivalence warning.
- Milestones re-planned for the expanded brief: real-time push, network watch and self-healing come next.
- README describes what the node does.

## 0.2.0 · N1

- Events stream (`events.sync`) with a growing `seq`, 90 days, secret filter on titles and details.
- Uptime: once-a-minute heartbeat, hourly up/down/asleep seconds (`uptime.sync`) and outage events that tell a crash ("Down") from a clean reboot ("Restarted") and a suspend ("Suspended").
- Disk usage per mount per hour (`disk.sync`) and fill-up forecasts.
- Offline alerts with the app's tracker semantics: disk full and disk filling, rules pushed by the app (`alerts.rules.get/set`), active alerts (`alerts.active`) that survive restarts.

## 0.1.0 · N0

- First version: `install` / `uninstall [--purge]` as a hardened systemd service with its own account and a group-gated unix socket; `bridge`, `status`, `query`.
- Metrics sampled every 10 s straight from `/proc`, `/sys` and `statvfs` (vendored Nightgrid parser), averaged per minute and rolled up to 15-minute and hourly tiers (14 / 90 / 400 days).
- Protocol 1: `hello` with version and capability negotiation, `node.info`, `metrics.latest`, `metrics.history`, `metrics.sync`.
- CI: lint and tests on x86_64 and arm64, static musl builds, real install tests on systemd (runners, Debian 12, Ubuntu 22.04, Fedora 41). Release workflow with checksums, minisign and build provenance.
