# 0012 · Service checks from the node

Node milestone: **N7a** (nightgrid-node 0.16.0, `checks` 1). Status: **proposed** for the app. Builds on 0002 (events and alerts) and the app's Services board.

## What the person gets

- **Service checks with the app closed.** The Services board's HTTP(S) checks keep running every minute from inside the LAN. "Service down" and "certificate expiring" alerts arrive in "while you were away".
- **"Down" or "only unreachable from here"?** When the laptop is off the LAN or the tailnet hiccups, the board can show what the node sees instead of a false "down".
- **Node-to-node checks.** A `tcp://other-host:22` check pushed to one node tells whether another host answers inside the LAN.

## 1. Turning it on

When `checks` is in `caps`, read `node.info.access.checks`:

- **`ok`**: the runner is reporting. Use the node (sections 2 and 3).
- **`denied`**: on the node's settings page, offer "Run service checks from this server while Nightgrid is closed." The button runs `sudo nightgrid-node allow checks` with the app's existing sudo confirmation, and shows the report's `warnings[0]`.
  - Checks only reach `[checks] networks` (private ranges by default). For a service on a public address, show the line root would add to `/etc/nightgrid-node/config.toml`, then `allow checks` again. The app never edits the config itself.
- **`off`**: root disabled it. Say so and change nothing.

## 2. Which node checks what

- On connect, and whenever services change, push the services with checks enabled to one node per network.
  - Pick the node the service's host belongs to; if the service has no host, pick the first node with `access.checks == "ok"`.
  - Call `checks.set` with `{checks: [{id: "<service id>", name, url, expectStatus, allowUntrusted}]}`.
  - Push the whole list for that node each time; it replaces the previous one.
- A `bad_request` names the check (for example, a URL the node can't parse). Show it on that service and keep checking it from the app.
- For node-to-node checks, add `tcp://<host address>:22` (or any port the host serves) as a hidden check for each other host with a node, and push it to a node on the same LAN.

## 3. Board and alerts

- **Board:** call `checks.latest` when the board opens and on reconnect. Each reading's `result` is the app's own `CheckResult`, so render it with the existing row. Add "checked from <node host> 14:02" from `at`.
- **Reachability:** when the app's own check says down and the node's reading (under 3 minutes old) says up, show "up (from <node host>); not reachable from here" instead of down.
- **Alerts:** the node raises `serviceDown` (`svc:<id>`) and `certExpiring` (`svc:<id>:cert`) with the app's own keys and rules. Push `serviceDown`, `certExpiring` and `certDays` with the rest of `alerts.rules.set`, which the app already does.
  - While a node checks a service (`access.checks == "ok"`), the app should not raise those alerts itself for it. Same keys, so de-duplicating by `kind` + `key` is enough.
  - The node's alerts arrive through `events.sync`. Folding by host (`causedBy`) stays the app's.

## 4. Checks

- On a host with node 0.16.0 and `allow checks`:
  - push a check for a LAN web server; `checks.latest` shows it up within a minute.
  - stopping the server raises "SERVICE DOWN" within about 2½ minutes with the app closed.
- A self-signed HTTPS service is down unless "allow untrusted" is ticked, and shows its certificate expiry either way.
- A check of a public address fails with "… is outside [checks] networks" until root adds it.
- Hosts without the node, or without `allow checks`, check exactly as before.
