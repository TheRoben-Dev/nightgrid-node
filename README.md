# Nightgrid Node

An optional, lightweight agent for Linux servers that extends [Nightgrid](https://github.com/TheRoben-Dev/nightgrid), the homelab desktop app. Nightgrid works over plain SSH while it's open; Nightgrid Node keeps collecting when the app is closed or your computer is offline, and hands the history over when the app reconnects.

- **Optional.** Every Nightgrid feature works without it. The node only makes things faster and gap-free.
- **Tiny.** One static binary (~2 MB, x86_64 and aarch64), ~2.5 MB of RAM, no measurable idle CPU.
- **No open ports, no outbound connections, no telemetry.** The app talks to it through the SSH connection it already has. systemd enforces this (`IPAddressDeny=any`, unix sockets only).
- **Least privilege.** Its own system account, a heavily sandboxed unit, read-only. Root decides what it may do.

Status: **N0** — install/uninstall, metrics history with the app's 1-minute / 15-minute / hourly tiers (400 days), and sync. See [docs/MILESTONES.md](docs/MILESTONES.md) for what's next.

## Install by hand

The app will do this for you (see [docs/app-integration](docs/app-integration/)). By hand, on the server:

```sh
# Download nightgrid-node-vX.Y.Z-<arch>-linux and SHA256SUMS from Releases, then:
sha256sum -c --ignore-missing SHA256SUMS
chmod +x nightgrid-node-*-linux
sudo ./nightgrid-node-*-linux install          # adds you (the sudo user) to the nightgrid-node group
# log in again so the group applies, then:
nightgrid-node status
```

Remove it with `sudo nightgrid-node uninstall` (keeps history and config) or `sudo nightgrid-node uninstall --purge` (removes everything, including the service account). If the binary is gone, [dist/uninstall.sh](dist/uninstall.sh) does the same.

Works on any systemd distro: tested on Debian, Ubuntu and Fedora; Proxmox VE and Fedora Atomic / Bazzite use the same paths.

## Commands

```
nightgrid-node run                 the service (systemd starts it)
nightgrid-node install [--user NAME]... [--no-start] [--json]
nightgrid-node uninstall [--purge] [--json]
nightgrid-node status              what the running node reports
nightgrid-node query METHOD [JSON] one protocol request, e.g. query metrics.latest
nightgrid-node bridge              stdin/stdout ⇄ the node's socket (used by the app over SSH)
```

Configuration: `/etc/nightgrid-node/config.toml` (root-owned; every setting is in the file `install` writes).

## Docs

- [Design](docs/DESIGN.md): architecture, transport, storage, sync, security model, upgrades, feature map
- [Protocol](docs/PROTOCOL.md): versioned NDJSON protocol with capability negotiation
- [Milestones](docs/MILESTONES.md) and [Decisions](docs/DECISIONS.md)
- [App integration specs](docs/app-integration/): what the Nightgrid app needs to do
- [Security](SECURITY.md)

## Development

```sh
cargo test                       # unit + end-to-end tests (no root needed)
cargo clippy --all-targets -- -D warnings
cargo zigbuild --release --target x86_64-unknown-linux-musl   # static build (pip install ziglang; cargo install cargo-zigbuild)
cargo run -- run --socket /tmp/ngn.sock --data-dir /tmp/ngn --config /dev/null   # run without installing
cargo run -- query node.info --socket /tmp/ngn.sock
```

Parts of the metrics code are vendored from Nightgrid (MIT, attributed in the file headers).

## License

MIT
