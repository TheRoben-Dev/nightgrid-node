# 0036 · Windows node W3c: NUT servers

Node: **0.41.0** (W3c of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Builds on the Linux UPS spec, [0029](0029-windows-node-ups.md) and [0035](0035-windows-node-checks.md). Additive: no app-facing method, field, frame or link request changed.

## What the person gets

A Windows host can watch a UPS served by NUT on the LAN (a NAS or a Raspberry Pi running `upsd`), raise UPS alerts, and power itself off through the `[ups]` shutdown guard, as a Linux host does.

## What the app sees

- `health.latest` `ups` entries for targets like `ups@nas` on Windows, with the same reading fields and the same `error` texts as `upsc` (`Unknown UPS`, `Data stale`, `Driver not connected`).
- Without the check runner, or with the server outside `[checks] networks`, the reading's `error` starts with `no recent answer for … from the check runner`.
- `ups.guard`, `ups.cancel` and the UPS alerts are unchanged.

## What to change in the app

- When the person adds a NUT target for a Windows host on 0.41.0 or later, tell them it needs `nightgrid-node allow health` and `nightgrid-node allow checks` in an elevated PowerShell, and the server's address inside `[checks] networks`.
- Nothing else: `ups.report` is internal and the app never calls it.
