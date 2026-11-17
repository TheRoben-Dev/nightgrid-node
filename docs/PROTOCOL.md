# Protocol

Version **1**. Rust types: [`crates/proto`](../crates/proto/src/lib.rs) (`nightgrid-node-proto`), which the app can use as a pinned git dependency.

## Framing

- A byte stream: the node's unix socket, reached through SSH streamlocal forwarding or through `nightgrid-node bridge` on an exec channel.
- UTF-8 JSON, one object per line, `\n`-terminated. A line over 4 MiB gets a `too_large` error and the connection is closed.
- The client sends requests; the node answers each with exactly one response carrying the request's `id`, in order. In version 2 the node also pushes **frames** (lines with `stream` and no `id`) for streams the client subscribed to; they can arrive between a request and its response, so clients tell them apart by the missing `id`.

```json
{"id": 1, "method": "hello", "params": {"protoMin": 1, "protoMax": 1, "client": "nightgrid/0.10.50"}}
{"id": 1, "result": {"proto": 1, "node": {...}, "caps": {"metrics": 1, "metrics.sync": 1, "node": 1}}}
{"id": 2, "method": "metrics.sync", "params": {"res": 60, "after": 1763251200}}
{"id": 2, "error": {"code": "bad_request", "message": "res must be 60, 900 or 3600"}}
```

Field names are camelCase. Unknown fields are ignored by both sides. Times are unix seconds unless a field says milliseconds.

## Errors

| `code` | Meaning |
|---|---|
| `bad_request` | Not a request, or params don't fit the method |
| `unknown_method` | No such method in this protocol version |
| `hello_required` | The first request on a connection must be `hello` |
| `unsupported_proto` | No protocol version in common |
| `capability_disabled` | The method's capability is switched off on this node |
| `too_large` | Line over 4 MiB |
| `internal` | Failed on the node; details are only in the node's journal |
| `forbidden` | This caller may not use the method (0.7.0+: `net.report` from anyone but the helper) |

Clients treat unknown codes like `internal`.

## Session

### `hello`

Must come first. Picks the newest version in both ranges.

Params: `protoMin`, `protoMax`, `client` (free text for the node's log).

Result (`Welcome`):

```json
{
  "proto": 1,
  "node": {"nodeId": "5f0c…32 hex", "version": "0.1.0", "hostname": "server01", "arch": "x86_64"},
  "caps": {"node": 1, "metrics": 1, "metrics.sync": 1}
}
```

`nodeId` is random and made when the node's database is created. It survives upgrades and plain uninstalls and changes only after `uninstall --purge`: a new id means the history started over, so the app resets its sync cursors for that host.

## Streams (protocol 2)

Versions: the node speaks 1–2 (0.4.0 and later). A client offering `protoMax: 2` gets streams; a client offering only 1 gets exactly the version 1 behaviour.

### `subscribe` / `unsubscribe`

Params: `{"streams": ["events", "metrics", "containers"]}`. Result: the streams now subscribed, `{"streams": [...]}`. Unknown stream names are `bad_request`; on protocol 1 both methods are `unknown_method`. Subscriptions end with the connection.

Frames:

```json
{"stream": "events", "data": {"seq": 42, "at": 1763290000, "kind": "container", "level": "warn", "title": "web restarted", "detail": "after exit 1"}}
{"stream": "metrics", "data": {"ts": 1763290000123.0, "cpu": 4.2, "memTotal": 8000000000, ...}}
{"stream": "containers", "data": {"access": "ok", "at": 1763290000, "containers": [...]}}
{"stream": "lagged", "data": {"stream": "metrics", "missed": 3}}
```

| Stream | `data` | When |
|---|---|---|
| `events` | an `Event`, as `events.sync` returns it (same `seq`) | as soon as it is recorded |
| `metrics` | a `Sample`, as `metrics.latest` returns it | every 2 s while anyone subscribes (the node samples faster then; back to the configured interval when nobody does) |
| `containers` | `ContainersResult`, as `containers.list` returns it | when a container's state or health, or access, changes; read every 3 s while anyone subscribes |
| `lagged` | `{stream, missed}` | the client read too slowly and `missed` frames of `stream` were dropped |

The node never buffers without bound for a slow client: each stream keeps a short queue per connection (256 events, 16 samples, 16 container lists) and drops the oldest. After `lagged` on `events`, call `events.sync` from the last `seq` seen to fill the gap. Events written while the app wasn't subscribed are not replayed by `subscribe`; sync first, then subscribe, then sync once more from the last `seq` to close the race.

## Capabilities

| Capability | Methods | Since |
|---|---|---|
| `node` 1 | `node.info` | N0 |
| `metrics` 1 | `metrics.latest`, `metrics.history` | N0 |
| `metrics.sync` 1 | `metrics.sync` | N0 |
| `events` 1 | `events.sync` | N1 |
| `uptime` 1 | `uptime.sync` | N1 |
| `disk` 1 | `disk.sync` | N1 |
| `alerts` 1 | `alerts.active`, `alerts.rules.get`, `alerts.rules.set` | N1 |
| `containers` 1 | `containers.list`; `container` events; container-down alerts | N1b |
| `logins` 1 | `login` events (with `allow journal`) | N2b |
| `files` 1 | `file` events for the paths root listed | N2b |
| `net` 1 | `net.devices`; `network` events | N3a |
| `net.dhcp` 1 | `net.dhcp`; DHCP `network` events; device `name` (with `allow netwatch`) | N3b |
| `actions` 1 | `actions.policy`, `actions.run`, `actions.log`; `action` events | N4a |
| `actions` 2 | `unit.*` and `power.*` actions through the root helper; `units`, `helper` in the policy | N4b |
| `heal` 1 | `heal.rules.get`, `heal.rules.set`, `heal.status` | N4a |
| `changes` 1 | `changes.begin`, `changes.confirm`, `changes.revert`, `changes.list` | N4c |
| `health` 1 | `health.latest`; SMART, pool and UPS alerts; update events (with `allow health`) | N5a |
| `logs` 1 | `logs.status`, `logs.samples`, `logs.searches.get`, `logs.searches.set`; log alerts | N5b |

### `node.info` (`node` 1)

```json
{
  "node": {...},
  "started": 1763290000,
  "intervalSecs": 10,
  "retention": {"minuteDays": 14, "rollupDays": 90, "hourlyDays": 400},
  "coverage": [
    {"res": 60, "points": 20160, "oldest": 1762080000, "newest": 1763289960},
    {"res": 900, "points": 1344, "oldest": 1762080000, "newest": 1763289000},
    {"res": 3600, "points": 336, "oldest": 1762077600, "newest": 1763287200}
  ],
  "rssBytes": 2633728,
  "dbBytes": 3145728,
  "access": {"docker": "ok", "journal": "denied", "netwatch": "denied", "helper": "denied", "health": "ok"}
}
```

`access` (0.5.0+; `netwatch` 0.7.0+; `helper` 0.9.0+, `ok` when the root helper's socket is installed; `health` 0.11.0+, `ok` when the health helper's socket is installed; `ok` while the helper has reported in the last 2½ minutes) says which optional access root has granted: `ok`, `denied` (root hasn't run `nightgrid-node allow <name>`), `absent` (nothing to read on this host) or `off` (disabled in the config).

### `metrics.latest` (`metrics` 1)

The newest sample, or `null` before the first one. Same shape as the app's `metrics::Sample` (`ts` in milliseconds, `cpu`/`cores` busy %, `mounts`, `net`, `disks`, `temps`, `cpuTemp`, `gpus`, …). The first sample after a start has `cpu: null` and zero rates, like the app's.

### `metrics.history` (`metrics` 1)

Params: `from`, `to` (unix seconds, inclusive), optional `res` (60, 900 or 3600). Without `res` the tier is picked like the app does: ≤ 2 days minute points, ≤ 31 days 15-minute rollups, longer hourly.

Result: `{"res": 60, "points": [Point…], "truncated": false}`, oldest first, at most 5000 points (`truncated` when the range held more).

`Point` matches the app's `metrics::history::Point` (`ts` bucket start in seconds, `cpu`, `mem`, `swap`, `load1`, `disk`, `netRx`, `netTx`, `diskRead`, `diskWrite`, `temp`, `gpu`, `gpuMem`, `gpuTemp`, `gpuPower`, each nullable) plus `n`: samples in a minute point, minutes in a rollup.

### `metrics.sync` (`metrics.sync` 1)

Params: `res` (60, 900 or 3600), `after` (cursor, unix seconds), optional `limit` (1–5000, default 5000).

Result:

```json
{"res": 60, "points": [Point…], "next": 1763289960, "more": false, "oldest": 1762080000}
```

- Only **closed** buckets (`ts + res <= now`), with `ts > after`, oldest first. A rollup that is still collecting minutes comes in a later sync, final.
- Call again with `after = next` while `more` is true.
- `oldest` is the oldest bucket this tier still holds. If it is newer than the client's cursor, the span in between has aged out of this tier; fill it from a coarser one.

### Cursor syncs (`events`, `uptime`, `disk`)

`events.sync`, `uptime.sync` and `disk.sync` take `{"after": cursor, "limit"?: n}` (limit 1–2000, default 2000) and return rows oldest first plus `next` (pass it as `after` next time) and `more`. Like `metrics.sync`, the cursor lives in the app and repeated calls are harmless.

### `events.sync` (`events` 1)

The cursor is the event's `seq`, which only grows.

```json
{"events": [Event…], "next": 42, "more": false, "oldest": 3}
```

`oldest` is the oldest `seq` still kept (90 days, at most 20 000 events). If it is above `after + 1`, events in between were pruned.

```json
{"seq": 41, "at": 1763290000, "kind": "alert", "level": "warn",
 "title": "DISK FULL · server01", "detail": "/srv is 91% full (40 GB free)",
 "alert": {"id": 3, "key": "disk:/srv", "kind": "diskFull", "severity": "warn",
           "title": "server01", "detail": "/srv is 91% full (40 GB free)", "since": 1763290000, "resolved": null}}
```

- `kind`: `alert` (fired, worse, or changed: `level: info` when it got better), `recovered` (`level: ok`, `alert.resolved` set), `boot`, `outage`, `container` (N1b), `login` and `file` (N2b), `network` (N3a); unknown kinds read as `other`.
- `login`: "alice logged in from 10.0.0.5" (`detail`: "ssh, publickey"), or failed logins summed per minute: "failed login for root from 192.0.2.7" / "12 failed logins from 3 addresses" (`detail`: users and the top addresses; `warn` from 10 failures). Needs `allow journal`.
- `file`: "changed /etc/ssh/sshd_config", "added /etc/cron.d/backup", "removed …". `detail` lists size, a short SHA-256, mode and owner changes, never contents; "(while the node was not running)" when found at start. `warn` for SSH, sudoers, passwd, shadow and group files.
- `network`: "new device 192.168.1.23" (`detail`: "aa:bb:cc:dd:ee:ff on eth0"), "N new devices" (more than 20 in one read), "device moved 192.168.1.20 → 192.168.1.21" (a new lease), "address 192.168.1.50 answers from a new MAC" (`detail`: "old → new"), "IP conflict on 192.168.1.50" (`warn`: the address keeps switching between MACs; `critical` on the gateway), "gateway 192.168.1.1 answers from a new MAC" (`warn`: a replaced router, or ARP spoofing), and one "network watch started" on a fresh install. With `allow netwatch` (N3b): "DHCP server 192.168.1.1" (the first one seen, or one root listed), "another DHCP server on the LAN: 192.168.1.66" (`warn`: possibly rogue), "DHCP server 192.168.1.1 hands out a different router / DNS / router and DNS" (`warn`).
- `level`: `info`, `ok`, `warn`, `critical`.
- `outage` events carry `"outage": {"start", "end", "state": "down" | "asleep"}`. Titles: `Down` (the host or node went away without a clean stop), `Restarted` (clean stop, then a new boot), `Suspended` (the host slept).
- Title and detail go through the app's secret filter and are capped (200 / 600 characters).
- Alert keys have no host segment: node `disk:/srv` is app `disk:<hostId>:/srv`, `fill:/srv` is `fill:<hostId>:/srv`.

### `uptime.sync` (`uptime` 1)

Cursor: hour start (unix seconds). Rows are closed hours only, kept 400 days.

```json
{"hours": [{"hour": 1763287200, "up": 3540, "down": 0, "asleep": 60}], "next": 1763287200, "more": false}
```

Seconds the node saw the host up, down (no clean stop) or asleep (suspended, or cleanly restarted). Time the node itself was stopped while the host stayed up is not counted at all, so `up + down + asleep` can be under 3600; the app fills the rest from what it saw itself.

### `disk.sync` (`disk` 1)

Cursor: hour start. The last usage per mount in each closed hour, kept 90 days, same as the app's disk-usage table.

```json
{"rows": [{"mount": "/srv", "ts": 1763287200, "used": 412000000000, "size": 960000000000}], "next": 1763287200, "more": false}
```

A page never splits an hour, so it can hold a few rows over `limit`.

### `alerts.active` (`alerts` 1)

The alerts active right now, worst and newest first: `[Alert…]`.

### `alerts.rules.get` / `alerts.rules.set` (`alerts` 1)

The node evaluates the rules it has implemented with the app's field names. `set` takes the whole object (missing fields reset to defaults, unknown fields are ignored, so the app can send its full `AlertSettings`) and returns the stored rules.

```json
{"enabled": true, "diskFull": true, "diskPercent": 90, "diskForecast": true, "forecastDays": 14}
```

`diskPercent` must be 50–99 and `forecastDays` 1–90, else `bad_request`. Rules are stored in the node's state directory and survive restarts and upgrades.

### `containers.list` (`containers` 1)

What the watcher last read (every 15 s by default):

```json
{"access": "ok", "at": 1763290000, "containers": [
  {"id": "0123456789ab", "name": "web", "image": "nginx:1.27", "state": "running",
   "status": "Up 2 hours (healthy)", "health": "healthy", "project": "site"}
]}
```

- `access`: `ok`; `off` (disabled in the config); `absent` (no engine socket); `denied` (root has not run `nightgrid-node allow docker`); `error` (with `message`). The app uses its own Docker-over-SSH path whenever it isn't `ok`.
- Container events reach `events.sync` with `kind: container`, the app's titles and levels ("web restarted", "db exited", "big ran out of memory", "api unhealthy"). They are written about two minutes after they happen, so a crash followed by a restart is one entry.
- Container-down alerts use keys `docker:<name>` (app: `docker:<hostId>:<name>`) and the `containerDown` rule. Only containers seen running alert when they stop; a clean exit (0 or 143) forgets them.

### `net.devices` (`net` 1)

The LAN inventory from the kernel's IPv4 neighbour table, read every 30 s by default. Most recently seen first, at most 1000.

```json
{"at": 1763290000, "gateway": "192.168.1.1", "devices": [
  {"mac": "aa:bb:cc:dd:ee:ff", "ip": "192.168.1.23", "iface": "eth0",
   "firstSeen": 1763000000, "lastSeen": 1763290000, "online": true}
]}
```

- `at` is 0 before the first read; `gateway` is left out when there is no IPv4 default route.
- `online`: in the table at the last read. The table only holds devices this host exchanged traffic with recently, so `online: false` means "not seen lately", not "off".
- No vendor: the app looks the MAC up in its own OUI table (`netmap::mac_vendor`).
- Container bridges (`docker0`, `br-*`, `veth*`, …) and interfaces root lists in `[network] ignore_ifaces` are left out. Devices unseen for 400 days are forgotten.

- `name` (0.7.0+, with `allow netwatch`): the host name the device asked DHCP for. Left out when unknown.

### `net.dhcp` (`net.dhcp` 1)

DHCP servers the network watch helper has seen (needs `sudo nightgrid-node allow netwatch`). Servers unseen for 30 days are dropped.

```json
{"watching": true, "servers": [
  {"ip": "192.168.1.1", "mac": "aa:bb:cc:dd:ee:01", "firstSeen": 1763000000, "lastSeen": 1763290000,
   "router": "192.168.1.1", "dns": ["192.168.1.1"], "subnet": "255.255.255.0", "leaseSecs": 86400, "trusted": false}
]}
```

- `watching`: the helper reported in the last 2½ minutes. `false` with an empty list means it isn't allowed or isn't running.
- `trusted`: root listed the address in `[network] dhcp_servers`.
- A server seen only in a client's request has no `mac`, `router` or `dns` until one of its own replies is seen.

### `net.report` (internal)

How the helper hands over what it saw: `{"arp": [{ip, mac, iface}], "dhcp": [DhcpSeen]}`. Only a connection from the service's own account may call it; anyone else gets `forbidden`. Apps never call it.

### `actions.policy` (`actions` 1)

What root allows in `[actions]`, so the app can grey out the rest:

```json
{"allowed": ["container.restart", "unit.restart"], "known": ["container.restart", "container.start", "container.stop", "images.prune",
 "unit.restart", "unit.start", "unit.stop", "power.reboot", "power.poweroff"], "maxPerHour": 20,
 "units": ["nginx.service"], "helper": true}
```

`units` (0.9.0+) are the systemd units root listed in `[actions] units`; `helper` says the root helper is installed (`allow helper`). Unit and power actions need both the action in `allowed` and the helper.

### `actions.run` (`actions` 1)

Params: `{"action": "container.restart", "target": "web", "dryRun": false}`. `target` is the container name for `container.*`, the full unit name (from the policy's `units`) for `unit.*`, and absent for `images.prune` and `power.*`. Answers with the logged record (see `actions.log`); a refusal is a normal answer with `outcome: "refused"` and the reason in `detail`, not an error. The node refuses an action root didn't allow, an unknown container, any action without engine access, and real runs past `max_per_hour` (all sources together; dry runs and refusals don't count). Container actions can take up to ~10 s; the node answers other clients meanwhile.

Unit and power actions (`actions` 2) are carried out by the root helper, which re-checks root's config itself. `power.reboot` and `power.poweroff` are scheduled one minute ahead with `shutdown`, so the record and its event reach the app first; root can cancel with `shutdown -c`. Self-healing rules can't use unit or power actions.

Outcomes: `ok`, `dryRun`, `failed` (the engine said no), `refused`. Every call also adds an `action` event: "restarted web" (`info`), "dry run: would restart web" (`info`), "could not restart web" / "refused to restart web" (`warn`). The event detail starts with who asked: `app (uid 1000)` or `rule <id>`.

### `actions.log` (`actions` 1)

The audit log, by cursor like `events.sync` (`after`, `limit` up to 500):

```json
{"records": [{"seq": 7, "at": 1763300000, "source": "rule web-unhealthy", "action": "container.restart",
  "target": "web", "dryRun": false, "outcome": "ok", "detail": "restarted web", "hash": "9f2c…"}],
 "next": 7, "more": false, "verified": true}
```

Each record's `hash` is SHA-256 over the previous record's hash and its own fields, so an edited or deleted row breaks the chain. `verified` says the returned page chains correctly (and onto the record before it). The node keeps the last 100 000 records.

### `heal.rules.get` / `heal.rules.set` (`heal` 1)

`{"rules": [HealRule]}`, at most 32. Set replaces all rules and answers with them as stored (defaults filled in).

```json
{"id": "web-unhealthy", "enabled": false, "dryRun": true, "when": "containerUnhealthy", "target": "web",
 "forMinutes": 5, "action": "container.restart", "maxPerHour": 3, "maxPerDay": 10, "giveUpAfter": 3}
```

- `when`: `containerUnhealthy` (health check says unhealthy), `containerDown` (the container-down alert is active and it isn't running), `diskFull` (the disk-full alert is active for the mount). `target` is a container name or mount, or `*`.
- `container.*` actions need a container condition and act on the matching container; `images.prune` fits any condition.
- Defaults: **off and dry-run**. Ranges: `forMinutes` 1-1440, `maxPerHour` 1-60, `maxPerDay` 1-500, `giveUpAfter` 1-20. Ids are 1-40 letters, digits, `-`, `_`, unique.

Per rule and target, the node runs the action once the condition has held `forMinutes`, waits `forMinutes` again after each run, keeps to the hour and day caps (which survive the condition flapping), and after `giveUpAfter` runs without the condition clearing it stops and adds a `warn` `action` event "self-healing gave up on <target>". When the condition clears, its count starts over. Runs go through `actions.run`'s checks, so a rule can never do more than root allowed.

### `heal.status` (`heal` 1)

```json
{"enabled": 1, "tracking": [{"rule": "web-unhealthy", "target": "web", "since": 1763300000, "attempts": 1, "lastRun": 1763300300, "gaveUp": false}]}
```

### `changes.begin` (`changes` 1)

Snapshots files before the app changes them and arms a dead-man timer: unless the app calls `changes.confirm` in time, the node puts the files back (even across a crash or reboot of the node). Needs `files.rollback` in `[actions] allow`, the paths under `[rollback] paths`, and the root helper.

```json
{"paths": ["/etc/ssh/sshd_config"], "confirmWithinSecs": 120, "note": "move SSH to port 2222",
 "onRevert": {"action": "unit.restart", "target": "ssh.service"}}
```

- `paths`: 1-32 absolute file paths, each at most 1 MiB. A file that doesn't exist yet is removed on revert. Symlinks, other file types and anything outside root's list are refused, by the helper itself.
- `confirmWithinSecs`: 30-3600.
- `onRevert` (optional): an allowed `container.*` or `unit.*` action run after the files are back.
- At most 8 changes wait at once.

Answers with the `Change`; errors (refusals, a path not allowed) are `bad_request` with the reason, and are logged too.

```json
{"id": "5cc513397f706ff0", "created": 1763300000, "deadline": 1763300120, "paths": ["/etc/ssh/sshd_config"],
 "onRevert": {"action": "unit.restart", "target": "ssh.service"}, "note": "move SSH to port 2222",
 "state": "pending", "detail": "saved 1 file"}
```

`state`: `pending`, `confirmed`, `reverted`, `failed` (with `detail`; a failed revert keeps the snapshot in `/var/lib/nightgrid-node-rollback/<id>` for root). `finished` is set once it isn't pending.

### `changes.confirm` / `changes.revert` (`changes` 1)

Params `{"id": "..."}`. Confirm keeps the change and drops the snapshot; revert puts the files back now and runs `onRevert`. Both answer with the updated `Change`; a change that isn't pending is `bad_request`. Every step is an `action` event ("change started / confirmed / reverted: \"note\"", "could not revert …" as `warn`) and an `actions.log` record with action `files.rollback` and the change id as target.

### `changes.list` (`changes` 1)

`{"changes": [Change]}`: pending ones first, then the latest 50 others.

### `health.latest` (`health` 1)

What the health helper last read. Each reading is `{"at": <unix s>, "report": {...}}` or `{"at": ..., "error": "..."}`; a kind not read yet is left out.

```json
{
  "access": "ok",
  "smart": {"at": 1763290060, "report": {"disks": [Disk], "health": "ok"}},
  "pools": {"at": 1763290000, "report": {"pools": [Pool], "health": "warn"}},
  "updates": {"at": 1763290030, "report": {"manager": "apt", "updates": [PackageUpdate], "rebootRequired": false, "rebootReasons": [], "note": null}},
  "ups": [{"target": "ups@localhost", "at": 1763290080, "reading": UpsReading}]
}
```

`Disk`, `Pool`, `PackageUpdate` and `UpsReading` are the app's own types (`core::system::smart`, `core::system::pools`, `core::system::packages`, `core::ups`, vendored from app commit 8eedcf2), so the app can render them with its existing views. `access` is `ok`, `denied` (root hasn't run `allow health`) or `off` (`[health] enabled = false`).

Schedule: pools every 5 minutes, SMART every hour (first 60 s after start, so a start doesn't spin every disk up at once), updates every 6 hours (first after 30 s), UPS every 30 s. Update checks only read the package manager's local cache (`apt list --upgradable`, `dnf -C check-update`, `pacman -Qu`, rpm-ostree's staged deployment); `note` says when the lists are stale.

Alerts (`alerts.active`, `alert` events), with the app's keys and switches:

| Kind | Key | When | Switch |
|---|---|---|---|
| `smartWarning` | `smart:<device>` | a disk's health is `warn` (`warn`) or `bad` (`critical`) | `smartWarning` |
| `poolDegraded` | `pool:<Kind>:<name>` | a pool is `warn` or `bad` | `poolDegraded` |
| `upsOnBattery` | `ups:<target>:power` | on battery (`warn`); low battery or under 5 minutes left (`critical`) | always (with `enabled`) |
| `upsOnBattery` | `ups:<target>:rb` | the UPS asks for a new battery (`warn`) | always |

Info events (`other`): "N package update(s) available" when the count goes from zero to some, and "reboot required" when it becomes required.

### `logs.status` (`logs` 1)

The log watcher's last round:

```json
{
  "at": 1763290600, "everySecs": 600, "anomaly": true,
  "journal": "ok", "containers": "denied",
  "sources": [{"source": "journal/nginx", "errors": 3, "newPatterns": ["upstream timed out (#) while …"]},
              {"source": "container/web", "errors": 41, "burstUsual": 6.5}],
  "searches": [{"id": "s1", "hits": 2}]
}
```

`journal` and `containers` are the access words of `node.info` (`ok`, `denied`, `absent`, `off`). `sources` lists what the anomaly check followed (only with `anomaly`, which is `logAnomaly` in the alert rules); sources are `journal/<identifier>` and `container/<name>`, at most 64.

### `logs.samples` (`logs` 1)

Params `{"source": "journal/nginx"}` → `{"lines": [...]}`: up to 40 recent error lines of that source, newest last, without lines that look like they hold a secret (the app's `redact::line_has_secret`). Kept in memory only.

### `logs.searches.get` / `logs.searches.set` (`logs` 1)

`{"searches": [LogSearch]}`, at most 32, replacing the whole list. `LogSearch` is the app's saved search with alerts on, minus hosts and time range: `id` (1-64 of `A-Za-z0-9-_.`, unique), `name`, `text` (≤500; empty matches every line), `regex`, `caseSensitive`, `journal`, `containers`, `unit`, `container` (name contains), `priority` (0-7, most verbose). A bad regular expression is `bad_request`. Regular expressions use the `regex-lite` syntax (Rust `regex` without Unicode classes) and run in linear time.

Alerts, every round, with the app's keys minus the host id:

| Kind | Key | When |
|---|---|---|
| `logBurst` | `logs:<source>:burst` | at least 20 error lines and 4× the usual count (`anomaly`) |
| `logNewError` | `logs:<source>:new` | an error pattern not seen before in that source (`anomaly`) |
| `logMatch` | `logsearch:<id>` | a saved search matched lines since the last round |

A finding stays active for at least 150 s, then clears at the next round without it.

## Planned (not in version 1 yet)

| Capability | Methods | Milestone |
|---|---|---|
| `checks` | `checks.set`, `checks.latest` | N7 |
