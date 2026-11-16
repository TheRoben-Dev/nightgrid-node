# 0007 · Actions and self-healing

Node milestone: **N4a** (nightgrid-node 0.8.0), protocol 1 and 2, capabilities `actions` 1 and `heal` 1. Status: **proposed** for the app. Builds on 0002 (events), 0003 (containers) and 0004 (live streams).

## What the person gets

- **Container buttons that work with the app's own SSH session closed elsewhere.** Restart, start and stop a container, and prune dangling images, through the node instead of `docker` over SSH, with every run logged on the server.
- **Self-healing.** "If `web` is unhealthy for 5 minutes, restart it", evaluated on the server while the app is closed. Off and dry-run until the person turns it on, capped, and it gives up (with a warning) when restarting doesn't help.
- **An audit trail** of everything the node did or was asked to do, with who asked, that the app can check for tampering.

## 1. Policy

On connect (when `actions` is in `caps`) call `actions.policy`. Only the actions in `allowed` may run on this host; the rest are root's to allow:

> "Restarting containers from Nightgrid is off on this server. Root can turn it on: add `"container.restart"` to `[actions] allow` in /etc/nightgrid-node/config.toml, then `sudo systemctl restart nightgrid-node`."

Container actions also need `node.info.access.docker == "ok"` (spec 0003).

## 2. Running an action

Where the containers view has restart/start/stop today, a host with `actions` and the action allowed calls `actions.run` instead of running `docker` over SSH:

```json
{"action": "container.restart", "target": "web"}
```

- Show a spinner; a restart can take ~10 s.
- `outcome`: `ok` → done (the `containers` stream shows the new state); `failed` → show `detail`; `refused` → show `detail` (it says what root needs to do, or the rate limit).
- Without `actions`, or with the action not allowed, keep today's SSH path.

## 3. Self-healing rules

A "Self-healing" section per node (only with `heal`):

- List of rules from `heal.rules.get`. Each row: condition ("web is unhealthy for 5 min"), action ("restart it"), a switch for **On**, and a **Dry run** toggle that starts on.
- Editor fields map 1:1 to `HealRule` (see PROTOCOL): `when` (unhealthy / down / disk full), `target` (pick a container or mount, or "any"), `forMinutes`, `action` (only `allowed` ones are selectable), `maxPerHour`, `maxPerDay`, `giveUpAfter`.
- Save sends the whole list with `heal.rules.set`; show a `bad_request` message inline.
- A quick-add on a container's menu: "Restart automatically when unhealthy" creates `{when: containerUnhealthy, target: <name>, action: container.restart}` in dry run and says so.
- Status from `heal.status`: "watching web since 14:02, 1 run" and "gave up on web" (with the warning colour).

## 4. Timeline and log

- `action` events come through `events.sync` and the `events` stream. Map to a new `timeline::EventKind::Action` (or `Unit` until it exists). Titles are ready to show ("restarted web", "dry run: would restart web", "self-healing gave up on web"); the detail starts with who asked (`app (uid 1000)` or `rule <id>`).
- An "Activity" list per node pages `actions.log` by `seq`. If `verified` is false, show "The node's action log doesn't check out; it may have been edited" in the warning colour.
- Keep the newest `hash` the app has seen per node. If a later page no longer chains to it (the record with that `seq` has another hash), show the same warning. This is the outside anchor the node can't provide on its own (decision N-030).

## Tests in the app

- `actions.policy` with nothing allowed shows the root hint and keeps the SSH buttons.
- `actions.run` fixtures for each outcome show the right message.
- Rules round-trip through `heal.rules.set` / `get` with defaults filled in, and a `bad_request` shows inline.
- `action` events from a fixture appear in the timeline with the right level.
- A log page with `verified: false`, or one whose hash for a known `seq` changed, shows the warning.
- A host without `actions`/`heal` shows no self-healing section.
