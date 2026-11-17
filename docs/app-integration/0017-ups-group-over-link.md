# 0017 · UPS group shutdown over the link

Node milestone: **N8c** (nightgrid-node 0.21.0). Status: **proposed** for the app. Builds on 0014 (UPS shutdown per host) and 0016 (pairing nodes).

## What the person gets

- **Group shutdown with the app closed,** for hosts that can't read the UPS themselves. The host the UPS is plugged into leads. When its countdown starts, the linked hosts that follow it start their own, so they power off first and the UPS host goes last, the same order as the app's own group shutdown (D-112).
- Power coming back, or a cancel on the UPS host, ends the countdowns everywhere. A cancel on a follower keeps only that host on.

## 1. Setting it up

In the app's UPS group settings, for a group whose hosts all run node 0.21.0 or later and are linked (0016):

1. **The UPS host:** its own guard as in 0014: `[ups] shutdown`, the UPS in `[health] ups`, and its limits. Give it the longest `countdown_secs` in the group (the app can suggest 300).
2. **Each follower:** root's config needs `[ups] shutdown = true`, `"ups"` in `[peers] serve`, `power.poweroff` in `[actions] allow`, and the root helper (`allow helper`). It doesn't need a NUT client or `[health] ups`. The app shows the lines to add and the command to apply them (`sudo systemctl restart nightgrid-node nightgrid-node-peer`). If the app can already edit host files with the person's sudo confirmation, it can offer to do this, showing the change first. `ups.guard.problem` is `null` when it is ready.
3. **Pairing:** on each follower, set `followUps: true` on the UPS host's entry in `peers.set`. Nothing changes on the UPS host: it tells all its peers, and only followers act.

Show each follower's `ups.guard` (phase, `ledBy`, `problem`) in the group view, as for the per-host guard.

## 2. During an outage

- The UPS host's timeline gets "UPS shutdown in 5 min". Each follower's gets "UPS shutdown in 60 s" with the detail "nas is shutting down for ups@nas (on battery for 5 min). This host follows it…" within about 10 seconds.
- **Cancel** on a follower is the usual `ups.cancel` there. **Cancel all** is `ups.cancel` on the UPS host: the followers then get "UPS shutdown cancelled" by themselves.
- After power returns, the followers' countdowns end ("UPS shutdown cancelled: power is back"). A follower that already powered off stays off, as with any UPS shutdown.

## 3. Checks

- With a dry-run group, take the UPS host on battery past its limit. Each follower shows `phase: countdown` and `ledBy` set to the UPS host, and then a `dryRun` `power.poweroff` from source `ups` in its action log.
- Restore power before the followers' countdowns end. Every follower goes back to `idle`.
- A follower without `followUps` on the UPS host's entry does nothing.
