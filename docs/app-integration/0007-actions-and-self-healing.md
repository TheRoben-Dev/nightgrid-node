# 0007 · Actions and self-healing

Node milestones: **N4a** (nightgrid-node 0.8.0, `actions` 1, `heal` 1) **N4b** (0.9.0, `actions` 2) and **N4c** (0.10.0, `changes` 1). Status: **proposed** for the app. Builds on 0002 (events), 0003 (containers) and 0004 (live streams).

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

## 4. Units and power (`actions` 2)

`node.info.access.helper`:

| Value | Panel shows | Button |
|---|---|---|
| `ok` | "Root helper installed: units and power can be controlled" | **Remove** → `sudo nightgrid-node deny helper --json` |
| `denied` | "Root helper not installed" | **Install** → confirmation, then `sudo nightgrid-node allow helper --json` |

Confirmation text: "This lets the node ask a small root helper to restart, start or stop the services you list, and to reboot or power off, but only for what you allow in /etc/nightgrid-node/config.toml. Nothing is allowed until you edit that file." Run through the existing sudo flow and show `warnings`.

- **Services view:** for a unit in `actions.policy.units` with `unit.restart` (etc.) allowed and `helper: true`, Restart/Start/Stop call `actions.run` with `target` the unit name instead of `sudo systemctl` over SSH. Otherwise keep today's path.
- **Power:** Reboot and Power off (with the app's own confirmation) call `power.reboot` / `power.poweroff` when allowed. Show `detail` ("reboot in 1 minute (cancel on the host with: shutdown -c)") and mark the host as going down; the app's offline handling covers the rest.
- To list units or allow actions, show: "Edit `[actions]` in /etc/nightgrid-node/config.toml on the server, then `sudo systemctl restart nightgrid-node`."

## 5. Safe changes (`changes` 1)

Wherever the app edits a file on the host over SSH that could cut it off (compose files and `.env`, `sshd_config`, network or firewall config), and the host has `changes` with `files.rollback` allowed:

1. `changes.begin` with the files it will touch, `confirmWithinSecs` (default 120), a `note` ("Change SSH port to 2222") and, where it fits, `onRevert` (restart the unit or container that reads them).
2. Make the change as today.
3. Check it worked by reconnecting (a new SSH session, not the old one) and, for services, the unit or container is up.
4. `changes.confirm`. If the check fails, `changes.revert` at once.

If the app can't reach the host any more, it does nothing: the node reverts at the deadline. Show a countdown while pending ("Reverting in 1:42 unless confirmed") with **Keep** and **Undo** buttons, and pending changes from `changes.list` on reconnect. A path outside `[rollback] paths` comes back as `bad_request` saying so; then offer to go on without a safety net, or show: "List the folder in `[rollback] paths` in /etc/nightgrid-node/config.toml and run `sudo nightgrid-node allow helper`."

## 6. Timeline and log

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
- A change that the fixture never confirms shows the countdown and then "change reverted"; Keep calls `changes.confirm`.
- `access.helper: denied` offers Install; with `actions` 2, a listed unit's Restart goes through `actions.run`; power shows the cancel hint.
