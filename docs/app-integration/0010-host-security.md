# 0010 · Host security

Node milestone: **N6b** (nightgrid-node 0.14.0, `security` 1). Status: **proposed** for the app. Builds on 0002 (events and alerts), 0005 (logins and files) and the app's security audit (M10d).

## What the person gets

- **"Something new is listening"** in the timeline with the app closed: a database bound to every interface, a test server left running, a container publishing a port. Louder when it's reachable from the network.
- **An alert when SSH is being brute-forced**, not only a line in the timeline.
- **Wider file watching**: changes to anyone's `authorized_keys`, PAM, `ld.so.preload`, cron (including users' crontabs), package sources and the compose files of running containers.

## 1. Ports

- **Timeline:** events with `kind: "port"` render like `file` events. Older app builds see them as `other`, which already shows title and detail, so nothing breaks before this lands. Add a filter chip "Ports" next to "Files" and "Logins".
- **Security view:** where the app shows its own audit's listeners (`system::security::Listener`), it can use `ports.list` when the host has `security` in caps. It shows the same fields, the same `scope` words and the same service names. The node can't see process names (`process` is always null), so keep the audit's "Run audit" button. When the person runs it, the audit's processes win for the same proto, port and scope.
- **Containers:** `containers.list` entries now carry `ports` (`proto`, `addr`, `port`, `privatePort`), so the Docker view can show published ports without its own `docker ps`.

## 2. Login bursts

- Add `loginBurst` to `AlertKind` (title "LOGIN BURST", the same grace period as the app's other immediate alerts) and a switch in alert settings, "Brute-force attempts (many failed SSH logins)". It defaults to **on** in the app.
- Push it with the rest of `alerts.rules.set` (0002). The node defaults it to `false`, so it only fires once the app knows the kind. **Don't send `loginBurst: true` before `LoginBurst` is in the app's `AlertKind`**, or alert parsing fails for that host.
- The key is `logins:burst` (app: `logins:<hostId>:burst`). The detail lists the top addresses, e.g. "57 in the last 5 minutes from 203.0.113.7 ×50, 198.51.100.2 ×7". The threshold is root's (`[logins] burst`, default 20) and the window is fixed at 5 minutes.
- It needs `allow journal`, the same grant as logins (0005); show the existing journal offer if `access.journal` isn't `ok`.
- N6c will add optional, time-limited blocking of the top addresses. Leave room for an action on this alert.

## 3. Wider file baseline

Nothing new is needed for this: these are ordinary `file` events. Two things help the person:

- On the node's settings page, next to the health offer (0008), add one line: "With health checks allowed, the node also watches `authorized_keys` in home folders and root-only files."
- `warn` file events for `authorized_keys`, cron, PAM and `ld.so.preload` deserve the same treatment as sshd and sudoers changes, wherever the app highlights those.

## 4. Checks

- On a host with node 0.14.0, `python3 -m http.server 18765` shows "new listener 18765/tcp" (warn) within about 30 s, and "stopped listening" after it's stopped.
- With `allow journal` and the switch on, 25 failed SSH logins from one address raise "LOGIN BURST" within a minute, and it clears 5 minutes after they stop.
- With `allow health`, adding a key to a user's `~/.ssh/authorized_keys` shows "added …/authorized_keys" or "changed …" with a hash and no key text.
- Hosts without the node, or with an older node, behave exactly as before.
