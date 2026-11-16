# 0005 · Logins and file changes

Node milestone: **N2b** (nightgrid-node 0.5.0, protocol 1 and 2). Status: **proposed** for the app. Builds on 0002 (events sync) and 0004 (live streams).

## What the person gets

- **Who logged in, even while the app was closed.** "alice logged in from 10.0.0.5" and "12 failed logins from 3 addresses" in the host timeline, live while connected.
- **Changes to important files.** "changed /etc/ssh/sshd_config" with what changed (size, hash, mode, owner), so an unexpected edit to sudoers, SSH config, cron or systemd units stands out. Contents never leave the server.

## 1. Timeline

Both arrive through `events.sync` and the `events` stream; there is nothing new to call.

| Node `kind` | App `timeline::EventKind` | Notes |
|---|---|---|
| `login` | add `Login` (or map to `Unit` until it exists) | `warn` level for 10+ failures in a minute |
| `file` | add `File` (or map to `Unit` until it exists) | `warn` for SSH, sudoers, passwd, shadow, group |

Adding the two kinds to `core/src/timeline.rs` (`EventKind::Login`, `EventKind::File`, with `as_str` `"login"` and `"file"`) is the clean path; the stored `kind` column is text, so no migration is needed.

## 2. Node panel

`node.info.access.journal`:

| Value | Panel shows | Button |
|---|---|---|
| `ok` | "Recording SSH logins" | **Stop** → `sudo nightgrid-node deny journal --json` |
| `denied` | "Not recording logins" | **Allow** → confirmation, then `sudo nightgrid-node allow journal --json` |
| `absent` | "No journal on this host" | none |
| `off` | "Switched off in the node's config" | none |

Confirmation text: "This lets the node read the system journal. That includes every program's logs, which can contain secrets. The node only keeps SSH login lines." Run through the existing sudo flow; show `warnings`; reconnect after (the service restarts).

The file watcher needs no grant. The panel can show the watched list as read-only text with "Edit /etc/nightgrid-node/config.toml on the server to change it".

## 3. Access audit

The app's access audit (who can log in, keys, sudo) can link to the latest `file` events for `/etc/ssh`, `sudoers` and `authorized_keys` changes, and to `login` events, instead of reading `auth.log` over SSH when the node has the `logins` capability.

## Tests in the app

- `login` and `file` events from a fixture show with the right kind and level.
- `access.journal: denied` offers Allow; `ok` offers Stop.
- A `file` event's detail is shown as is (it never contains file contents).
