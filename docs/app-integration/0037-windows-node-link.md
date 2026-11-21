# 0037 · Windows node W3d: node-to-node link

Node: **0.42.0** (W3d of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Builds on the Linux link spec and [0035](0035-windows-node-checks.md). Additive: no method, field, frame or link request changed.

## What the person gets

A Windows host can be paired with other nodes (Linux or Windows). It can check whether they are up, follow a UPS shutdown that a paired node starts, and be a member of a hub, as a Linux host can.

## What the app sees

- `hello` caps now include `peers` and `hub.member` on Windows. `peers.set`, `peers.get` and `peers.latest` work as on Linux, and so do `hub.member.get` and `hub.member.set`. The fingerprint, the `listening` address and the readings have the same shape.
- `peers.latest` `access` is `off` until an administrator runs `allow peers`, as on Linux.

## What to change in the app

- Offer pairing for Windows hosts on 0.42.0 or later.
- When `access` is `off` on Windows, the hint is `nightgrid-node allow peers` in an elevated PowerShell. After a change to `[peers] networks`, `listen` or `port` in `%ProgramData%\Nightgrid Node\config.toml`, they run it again.
