# 0002 · Events, uptime, disk usage and offline alerts

Node milestone: **N1** (nightgrid-node 0.2.0, protocol 1). Status: **proposed** for the app. Builds on [0001](0001-install-and-metrics-sync.md): same channel, same `NodeState`, same per-node cursor table.

Everything here runs only for a host whose `hello` listed the capability. Without it the app does exactly what it does today.

## What the person gets

- **While you were away.** After the app was closed, the alerts panel lists what fired and cleared on each node host in the meantime ("DISK FULL · server01, 03:12 → 03:41"), and the host timeline shows them with the time they happened, not the time the app found out.
- **Uptime without gaps.** Host uptime bars and SLA reports count the hours the app was closed, using the node's heartbeat: real crashes show as down, planned reboots and suspends as asleep.
- **Forecasts from the full history.** "Full in about N days" uses disk usage the node recorded while the app was closed.
- **Alerts keep working with the app closed.** Disk-full and filling-up alerts are evaluated on the node with the person's own thresholds, which the app pushes.

## 1. Sync on connect

Right after the metrics sync of 0001, for each capability present, in this order:

| Capability | Call | Cursor (per node id) | Writes to |
|---|---|---|---|
| `disk` | `disk.sync {after}` | hour start | `disk_usage` |
| `uptime` | `uptime.sync {after}` | hour start | `uptime_hours` |
| `events` | `events.sync {after}` | `seq` | `host_events`, `outages`, alert history |

Loop while `more` is true, saving the cursor after each page in the same transaction as the rows (as in 0001). Store cursors in 0001's `node_sync` table with reserved `res` values: `-1` events, `-2` uptime, `-3` disk. Start new nodes at `after = 0`; the node's own retention bounds the first sync (90 / 400 / 90 days).

Re-syncs every 15 minutes while connected, so a long session also picks up node-side events (until protocol 2 streams in N4).

### Disk usage

Insert into `disk_usage (host_id, mount, ts, used, size)` with `INSERT OR IGNORE`: an hour the app recorded itself wins. Nothing else changes: `Store::disk_forecasts` now simply has more points.

### Uptime hours

The node reports only what it witnessed, and so does the app. Merge per hour with the larger value of each column:

```sql
INSERT INTO uptime_hours (kind, subject, hour, up, down, asleep) VALUES ('host', ?1, ?2, ?3, ?4, ?5)
ON CONFLICT (kind, subject, hour) DO UPDATE SET
  up = max(up, excluded.up), down = max(down, excluded.down), asleep = max(asleep, excluded.asleep);
```

Then clamp so `up + down + asleep <= 3600` (take from `up` first). Hours where both saw the host mostly agree; hours where only one did get that one's numbers.

### Events

For each event:

- `kind: alert` / `recovered` → `Store::event_add(host, at, Alert|Recovered, level, title, detail)`. Titles already have the app's `LABEL · host` shape.
- `kind: outage` → insert into `outages (kind 'host', subject host_id, start, end, detail)` when `outage.state == "down"`, and a timeline entry: `Power` for "Restarted" and "Suspended" (info), `Alert` for "Down" (critical). The hours are already in `uptime_hours` from `uptime.sync`.
- `kind: boot` → skip if the app's own host-log reader (`timeline::host_script`) already shows a boot within ±2 minutes; otherwise `Boot`.
- `kind: container` (N1b) and `other` → timeline entry with the closest kind, or skip.

Text has passed the same secret filter as the app's (`redact::line_has_secret`) on the node, and the app's `tidy` runs again on insert.

## 2. Alerts

### Keys

Node alert keys have no host segment. Map them before they touch the app's tracker or history:

| Node key | App key |
|---|---|
| `disk:/srv` | `disk:<hostId>:/srv` |
| `fill:/srv` | `fill:<hostId>:/srv` |

Rule: insert `<hostId>:` after the first `:`. `kind`, `severity`, `title`, `detail`, `since` and `resolved` already match the app's `Alert`.

### While you were away

Add `Tracker::import(&mut self, alerts: Vec<Alert>)` to the app's `alerts.rs`: resolved alerts go into `history` (with new local ids); an unresolved one becomes active unless the app already has the key active. During sync, collect the `alert` objects of `alert` / `recovered` events (the latest per key wins) and import them.

Then reconcile with `alerts.active`:

- Node-active, app-inactive → active in the app with the node's `since`. Notifications: one summary notification per host ("3 alerts while you were away on server01"), not one per alert, and none for alerts that fired and resolved while the app was closed.
- App-active for a node-evaluated kind (`diskFull`, `diskFilling`) but not node-active → leave it to the app's own next round; the clear hold applies as usual.

The app keeps evaluating disk alerts itself while connected (the node's view and the app's agree, since the code is the same). Fired/resolved events for the same key within the same minute from both sides are expected; dedupe timeline entries by `(kind, key, at ± 90 s)`.

UI: an "Away" section at the top of the alerts panel, cleared when the person opens it, listing imported alerts with a small "from node" marker.

### Pushing rules

Whenever alert settings change, and once after each `hello`, call `alerts.rules.set` with the app's whole `AlertSettings` serialized as it is stored (camelCase). The node ignores fields it doesn't know and answers with what it stored; if it answers `bad_request` (a value outside 50–99 % or 1–90 days), show "the node kept its previous rules" in the node panel. Per-host overrides, if the app adds them, are sent per host.

`alerts.rules.get` is for the node panel only ("Evaluating on the node: disk full at 90 %, filling within 14 days").

## 3. Node panel additions

Below the coverage table from 0001:

- "Alerts on the node: N active" with the list from `alerts.active`.
- "Uptime recorded since <oldest hour>", from the first `uptime.sync` page.
- "Events: <count> since <date>"; if `events.sync` returned `oldest > cursor + 1`, a muted "some older events were pruned on the node".

## 4. Tests in the app

- Merge: hours from app and node overlap → max per column, clamped to 3600.
- Key mapping for `disk:` and `fill:`, including mounts with `:` in the path (only the first `:` counts).
- Import: a fired and resolved pair while away → history only, no notification; a fired-only → active and one summary notification.
- A node without `events` / `uptime` / `disk` → none of the new calls are made.
- A fake node over a socketpair replaying fixtures from `nightgrid-node-proto` (tests in this repo serialize the same types).
