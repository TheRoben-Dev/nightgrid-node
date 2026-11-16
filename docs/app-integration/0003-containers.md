# 0003 · Containers

Node milestone: **N1b** (nightgrid-node 0.3.0, protocol 1). Status: **proposed** for the app. Builds on [0001](0001-install-and-metrics-sync.md) and [0002](0002-events-uptime-disk.md).

Only for hosts whose `hello` lists `containers`. Without it the app does exactly what it does today (Docker API over SSH, `core/src/docker/transport.rs`).

## What the person gets

- **Container crashes while away.** "web exited (exit 137)" and "web restarted" show in the host timeline at the time they happened, and a container that went down raises "CONTAINER DOWN · web on server01" in the alerts panel's "while you were away" list.
- **An "Allow container watching" switch** in the node panel, with the root-equivalence warning, so watching is a deliberate choice.

## 1. Node panel

Call `containers.list` after `hello` and show `access`:

| `access` | Panel shows | Button |
|---|---|---|
| `ok` | "Watching N containers" | **Stop watching** → `sudo nightgrid-node deny docker --json` |
| `denied` | "The node can't read containers yet" | **Allow container watching** → confirmation, then `sudo nightgrid-node allow docker --json` |
| `absent` | "No Docker or Podman socket on this host" | none |
| `off` | "Switched off in the node's config" | none |
| `error` | `message` | none |

The confirmation for **Allow** must say, in the app's words: "This gives the node access to Docker's socket. Anyone with that access can control the host, the same as root. The node only reads container lists and events." It runs through the app's existing sudo flow (password on stdin, D-048). Show the report's `warnings` afterwards. The service restarts, so reconnect the node client.

## 2. Sync

Nothing new to call: container entries arrive through `events.sync` (0002) with `kind: "container"`. Map them to the app's `timeline::EventKind::Container`. Titles, levels and details already match `timeline::container_events`, so no reformatting is needed.

Dedupe against the app's own Docker-events read for the same host (`timeline::container_events` from the Docker API over SSH) by `(title, at ± 5 s)`. The node writes entries about two minutes after they happen so that a crash and the restart that follows are one entry. Both sides use the same two-minute merge window, so they agree.

## 3. Alerts

- Key mapping (0002 rule): `docker:web` → `docker:<hostId>:web`.
- Push `containerDown` with the rest of `AlertSettings` (0002 already sends the whole object; nodes from 0.3.0 use this field).
- While connected, the app keeps running its own `ContainerWatch` too. Both produce the same key, so the tracker sees one alert.

## 4. Later: live

With protocol 2 (N2), the app subscribes to the `containers` stream instead of polling `docker ps` over SSH while a host page is open. That is spec 0004.

## Tests in the app

- `access: denied` → the panel offers Allow; `ok` → it offers Stop watching.
- An imported `container` event that duplicates one read over SSH appears once.
- `docker:web` from the node and `docker:<hostId>:web` from the app's own watch are one alert.
