# Protocol

Version **1**. Rust types: [`crates/proto`](../crates/proto/src/lib.rs) (`nightgrid-node-proto`), which the app can use as a pinned git dependency.

## Framing

- A byte stream: the node's unix socket, reached through SSH streamlocal forwarding or through `nightgrid-node bridge` on an exec channel.
- UTF-8 JSON, one object per line, `\n`-terminated. A line over 4 MiB gets a `too_large` error and the connection is closed.
- The client sends requests; the node answers each with exactly one response carrying the request's `id`. In version 1 answers come in order; clients should still match on `id` (version 2 adds pushed stream frames between answers).

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
  "dbBytes": 3145728
}
```

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

- `kind`: `alert` (fired, worse, or changed: `level: info` when it got better), `recovered` (`level: ok`, `alert.resolved` set), `boot`, `outage`, `container` (N1b), unknown kinds read as `other`.
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

## Planned (not in version 1 yet)

| Capability | Methods | Milestone |
|---|---|---|
| `containers` | container events into `events` (needs the opt-in docker group) | N1b |
| `checks` | `checks.set`, `checks.latest` | N1b |
| `logs` | `logs.rules.set`, `logs.findings` | N2 |
| protocol 2 | `subscribe {stream}` / `unsubscribe`, pushed `{"stream": …, "data": …}` frames | N4 |
| `act.*` | typed actions, each opt-in by root, every call audited | N5 |
