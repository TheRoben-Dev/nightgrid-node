# 0014 · UPS shutdown with the app closed

Node milestone: **N7c** (nightgrid-node 0.18.0, `ups` 1). Status: **proposed** for the app. Builds on 0007 (actions), 0008 (health, UPS readings) and the app's group shutdown (M10s, D-112).

## What the person gets

- **Hosts shut down cleanly on a long power cut, even with Nightgrid closed.** Each host with a node follows the UPS, announces a countdown in the timeline and powers itself off.
- **The order they choose.** Storage and the NUT server get larger limits, so they go last.
- **A Cancel button** while a countdown runs, like the app's own group shutdown.

## 1. Availability

With `ups` in `caps`, call `ups.guard`:

- **`enabled: false`:** on the node's settings page, next to the UPS reading (0008), offer "Power this server off on a long power cut, even when Nightgrid is closed." It shows what root needs to do, with the app's sudo confirmation if the person wants the app to run it:
  - In `/etc/nightgrid-node/config.toml`, set `[ups] shutdown = true` and the limits, add `"power.poweroff"` to `[actions] allow`, and list the UPS in `[health] ups`. The app never edits that file itself, so show the lines.
  - Run `sudo nightgrid-node allow helper` and `sudo nightgrid-node allow health`.
  - Then run `sudo systemctl restart nightgrid-node`.
  - Suggest larger `on_battery_minutes` for the host serving NUT and for storage.
- **`problem` set:** show it on the settings page as is.
- **`dryRun: true`:** say "test mode: only logged".

## 2. During an outage

- Refresh `ups.guard` on `alert` events of kind `upsOnBattery` and on events whose title starts with "UPS shutdown".
- **`countdown`:** show the app's existing group-shutdown banner for this host: "<host> powers off at 14:02 (on battery for 5 min)", with **Cancel**, which calls `ups.cancel`.
- **`shuttingDown`:** "Powering off". The action log shows `power.poweroff` with source `ups`.
- **The app's own group shutdown:** when a host's node has `enabled: true` and no `problem`, leave that host out of the app's group shutdown. The node covers it, also with the app closed, and the two would otherwise race.

## 3. Checks

On a host with node 0.18.0, a NUT UPS in `[health] ups`, the helper, `power.poweroff` allowed, and `[ups] shutdown` with `dry_run = true`:

- Pull the UPS's mains plug. After `on_battery_minutes`, the timeline shows "UPS shutdown in 60 s". A minute later, the action log shows a dry-run `power.poweroff` from `ups`.
- Plug it back in during a countdown. "UPS shutdown cancelled: power is back" appears.
- Cancel in the app during a countdown. Nothing more happens until power is back.
- Hosts without the node, or with `[ups] shutdown` off, behave exactly as before.
