# 0008 · Host health and logs

Node milestones: **N5a** (nightgrid-node 0.11.0, `health` 1) and **N5b** (0.12.0, `logs` 1). Status: **proposed** for the app. Builds on 0002 (events and alerts) and 0004 (live streams).

## What the person gets

- **Disk, pool and UPS problems while the app is closed.** A disk starting to fail, a degraded ZFS/mdadm/Btrfs pool, or the server running on battery shows up in "while you were away" with the time it started, instead of only when the app next runs its SMART scan.
- **Health views without sudo prompts.** The Disks, Pools, Updates and UPS views can show the node's latest reading at once instead of running the scan over SSH with sudo every time.
- **"Updates available" and "reboot required" in the timeline.**
- **Log alerts with the app closed.** Error bursts, new kinds of errors and saved-search matches in the journal and containers, found on the server every 10 minutes.

Everything the node reports uses the app's own types and verdicts (vendored from app commit 8eedcf2), so existing views render it unchanged.

## 1. Turning it on

When `health` is in `caps`, read `node.info.access.health`:

- `ok`: use the node (sections 2 and 3).
- `denied`: on the node's settings page, offer: "Let the node check disks, pools, updates and the UPS while Nightgrid is closed." The button runs, with the app's existing sudo confirmation:

  ```
  sudo nightgrid-node allow health
  ```

  and shows the report's `warnings[0]`. `deny health` turns it off again.
- `off`: root disabled it in `[health]`; say so and change nothing.

UPS targets are root's to list: if the app knows the host's NUT target (`core::ups`), the settings page can show the line to add, `ups = ["ups@localhost"]` under `[health]` in `/etc/nightgrid-node/config.toml`, then `sudo systemctl restart nightgrid-node`. The app never edits that file itself.

## 2. Views

With `access.health == "ok"`, call `health.latest` when a health view opens (and on reconnect):

- **Disks:** `smart.report` is a `DisksReport`; show it with the existing view, and "checked 14:02" from `smart.at`. A **Scan now** button keeps today's SSH + sudo scan; the node rereads every hour.
- **Pools:** `pools.report` is a `PoolsReport`.
- **Updates:** `updates.report` is an `UpdatesReport`. The node only reads the local package cache, so show `note` (stale lists) and keep the app's own **Check now** (which refreshes lists over SSH) and upgrade flow unchanged.
- **UPS:** `ups[]` has one `UpsReading` per target root listed.
- A reading with `error` instead of `report`: show the error in the view's empty state ("smartctl isn't installed (install smartmontools)"), and fall back to the SSH path if the person asks to scan.

Without `health`, or with access not `ok`, keep today's behaviour exactly.

## 3. Alerts and events

The node raises `smartWarning`, `poolDegraded` and `upsOnBattery` alerts with the app's keys (see PROTOCOL, `health.latest`), honouring `smartWarning` and `poolDegraded` from the rules the app pushes with `alerts.rules.set` (0002). They arrive through `events.sync` and the `events` stream like other alerts, so no new code is needed beyond:

- Map `upsOnBattery` with key suffix `:power` to "on battery" and `:rb` to "replace battery" in alert titles if the app shows kind-specific wording.
- When the app also runs its own SMART or pool checks for the host, prefer the node's alerts while `access.health == "ok"` to avoid duplicates (same keys, so de-duplicating by `kind` + `key` is enough).

Info events with kind `other`, titled "N package updates available" and "reboot required", go in the timeline as they are.

## 4. Logs (`logs` 1)

The app's log anomaly check (M10g) and saved searches with alerts can run on the node instead of over SSH every 10 minutes:

- **Rules:** `logAnomaly` is part of the alert rules the app already pushes with `alerts.rules.set` (0002). Push it as the person set it.
- **Saved searches:** whenever saved searches change, and on connect, push the ones with alerts on whose hosts include this host: `logs.searches.set` with `{id: <saved search id as text>, name, text, regex, caseSensitive, journal, containers, unit, container, priority}`. Show a `bad_request` message on the search (a regex the node's `regex-lite` doesn't support, such as one with Unicode classes) and keep checking that one over SSH.
- **While the node handles a host** (`logs` in caps, and `logs.status.journal` or `.containers` is `ok`), skip that host in the app's own log anomaly round and saved-search round, so alerts don't double. The node's keys are the app's without the host id.
- **What broke:** where the app shows recent error lines of a source, call `logs.samples` with `journal/<identifier>` or `container/<name>`.
- **Access:** journal reads need `allow journal`, container logs `allow docker`; if a settings page shows the node's access, `logs.status` has both words.

## 5. Checks

- On a host with node 0.11.0 and `allow health`, a failing-disk fixture (or a degraded md array in a VM) appears in Disks/Pools and as an alert with the app closed.
- Unplugging a NUT UPS's mains raises "on battery" within a minute.
- With node 0.12.0, `allow journal` and `logAnomaly` on, `logger -p user.err -t demo "fatal: something new"` (after one quiet round) raises a "new error" alert for `demo`; a saved search for its text raises a "log match" alert.
- Hosts without the node, with an older node, or with `access.health` `denied` behave exactly as before.
