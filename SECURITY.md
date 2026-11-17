# Security

## Reporting

Please report vulnerabilities privately through GitHub's **Report a vulnerability** (Security tab of this repository), not in public issues.

## Model in short

- Runs as the unprivileged `nightgrid-node` system account under a sandboxed systemd unit (`NoNewPrivileges`, no capabilities, `ProtectSystem=strict`, syscall filter).
- No network access at all (`IPAddressDeny=any`, `RestrictAddressFamilies=AF_UNIX`). The only way in is the unix socket `/run/nightgrid-node/node.sock` (mode 0660, group `nightgrid-node`); members of that group are chosen when installing.
- Read-only today. What it may do is set by root in `/etc/nightgrid-node/config.toml`; nothing sent over the socket can widen that.
- Stores no secrets.
- Releases: static binaries with `SHA256SUMS` and GitHub build provenance (minisign signatures if a key is ever configured) (`gh attestation verify <file> --repo TheRoben-Dev/nightgrid-node`).

## Container access (`allow docker`)

The container watcher needs the engine's socket. `sudo nightgrid-node allow docker` writes `/etc/systemd/system/nightgrid-node.service.d/docker.conf`, which adds the socket's group (normally `docker`) to the service. **Anyone who can use that socket can control the engine, and through it the host: this is root-equivalent.** The node itself only sends `GET` requests (container list and events), and the rest of the sandbox stays in place, but a flaw in the node could be used for more. Grant it on hosts where that trade-off is fine; `sudo nightgrid-node deny docker` takes it away. Sockets owned by group `root` are refused.

## Journal access (`allow journal`)

Login events need the system journal. `sudo nightgrid-node allow journal` writes `/etc/systemd/system/nightgrid-node.service.d/journal.conf`, adding the journal's group (normally `systemd-journal`). That group can read every log on the host, which can include secrets programs logged. The node only keeps sshd's login lines, after the same secret filter as the app. `sudo nightgrid-node deny journal` takes it away.

## Watched files

The file watcher only records metadata (size, owner, mode, time) and a short SHA-256 of files the service can read. It never stores or sends contents. Root chooses the list in `/etc/nightgrid-node/config.toml`. With `[files] baseline` on (the default) the list also includes more of `/etc`, `authorized_keys` of root and every login user, and the compose files of running containers. With `allow health`, those snapshots are taken by the read-only health helper (which can read homes and root-only files, read-only), and only the metadata and a 16-hex-digit hash prefix reach the service. The helper works out the paths from root's config and `/etc/passwd` itself; the service can't ask it about other files (decision N-037).

## Listening ports and failed logins

Listening sockets are read from `/proc/net` like any user can; nothing is probed or opened. Login-burst alerts count the failed logins the journal watcher already reads (`allow journal`). Neither adds a privilege.

## Network watch

The device inventory comes from `/proc/net/arp` and `/proc/net/route`, which any user can read. The node sends no packets for it and the service still has no network address family (`RestrictAddressFamilies=AF_UNIX`). It stores MAC and IPv4 addresses of devices on the LAN in its database; `uninstall --purge` removes them. 

## Network watch helper (`allow netwatch`)

DHCP and ARP from other devices can only be seen with a packet socket. `sudo nightgrid-node allow netwatch` installs and starts `nightgrid-node-netwatch.service`, a second process under the same service account whose only privilege is `CAP_NET_RAW` (decision N-021). It is listen-only: a kernel BPF filter passes it ARP and DHCP (UDP 67/68) and nothing else, it never sends a packet, it has no IP sockets (`RestrictAddressFamilies=AF_UNIX AF_PACKET`, `IPAddressDeny=any`) and nothing to write. It hands addresses, MACs, DHCP settings and DHCP host names to the main service, which accepts those reports only from its own account. `sudo nightgrid-node deny netwatch` stops and removes it.

## Actions and self-healing

The node changes nothing on the host unless root lists an action type in `[actions] allow` in `/etc/nightgrid-node/config.toml` (empty by default). Actions are typed (`container.restart` and so on, with the container name checked against the engine's list), never shell strings, and container actions go through the engine socket that `allow docker` already grants. Self-healing rules come from the app, are off and dry-run by default, and can only run allowed actions within a global hourly cap and their own caps. Every request, including dry runs and refusals, is written to a hash-chained audit log (`actions.log`) and the timeline.

## Root helper (`allow helper`)

Unit and power actions need root. `sudo nightgrid-node allow helper` installs a socket that only the service account can reach; each request starts one short-lived root process that re-reads the root-owned config, runs `systemctl` on a unit root listed in `[actions] units` or schedules `shutdown` a minute ahead, answers and exits. A compromised service can ask only for what root listed. The helper has no network, no new privileges, a read-only system and only `CAP_SYS_BOOT` (decision N-032). `sudo nightgrid-node deny helper` removes it.

For rollbacks (`files.rollback`), the helper also copies and puts back files under `[rollback] paths`, keeping their owner and mode, so its unit holds `CAP_DAC_OVERRIDE`, `CAP_DAC_READ_SEARCH`, `CAP_CHOWN` and `CAP_FOWNER` as well; the system stays read-only to it except for those paths (written into the unit by `allow helper`) and its snapshot directory, `/var/lib/nightgrid-node-rollback` (0700). Keep the list narrow: whatever is listed, the service can ask to have put back to an earlier version (decision N-033).

## Health helper (`allow health`)

Disk SMART needs raw device commands, so `sudo nightgrid-node allow health` installs a second, read-only helper the same way: a socket only the service account can reach, one short-lived root process per read. It runs fixed scripts (smartctl, zpool, `/proc/mdstat`, Btrfs sysfs, the package manager's local cache, and `upsc` only for targets in `[health] ups`), never a command from the service. Its unit has only `CAP_SYS_RAWIO`, `CAP_SYS_ADMIN` and `CAP_DAC_READ_SEARCH`, a read-only file system, no new privileges, and network only to localhost and private LAN ranges (for a NUT server), never the internet (decision N-034). Since 0.14.0 it can also read home directories read-only (`ProtectHome=read-only`), for the file baseline's `authorized_keys`. `sudo nightgrid-node deny health` removes it.

## Power (`allow power`)

`sudo nightgrid-node allow power` lets the service's group read the CPU energy counters (RAPL), through a udev rule in `/etc/udev/rules.d/70-nightgrid-node-power.rules`. The kernel makes them root-only because sampling them very fast can hint at what other programs compute (CVE-2020-8694); the node reads them at most once a second. `deny power` makes them root-only again (decision N-036). Everything else in usage (cgroups, `/proc`) is readable by any user and needs no grant.

## Logs

Log watching reads only what `allow journal` and `allow docker` already grant. Lines are kept in memory (up to 40 recent error lines per source, for the app's "what broke" view) and never written to disk; lines that look like they carry a secret (private keys, tokens, `password=` and similar, the app's own check) are dropped before they are kept and are never put in an alert (decision N-035).

Full details: [docs/DESIGN.md §7](docs/DESIGN.md#7-security-model).
