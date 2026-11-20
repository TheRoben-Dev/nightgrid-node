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
| systemd unit, account `nightgrid-node` | Windows service `nightgrid-node` running as the virtual account `NT SERVICE\nightgrid-node` | No password, no logon rights, not an administrator. Created by `install` through the Service Control Manager. |
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
| `logins` (SSH logins and failures, bursts) | Direct | Event Log: OpenSSH/Operational (sshd) and Security 4624/4625 | W1, after `allow eventlog` |
| `logs` (error bursts, new patterns, saved searches) | Direct | Event Log, level Error and Critical, per provider | W1, after `allow eventlog` |
| Boots, crashes, unexpected shutdowns, service failures as events | Direct | System log: 6005/6006/6008, Kernel-Power 41, Service Control Manager 7031/7034 | W1 |
| `files` (watched files) | Direct | Same poller; owner and mode become the owner SID and a hash of the DACL | W1 |
| `security`: listening ports | Direct | `GetExtendedTcpTable`/`GetExtendedUdpTable` with owning process | W1 |
| `usage` per process and per service | Substitute | Per process from the process list, per service by mapping service PIDs (no cgroups) | W1 |
| `usage` power draw | Substitute | PDH `Power Meter` counters where the platform has them; otherwise absent (no RAPL from user mode) | W1 |
| `net` (LAN devices, IP conflicts) | Direct | `GetIpNetTable2` (the neighbour table; no raw sockets needed) | W1 |
| `net.dhcp` (rogue DHCP) | None | Needs packet capture (Npcap); no built-in way to listen on UDP 67/68 next to the DHCP client | — |
| `actions`, `heal`, fix menu (`fix`, `fix auto`) | Direct | Windows services instead of units: `restart`, `start`, `stop`; `reboot`, `poweroff`; containers as on Linux | W2, after `allow helper` |
| `fix journal` / `fix logs` / `fix packages` / `fix ssh` | Substitute | Event logs over their size, large log files, the Windows Update download cache, and the OpenSSH `authorized_keys` and `administrators_authorized_keys` ACLs (the usual Windows key-login failure) | W2 |
| `changes` (snapshot and rollback with dead-man timer) | Direct | Same paths model; the helper copies files and restores ACLs | W2 |
| `block` (block addresses, auto-block after failed logins) | Direct | Windows Firewall rules in the node's own group, each with an expiry the helper enforces | W2 |
| `health`: disks | Substitute | `MSFT_PhysicalDisk` health and `MSFT_StorageReliabilityCounter` (temperature, wear, errors) instead of SMART attributes | W2, after `allow health` |
| `health`: pools | Substitute | Storage Spaces pools and virtual disks instead of ZFS/mdraid/btrfs | W2 |
| `health`: updates | Substitute | Windows Update search (COM), pending reboot from the usual registry keys | W2 |
| `health`: UPS, `ups` shutdown guard | Substitute | A USB HID UPS shows up as a battery (`Win32_Battery`): charge, on battery, runtime. A NUT server on the LAN is read like on Linux (`upsc` protocol over the check runner) | W2 |
| `containers` | Direct | Docker Engine on `\\.\pipe\docker_engine`, after `allow docker` (adds the service to `docker-users`, root-equivalent like on Linux) | W3 |
| `checks` (service checks from the LAN) | Direct | Separate service `nightgrid-node-checker`, outbound only to `[checks] networks` by a service hardening rule | W3 |
| `peers`, `serve`, `hub.member` (node-to-node link) | Direct | Separate service `nightgrid-node-peer`, inbound only on the tailnet or LAN address and port | W3 |
| Status page | Direct | Separate service, tailnet address only | W3 |
| `jobs` (scheduled jobs) | Substitute | Task Scheduler tasks under `\Nightgrid\` (what the app creates on Windows) instead of systemd timers | W3 |
| `jobs` backups | Substitute | Path jobs as on Linux; Windows Server Backup results from its event log instead of restic/borg units | W3 |
| `summary.daily` | Direct | Built from the same events | W0 (whatever is collected) |
| `upgrade FILE --sha256` | Direct | The running exe is renamed aside, the new one put in place and the service restarted; reverts by itself | W2 |
| `allow power` | None | Not needed: the power counters (where they exist) need no grant | — |
| OpenRC/sysvinit, legacy 32-bit | None | Not applicable | — |
| `nightgrid-hub` on Windows | Later | The hub reuses the node's pieces; possible once W3 is in | after W3 |

## 4. Milestones

- **W0 (0.29.0):** the crate builds for `x86_64-pc-windows-msvc`. Named-pipe transport, the Windows service and its hardening, the native collector, history, uptime, disk usage and alerts, `install`/`uninstall`/`doctor`/`status`/`bridge` and the rest of the read-only CLI. CI builds and tests on a real Windows Server runner, installs the service, talks to it through `bridge` and uninstalls it. Release assets `nightgrid-node-vX.Y.Z-x86_64-windows.exe` in `SHA256SUMS` and `manifest.json`. App spec [0024](app-integration/0024-windows-node.md).
- **W1:** Event Log sources (`allow eventlog`): logins, logs, boots and service failures; watched files; ports; usage; LAN devices.
- **W2:** the helper service and `allow helper`/`allow health`: actions, self-healing, the fix menu, rollbacks, blocking, disks, pools, updates, UPS and the shutdown guard, `upgrade`.
- **W3:** containers, the check runner, the link, the status page, jobs and backups.

## 5. Same commands

The binary, subcommands and flags are the same. Differences:

- "(root)" means an elevated Administrator session.
- `--config` defaults to `%ProgramData%\Nightgrid Node\config.toml`, `--socket` takes a pipe name (`\\.\pipe\nightgrid-node`).
- `allow journal` is accepted as an alias of `allow eventlog`.
- `nightgrid-node man` prints the manual as text (there is no `man` on Windows); `completions powershell` prints a completer for `$PROFILE` (on every platform).
- Commands that aren't on Windows yet (for example `allow netwatch` or `fix`) say so and exit 1 instead of being unknown; with `--json` they print `{"error":{"code":"not_on_windows","message":…}}`.
