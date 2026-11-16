# Nightgrid Node

An optional, lightweight agent for Linux servers that extends [Nightgrid](https://github.com/TheRoben-Dev/nightgrid), the homelab desktop app.

Nightgrid is agentless: it does everything over SSH, but only while the app is open. Close the laptop and nobody is watching. Nightgrid Node runs on the server and keeps watching. When the app reconnects, it hands over everything that happened, so graphs, uptime bars, the timeline and alerts have no gaps.

- **Optional.** Every Nightgrid feature works without it. The node makes things gap-free and faster; it is never required.
- **Tiny.** One static binary (about 2 MB, x86_64 and aarch64), about 2.5 MB of RAM, no measurable idle CPU.
- **No open ports, no outbound connections, no telemetry, no cloud.** The app talks to the node through the SSH connection it already has. systemd enforces this (`IPAddressDeny=any`, unix sockets only).
- **Least privilege.** Its own system account and a heavily sandboxed unit. Root decides what it may do, one grant at a time.

## What it does

While the app is closed, the node:

| | What | Since |
|---|---|---|
| 📈 | **Records metrics**: CPU, memory, swap, load, disks, network, temperatures and GPUs every 10 s. Keeps 1-minute points for 14 days, 15-minute ones for 90 days and hourly ones for 400 days, the same tiers as the app. | 0.1 |
| ⏱ | **Tracks uptime**: a heartbeat every minute. After a reboot it can tell a crash or power loss ("Down") from a clean reboot ("Restarted") and from a suspend ("Suspended"). | 0.2 |
| 💾 | **Watches disks**: hourly usage per mount and a "full in about N days" forecast. | 0.2 |
| 🚨 | **Evaluates alerts**: disk full, disk filling up and container down, with your thresholds from the app and the app's own grace periods. Alerts are kept and delivered when the app comes back ("while you were away"). | 0.2 / 0.3 |
| 🐳 | **Watches containers** (opt-in): states, crashes, restarts, out-of-memory kills and health changes, through Docker's or Podman's socket. Read-only. | 0.3 |
| 🗒 | **Keeps a timeline**: alerts firing and clearing, outages, boots and container events, numbered so the app pulls only what's new. | 0.2 |
| 🔑 | **Records logins** (opt-in): SSH logins and failed logins (summed per minute), from the journal. | 0.5 |
| 📄 | **Watches important files**: `sshd_config`, sudoers, `passwd`/`shadow`, cron, `hosts`, `fstab`, systemd units. It records what changed (size, hash, owner, mode), never the contents. | 0.5 |
| 🌐 | **Watches the LAN**: keeps an inventory of devices from the kernel's neighbour table and notes new devices, address changes, IP conflicts and a gateway that suddenly answers from another MAC (possible ARP spoofing). No privileges, nothing sent. With `allow netwatch`, a listen-only helper also spots DHCP servers, a rogue one, and devices' names. | 0.6 / 0.7 |
| 🩹 | **Heals** (opt-in): typed actions root allows (restart, start or stop a container or a listed systemd unit, prune dangling images, reboot or power off) from the app or from self-healing rules such as "unhealthy for 5 minutes → restart". Rules are off and dry-run by default, rate-limited, give up when the problem doesn't clear, and every run lands in a tamper-evident audit log. Risky edits can be wrapped in a rollback that puts the files back unless the app confirms it still reaches the host. | 0.8–0.10 |
| 🩺 | **Checks host health** (opt-in): disk SMART, ZFS/mdadm/Btrfs pools, pending updates and reboot-required, and UPS status through NUT, with the app's own checks and verdicts, through a read-only helper. Alerts for failing disks, degraded pools and power cuts while the app is closed. | 0.11 |
| ⚡ | **Pushes live**: while the app is connected, new events, alerts, container changes and metrics every 2 s are pushed as they happen, instead of the app polling over SSH. | 0.4 |

When the app reconnects, it asks for everything after its last cursor and merges it in. The node keeps no per-app state, so several computers running Nightgrid can sync from the same node.

### Coming next

See [docs/MILESTONES.md](docs/MILESTONES.md) for detail. The order is:

1. **Logs**: journal and container log anomalies using the app's rules.
2. **Fine-grained stats and host security**: 1 s per-process and per-container stats, power draw, file integrity, new listening ports, failed-login bursts.
3. **Node-to-node and service checks, scheduled jobs, group actions**: from inside the LAN, with the app closed.
4. **Upgrades with automatic revert, an optional tailnet transport and a status page.**

Anything that acts on its own (self-healing, auto-block, scheduled power) is off unless you turn it on, and every run is logged.

## How it fits together

```
Nightgrid app ──SSH──> server: sshd ──> nightgrid-node bridge ──> /run/nightgrid-node/node.sock
                                                                    │
                                          nightgrid-node run (user nightgrid-node)
                                            ├ sampler: /proc, /sys, statvfs
                                            ├ engine: uptime, disk usage, alerts
                                            ├ containers (only if root allowed it)
                                            └ SQLite history in /var/lib/nightgrid-node
```

The app speaks a small, versioned JSON-lines protocol to the socket. It starts with `hello`, which lists the node's capabilities, and the app only uses what is listed. Anything missing falls back to plain SSH. See [docs/PROTOCOL.md](docs/PROTOCOL.md).

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

Optional: let it watch containers, SSH logins and DHCP. Read [SECURITY.md](SECURITY.md) first: Docker's group is root-equivalent, the journal's group can read every log, and the network helper may read raw packets (it is filtered to ARP and DHCP and only listens).

```sh
sudo nightgrid-node allow docker     # undo with: sudo nightgrid-node deny docker
sudo nightgrid-node allow journal    # SSH logins; undo with: sudo nightgrid-node deny journal
sudo nightgrid-node allow netwatch   # DHCP and rogue DHCP (listen-only helper); undo with: sudo nightgrid-node deny netwatch
```

Remove it with `sudo nightgrid-node uninstall` (keeps history and config) or `sudo nightgrid-node uninstall --purge` (removes everything, including the service account). If the binary is gone, [dist/uninstall.sh](dist/uninstall.sh) does the same.

Works on any systemd distro. It is tested in CI on Debian 12, Ubuntu 22.04/24.04 (x86_64 and arm64) and Fedora 41. Proxmox VE and Fedora Atomic / Bazzite use the same paths.

## Commands

```
nightgrid-node run                 the service (systemd starts it)
nightgrid-node install [--user NAME]... [--no-start] [--json]
nightgrid-node uninstall [--purge] [--json]
nightgrid-node allow docker        let it read containers (root; root-equivalent)
nightgrid-node allow journal       let it read the journal for SSH logins (root)
nightgrid-node allow netwatch      start the listen-only ARP/DHCP helper (root; CAP_NET_RAW)
nightgrid-node allow helper        install the root helper for unit and power actions (root)
nightgrid-node allow health        install the read-only SMART/pools/updates/UPS helper (root)
nightgrid-node deny docker|journal|netwatch|helper|health   take that away again
nightgrid-node netwatch            the helper (its unit starts it)
nightgrid-node status              what the running node reports
nightgrid-node query METHOD [JSON] one protocol request, e.g. query alerts.active
nightgrid-node watch STREAM...     live frames: events, metrics, containers
nightgrid-node bridge              stdin/stdout ⇄ the node's socket (used by the app over SSH)
```

Configuration: `/etc/nightgrid-node/config.toml` (root-owned; every setting is in the file `install` writes).

## Docs

- [Design](docs/DESIGN.md): architecture, transport, storage, sync, security model, automatic actions, feature map
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

Parts of the metrics, alert, forecast, timeline and secret-filter code are vendored from Nightgrid (MIT, attributed in the file headers).

## License

MIT
