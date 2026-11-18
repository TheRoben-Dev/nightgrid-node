# 0020 · Hub: fleet sync and notifications (future)

Node milestones: **H1** (built in 0.26.0: setup and `fleet` 1), **H2** (notifications, not built). Status: **future** for the app, a proposal for after the app's compatibility work on specs 0001 to 0019. Design: [HUB.md](../HUB.md).

This spec is **additive and versioned**. It changes nothing in 0001 to 0019, in the Lite socket methods, in the protocol or in the peer link. An app that never implements it keeps working exactly as today, with or without a hub on the network, and a hub never needs the app to implement it to stay harmless.

## What the person gets

- **"Make this host the hub"** for a site (a host group marked as one network). One hub per site.
- With a hub, the network is watched as a whole while the app is closed: a host that goes down, and every alert from every member, reach the person's phone or inbox through ntfy, Discord, a webhook or email.
- The app syncs the whole site from the hub in one connection, instead of one per host, and falls back to direct sync per host whenever the hub is away, without gaps or duplicates.

## 1. When to offer it

- The site has at least two hosts with Lite (`hello` capability `peers` ≥ 1 and, on every member, `hub.member` ≥ 1).
- The candidate host has Lite installed and runs systemd on Linux with at least 1 GB of RAM (from `metrics.latest`). Suggest the host that is always on (best uptime over 30 days), reads the UPS (`health.latest.ups`), and has the most free memory. The person confirms.
- If a site already has a hub, show it instead and don't offer a second one.

Members on Lite releases without `hub.member` are shown as "needs Node 0.26 or newer" and stay synced directly.

## 2. Setting up a hub

All root steps go through the app's existing sudo flow and confirmation rules.

1. Upload and verify the `nightgrid-hub` binary from the same release as Lite (checksum and provenance, as in 0001 and 0015), then run `sudo <tmp>/nightgrid-hub install --json` on the chosen host. Show the report's warnings (state directory on an SD card, no disk encryption).
2. Run `sudo nightgrid-hub allow collect --json`. Its report gives the collector's certificate fingerprint and the hub's `hubId`.
3. For each member:
   1. Run `sudo nightgrid-node allow peers --json` if the link is off (as in 0016).
   2. Pair the member with the hub exactly as in 0016 §2 (the hub is a normal peer: id `hubId`, the collector's fingerprint, the hub host's tailnet address). The hub's collector answers the member's existing pings, so the member's Linked hosts list shows the hub like any peer. For the other members nothing is added to the hub host's own Lite `peers.set`; the hub host itself is a member too and is paired with its hub the same way (its collector address on the same host).
   3. Call `hub.member.set {"hubId": "…"}` on the member.
   4. Run `sudo nightgrid-node serve add sync --json` on the member (root consent to serve the hub; `serve remove sync` undoes it).
4. On the hub's socket, call `fleet.members.set` with every member's `{nodeId, name, address, fingerprint}`.
5. Within two minutes `fleet.overview` lists each member with `state: "ok"`. Members that stay `consent` or `unreachable` show the reason the hub gives.

Removing the hub reverses these steps: `fleet.members.set []`, `hub.member.set {"hubId": null}` and unpair on each member, `serve remove sync`, then `sudo nightgrid-hub uninstall`.

## 3. The hub socket

`/run/nightgrid-hub/hub.sock`, reached over SSH exactly like the Lite socket (0001 §2: `direct-streamlocal`, or an exec channel running `nightgrid-hub bridge`). Same framing, same `hello`, protocol 2 with streams. The app only calls what `hello` lists.

| Capability | Methods | Notes |
|---|---|---|
| `fleet` 1 | `fleet.members.get` / `fleet.members.set` | Member list, set only by the app |
| | `fleet.overview` | Per member: `id`, `name`, `state` (`new`, `ok`, `stale`, `unreachable`, `consent`, `version`), `lastContact`, `version`, `hostname`, `alerts` (active count), `error`. Also the hub's `hubId`, `fingerprint`, `listening`, `collector` and `storeBytes`. (`skewSecs` comes later, additively) |
| | `fleet.sync {nodeId, method, params}` | Calls one of `metrics.sync`, `events.sync`, `uptime.sync`, `disk.sync`, `alerts.active`, `node.info` for that member from the hub's copy. Params and result are exactly the Lite method's |
| | `fleet.events.sync {after}` | The hub's own events (`hub` warnings, `hostOffline`), with the same shape as `events.sync` |
| `notify` 1 | `notify.channels.get` / `notify.channels.set` | Channels without secrets; secrets are set by root (§5) |
| | `notify.test {channel}` | Sends one test message |
| | `notify.log` | Recent deliveries and failures |
| | `notify.settings.get` / `.set` | Quiet hours, rate limit, minimal mode, resolved messages, heartbeat URL name |

Exact schemas for `fleet` 1 are in [PROTOCOL.md](../PROTOCOL.md#hub-socket-fleet-1); `notify` comes with H2.

## 4. Syncing through the hub

The hub keeps each member's own `seq` numbers and bucket timestamps, so **the app's existing per-node cursors are valid for both paths**:

- When the hub is reachable and `fleet.overview` shows a member `ok`, sync that member with `fleet.sync`, using the same cursor the app keeps for the member's `node_id` today.
- When the hub is unreachable, or a member is `stale`, `unreachable` or `consent` there, sync that member directly as in 0001 and 0002, from the same cursor.
- Switching back and forth is always safe: an overlap is dropped by the existing `ON CONFLICT DO NOTHING` and cursor rules; a gap can't happen because the cursor only moves forward on data the app stored.
- Live streams (0004) stay direct to each host: the hub is for history and for the app being closed, not for 2-second metrics.
- Actions, heal rules, alert rules and everything that writes stay direct to each host, as today. The hub has no write path to members.

Pushing alert rules still goes to each member (`alerts.rules.set`), because members evaluate their own alerts. The hub only evaluates `hostOffline` for members, with the app's grace period.

## 5. Notifications

- **Settings → Notifications** gets, per site with a hub, a choice: **Hub delivers** (recommended) or **App delivers** (today's behaviour).
- With **Hub delivers**: the app moves its webhook to the hub (asking first), adds email if wanted, and keeps native notifications only for hosts not covered by a hub, and only while open. On reconnect, alerts that the hub already sent are shown in "while you were away" but not notified again: the app compares the alert key and time against `notify.log`.
- Secrets (webhook URL, SMTP password) are set with `sudo nightgrid-hub notify set-secret NAME` through the sudo flow, the value on stdin, never on a command line. The app shows back only scheme and host, as for its own webhook.
- If a channel endpoint is on the public internet, the app says so once ("messages leave your network to ntfy.sh") and offers minimal mode.
- `notify.test` behind a **Send test** button per channel.

## 6. Showing it

- The site's page gets a **Hub** card: which host, its state, members and their `state`, store size and how many days of minutes it holds, and the last delivery per channel.
- A member's page shows "synced through hub pi" or "synced directly" in its sync status.
- When the hub is unreachable: "Hub pi unreachable since 14:02. Syncing hosts directly." and nothing else changes.
- `hub` warnings (two hubs, clock skew, store at cap, restarts) show on the Hub card and in the site's timeline.

## 7. Checks

- A site with a hub and three members: close the app, stop one member's Lite service. A notification "… is offline" arrives within about three minutes, and "back online" when it is started.
- Open the app after an hour away: every member's graphs and timeline have no gap, and the app made one connection to the hub for history.
- Stop the hub, then open the app: members sync directly from the same cursors, with no gap and no duplicate events.
- A member without `sync` in its `serve` list shows `consent` and is synced directly.
- Set up a second hub in the same site from the command line: both raise the "only one hub per network" warning, and the app shows it.
- An app build without this spec, against a site with a hub: nothing changes for it.
