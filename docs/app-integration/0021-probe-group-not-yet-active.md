# 0021 · Probe: "installed, not running" right after install

Node: any version (no node change). Status: **proposed** for the app, a bug fix. Builds on 0001 (the node session). Found on the first real install (lumen, Nightgrid 1.1.0, node 0.26.0).

## What goes wrong

Right after `nightgrid-node install --user NAME`, NAME is in the `nightgrid-node` group in `/etc/group`, but the SSH login the app already has still runs with its old groups. The app shows the host as **"installed, not running"**, with the detail line **"installed, but its service is active"**, which contradicts itself. Reconnecting SSH makes it work.

## Why

`PROBE` in `core/src/node/install.rs` checks `[ -S /run/nightgrid-node/node.sock ]`. The socket's directory is `0750 nightgrid-node:nightgrid-node` (by design, DESIGN.md and DECISIONS.md), so a login without the group in its current groups can't stat anything inside it, and the test fails although the socket is there. `route()` then returns `Absence::NotRunning` from `!p.sock`, before it ever looks at `p.member`. The `Route::Sg` path (`sg nightgrid-node -c 'nightgrid-node bridge'`), which exists for exactly this case, is never reached.

## Fix (app only)

In `route()`, use the unit state when the socket isn't visible:

```rust
pub fn route(p: &Probe) -> Result<Route, Absence> {
    if !p.sock {
        if !p.bin {
            return Err(Absence::NotInstalled);
        }
        if p.unit != "active" && p.unit != "activating" && p.unit != "reloading" {
            return Err(Absence::NotRunning);
        }
        // Running, but this login can't see into /run/nightgrid-node (0750).
        return if p.member { Ok(Route::Sg) } else { Err(Absence::NoAccess) };
    }
    if p.writable { Ok(Route::Socket) } else if p.member { Ok(Route::Sg) } else { Err(Absence::NoAccess) }
}
```

When `unit` is empty (no systemctl, or the probe couldn't run it), keep today's `NotRunning`.

Optionally, after a successful **Install/Update** from the app, drop and reopen the SSH connection, so the new login has the group and uses the direct socket route.

## Tests

- `sock no, bin yes, unit active, member yes` gives `Route::Sg`.
- `sock no, bin yes, unit active, member no` gives `NoAccess` ("isn't in the group; Update adds it").
- `sock no, bin yes, unit failed` and `unit ""` give `NotRunning`.
- Existing cases are unchanged.

## Workaround until then

Reconnect the host in the app (or restart the app), then press **Check again**.
