# 0030 · Windows node W2e: blocking addresses

Node: **0.35.0** (W2e of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Builds on [0026](0026-windows-node-w2a.md) and the Linux block spec. Additive: no method, field, frame or link request changed.

## What the person gets

Blocking an address from the app, and auto-blocking addresses that keep failing logins, work on a Windows host the way they do on Linux.

## Setting it up

The node needs `allow helper` (0026). An administrator then lists the actions in `C:\ProgramData\Nightgrid Node\config.toml` and restarts the service:

```toml
[actions]
allow = ["firewall.block", "firewall.unblock"]

[block]
auto = true        # optional: failed logins past the threshold block the address
minutes = 60
```

## What the app sees

- `hello` caps now include `block` on Windows. `actions.policy` lists `firewall.block` and `firewall.unblock` once they are allowed.
- `actions.run` with `firewall.block` and an address works as on Linux, and so do `firewall.unblock`, `block.list` and the action log. Private, local and VPN addresses, and addresses in `[block] never`, are refused as on Linux.
- Failed logins counted for auto-block include SSH (OpenSSH/Operational), Remote Desktop and network logons (Security 4625), as for the login events since 0025.
- If Windows Firewall is off for a network profile, the block still succeeds, but its `detail` says it does nothing on that profile. Show the detail as it is.
- The rules are visible in Windows Defender Firewall under the group "Nightgrid Node blocks". The node removes them when they expire, and `deny helper` and `uninstall` remove all of them.

## What to change in the app

- Offer "Block address" and auto-block settings for Windows hosts on 0.35.0 or later, as for Linux.
