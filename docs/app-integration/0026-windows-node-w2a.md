# 0026 · Windows node W2a: admin helper, service and power actions

Node: **0.31.0** (W2a of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Builds on [0024](0024-windows-node.md) and [0025](0025-windows-node-w1.md). Additive: no frozen method, field, frame or link request changed. The app already handles everything below on Linux; this spec lists only what reads differently on Windows.

## What the person gets

On Windows Server, once an administrator has run `allow helper` and listed what may run:

- Restart, start or stop a Windows service from the app (`W32Time`, `sshd`, an application's service).
- Reboot or power off the host, a minute ahead, from the app.
- The same allow list, rate limit and hash-chained audit log as on Linux (`actions.policy`, `actions.run`, `actions.log`).

## 1. Capabilities

A 0.31.0 Windows node's `hello` adds `actions` (version 2, as on Linux) to 0025's list. It still leaves out `heal`, `changes`, `health`, `containers`, `checks`, `jobs`, `peers` and `net.dhcp`. Keep going by `hello`.

## 2. `allow helper` (the helper grant)

```powershell
nightgrid-node allow helper --json
```

Run it as an administrator, like `sudo nightgrid-node allow helper` on Linux. `deny helper` takes it back. Both print the same report as on Linux. Unlike Linux, neither restarts the node: it sees the helper the next time it asks.

What it installs: the service `nightgrid-node-helper` (LocalSystem, manual start, only the shutdown privilege, no network). The node starts it when it has a request, and it stops after three idle minutes. Only the node's own service may start it or talk to it; members of `Nightgrid Node Users` can't.

Where the app reads the helper grant today:

- `node.info` access `helper`: `ok` once the helper service is installed (whether or not it runs right now), `denied` before.
- `actions.policy` field `helper`: the same.
- `doctor --json` has a `helper` finding: `ok` ("installed, starts when the node needs it"), or `info` with the hint to run `allow helper`.
- Label the grant "Admin helper" instead of "Root helper" for Windows hosts, and show the command without `sudo`.

## 3. What reads differently

### Units are service names

`[actions] units` on Windows takes service key names, the ones `Get-Service` shows under `Name`: `W32Time`, `sshd`, `MSSQL$SQLEXPRESS`. Letters, digits and `_ . - $`; the node's own services can't be listed. `actions.policy` returns them in `units` as on Linux, and `actions.run` takes one as `target` of `unit.restart`, `unit.start` or `unit.stop`.

- Show the list as "Services" instead of "Units" for Windows hosts, and don't append `.service` anywhere.
- A restart is a stop and then a start, each waiting up to a minute. A service that other running services depend on can't be stopped: the run fails with Windows' message, as `detail`.
- The config example to show an administrator is:

```toml
[actions]
allow = ["unit.restart", "power.reboot"]
units = ["W32Time"]
```

in `C:\ProgramData\Nightgrid Node\config.toml`, followed by `Restart-Service nightgrid-node`.

### Power

`power.reboot` and `power.poweroff` run `shutdown.exe /r` or `/s` with a 60-second delay and a planned reason, so the restart is logged in the System log with the comment "Requested from Nightgrid Node". The result's `detail` is "reboot in 1 minute (cancel on the host with: shutdown /a)". Show that cancel command, not `shutdown -c`, for Windows hosts.

### Not on Windows yet

`firewall.block`, `firewall.unblock` and `files.rollback` are left out of `actions.policy.allowed` on Windows, even when the config lists them, and `actions.run` refuses them with "… is not available on Windows yet". They come with W2b. Container actions are refused because there is no container access, as on a Linux host without Docker.

## 4. Nothing else changes

Install, upgrade, the bridge and the probe are as in 0024. Update the `win::support` "node" note to: "History, uptime, disk usage, alerts, logins, logs, ports, LAN devices, usage, watched files, and service and power actions. Health, the fix menu and containers come in later node releases."
