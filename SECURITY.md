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

Full details: [docs/DESIGN.md §7](docs/DESIGN.md#7-security-model).
