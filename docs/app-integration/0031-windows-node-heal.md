# 0031 · Windows node W2f: self-healing

Node: **0.36.0** (W2f of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Builds on [0026](0026-windows-node-w2a.md) and the Linux self-healing spec. Additive: no method, field, frame or link request changed.

## What the person gets

Self-healing rules work on a Windows host as on Linux. A full disk can run an allowed action. A service an administrator listed is restarted when it crashes, with the waits, caps, give-up and dry run the app already shows.

## What the app sees

- `hello` caps now include `heal` on Windows. `heal.rules.get`, `heal.rules.set` and `heal.status` work as on Linux.
- Rules the app pushes keep the same conditions. Container conditions never hold on Windows until containers arrive (W3).
- `disk-full` targets on Windows are volume roots as the disk-full alert names them (`C:\`, `D:\`), or `*`.
- Actions still need root's `[actions] allow`. On Windows that means `unit.*`, `power.*` and `firewall.*` for now. The local fixes (`fix.*`) come with the fix menu.
- Host autofix rules in `C:\ProgramData\Nightgrid Node\fixes.toml` can use `when = "unit-failed"` with a Windows service name, which matches a listed service that stopped with an error. Their runs show in `heal.status` and in the action log as `autofix <id>`.

## What to change in the app

- Offer the self-healing editor for Windows hosts on 0.36.0 or later. For Windows, hide the container conditions and offer volume roots for disk rules.
