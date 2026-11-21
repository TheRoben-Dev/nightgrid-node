# 0038 · Windows node W3e: status page

Node: **0.43.0** (W3e of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Builds on [0018](0018-status-page.md) and [0037](0037-windows-node-link.md). Additive: no method, field or frame changed.

## What the person gets

The "Phone status page" from [0018](0018-status-page.md) on a Windows host: a read-only page they bookmark on their phone, reachable over the tailnet with the app closed, showing the host's state, active alerts, the UPS guard and linked hosts.

## What the app sees

- `nightgrid-node allow statuspage --json` and `deny statuspage --json` behave as on Linux: the report's `url` is the secret link, running `allow` again keeps it, and `deny` makes it stop working.
- There is no new capability; the app offers the page on a Windows node at 0.43.0 or later (`node.info` version), as it does for Linux at 0.22.0.

## What to change in the app

- Offer "Phone status page" for Windows hosts on 0.43.0 or later.
- On Windows, run the two commands through the app's elevated PowerShell flow instead of `sudo`, with the same confirmation.
- Settings live in `[statuspage]` of `%ProgramData%\Nightgrid Node\config.toml`; after changing `listen`, `port` or `networks`, run `allow statuspage` again.
- The page needs Tailscale (or Headscale) on the Windows host for the default `listen = "tailnet"`; without a tailnet address the service waits and retries, and `doctor` still shows it running. Say so when the host has no tailnet address.
