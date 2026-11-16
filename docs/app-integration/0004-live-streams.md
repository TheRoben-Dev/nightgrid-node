# 0004 · Live streams (protocol 2)

Node milestone: **N2a** (nightgrid-node 0.4.0, protocol 2). Status: **proposed** for the app. Builds on 0001–0003.

A node from 0.4.0 speaks protocol 1 and 2. The app keeps working with older nodes, and with this node over protocol 1, unchanged.

## What the person gets

- **Live tiles without SSH polling.** On a host with the node, the metric tiles of a visible host update every 2 s from the node instead of from running the `COLLECT` script over SSH, so there's no shell spawned on the server every 2 s.
- **Alerts and container changes arrive instantly.** A disk alert, a crashed container or an outage shows up the moment the node records it, while the app is open, not at the next poll.

## 1. Hello

Send `protoMin: 1, protoMax: 2`. If `proto` in the answer is 2, streams are available. Keep one node connection per host (0001) and use it for both requests and streams.

## 2. Reading the connection

Frames and responses share the connection. Read lines in one task:

- A line with `id` is a response; route it to the waiting request by `id`.
- A line with `stream` and no `id` is a frame; route it by `stream`.

Frames can arrive between a request and its response. Requests stay one-at-a-time per connection as in 0001, but the reader must not assume the next line is the response.

## 3. What to subscribe to, and when

| Stream | Subscribe while | Replaces | Handling |
|---|---|---|---|
| `events` | the node connection is up | the 15-minute `events.sync` re-sync from 0002 | Insert like a synced event (0002 §1), then advance the `events` cursor to `seq`. |
| `metrics` | the host is visible (`hub().is_visible(host_id)` in `app/src/metrics_cmd.rs`) | `COLLECT` over SSH at `metrics_interval_secs` | Feed `data` to the same path a parsed `COLLECT` sample takes; it is the same `Sample` shape. Unsubscribe when the host is no longer visible; the background rate stays on SSH or `metrics.latest`. |
| `containers` | a host's Docker page is open and `containers.list` says `access: ok` | `docker ps` over the Docker socket every few seconds | Replace the list's state and health; keep using the Docker API over SSH for details, logs and stats. |

Ordering on connect, so no event is missed:

1. `events.sync` from the cursor until `more` is false.
2. `subscribe {"streams": ["events"]}`.
3. `events.sync` once more from the cursor (closes the gap between 1 and 2). Duplicates are harmless because `seq` is unique.

## 4. Lagged and lost connections

- `{"stream": "lagged", "data": {"stream": "events"}}` → run `events.sync` from the cursor.
- `lagged` for `metrics` or `containers` → nothing; the next frame is current.
- Connection lost → fall back to SSH polling at once, reconnect with backoff (as in 0001), then repeat the order above.

## 5. Polling cost

The node samples every 2 s only while a `metrics` subscriber is connected and returns to its 10 s interval within a second after. Unsubscribe as soon as the host is hidden; don't keep a subscription only to keep a connection warm.

## Tests in the app

- A response and a frame interleaved on one connection are both delivered.
- `proto: 1` from an older node → no subscribe, SSH polling as today.
- `lagged` on `events` triggers exactly one `events.sync`.
- Hiding a host unsubscribes `metrics` and SSH polling continues at the background rate.
