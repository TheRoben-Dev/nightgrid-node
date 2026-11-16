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

The file watcher only records metadata (size, owner, mode, time) and a short SHA-256 of files the service can read. It never stores or sends contents. Root chooses the list in `/etc/nightgrid-node/config.toml`.

## Network watch

The device inventory comes from `/proc/net/arp` and `/proc/net/route`, which any user can read. The node sends no packets for it and the service still has no network address family (`RestrictAddressFamilies=AF_UNIX`). It stores MAC and IPv4 addresses of devices on the LAN in its database; `uninstall --purge` removes them. 

## Network watch helper (`allow netwatch`)

DHCP and ARP from other devices can only be seen with a packet socket. `sudo nightgrid-node allow netwatch` installs and starts `nightgrid-node-netwatch.service`, a second process under the same service account whose only privilege is `CAP_NET_RAW` (decision N-021). It is listen-only: a kernel BPF filter passes it ARP and DHCP (UDP 67/68) and nothing else, it never sends a packet, it has no IP sockets (`RestrictAddressFamilies=AF_UNIX AF_PACKET`, `IPAddressDeny=any`) and nothing to write. It hands addresses, MACs, DHCP settings and DHCP host names to the main service, which accepts those reports only from its own account. `sudo nightgrid-node deny netwatch` stops and removes it.

Full details: [docs/DESIGN.md §7](docs/DESIGN.md#7-security-model).
