# 0024 · Nightgrid Node on Windows Server

Node: **0.29.0** (W0 of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Additive: the protocol, the methods, the frames and the link are unchanged. A Windows node speaks the same NDJSON over the same `bridge`, and simply advertises fewer capabilities in `hello`. An app that ignores this spec keeps working exactly as today: it never offers the node on Windows hosts (`win::support` "node" = No, and the early `return None` in `node_cmd.rs`).

## What the person gets

- Metric history, uptime with outages, disk usage trends and forecasts, and the node's alerts on Windows Server (tested on 2022 and 2025; 2019 and Windows 10/11 use the same APIs), collected while the app is closed, like on Linux.
- The same commands on the host as on Linux: `nightgrid-node status`, `doctor`, `events`, `query`, `watch`, `install`, `uninstall`, from an elevated PowerShell or cmd.

## 1. What changes in the app

1. `win::support` "node": `Partial` with the note "History, uptime, disk usage and alerts. Logins, logs, actions, health and containers come in later node releases." (Update the note as later milestones land; the capabilities in `hello` are the truth.)
2. `node_cmd.rs`: drop the `if conn.is_windows() { return None; }` early return once the steps below are in. Every call already goes through `hello`'s capabilities, so methods a Windows node doesn't advertise are never called.
3. Install, probe and bridge commands for Windows, below. Everything after the bridge is open (sync, streams, events, alerts) is the same code as on Linux.

## 2. Picking the file

Release files: `nightgrid-node-<tag>-x86_64-windows.exe`, a static (no VC++ runtime needed) 64-bit console exe, listed in `SHA256SUMS` with build provenance like the Linux files.

`manifest.json` gains two top-level keys, present only when the release has Windows builds (unknown keys are ignored by older apps, as 0022 says):

```json
{
  "manifest": 1,
  "version": "0.29.0",
  "programs": {"nightgrid-node": {"x86_64": {"file": "nightgrid-node-v0.29.0-x86_64-linux", "…": "…"}}},
  "windows": {
    "nightgrid-node": {"x86_64": {"file": "nightgrid-node-v0.29.0-x86_64-windows.exe", "size": 5123456, "sha256": "…64 hex"}}
  },
  "windowsArch": {"AMD64": "x86_64"}
}
```

`windows` is apart from `programs` on purpose, so an app that only knows `programs` can never pick a Windows file for a Linux host. Read the host's arch with `$env:PROCESSOR_ARCHITECTURE` and map it through `windowsArch`; anything else (ARM64, x86) is "not supported" for now. A release without `windows` has no Windows build: hide the node on Windows hosts for it.

## 3. Install

Over the existing SSH connection (`conn.is_windows()`), as an account in Administrators. Windows OpenSSH gives administrators a full (elevated) token, so no UAC prompt and no sudo flow is involved; check with the existing `IS_ADMIN` snippet first and show "Needs an administrator account" otherwise.

1. Download and verify the exe against `manifest.json` / `SHA256SUMS` in the app, as for Linux.
2. Upload it over SFTP to a fresh folder under the account's temp folder, for example `%TEMP%\nightgrid-<random>\nightgrid-node.exe`. The app's PowerShell bootstrap can make the folder and print its path.
3. Run it, through the usual `powershell -EncodedCommand` bootstrap:
   ```powershell
   & 'C:\Users\…\AppData\Local\Temp\nightgrid-…\nightgrid-node.exe' install --user <login> --json
   ```
   `--user` is the SSH login the app uses for this host, so later sessions may open the pipe without being an administrator (it is added to the local group `Nightgrid Node Users`, the Windows counterpart of the `nightgrid-node` group). Leave it out when the login is itself an administrator; Administrators can always open the pipe.
4. Remove the temp folder. `install` copied itself to `%ProgramFiles%\Nightgrid Node\nightgrid-node.exe` (a fixed path; `steps` names it) and put that folder on the machine `PATH`.

The report has the same shape as on Linux, with Windows facts:

```json
{"action": "install", "version": "0.29.0", "steps": ["…"], "users": ["backup"], "warnings": [],
 "arch": "x86_64", "assetArch": "x86_64", "libc": "windows", "init": "scm",
 "kernel": "10.0.20348", "busybox": false, "limits": ["…"]}
```

- `init: "scm"` and `libc: "windows"` are new values; treat them like any other unknown value (show them, don't branch on them beyond "this is Windows").
- `limits` lists what this release can't do on Windows yet, one sentence each. Show them as notes under "Installed", never as errors.
- Errors with `--json` print `{"error": {"code": "not_root" | "failed", "message"}}` and exit 1, as in 0022. `not_root` here means "not an elevated administrator".

**Upgrade:** run `install` again with the new exe, the same way. It replaces the binary (the running one is renamed aside, since Windows can't overwrite a running exe), restarts the service and keeps the node id and history. `nightgrid-node upgrade` comes with the helper in W2.

**Uninstall:** `nightgrid-node uninstall --json` (keeps the history), or `uninstall --purge --json` (removes `%ProgramData%\Nightgrid Node`, the group, the firewall rules and the PATH entry). Run as an administrator. There is no `uninstall.sh` counterpart: the exe uninstalls itself.

## 4. Session: the bridge

Open an exec channel with `nightgrid-node bridge` (on `PATH` after install; a new SSH session sees the machine `PATH`). If the app wants to be independent of `PATH`, use the full path from the install report, quoted for cmd: `"C:\Program Files\Nightgrid Node\nightgrid-node.exe" bridge`. The default shell of Windows OpenSSH is cmd unless the host changed it, and the quoted form works in both cmd and PowerShell (in PowerShell prefix `& `; if the app sends it through its `powershell -EncodedCommand` bootstrap, write `& 'C:\Program Files\…\nightgrid-node.exe' bridge`).

Stdin and stdout are the NDJSON stream exactly as on Linux. `bridge` ends when stdin closes. Failures before `hello` come as one JSON line, as 0022 says:

- `{"error": {"code": "not_running", …}}`: no service is answering. Run the probe (§5).
- `{"error": {"code": "no_access", …}}`: the pipe exists but this account may not open it. The login is not in `Nightgrid Node Users` (or was added after this logon session began: a new SSH connection fixes that). Offer "Allow this login" which runs `install --user <login> --json` again as an administrator.

Streamlocal forwarding (`direct-streamlocal@openssh.com`) is not used on Windows: Windows OpenSSH doesn't forward named pipes. Always use `bridge`.

## 5. Probe

The Linux probe is a POSIX shell snippet; on Windows run this through the PowerShell bootstrap instead and parse the same `key=value` lines:

```powershell
$e = Join-Path $env:ProgramFiles 'Nightgrid Node\nightgrid-node.exe'
"installed=$([int](Test-Path $e))"
$s = Get-Service nightgrid-node -ErrorAction SilentlyContinue
"active=$(if ($s) { $s.Status.ToString().ToLower() } else { 'missing' })"
if (Test-Path $e) { "version=$((& $e version) -replace '^nightgrid-node (\S+).*','$1')" }
"pipe=$([int](Test-Path '\\.\pipe\nightgrid-node'))"
```

`active` is `running`, `stopped`, `startpending` and so on, or `missing`. For a full picture run `nightgrid-node doctor --json` (as on Linux: the host facts at the top level, and the findings `binary`, `config`, `service`, `pipe`, `memory`, `history`, `sandbox` and `limits`, with the same `check`/`level`/`detail` shape). `sandbox` is only checked from an elevated session.

## 6. Capabilities

A 0.29.0 Windows node's `hello` lists:

`node`, `metrics`, `metrics.sync`, `events`, `uptime`, `disk`, `alerts`, `summary`

and nothing else, so on Windows the app keeps its own SSH paths for everything else (containers, logins, logs, health, actions), exactly as it does today. 0.30.0 adds `logins`, `logs`, `security`, `net`, `usage` and `files`: see [0025](0025-windows-node-w1.md). Later milestones (W1 to W3 in [WINDOWS.md](../WINDOWS.md)) add capabilities to this list; they need no new spec work in the app beyond what 0005 to 0019 already describe, except where a later spec says so.

### Metrics

`metrics.latest`, `metrics.sync` and the `metrics` stream carry the same sample as on Linux. The node builds it from Windows performance counters in the same shape as the app's own Windows sampler (`core/src/win/metrics.rs`), so the app's parser and the numbers it already shows for Windows hosts line up:

- Disks are named `disk0`, `disk1`, … (physical disk numbers), mounts are drive letters (`C:\`) with their file system.
- Network interfaces keep their Windows names with `:` replaced by `_`.
- Temperatures come from ACPI thermal zones where the hardware has them (many VMs and some servers have none).
- GPU figures come from `nvidia-smi` when it is installed.

### Events and alerts

Boot and outage events, disk forecasts and the node's alerts are as on Linux. The sample's `kernel` is what the app's Windows sampler shows there (for example "Windows Server 2022 Datacenter 10.0.20348"), and `node.info.node.arch` is `x86_64`.

## 7. Security notes for the app's UI

- The node service runs as the virtual account `NT SERVICE\nightgrid-node` (a member of Performance Monitor Users, to read counters) with a write-restricted token and one privilege, and Windows blocks all of its network traffic (a service hardening rule). It opens no port. The pipe rejects remote clients.
- Who may read the node is Administrators plus the members of `Nightgrid Node Users`. Show the members where the app shows the `nightgrid-node` group's members on Linux (`Get-LocalGroupMember 'Nightgrid Node Users'`).
- Config: `%ProgramData%\Nightgrid Node\config.toml`, same format as on Linux, writable only by Administrators.
