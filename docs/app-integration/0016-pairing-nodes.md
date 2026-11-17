# 0016 · Pairing nodes

Node milestone: **N8b** (nightgrid-node 0.20.0). Status: **proposed** for the app. Builds on 0001 (install) and 0012 (the `allow` flow for the check runner).

## What the person gets

- **"Link with other hosts"** on a host with the node. Linked hosts check each other over the tailnet with the app closed, so the timeline says "peer nas unreachable" from the hosts that lost it, and a dead host can be told from a broken path.
- Nothing changes for anyone who doesn't link hosts: each node acts alone by default.

The link is the base for later features (UPS group shutdown, staged restarts, relayed alerts, N8c). Each of those will be a separate opt-in.

## 1. When to offer it

The host's welcome has capability `peers` ≥ 1. `node.info.access.peers` says where it stands:

- `off`: the link isn't installed. Offer "Turn on linking", which runs `sudo nightgrid-node allow peers --json` with the app's usual sudo confirmation, and shows the report's warning first, as for `allow checks`.
- `ok`: ready to pair.
- `denied`: it was on but stopped reporting. Show "The link on this host stopped. Run `sudo systemctl status nightgrid-node-peer` there."

Linking needs at least two hosts with `access.peers` `ok`.

## 2. Pairing

The app is the only thing that pairs nodes. It already reaches every host over SSH as an admin, so it is the trust anchor and there is no trust on first use.

To pair A and B:

1. Read `peers.latest` on both. Use `fingerprint` and `listening` from each. If `listening` is `null`, show "B has no tailnet address yet. Is Tailscale up there?" and stop.
2. Read `peers.get` on A, add `{id: B's node.info.node.nodeId, name: B's display name, address: B's listening, fingerprint: B's fingerprint}`, and send the whole list back with `peers.set`.
3. Do the same on B with A's details.
4. Keep the pair in the app's own host settings, with both fingerprints.

`peers.set` answers within a second. The nodes try each other within about 10 seconds, so the app can show "Linked" once `peers.latest` on A has a reading for B with `ok: true`. Give up after 60 seconds and show the reading's `error`.

Offer "Link all my hosts" as a shortcut: it pairs every pair among the selected hosts. With more than 64 hosts, the node refuses the list. That is far beyond a homelab, so a plain error is fine.

To **unpair**, remove the entry on both sides with `peers.set`.

**Re-pair** when a host's `peers.latest.fingerprint` differs from the one the app stored. That happens after `uninstall --purge` and a reinstall. Ask first: "nas has a new link key. Re-link it?" Never re-pair silently: a changed key is exactly what pinning is meant to catch.

## 3. Showing it

On the host page, add a **Linked hosts** list with one row per peer from `peers.latest.readings`, matched to `peers.get` by `id`:

- reachable or not, the round trip (`rttMs`), and when it was last read (`at`);
- the peer's `version`, if it differs from this host's;
- `error` under an unreachable row, as is.

The timeline already gets "peer X unreachable" and "peer X reachable again" as `other` events through `events.sync`, with no app change.

## 4. Checks

- Two hosts on node 0.20.0 with `allow peers`: link them, and both show the other as reachable within a minute.
- Stop the link on one (`sudo systemctl stop nightgrid-node-peer`). The other shows "peer … unreachable" in the timeline within about two minutes, and "reachable again" once it is started again.
- Pair a host with a wrong fingerprint (for example, an old one after a purged reinstall). It never shows as reachable, and the error says the certificate isn't the paired one.
- A host on 0.19.0 or older shows no linking option.
