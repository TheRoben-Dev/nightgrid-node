# Changelog

## 0.2.0 · N1 (unreleased)

- Events stream (`events.sync`) with a growing `seq`, 90 days, secret filter on titles and details.
- Uptime: once-a-minute heartbeat, hourly up/down/asleep seconds (`uptime.sync`) and outage events that tell a crash ("Down") from a clean reboot ("Restarted") and a suspend ("Suspended").
- Disk usage per mount per hour (`disk.sync`) and fill-up forecasts.
- Offline alerts with the app's tracker semantics: disk full and disk filling, rules pushed by the app (`alerts.rules.get/set`), active alerts (`alerts.active`) that survive restarts.

## 0.1.0 · N0 (unreleased)

- First version: `install` / `uninstall [--purge]` as a hardened systemd service with its own account and a group-gated unix socket; `bridge`, `status`, `query`.
- Metrics sampled every 10 s straight from `/proc`, `/sys` and `statvfs` (vendored Nightgrid parser), averaged per minute and rolled up to 15-minute and hourly tiers (14 / 90 / 400 days).
- Protocol 1: `hello` with version and capability negotiation, `node.info`, `metrics.latest`, `metrics.history`, `metrics.sync`.
- CI: lint and tests on x86_64 and arm64, static musl builds, real install tests on systemd (runners, Debian 12, Ubuntu 22.04, Fedora 41). Release workflow with checksums, minisign and build provenance.
