# 0001 · Install the node and sync metric history

Node milestone: **N0** (nightgrid-node 0.1.0, protocol 1). Status: **proposed** for the app.

This is a spec for the Nightgrid app's own project. Nothing here changes how the app behaves on hosts without the node: every step below is skipped when the node is absent, too old, unreachable or lacks a capability.

## What the person gets

- A **Nightgrid Node** section in a host's settings: what it is, what it can and can't do (read-only, no ports, no outbound network), and **Install** / **Update** / **Uninstall** buttons.
- After installing, graphs, long-range charts, forecasts and SLA reports for that host have no gaps for the time the app was closed, as long as the server was up.

## 1. Detecting the node

On every pooled connection to a host (no prompt, like D-038), once:

1. Open a `direct-streamlocal` channel to `/run/nightgrid-node/node.sock` (the Docker transport already does this, `core/src/docker/transport.rs`).
2. If that fails with "administratively prohibited" or similar, open an exec channel running `nightgrid-node bridge`. If the socket answers "permission denied" (the SSH user isn't in the group yet in this login), try `sg nightgrid-node -c 'nightgrid-node bridge'`.
3. Send `hello` with `protoMin: 1, protoMax: 1, client: "nightgrid/<version>"`.

Per host, keep a `NodeState` in memory:

| State | When | UI |
|---|---|---|
| `Absent` | no socket and `command -v nightgrid-node` is empty | "Not installed" + Install |
| `NoAccess` | socket exists, permission denied, `sg` also failed | "Installed, but `<user>` isn't allowed" + Fix (re-run install with `--user`) |
| `Ready { nodeId, version, caps }` | `hello` succeeded | version, history coverage from `node.info`, Update if older than the app's bundled expectation |
| `Incompatible` | `unsupported_proto` | "Update the node" |

Types: depend on `nightgrid-node-proto` as a pinned git dependency (`git = "https://github.com/TheRoben-Dev/nightgrid-node", tag = "v0.1.0"`), or copy the types; the JSON is documented in [PROTOCOL.md](../PROTOCOL.md). `Point` deserializes into the app's own `metrics::history::Point` (the extra `n` field is ignored).

Suggested home: `core/src/node/` (`client.rs` for the channel + NDJSON framing, `sync.rs` for the algorithm below, `install.rs` for the flows), `app/src/node_cmd.rs` for commands, `stores/node.svelte.ts` and a `NodePanel.svelte` in host settings.

## 2. Install and update

Confirmation names the host and lists what will happen (security rule 5). Then:

1. `uname -m` → `x86_64` or `aarch64` (anything else: "not supported").
2. Get the release binary `nightgrid-node-v<ver>-<arch>-linux` from this repo's GitHub Releases (the same origin class as the app's own updater, rule 7), or let the person pick a local file. Verify the SHA-256 against `SHA256SUMS` from the same release (decided 2026-11-16: checksums plus GitHub build provenance are enough for now). If minisign signatures are added later (`.minisig` next to each file), the app also verifies them against a node public key pinned in the app, separate from its updater key. Cache verified binaries in the app data dir. Never upload an unverified file.
3. `mktemp -d` on the host (mode 0700), upload over SFTP, `chmod 700`.
4. Run, through the existing `sudo::run` flow (password on stdin, D-048):
   `<tmp>/nightgrid-node install --json --user <ssh login user>`
5. Parse the JSON report (`action`, `version`, `steps[]`, `users[]`, `warnings[]`), show the steps, remove the temp dir.
6. Drop and reopen the pooled connection (or rely on the `sg` fallback) so the new group membership applies, then detect again.

Update is the same flow with the newer binary; history and node id are kept.

## 3. Uninstall

Confirmation with a checkbox **"Also delete its history on the server"** (→ `--purge`, which also removes the account and gives the next install a new node id).

`sudo nightgrid-node uninstall --json [--purge]`. If the binary is missing (`command -v` empty) but the unit exists, run the fallback script [dist/uninstall.sh](../../dist/uninstall.sh) (embed it in the app; it uses only `systemctl`, `rm`, `userdel`, `groupdel`).

Afterwards the app forgets the node state and its sync cursors for that host.

## 4. Metric history sync

Runs after each successful `hello` where `caps["metrics.sync"] >= 1`, one host at a time, in the background, never prompting.

### Storage changes in the app

- Migration: `node_sync (host_id INTEGER REFERENCES hosts(id) ON DELETE CASCADE, node_id TEXT, res INTEGER, cursor INTEGER, PRIMARY KEY (host_id, res))` (history, not config: not in profile exports).
- `Store::merge_rollup_point(host_id, res, point)`: insert a 900 or 3600 row with the node's `n`, and on conflict **replace it only if the node's `n` is larger** (the node ran the whole bucket, the app maybe a part of it):
  `INSERT … ON CONFLICT (host_id, res, ts) DO UPDATE SET <cols> = excluded.<cols>, n = excluded.n WHERE excluded.n > metric_points.n`.
- Minute points go through the existing `insert_metric_point`, unchanged (duplicates are ignored, rollups are folded in).

### Algorithm

```
node_id changed since last time → delete node_sync rows for this host
c60 = cursor(60) or (now - 400 days)
page = metrics.sync(res=60, after=c60, limit=5000)
oldest60 = page.oldest            # where the node's minute tier starts
if oldest60 is set and c60 < oldest60 - 60:
    # The app was away longer than the node keeps minutes: fill the older
    # span from rollups, but only buckets that end before the minutes begin,
    # so no minute is ever counted twice.
    for res in [3600, 900]:
        bound = floor(oldest60 / res) * res
        c = cursor(res) or (now - 400 days)
        loop metrics.sync(res, after=c):
            for p in points with p.ts + res <= bound: merge_rollup_point(host, res, p)
            c = next; save cursor(res)
        until !more or next >= bound
loop:
    for p in page.points: insert_metric_point(host, p)
    save cursor(60) = page.next          # in the same transaction as the inserts
    if !page.more: break
    page = metrics.sync(res=60, after=page.next, limit=5000)
```

Notes:

- Only closed buckets are ever served, so a point never changes after it has been synced.
- Run the inserts on the store's blocking thread in batches (one transaction per page) so the UI stays smooth; 5000 points is about 3.5 days of minutes.
- The app keeps its own minute retention (default 7 days); minute points older than that are pruned by the app as usual, the rollups stay.
- Nothing changes in `HistoryChart` or the SLA code: they read the app's store, which now has the node's points.

### Live samples (optional in N0)

With `caps.metrics >= 1` the poller may call `metrics.latest` instead of running `COLLECT` over SSH (same `Sample` shape; the node samples every 10 s, so keep the SSH script for the 2 s "visible" rate until protocol 2 streams, see [0004](0004-live-streams.md)).

## 5. Settings

- Per host: **Use Nightgrid Node when installed** (default on). Off = behave as if absent.
- Global: none needed in N0.

## 6. Testing

- Unit: the sync algorithm against a fake node (in-memory list of points with a minute-retention cutoff), covering first sync, resume, node id change, gap longer than minute retention, overlap with app-recorded minutes, and a rollup the app only half-filled.
- Integration: the app's SSH test environment can run the real binary without root: `nightgrid-node run --socket /tmp/n.sock --data-dir /tmp/n --config /dev/null` inside the `openssh-server` container, and the client connects with streamlocal and with `nightgrid-node bridge --socket /tmp/n.sock`.
- `cargo test` in this repo exercises the node side (protocol, sync paging, closed-bucket rule, install flows) and CI runs real installs on systemd.

## 7. Done when

- Install, update and uninstall work from the app on Debian/Ubuntu, Fedora (incl. Atomic/Bazzite) and Proxmox VE hosts, with sudo by password or NOPASSWD.
- With the app closed for a day, reopening it fills that day's minute graph for the host within a few seconds of connecting, and a month-long chart has no gap.
- A host without the node behaves exactly as before.
