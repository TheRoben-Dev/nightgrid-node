# 0029 · Windows node W2d: UPS on USB and the shutdown guard

Node: **0.34.0** (W2d of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Builds on [0028](0028-windows-node-health.md) and [0014](0014-ups-shutdown.md). Additive: no method, field, frame or link request changed.

## What the person gets

A Windows host with a UPS on USB shows the UPS (on battery, charge, runtime) and can power itself off on battery with the app closed, like a Linux host with NUT.

## Setting it up

An administrator edits `C:\ProgramData\Nightgrid Node\config.toml` and restarts the service (`Restart-Service nightgrid-node`):

```toml
[health]
ups = ["battery"]

# Optional, as on Linux (0014):
[ups]
shutdown = true
on_battery_minutes = 5

[actions]
allow = ["power.poweroff"]
```

The node also needs `allow health` (0028), and `allow helper` (0026) for the power-off.

## What the app sees

- `hello` caps now include `ups` on Windows.
- `health.latest` → `ups`: one entry with `target: "battery"` and the usual `UpsReading`. `status` holds the NUT flags the node derives (`OL`, `OB`, `LB`, `CHRG`, `RB`). `charge`, `runtime` and `model` are filled when Windows reports them. `load` and `inputVoltage` stay empty, because Windows doesn't report them.
- With no UPS connected, `error` says Windows sees no UPS or battery.
- A NUT target such as `ups@nas` on Windows gets an `error` saying only `"battery"` is read for now.
- `ups.guard` and `ups.cancel` work as on Linux. The power-off is `shutdown.exe` one minute ahead (0026), so the host goes down a minute after the countdown ends.

## What to change in the app

- For Windows hosts on 0.34.0 or later, offer `"battery"` as the UPS choice, and show the node's UPS reading and guard as for Linux.
- Don't offer NUT server targets for a Windows node yet.
