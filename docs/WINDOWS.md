# Nightgrid Node on Windows Server

Status: plan plus W0 (this document is updated with each milestone). Decisions are N-070 onwards in [DECISIONS.md](DECISIONS.md).

Goal (Roben, 2026-11-20): a Windows Server build of Nightgrid Node with feature parity with the Linux node and, where possible, the same commands and flags. The app already reads Windows Server hosts agentlessly over SSH and PowerShell (app decisions D-191 and D-192); the node does for Windows what it does for Linux: keep collecting with the app closed, and hand the history over on reconnect.

The rules carry over unchanged:

- **Same protocol.** The same NDJSON methods, streams, capabilities and frozen shapes. A Windows node advertises only the capabilities it really implements, so the app falls back to its SSH path for the rest exactly as it does for an older node. Nothing app-facing is renamed or changed; Windows-only additions are additive.
- **No open ports, no outbound connections, no cloud, no telemetry.** Access is SSH only (Windows OpenSSH, built in from Server 2019, an optional install on 2016), through `nightgrid-node bridge` on an exec channel.
- **The daemon never runs as an administrator.** Privileged reads and actions go through a separate helper service with fixed, typed operations from an administrator-owned policy (the Windows form of N-010).
- **Same numbers as the app.** The metrics collector prints the same `@@section` text as the app's own Windows sampler (`core/src/win/metrics.rs`), so history synced from the node and samples the app took itself line up exactly.

Supported: Windows Server 2016, 2019, 2022 and 2025 (and Windows 10/11, which use the same APIs), x86_64. ARM64 Windows can follow once there is demand.

## 1. Shape

```
 Nightgrid app                     Windows Server
┌──────────────────┐  SSH   ┌──────────────────────────────────────────────────────────┐
│ node client      │ ═════> │ sshd (Windows OpenSSH)                                    │
│ (NDJSON)         │        │  └ exec "nightgrid-node bridge"  (stdio ⇄ named pipe)     │
└──────────────────┘        │                                   │                       │
                            │  \\.\pipe\nightgrid-node  <───────┘                       │
                            │   (local only; Administrators + "Nightgrid Node Users")   │
                            │  service nightgrid-node   (NT SERVICE\nightgrid-node,     │
                            │   virtual account, write-restricted SID, one privilege,   │
                            │   all network blocked by a service hardening rule)        │
                            │   ├ sampler: PDH raw counters, memory, volumes            │
                            │   ├ engine: uptime, disk usage, alerts                    │
                            │   └ SQLite: %ProgramData%\Nightgrid Node\data\node.db     │
                            │  service nightgrid-node-helper (LocalSystem, on demand;   │
                            │   only the node's service SID may start it or open its    │
                            │   pipe; typed operations from the admin-owned policy)     │
                            └──────────────────────────────────────────────────────────┘
```

## 2. How each Linux piece maps

| Linux | Windows | Notes |
|---|---|---|
| systemd unit, account `nightgrid-node` | Windows service `nightgrid-node` running as the virtual account `NT SERVICE\nightgrid-node` | No password, no logon rights, not an administrator. Created by `install` through the Service Control Manager. It is a member of the builtin Performance Monitor Users, which a non-interactive account needs to read performance counters (read-only). |
| `NoNewPrivileges`, empty capability set | Required privileges limited to `SeChangeNotifyPrivilege` | Windows removes every other privilege from the service's token. |
| `ProtectSystem=strict`, `ReadWritePaths` | Service SID type *restricted* (a write-restricted token) | The service can only write where its own SID is granted: its data folder. |
| `IPAddressDeny=any`, `RestrictAddressFamilies=AF_UNIX` | Windows Service Hardening rules that block all inbound and outbound traffic for the service SID | Enforced by Windows even when the firewall profile is off. |
| `MemoryMax=64M` | A job object with a 64 MB process memory limit, set by the service on itself at start | |
| Unix socket `/run/nightgrid-node/node.sock`, mode 0660, group `nightgrid-node` | Named pipe `\\.\pipe\nightgrid-node`, remote clients rejected, DACL: SYSTEM, Administrators, the service SID and the local group `Nightgrid Node Users` | `install --user NAME` adds the SSH login to that group, like the Linux group. |
| Peer uid in the log | Caller's account SID and name from the pipe client's token | |
| `/etc/nightgrid-node/config.toml` | `%ProgramData%\Nightgrid Node\config.toml` | Same file format. Only Administrators and SYSTEM may change it; the service may read it. |
| `/var/lib/nightgrid-node` | `%ProgramData%\Nightgrid Node\data` | Only the service, Administrators and SYSTEM. |
| `/usr/local/bin/nightgrid-node` | `%ProgramFiles%\Nightgrid Node\nightgrid-node.exe`, folder added to the machine `PATH` | So `nightgrid-node bridge` works over SSH. `install --json` also reports the full path. |
| Root helper, socket-activated per request | Service `nightgrid-node-helper` as LocalSystem, demand start, stops when idle | Only the node's service SID may start it (service DACL) and open its pipe. Same typed operations, same policy file. |
| journald | Event Log (`wevtapi`) | |
| `sudo` | An elevated session (Administrators over Windows OpenSSH get a full token) | Commands that say "(root)" on Linux need "Administrator" here. |

## 3. Features: direct, substitute, not possible

**Direct** means the same capability and data shapes with a Windows source. **Substitute** means the same capability with a narrower or different source, with the difference noted. **None** means there is no reasonable Windows equivalent; the capability is not advertised and the app keeps its own path or note.

| Capability / command | Windows | Source | Milestone |
|---|---|---|---|
| `metrics`, `metrics.sync`, `metrics` stream | Direct | PDH raw counters (processor, physical disk, network interface, thermal zone), `GlobalMemoryStatusEx`, page files, fixed volumes, `nvidia-smi` | W0 |
| `node`, `node.info`, `status`, `query`, `watch`, `events`, `bridge`, `version`, `completions`, `man` | Direct | | W0 |
| `uptime` (host up minutes, outages at boot) | Direct | Heartbeats in the store, same rules | W0 |
| `disk` (usage trend, forecasts), `alerts` | Direct | | W0 |
| `install`, `uninstall [--purge]`, `--user`, `--no-start`, `--json` | Direct | SCM, ACLs, service hardening rules, local group | W0 |
| `doctor` | Direct | Service state, pipe, config, memory, each access | W0 |
| `logins` (SSH logins and failures, bursts) | Direct | Event Log: OpenSSH/Operational (sshd's lines, parsed like the journal's) and Security 4624/4625 (Remote Desktop logons; failed network logons, so RDP and SMB guessing count too) | W1 (0.30.0), after `allow eventlog` |
| `logs` (error bursts, new patterns, saved searches) | Direct | System and Application logs, levels mapped to journal priorities, per provider, with the provider's formatted message | W1 (0.30.0) |
| Boots, crashes, unexpected shutdowns, service failures as events | Direct | Boots and outages (down or asleep) from the node's own heartbeats as on Linux; service crashes (7031/7034) and unexpected shutdowns (6008, Kernel-Power 41) are System log errors, so `logs` reports them | W0 / W1 |
| `files` (watched files) | Direct | Same poller. Defaults: OpenSSH config and keys, hosts; the baseline adds start-up folders, Group Policy scripts, machine PowerShell profiles, scheduled tasks and users' `authorized_keys`. Owners aren't compared; mode is the read-only attribute | W1 (0.30.0) |
| `security`: listening ports | Direct | `GetExtendedTcpTable`/`GetExtendedUdpTable` with the owning process's name (every process, not only the node's) | W1 (0.30.0) |
| `usage` per process and per service | Substitute | `NtQuerySystemInformation` process list (no process handles needed), per service through the SCM's service PIDs; I/O counts every read and write, not only block I/O | W1 (0.30.0) |
| `usage` power draw | Later | PDH `Power Meter` counters where the platform has them (no RAPL from user mode); `absent` for now | — |
| `net` (LAN devices, IP conflicts) | Direct | `GetIpNetTable2` (the neighbour table; no raw sockets needed), gateway from `GetIpForwardTable2`, interfaces by alias | W1 (0.30.0) |
| `net.dhcp` (rogue DHCP) | None | Needs packet capture (Npcap); no built-in way to listen on UDP 67/68 next to the DHCP client | — |
| `actions` | Direct | Windows services instead of units: `restart`, `start`, `stop` of services listed in `[actions] units` (`W32Time`, not `w32time.service`); `reboot`, `poweroff` with `shutdown.exe` a minute ahead | **W2a (0.31.0)**, after `allow helper` |
| `heal` (the app's rules and `fixes.toml` autofix) | Direct | A failed unit is a listed service that stopped with an error (its SCM exit code) or an automatic one that isn't running; disk rules name volume roots such as `D:\` | **W2f (0.36.0)** |
| Fix menu (`fix`, `fix list`, `fix NAME`, `fix auto`) | Direct | Same menu and names, run through the admin helper. Detects failed listed services, full volumes, a pending reboot (`WinSxS\pending.xml`) | **W2g (0.37.0)**, after `allow helper` |
| `fix restart` / `fix reboot` / `fix run` | Direct | A listed service; `shutdown.exe /r` a minute ahead; a `fixes.toml` runbook (`C:\...` program, no shell, time limit) | **W2g (0.37.0)** |
| `fix packages` / `fix ssh` | Substitute | The Windows Update download cache (`SoftwareDistribution\Download`, links skipped); the ACLs of `administrators_authorized_keys` and the user's `.ssh\authorized_keys` reset to SYSTEM, Administrators and the user (the usual Windows key-login failure) | **W2g (0.37.0)** |
| `fix journal` / `fix logs` / `fix reset` | None | No systemd journal, logrotate or failed state; Windows rotates its event logs by size | — |
| `fix start` / `fix prune` | Direct | Through Docker's pipe; `fix auto` adds the container and image presets | **W3a (0.39.0)** |
| `changes` (snapshot and rollback with dead-man timer) | Direct | Same `[rollback] paths` model (`D:\srv\app`); the admin helper keeps snapshots in `%ProgramData%\Nightgrid Node Rollback` (SYSTEM and Administrators only) and puts back contents, the DACL and the read-only bit (the owner where it may set it), replacing a file in place | **W2h (0.38.0)**, after `allow helper` |
| `block` (block addresses, auto-block after failed logins) | Direct | Inbound Windows Firewall rules in the group `Nightgrid Node blocks`, each with its end time; the helper removes expired ones whenever it touches the group and the node asks it to once a minute after one is due | **W2e (0.35.0)**, after `allow helper` |
| `health`: disks | Substitute | `MSFT_PhysicalDisk` health and `MSFT_StorageReliabilityCounter` (temperature, wear, errors) instead of SMART attributes, read by the health helper `nightgrid-node-health` | **W2c (0.33.0)**, after `allow health` |
| `health`: pools | Substitute | Storage Spaces pools and virtual disks instead of ZFS/mdraid/btrfs | **W2c (0.33.0)** |
| `health`: updates | Substitute | Windows Update search (COM), pending reboot from the usual registry keys; hidden updates are left out | **W2c (0.33.0)** |
| `health`: UPS, `ups` shutdown guard | Substitute | `[health] ups = ["battery"]`: a USB HID UPS shows up as a battery (`Win32_Battery`): charge, on battery, low battery, runtime, read by the health helper and turned into the same reading as `upsc`, so alerts and `[ups] shutdown` work unchanged. `"ups@nas"`, a NUT server on the LAN, is read by the check runner with the node's own NUT client (`LIST VAR`, read only) because Windows has no `upsc` | **W2d (0.34.0)**; NUT servers **W3c (0.41.0)**, after `allow health` and `allow checks` |
| `containers` (list, events, `container.*` actions, logs) | Direct | Docker Engine on `\\.\pipe\docker_engine`, after `allow docker`, which sets `"group": "NT SERVICE\\nightgrid-node"` in `%ProgramData%\docker\config\daemon.json` (Administrator-equivalent, like the docker group on Linux); Docker applies it at its next restart | **W3a (0.39.0)** |
| `checks` (service checks from the LAN) | Direct | Separate service `nightgrid-node-checker` as its own virtual account (no administrator, write-restricted SID), outbound TCP only to `[checks] networks` by a Windows Service Hardening rule; it may read the config and write only `data\log\checker`. Windows Firewall won't take `::1/128` in a rule, so `allow checks` says so and checks of `localhost` should use `127.0.0.1` | **W3b (0.40.0)**, after `allow checks` |
| `peers`, `serve`, `hub.member` (node-to-node link) | Direct | Separate service `nightgrid-node-peer` as its own virtual account, inbound only on its listen port and outbound only with `[peers] networks` (Windows Service Hardening); its TLS key in `data\peer`, which the node's own service can't read | **W3d (0.42.0)**, after `allow peers` |
| `allow statuspage` (status page) | Direct | Separate service `nightgrid-node-status-page` as its own virtual account, inbound only on its port from `[statuspage] networks` and nothing outbound (Windows Service Hardening); listens on the tailnet address or loopback only; its secret link in `data\status-page`, which the node's own service can't read | **W3e (0.43.0)**, after `allow statuspage` |
| `jobs` (scheduled jobs) | Substitute | Task Scheduler tasks under `\Nightgrid\` (what the app creates on Windows) instead of systemd timers | W3 |
| `jobs` backups | Substitute | Path jobs as on Linux; Windows Server Backup results from its event log instead of restic/borg units | W3 |
| `summary.daily` | Direct | Built from the same events | W0 (whatever is collected) |
| `upgrade FILE --sha256` | Direct | The running exe is renamed aside, the new one put in place and the service restarted; reverts by itself | **W2b (0.32.0)** |
| `allow power` | None | Not needed: the power counters (where they exist) need no grant | — |
| OpenRC/sysvinit, legacy 32-bit | None | Not applicable | — |
| `nightgrid-hub` on Windows | Later | The hub reuses the node's pieces; possible once W3 is in | after W3 |

## 4. Milestones

- **W0 (0.29.0):** the crate builds for `x86_64-pc-windows-msvc`. Named-pipe transport, the Windows service and its hardening, the native collector, history, uptime, disk usage and alerts, `install`/`uninstall`/`doctor`/`status`/`bridge` and the rest of the read-only CLI. CI builds and tests on a real Windows Server runner, installs the service, talks to it through `bridge` and uninstalls it. Release assets `nightgrid-node-vX.Y.Z-x86_64-windows.exe` in `SHA256SUMS` and `manifest.json`. App spec [0024](app-integration/0024-windows-node.md).
- **W1 (0.30.0):** the Event Log reader and `allow eventlog`: SSH and RDP logins and failures, log errors from System and Application; watched files; ports with processes; usage per service and process; LAN devices. App spec [0025](app-integration/0025-windows-node-w1.md).
- **W2a (0.31.0):** the admin helper service and `allow helper`/`deny helper`; service and power actions. App spec [0026](app-integration/0026-windows-node-w2a.md).
- **W2b (0.32.0):** `upgrade FILE --sha256`. App spec [0027](app-integration/0027-windows-node-upgrade.md).
- **W2c (0.33.0):** `allow health` and the read-only health helper: disks, Storage Spaces pools, Windows Update. App spec [0028](app-integration/0028-windows-node-health.md).
- **W2d (0.34.0):** a UPS on USB (`"battery"`) and the UPS shutdown guard. App spec [0029](app-integration/0029-windows-node-ups.md).
- **W2e (0.35.0):** blocking addresses and auto-block after failed logins. App spec [0030](app-integration/0030-windows-node-blocks.md).
- **W2f (0.36.0):** self-healing: the app's rules and `fixes.toml` autofix rules, with failed services. App spec [0031](app-integration/0031-windows-node-heal.md).
- **W2g (0.37.0):** the fix menu (`fix`, `fix auto`) with the same names, Windows Update cache and OpenSSH key ACL substitutes, and runbooks. App spec [0032](app-integration/0032-windows-node-fixes.md).
- **W2h (0.38.0):** rollbacks (`changes.begin`, `confirm`, `revert`, the dead-man timer). App spec [0033](app-integration/0033-windows-node-rollback.md).
- **W3a (0.39.0):** containers through Docker Engine and `allow docker`. App spec [0034](app-integration/0034-windows-node-containers.md).
- **W3b (0.40.0):** the check runner service and `allow checks`/`deny checks`. App spec [0035](app-integration/0035-windows-node-checks.md).
- **W3c (0.41.0):** NUT servers on the LAN for UPS readings and the shutdown guard, through the check runner. App spec [0036](app-integration/0036-windows-node-nut.md).
- **W3d (0.42.0):** the node-to-node link service and `allow peers`/`deny peers`; `hub.member`. App spec [0037](app-integration/0037-windows-node-link.md).
- **W3e (0.43.0):** the status page service and `allow statuspage`/`deny statuspage`. App spec [0038](app-integration/0038-windows-node-status-page.md).
- **W3f and later:** jobs and backups.

## 5. Same commands

The binary, subcommands and flags are the same. Differences:

- "(root)" means an elevated Administrator session.
- `--config` defaults to `%ProgramData%\Nightgrid Node\config.toml`, `--socket` takes a pipe name (`\\.\pipe\nightgrid-node`).
- `allow journal` is accepted as an alias of `allow eventlog`.
- `nightgrid-node man` prints the manual as text (there is no `man` on Windows); `completions powershell` prints a completer for `$PROFILE` (on every platform).
- Commands that aren't on Windows (for example `allow netwatch`) or not yet (`peer`) say so and exit 1 instead of being unknown; with `--json` they print `{"error":{"code":"not_on_windows","message":…}}`.
