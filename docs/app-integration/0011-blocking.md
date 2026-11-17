# 0011 · Blocking addresses

Node milestone: **N6c** (nightgrid-node 0.15.0, `block` 1). Status: **proposed** for the app. Builds on 0007 (actions) and 0010 (login bursts).

## What the person gets

- **A "Block" button on a login-burst alert.** It blocks the noisiest address for an hour, through the node's root helper. The block ends by itself.
- **Optional auto-block.** With root's opt-in, an address that keeps failing SSH logins is blocked with the app closed, like fail2ban but logged in Nightgrid's audit trail.
- **A list of blocked addresses** on the host's security view, with an Unblock button.

## 1. Availability

With `block` in caps, read `actions.policy`:

- **`firewall.block` in `allowed` and `helper: true`:** show Block and Unblock.
- **Otherwise:** on the node's settings page, show what root needs to do, with the app's existing sudo confirmation if the person wants the app to run it.
  - Add `"firewall.block", "firewall.unblock"` to `[actions] allow` in `/etc/nightgrid-node/config.toml`. The app never edits that file itself, so show the line.
  - Run `sudo nightgrid-node allow helper`.
  - Optionally turn on `[block] auto`.
  - Then run `sudo systemctl restart nightgrid-node`.

## 2. Block and unblock

- **Block:** `actions.run` with `{action: "firewall.block", target: "<ip>"}`.
  - Offer it on a `loginBurst` alert for each address in its detail. Show the most frequent one first.
  - Also offer it from the timeline on `login` events with failures.
- **Refusals:** a refusal comes back as `outcome: "refused"` with the reason. For example, "192.168.1.9 is a local, private or VPN address". Show it as is.
- **Duration:** blocks last root's `[block] minutes`, which `block.list` gives as `minutes`, so the button can say "Block for 60 min".
- **Unblock:** `{action: "firewall.unblock", target: "<ip>"}`.

## 3. Blocked list

`block.list` returns `{auto, threshold, minutes, blocked: [{addr, since, until, source}]}`.

- Show it on the security view, with "until 15:42" and the source ("auto-block", or the app user).
- With `auto: true`, add a line above it: "Addresses with 10 failed logins in 5 minutes are blocked for 60 minutes."
- Refresh it when an `action` event with action `firewall.*` arrives.

## 4. Checks

- On a host with node 0.15.0, the root helper installed and both actions allowed:
  - Block 203.0.113.60 from the app. `nft list set inet nightgrid_node v4` on the host lists it with a timeout.
  - Unblock removes it.
- Blocking a LAN address shows the refusal and changes nothing.
- With `[block] auto = true` and `allow journal`, enough failed logins from one public address show "blocked … for N minutes" in the timeline with the app closed.
- Hosts without `block` in caps show no Block buttons.
