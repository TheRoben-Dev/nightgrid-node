# Changelog

## 0.1.0 · N0 (unreleased)

- First version: `install` / `uninstall [--purge]` as a hardened systemd service with its own account and a group-gated unix socket; `bridge`, `status`, `query`.
- Metrics sampled every 10 s straight from `/proc`, `/sys` and `statvfs` (vendored Nightgrid parser), averaged per minute and rolled up to 15-minute and hourly tiers (14 / 90 / 400 days).
- Protocol 1: `hello` with version and capability negotiation, `node.info`, `metrics.latest`, `metrics.history`, `metrics.sync`.
- CI: lint and tests on x86_64 and arm64, static musl builds, real install tests on systemd (runners, Debian 12, Ubuntu 22.04, Fedora 41). Release workflow with checksums, minisign and build provenance.
