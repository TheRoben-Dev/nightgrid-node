# 0025 · Windows node W1: logins, logs, ports, LAN, usage, files

Node: **0.30.0** (W1 of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Builds on [0024](0024-windows-node.md). Additive: no frozen method, field, frame or link request changed. Every capability below is one the app already handles on Linux; this spec only lists what reads differently on Windows.

## What the person gets

On Windows Server, with the app closed:

- SSH logins and failures (Windows OpenSSH), Remote Desktop logins, and password guessing over RDP or SMB, with the same burst alerts as on Linux.
- Error bursts and new error patterns from the System and Application logs, and saved log searches.
- Listening ports with the process behind each, and ports that open or close.
- LAN devices, new devices, IP changes and conflicts, and the default gateway.
- CPU, memory and I/O per Windows service and per process, with a day of history.
- Changes to the OpenSSH config and keys, the hosts file, start-up folders, Group Policy scripts, the machine PowerShell profiles and scheduled tasks.

## 1. Capabilities

A 0.30.0 Windows node's `hello` adds `logins`, `logs`, `security`, `net`, `usage` and `files` to 0024's list. It still leaves out `net.dhcp` (no rogue DHCP watch on Windows), `containers`, `actions`, `health`, `checks`, `jobs`, `peers` and the rest. Keep going by `hello`: the app calls what a node advertises and nothing else.

## 2. `allow eventlog` (the journal grant)

Logins need the Security log, which only members of the builtin group Event Log Readers (and administrators) may read. That is the counterpart of the `systemd-journal` group, and it is granted the same way:

```powershell
nightgrid-node allow eventlog --json
```

Run it as an administrator, like `sudo nightgrid-node allow journal` on Linux. `allow journal` is accepted as an alias, so the app can send the same command on both platforms. `deny eventlog` takes the grant back. Both restart the service if it was running, and print the same report as on Linux.

Where the app reads the journal grant today (`node.info` access `journal`, and `doctor`'s findings), a Windows node reports:

- `journal`: `ok` once the service is in Event Log Readers, `denied` before that, `off` with `[logins]` turned off. Show "Event Log" instead of "journal" in the label for Windows hosts. The action is "Allow" → `allow eventlog`.
- `doctor --json` has an `eventlog` finding (`ok`, or `info` with the hint to run `allow eventlog`).
- `netwatch` is always `off` on Windows: the wire watch (rogue DHCP) would need a packet capture driver. Hide the "Allow network watch" offer for Windows hosts.

## 3. What reads differently

### Logins

`login` events have the same titles and details as on Linux. The sources are the `OpenSSH/Operational` log (sshd's own lines, parsed like the journal's) and the Security log (4624 and 4625):

- A Remote Desktop logon is an accepted login whose method is `rdp`. The detail reads `rdp` where Linux has `ssh, publickey`.
- Failed network logons (logon types 3 and 10) from another address count as failed logins, so password guessing over SMB or RDP raises the same "failed logins" burst as SSH guessing does on Linux. The user name is the one that was tried.
- Local, service and loopback logons are never reported.

Windows records failed logons only when the "Logon" audit policy includes failures. Windows Server audits them by default; if a host has none, `allow eventlog` still works and SSH failures still come from OpenSSH's log.

### Logs

`logs.status`, `logs.samples` and saved searches work as on Linux. On Windows the "journal" source is the System and Application event logs:

- The identifier (`journal/<ident>`) is the event's provider name, such as `Service Control Manager` or `Microsoft-Windows-Kernel-Power`.
- The message is the provider's formatted message when it has one, else the event's data values joined with `; `.
- The levels map to journal priorities like this: Critical 2, Error 3, Warning 4, Information 6, Verbose 7. So "errors" means Error and Critical, as on Linux.

Service crashes (Service Control Manager 7031 and 7034) and unexpected shutdowns (6008, Kernel-Power 41) are errors in the System log, so they show up as error bursts and new patterns there. Boots and outages are the same `boot` and `outage` events as in 0024.

### Ports

`ports.list`, `port` events and the security audit are as on Linux, with these differences:

- `process` is filled for every listener, from the owning process's image name (`svchost.exe`, `sshd.exe`). On Linux the node only sees its own processes.
- Ephemeral ports start at 49152 (the Windows dynamic range), so a UDP socket at or above it isn't reported as a listener.

### LAN devices

`net.devices` and `network` events are as on Linux. `iface` is the Windows interface alias, such as `Ethernet` or `Ethernet 2`. That is the same name `[network] ignore_ifaces` takes.

### Usage

`usage.latest`, `usage.history` and the `usage` stream are as on Linux, with these differences:

- `service` items are Windows services, named by service name (`W32Time`, `sshd`, `nightgrid-node`). Their figures are those of the process the service runs in. When services share one process, the item is named for all of them: `a+b+c`, or `a+b+2 more` when there are more than three. Treat the name as opaque.
- `ioRead`/`ioWrite` count every read and write the process made, network and devices included. Linux only counts block I/O.
- `mem` is the working set.
- `cgroups` is `true` (services are measured).
- `power.access` is `absent`: there's no power reading on Windows yet.

### Files

`file` events are as on Linux. Paths are Windows paths. The default `[files] watch` on Windows is:

- `%ProgramData%\ssh\sshd_config`
- `%ProgramData%\ssh\administrators_authorized_keys`
- `%SystemRoot%\System32\drivers\etc\hosts`

`baseline = true` adds the following, as warnings where Linux would warn:

- `%ProgramData%\ssh`
- the all-users StartUp folder
- the Group Policy start-up, shutdown and logon script folders
- the machine PowerShell profiles
- `%SystemRoot%\System32\Tasks` (scheduled tasks, when the service may list it)
- `.ssh\authorized_keys` under every profile in `C:\Users` that the service can see into

Owners aren't compared on Windows, and "mode" is only the read-only attribute. Content changes (size and SHA-256) are reported as on Linux.

## 4. Nothing else changes

Install, upgrade, the bridge and the probe are as in 0024. Update the `win::support` "node" note to: "History, uptime, disk usage, alerts, logins, logs, ports, LAN devices, usage and watched files. Actions, health and containers come in later node releases."
