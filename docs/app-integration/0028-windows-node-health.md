# 0028 · Windows node W2c: health

Node: **0.33.0** (W2c of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Builds on [0024](0024-windows-node.md) and [0008](0008-health-and-logs.md). Additive: no method, field, frame or link request changed.

## What the person gets

Disks, Storage Spaces pools and pending Windows updates for a Windows host come from the node, with history and alerts while the app is closed, the same as `allow health` on Linux.

## Granting it

```powershell
nightgrid-node allow health     # elevated prompt; deny health takes it away
```

- This installs the service `nightgrid-node-health`: LocalSystem, demand start, a write-restricted token, no network. The node can start it and nothing more. It stops itself after three idle minutes.
- `doctor` has a `health` finding, which is `ok` once the service is installed and `info` with the hint `nightgrid-node allow health` before that.
- Without an elevated session it fails with the same `not_root` error as `install`.

## What the app sees

- `hello`/`node.info` caps now include `health` on Windows.
- `health.latest` and `health.history` work as on Linux. `access` reports the health access as on Linux.
- `smart.report`: one disk per physical disk. `health` comes from the Storage module's health status and operational status. Temperature, wear, power-on hours and read/write errors come from `MSFT_StorageReliabilityCounter` where the disk reports them. There are no SMART attribute tables.
- `pools.report`: Storage Spaces pools with `kind: "storageSpaces"`, their virtual disks and their health. The app already has this value.
- `updates.report`: `manager: "windowsUpdate"`. Security updates have "security" in `repo`. `rebootRequired` and `rebootReasons` come from the usual registry keys. Hidden updates are left out, and there are no held packages. The app already has this value.
- UPS readings aren't available yet. `ups.report` stays absent until W2d.
- Watched files (`files`) now also go through this helper, so files the node's account can't read are still hashed.

## What to change in the app

- For Windows hosts on 0.33.0 or later, offer "Allow health" with the command above, in place of the app's own SSH PowerShell reads.
- When the node reports health for a host, take disks, pools and updates from it, as for Linux.
- For older Windows nodes, keep the app's own reads.
