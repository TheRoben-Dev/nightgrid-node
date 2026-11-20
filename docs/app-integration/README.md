# App integration specs

The Nightgrid app repository is developed elsewhere and is read-only from here. Whatever the app has to do to use the node is written up in this folder, one spec per node milestone, for the app's own project to build.

| Spec | Node milestone | Status |
|---|---|---|
| [0001 · Install the node and sync metric history](0001-install-and-metrics-sync.md) | N0 | proposed |
| [0002 · Events, uptime, disk usage and offline alerts](0002-events-uptime-disk.md) | N1 | proposed |
| [0003 · Containers](0003-containers.md) | N1b | proposed |
| [0004 · Live streams (protocol 2)](0004-live-streams.md) | N2a | proposed |
| [0005 · Logins and file changes](0005-logins-and-files.md) | N2b | proposed |
| [0006 · Network watch](0006-network-watch.md) | N3a, N3b | proposed |
| [0007 · Actions and self-healing](0007-actions-and-self-healing.md) | N4a, N4b, N4c | proposed |
| [0008 · Host health and logs](0008-health-and-logs.md) | N5a, N5b | proposed |
| [0009 · Usage and power](0009-usage-and-power.md) | N6a | proposed |
| [0010 · Host security](0010-host-security.md) | N6b | proposed |
| [0011 · Blocking addresses](0011-blocking.md) | N6c | proposed |
| [0012 · Service checks](0012-service-checks.md) | N7a | proposed |
| [0013 · Backups and scheduled jobs](0013-backups-and-jobs.md) | N7b | proposed |
| [0014 · UPS shutdown](0014-ups-shutdown.md) | N7c | proposed |
| [0015 · Upgrades](0015-upgrades.md) | N8a | proposed |
| [0016 · Pairing nodes](0016-pairing-nodes.md) | N8b | proposed |
| [0017 · UPS group shutdown over the link](0017-ups-group-over-link.md) | N8c | proposed |
| [0018 · Status page link](0018-status-page.md) | N8d | proposed |
| [0019 · Daily summary facts for DRIFT](0019-daily-summary.md) | N9 | proposed |
| [0020 · Hub: fleet sync and notifications](0020-hub-fleet-sync.md) | H1, H2 | future (additive, after the compatibility work) |
| [0021 · Probe: "installed, not running" right after install](0021-probe-group-not-yet-active.md) | any | proposed (app bug fix) |
| [0022 · Old and 32-bit hosts, the release manifest, clearer failures](0022-legacy-hardware-and-releases.md) | 0.28.0 | proposed |
| [0023 · Links on the LAN, hub on several addresses, hub doctor, event kinds](0023-lan-links-and-hub-doctor.md) | 0.28.0 | proposed |
| [0024 · Nightgrid Node on Windows Server](0024-windows-node.md) | 0.29.0 (W0) | proposed |
| [0025 · Windows node W1: logins, logs, ports, LAN, usage, files](0025-windows-node-w1.md) | 0.30.0 (W1) | proposed |
| [0026 · Windows node W2a: admin helper, service and power actions](0026-windows-node-w2a.md) | 0.31.0 (W2a) | proposed |
| [0027 · Windows node W2b: upgrade](0027-windows-node-upgrade.md) | 0.32.0 (W2b) | proposed |

Rules every spec follows:

- The app must keep working unchanged on hosts without the node, with an older or newer node, or with a capability switched off.
- The app calls only methods whose capability `hello` listed.
- Anything that runs as root on the host goes through the app's existing sudo flow and confirmation rules.
