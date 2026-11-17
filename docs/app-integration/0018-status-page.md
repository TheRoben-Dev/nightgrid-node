# 0018 · Status page link

Node milestone: **N8d** (nightgrid-node 0.22.0). Status: **proposed** for the app. Builds on 0001 (install) and the `allow` flow of 0012.

## What the person gets

- **"Phone status page"** on a host: a read-only page they can bookmark on their phone. It works over the tailnet with the app closed and the laptop off, and shows the host's state, active alerts, the UPS guard and linked hosts.

## 1. Turning it on

Offer it when the welcome's capabilities include `peers` ≥ 1 (0.22.0 ships both together; the page has no capability of its own) and `node.info.node.version` is 0.22.0 or later.

1. Show the warning from the report first: anyone with the link and tailnet access can read this host's status.
2. Run `sudo nightgrid-node allow statuspage --json` with the usual sudo confirmation.
3. The report has `url`, for example `http://100.101.102.103:7420/3f9c…`. Show it with a **QR code** and a **Copy** button. Don't store it in the app's database or logs: the host keeps it, and running the command again prints the same link.
4. If `url` has `<this host's tailnet address>` in it, the host has no tailnet address yet. Say "Tailscale isn't up on this host. The page starts once it is," and show the link again later.

**Turn off** runs `sudo nightgrid-node deny statuspage --json`. Say that the old link stops working. Turning it on again makes a new link.

## 2. Checks

- Turn it on and open the link on a phone on the tailnet. The page shows the host with "all good" or its alerts, and refreshes by itself.
- Change one character of the link: "not found".
- Turn it off: the link stops working. Turn it on again: there is a new link.
