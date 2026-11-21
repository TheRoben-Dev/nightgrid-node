# 0034 · Windows node W3a: containers

Node: **0.39.0** (W3a of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Builds on the Linux containers spec and [0026](0026-windows-node-w2a.md). Additive: no method, field, frame or link request changed.

## What the person gets

A Windows host running Docker Engine shows its containers in the app as a Linux host does. That covers state, health, published ports, stops and crashes on the timeline, alerts when a container stops, logs, and restart, start, stop and image pruning where an administrator allowed them.

## What the app sees

- `hello` caps now include `containers` on Windows. `containers.list`, container events and the `container.*` and `images.prune` actions work as on Linux.
- Until an administrator runs `allow docker` and restarts Docker, `containers.list` reports `access: "denied"` (or `"absent"` without Docker), as on Linux before `allow docker`.
- Container names, images and states come from Docker Engine unchanged. Windows containers report the same states (`running`, `exited` and so on).
- Self-healing rules with container conditions now hold on Windows.

## What to change in the app

- Show the containers view for Windows hosts on 0.39.0 or later.
- When `access` is `denied` on Windows, the hint should be `nightgrid-node allow docker` followed by `Restart-Service docker`.
