# 0006 · Network watch

Node milestone: **N3a** (nightgrid-node 0.6.0, protocol 1 and 2). Status: **proposed** for the app. Builds on 0002 (events sync) and 0004 (live streams).

## What the person gets

- **The network map stays filled without a scan.** A host with the node keeps a list of the devices on its LAN around the clock, with when each was first and last seen. The map can show it straight away, and a scan still adds what only a scan finds (mDNS, Tailscale, UPnP, exposures).
- **New and changed devices in the timeline.** "new device 192.168.1.23", "device moved …", "IP conflict on 192.168.1.50", and a warning when the gateway answers from a new MAC (a replaced router, or ARP spoofing), even while the app was closed.

## 1. Map

When the selected scanner host has the `net` capability:

1. Call `net.devices` when the map opens, before or instead of a scan.
2. Turn each device into a `netmap::Device`: `ip`, `mac`, `seen: [Neighbour]`, `vendor: netmap::mac_vendor(mac)` (the node ships no OUI table), and `gateway` from the result marks `Seen::Gateway` as the scan does.
3. Show `firstSeen` / `lastSeen` in the device's details ("first seen 3 days ago"). A device with `online: false` is drawn dimmed: the host hasn't talked to it lately, which doesn't mean it is off.
4. A **Scan** merges into this list as today, keyed by MAC (`device_labels::key_for`), so labels keep working.

Without `net`, the map works exactly as now.

## 2. Timeline

Network events arrive through `events.sync` and the `events` stream; there is nothing new to call.

| Node `kind` | App `timeline::EventKind` | Notes |
|---|---|---|
| `network` | add `Network` (or map to `Unit` until it exists) | `warn` for IP conflicts and a gateway MAC change, `critical` for a conflict on the gateway |

The detail of "new device" events is "<mac> on <iface>". The app can add the vendor and the device's label (if the person named it on the map) when it shows the entry, e.g. "new device 192.168.1.23 · Espressif (iot)".

## 3. Node panel

Show "Watching the LAN: N devices, M online" from `net.devices`. Settings are root's: "Edit `[network]` in /etc/nightgrid-node/config.toml on the server" (interval, interfaces to ignore, or switch it off).

## Tests in the app

- `net.devices` from a fixture fills the map with vendors from the app's table and dimmed offline devices.
- A scan after that keeps first/last seen and labels for devices keyed by MAC.
- `network` events from a fixture show with the right kind and level.
- A host without `net` still scans as before.
