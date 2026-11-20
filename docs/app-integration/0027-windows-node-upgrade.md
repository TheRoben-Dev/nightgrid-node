# 0027 · Windows node W2b: upgrade

Node: **0.32.0** (W2b of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Builds on [0024](0024-windows-node.md). Additive: no method, field, frame or link request changed.

## What the person gets

The app updates a Windows node the way it updates a Linux one: it verifies the release, copies the exe over SSH and runs `upgrade`. A bad build puts the old one back by itself.

## The command

```powershell
nightgrid-node upgrade C:\Users\me\AppData\Local\Temp\nightgrid-node-v0.32.0-x86_64-windows.exe --sha256 <hex> --json
```

- Run it as an administrator (an elevated SSH session), where Linux uses `sudo`. Without one, it fails with the same `not_root` JSON error as `install`.
- `--sha256` is the exe's line in `SHA256SUMS`. Pick the file from `manifest.json`'s `windows` map, as for an install (0024).
- The report is the Linux `UpgradeReport`: `action`, `from`, `to`, `ok`, `reverted`, `steps`, `error`. The exit code is 0 only when `ok`.
- Refusals that change nothing come back with a non-zero exit code before anything is touched: a checksum mismatch, a file that isn't a Windows exe or isn't `nightgrid-node`, or an older version without `--allow-downgrade`.
- The old exe is kept as `C:\Program Files\Nightgrid Node\nightgrid-node.prev.exe`.
- A failed upgrade that was reverted has `ok: false, reverted: true`; show it as on Linux. If the revert failed too, `error` ends with the command an administrator can run.
- The node's timeline gets the same `upgrade` event as on Linux (`upgrade.note`).

## What to change in the app

- Offer "Update node" for Windows hosts from 0.32.0 on, using `upgrade` in place of the "run `install` again" path from 0024. For older Windows nodes, keep re-running `install`.
- The node may restart its own helper on the way. Nothing needs to be granted again: `allow eventlog` and `allow helper` survive the upgrade.
