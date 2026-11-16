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

## Planned (not in version 1 yet)

| Capability | Methods | Milestone |
|---|---|---|
| `events` | `events.sync {after: seq}` → `{events, next, more}` | N1 |
| `uptime` | `uptime.sync {after}` → hours and outages | N1 |
| `alerts` | `alerts.rules.get/set`, `alerts.active` | N1 |
| `checks` | `checks.set`, `checks.latest` | N1 |
| `logs` | `logs.rules.set`, `logs.findings` | N2 |
| protocol 2 | `subscribe {stream}` / `unsubscribe`, pushed `{"stream": …, "data": …}` frames | N4 |
| `act.*` | typed actions, each opt-in by root, every call audited | N5 |
