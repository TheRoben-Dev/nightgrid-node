# 0035 · Windows node W3b: service checks

Node: **0.40.0** (W3b of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Builds on the Linux service checks spec and [0024](0024-windows-node.md). Additive: no method, field, frame or link request changed.

## What the person gets

A Windows host can run the app's service checks (HTTP(S) status and certificate expiry, TCP connects) from inside the LAN with the app closed, as a Linux host does, and alert when a service goes down.

## What the app sees

- `hello` caps now include `checks` on Windows. `checks.set`, `checks.get` and `checks.latest` work as on Linux, with the same readings and errors (`… is outside [checks] networks` for an address the runner may not reach).
- `node.info` access `checks` is `denied` until an administrator runs `allow checks`, `ok` while the runner reports, and `off` when `[checks] enabled = false`, as on Linux.
- Service-down alerts and self-healing conditions from checks hold on Windows.

## What to change in the app

- Offer the "check from this host" option for Windows hosts on 0.40.0 or later.
- When `checks` access is `denied` on Windows, the hint is `nightgrid-node allow checks` in an elevated PowerShell (no `sudo`). After the person changes `[checks] networks` in `%ProgramData%\Nightgrid Node\config.toml`, they run it again.
- Nothing else changes: the runner on Windows is the service `nightgrid-node-checker`, which the app never talks to directly.
