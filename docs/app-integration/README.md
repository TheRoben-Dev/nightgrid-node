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

Rules every spec follows:

- The app must keep working unchanged on hosts without the node, with an older or newer node, or with a capability switched off.
- The app calls only methods whose capability `hello` listed.
- Anything that runs as root on the host goes through the app's existing sudo flow and confirmation rules.
