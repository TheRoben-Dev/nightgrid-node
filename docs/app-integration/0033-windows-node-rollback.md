# 0033 · Windows node W2h: rollbacks

Node: **0.38.0** (W2h of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Builds on [0026](0026-windows-node-w2a.md) and the Linux rollback spec. Additive: no method, field, frame or link request changed.

## What the person gets

A change the app makes to files on a Windows host can be undone the same way as on Linux. The files are snapshotted first, and they are put back unless the person confirms the change in time. That covers contents, the file's ACL and read-only bit, and files the change created or deleted.

## What the app sees

- `hello` caps now include `changes` on Windows. `changes.begin`, `changes.confirm`, `changes.revert` and `changes.list` work as on Linux, with the same states and the same dead-man timer.
- Paths are Windows paths under root's `[rollback] paths`, such as `D:\srv\app\app.conf`. Paths in the node's own folders, outside the list, or not on a drive letter are refused with a message, as on Linux.
- `onRevert` takes an allowed `unit.*` action with a Windows service name, for example restarting the service that reads the file. Container actions are refused until containers arrive (W3).
- It still needs `files.rollback` in `[actions] allow` and the admin helper (`allow helper`).

## What to change in the app

- Offer guarded file changes for Windows hosts on 0.38.0 or later. Use Windows paths, and offer service restarts as the revert action.
