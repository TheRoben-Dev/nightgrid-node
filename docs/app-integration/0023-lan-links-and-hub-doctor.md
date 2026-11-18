# 0023 · Links on the LAN, hub on several addresses, hub doctor, event kinds

Node: **0.28.0**. Status: **proposed** for the app. Builds on 0016 (pairing), 0020 (hub) and 0002 (events). Additive: no frozen method, field, frame or link request changed, and every new field is missing (read it as before) from an older node or hub.

## What the person gets

- Hosts that aren't on the tailnet can still be linked and join a hub, over the LAN.
- One hub serves members on the tailnet and on the LAN at the same time.
- "Check the hub" in the app, with the same findings list as the node's doctor.
- A faster first sync on busy hosts, which skips the event kinds the app doesn't show.

## 1. Links over the LAN (`[peers] listen = "lan"`)

`[peers] listen` (node config, root's) takes `"lan"` besides `"tailnet"`, `""` and `ip:port`. The link then listens on this host's LAN address: the source address of the route to the first non-tailnet entry of `[peers] networks`, or else of the default route. It is never `0.0.0.0`, it is looked up again every 30 s (a new DHCP lease moves it), and `peers.latest.listening` shows the address in use.

In the app's pairing flow (0016 §2):

- Pair with the address from the other node's `peers.latest.listening`, as today. On a host whose `listening` is `null` and that has no tailnet address, offer "Use the LAN": the app writes `listen = "lan"` and adds the LAN range to `networks` through its usual root config flow (0016 §1), runs `sudo nightgrid-node allow peers` again, and reads `listening` back.
- Show the address kind next to it: "tailnet" for `100.64.0.0/10` and `fd7a:115c:a1e0::/48`, "LAN" otherwise. On "LAN", say once that the link is mutual TLS with pinned certificates but that the LAN can see that the port is open.
- If `listening` changes after a pairing, the paired nodes must learn the new address: re-run `peers.set` on them with the new address the next time the app sees the change. A DHCP reservation avoids this, so suggest one.

## 2. A hub on more than one address (`[collect] listen` list)

`[collect] listen` (hub config) takes one entry or a list of up to 8 (`["tailnet", "lan"]`, or explicit `ip:port`s). `fleet.overview` keeps `listening` (the first) and adds `listenAll`. `nightgrid-hub allow collect --json` reports both.

- When pairing a member with its hub (0020 §2), use the `listenAll` entry the member can reach: the tailnet one for a tailnet member, the LAN one for a member whose own `listening` is a LAN address. Without `listenAll` (older hub), use `listening`.
- Add the LAN range to `[collect] networks` when any member is on the LAN, then `sudo nightgrid-hub allow collect` again.
- Show `listenAll` in the hub card ("answers on 100.101.2.3:17420, 192.168.1.10:17420").

## 3. Hub doctor

`nightgrid-hub doctor [--config PATH] [--json]`, run as the login user (no sudo). The shape is the node's doctor's: `{"version", "findings": [{"check", "level", "detail"}], "arch", "libc", "init", "kernel", …}`, levels `ok`, `info`, `warn`, `fail`, and exit status 1 on any `fail`.

Checks: `host`, `install`, `node` (the Lite node it needs), `init`, `config`, `unit nightgrid-hub`, `unit nightgrid-hub-collect`, `socket`, `collector` (reporting, and answering on every `listen` entry), `members` (who can't be pulled, and why), `store` (against `max_gb` and the disk's free space).

- Add "Check the hub" next to the node's "Check this host" and render the findings the same way.
- Before saving a hub config the app edited, upload it to a temp file and run `nightgrid-hub doctor --config <file> --json`. A `config` finding at `fail` means don't install it, and show its `detail`.
- `doctor` exists from hub 0.28.0. An older hub answers `unknown command "doctor"` on stderr with exit status 1: hide the button when the hub's version is below 0.28.0.

## 4. Event kinds (`events.sync` `kinds`)

`events.sync` (and the same through the hub's `fleet.sync`) takes an optional `kinds` list, with the same capability version ([PROTOCOL.md](../PROTOCOL.md)):

```json
{"after": 0, "kinds": ["alert", "recovered", "boot", "outage", "container", "action"]}
```

- Only events of those kinds come back. When the page isn't full, `next` is the newest `seq` on the node, so the cursor moves past the skipped events. The app must therefore keep a **separate cursor per kinds set**: a later sync of the skipped kinds starts from its own older cursor.
- An older node ignores `kinds` and returns everything. Filter on the app side as well, so both behave the same.
- Use it for the first sync of a host with a long history, or on a slow link. The default sync (no `kinds`) is unchanged.

## 5. Also stated in PROTOCOL.md now

These were already true, and are now written down, so the app may rely on them:

- **Empty page:** with no events (or rows) after the cursor, the answer is `next == after` and `more: false`. The `kinds` case is the one exception (§4), where `next` may be higher.
- **Subscribe and unsubscribe:** subscriptions add up per connection. `subscribe` with a stream already on is a no-op, `unsubscribe` with a stream already off is a no-op, and closing the connection ends all of them.
- **Alert kinds** stay camelCase (`diskFull`, `hostOffline`, …), including new ones. Unknown alert kinds read as a generic alert.

## 6. Checks

- Two hosts without Tailscale, both on `listen = "lan"`: pair, ping both ways, then change one host's DHCP address and see `listening` move within a minute.
- A hub with `listen = ["tailnet", "lan"]`: one tailnet member and one LAN-only member both reach `ok`.
- Hub doctor on a healthy hub has no `fail`. An uploaded config with `max_gb = 0` is refused.
- First sync with `kinds` on a host with 20 000 login events: one or two pages, and a later full sync still gets the login events.
