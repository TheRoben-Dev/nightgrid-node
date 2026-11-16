# Security

## Reporting

Please report vulnerabilities privately through GitHub's **Report a vulnerability** (Security tab of this repository), not in public issues.

## Model in short

- Runs as the unprivileged `nightgrid-node` system account under a sandboxed systemd unit (`NoNewPrivileges`, no capabilities, `ProtectSystem=strict`, syscall filter).
- No network access at all (`IPAddressDeny=any`, `RestrictAddressFamilies=AF_UNIX`). The only way in is the unix socket `/run/nightgrid-node/node.sock` (mode 0660, group `nightgrid-node`); members of that group are chosen when installing.
- Read-only today. What it may do is set by root in `/etc/nightgrid-node/config.toml`; nothing sent over the socket can widen that.
- Stores no secrets.
- Releases: static binaries with `SHA256SUMS`, minisign signatures, and GitHub build provenance (`gh attestation verify <file> --repo TheRoben-Dev/nightgrid-node`).

Full details: [docs/DESIGN.md §7](docs/DESIGN.md#7-security-model).
