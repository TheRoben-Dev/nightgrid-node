# App integration specs

The Nightgrid app repository is developed elsewhere and is read-only from here. Whatever the app has to do to use the node is written up in this folder, one spec per node milestone, for the app's own project to build.

| Spec | Node milestone | Status |
|---|---|---|
| [0001 · Install the node and sync metric history](0001-install-and-metrics-sync.md) | N0 | proposed |

Rules every spec follows:

- The app must keep working unchanged on hosts without the node, with an older or newer node, or with a capability switched off.
- The app calls only methods whose capability `hello` listed.
- Anything that runs as root on the host goes through the app's existing sudo flow and confirmation rules.
