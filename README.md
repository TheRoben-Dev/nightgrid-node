# Nightgrid Node

An optional, lightweight agent for Linux servers that extends [Nightgrid](https://github.com/TheRoben-Dev/nightgrid), the homelab desktop app.

Nightgrid is agentless: it does everything over SSH, but only while the app is open. Close the laptop and nobody is watching. Nightgrid Node runs on the server and keeps watching. When the app reconnects, it hands over everything that happened, so graphs, uptime bars, the timeline and alerts have no gaps.

- **Optional.** Every Nightgrid feature works without it. The node makes things gap-free and faster; it is never required.
- **Tiny.** One static binary (a few MB; x86_64, aarch64, armv7, armv6 and i686), a few MB of RAM, no measurable idle CPU. Runs on old and small boxes too, such as a 512 MB ARM NAS.
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
| 📄 | **Watches important files**: `sshd_config`, sudoers, `passwd`/`shadow`, cron, `hosts`, `fstab`, systemd units, and a wider baseline: PAM, `ld.so.preload`, package sources, everyone's `authorized_keys` and your compose files. It records what changed (size, hash, owner, mode), never the contents. | 0.5 / 0.14 |
| 🌐 | **Watches the LAN**: keeps an inventory of devices from the kernel's neighbour table and notes new devices, address changes, IP conflicts and a gateway that suddenly answers from another MAC (possible ARP spoofing). No privileges, nothing sent. With `allow netwatch`, a listen-only helper also spots DHCP servers, a rogue one, and devices' names. | 0.6 / 0.7 |
| 🩹 | **Heals** (opt-in): typed actions root allows (restart, start or stop a container or a listed systemd unit, prune dangling images, reboot or power off) from the app or from self-healing rules such as "unhealthy for 5 minutes → restart". Rules are off and dry-run by default, rate-limited, give up when the problem doesn't clear, and every run lands in a tamper-evident audit log. Risky edits can be wrapped in a rollback that puts the files back unless the app confirms it still reaches the host. | 0.8–0.10 |
| 🩺 | **Checks host health** (opt-in): disk SMART, ZFS/mdadm/Btrfs pools, pending updates and reboot-required, and UPS status through NUT, with the app's own checks and verdicts, through a read-only helper. Alerts for failing disks, degraded pools and power cuts while the app is closed. | 0.11 |
| 📜 | **Watches logs** (opt-in): error bursts and new kinds of errors in the journal and containers, with the app's own detector, and the app's saved searches with alerts, every 10 minutes. Lines stay in memory and secret-looking ones are dropped. | 0.12 |
| 🛡 | **Watches the attack surface**: a socket that starts listening (with the container publishing it) shows up in the timeline, louder when it's reachable from the network, and a burst of failed SSH logins becomes an alert. No privileges needed beyond the journal grant. | 0.14 |
| 🚧 | **Blocks brute-forcers** (opt-in, off by default): addresses that keep failing SSH logins, or one you pick in the app, are blocked for a while in a firewall table of the node's own, which expires by itself. Never your LAN or tailnet, never an address that logged in recently, and every block is logged. | 0.15 |
| 💾 | **Watches backups and scheduled jobs**: the backup jobs you set up in the app (restic, borg, rsync, rclone) alert when one fails or gets stale, and every run of a backup or of the app's scheduled jobs lands in the timeline, scheduled reboots included. systemd runs the schedules; the node only watches. | 0.17 |
| 🔋 | **Shuts down on a long power cut** (opt-in, off by default): when the UPS has run on battery too long or gets low, the host announces a countdown and powers itself off, with the app closed. Each host has its own limits, so storage can go last; power coming back or a tap in the app cancels it. | 0.18 |
| 📡 | **Checks your services from inside the LAN** (opt-in): the app's HTTP(S) and TCP checks, with status codes and certificate expiry, every minute with the app closed, so "down" is told from "unreachable from my laptop". A separate runner does it, allowed to reach only the networks you list; the node itself stays offline. | 0.16 |
| 📊 | **Measures per service, container and process**: CPU, memory, disk and network per systemd service and container (from cgroups), the busiest processes, and power draw from the CPU's energy counters (opt-in). Every second while you watch, every 10 s otherwise, with a day of one-minute history. | 0.13 |
| ⬆️ | **Upgrades safely**: the app hands over a new release with its checksum; the node installs it only if the checksum matches, checks that the new version comes up and stays up, and puts the old one back by itself if not. | 0.19 |
| 🔗 | **Links nodes** (opt-in, off by default): nodes you pair in the app check each other over the tailnet with mutual TLS, so a dead host is told from a broken path, with the app closed. Hosts without a UPS reading can follow the UPS host's shutdown. Each node still acts alone unless you pair it; it answers only what you allow and listens only on the tailnet address. | 0.20 / 0.21 |
| 📱 | **Shows a status page** (opt-in, off by default): a read-only page for your phone on the tailnet, with the host's state, alerts, UPS and linked hosts, at a secret link. No scripts, nothing it can change. | 0.22 |
| 📝 | **Prepares a daily summary** for the app's DRIFT helper: what broke, what changed and what is filling up while the app was closed, as short facts a small local model can use. Made on request; nothing leaves the host. | 0.23 |
| ⚡ | **Pushes live**: while the app is connected, new events, alerts, container changes and metrics every 2 s are pushed as they happen, instead of the app polling over SSH. | 0.4 |

When the app reconnects, it asks for everything after its last cursor and merges it in. The node keeps no per-app state, so several computers running Nightgrid can sync from the same node.

### Coming next

See [docs/MILESTONES.md](docs/MILESTONES.md) for detail. The order is:

1. **Daily summaries** for the app's DRIFT helper.

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
VER=v0.28.0
case $(uname -m) in                            # the release arch for this machine
  x86_64|amd64) ARCH=x86_64 ;;  aarch64|arm64) ARCH=aarch64 ;;
  armv7*|armv8l) ARCH=armv7 ;;  armv6*) ARCH=armv6 ;;  i?86|x86) ARCH=i686 ;;
  *) echo "no build for $(uname -m)"; exit 1 ;;
esac
URL=https://github.com/TheRoben-Dev/nightgrid-node/releases/download/$VER
curl -fL -O "$URL/nightgrid-node-$VER-$ARCH-linux" -O "$URL/SHA256SUMS"
sha256sum -c --ignore-missing SHA256SUMS      # BusyBox: grep "node-$VER-$ARCH-linux" SHA256SUMS | sha256sum -c
chmod +x nightgrid-node-$VER-$ARCH-linux
sudo ./nightgrid-node-$VER-$ARCH-linux install # copies itself to /usr/local/bin, adds you (the sudo user) to the nightgrid-node group
# log in again so the group applies, then:
nightgrid-node status
```

The binary carries its own systemd units, so there is nothing else to download. Optionally check where it was built: `gh attestation verify nightgrid-node-$VER-$ARCH-linux --repo TheRoben-Dev/nightgrid-node`.

Optional: let it watch containers, SSH logins and DHCP. Read [SECURITY.md](SECURITY.md) first: Docker's group is root-equivalent, the journal's group can read every log, and the network helper may read raw packets (it is filtered to ARP and DHCP and only listens).

```sh
sudo nightgrid-node allow docker     # undo with: sudo nightgrid-node deny docker
sudo nightgrid-node allow journal    # SSH logins; undo with: sudo nightgrid-node deny journal
sudo nightgrid-node allow netwatch   # DHCP and rogue DHCP (listen-only helper); undo with: sudo nightgrid-node deny netwatch
```

Remove it with `sudo nightgrid-node uninstall` (keeps history and config) or `sudo nightgrid-node uninstall --purge` (removes everything, including the service account). If the binary is gone, [dist/uninstall.sh](dist/uninstall.sh) does the same.

Works on any systemd distro. It is tested in CI on Debian 12, Ubuntu 22.04/24.04 (x86_64 and arm64) and Fedora 41, and on Debian 12 armhf under qemu. Proxmox VE and Fedora Atomic / Bazzite use the same paths.

Older and smaller hosts (0.28.0):

- **Old systemd** (215 and later, such as ReadyNAS OS 6): the unit is fitted to the version. Settings it doesn't know are commented out with the version they need, and older spellings are used where they exist (`MemoryLimit=`, `ProtectSystem=full`). `nightgrid-node doctor` lists what was left out.
- **No systemd** (Alpine and other OpenRC hosts; sysvinit with `start-stop-daemon`, BusyBox included): `install` writes `/etc/init.d/nightgrid-node` and enables it, with the same account, socket and modes. Without systemd's sandbox the node is still unprivileged. The extra access (`allow …`), `serve`, `upgrade` and the hub need systemd, and they say so (`{"error":{"code":"no_systemd",…}}` with `--json`). Tested in CI on Alpine (OpenRC, x86_64 and armv7) and Debian with sysvinit (x86_64 and i686).
- **32-bit** (armv7, armv6, i686): the same features. Counters and sizes are 64-bit throughout, and times work past 2038. `install --json` and `doctor` report `arch`, `libc`, `init` and `kernel`.

## Commands

On a terminal, `nightgrid-node` (or `nightgrid-hub`) on its own opens a menu: arrow keys or j/k, number keys, Enter, q. Every settings-style command (`fix`, `allow`/`deny` with no name, `serve` with no arguments) opens the same menu, in the Nightgrid app's colours. Piped, or with `NO_COLOR` or `NIGHTGRID_PLAIN=1`, it prints a plain numbered list instead.

### Fix things on the host

```
nightgrid-node fix                     menu: what looks wrong, every fix, automation
nightgrid-node fix list [--json]       the same as a list
nightgrid-node fix restart nginx.service   restart a unit (or a container: fix restart web)
nightgrid-node fix reset UNIT          clear a unit's failed state
nightgrid-node fix start CONTAINER     start a stopped container
nightgrid-node fix journal             shrink the journal (to journal_keep, 500M)
nightgrid-node fix logs                rotate all logs now (logrotate --force)
nightgrid-node fix packages            clean the package cache (apt, dnf, zypper, pacman, apk)
nightgrid-node fix prune               remove unused container images
nightgrid-node fix ssh [USER]          fix ~/.ssh and authorized_keys permissions
nightgrid-node fix reboot              reboot in one minute (type "reboot" to confirm)
nightgrid-node fix run RUNBOOK         run a command root defined in fixes.toml
nightgrid-node fix auto                menu: what the node may fix by itself
nightgrid-node fix auto NAME on|preview|off   services, containers, stopped, journal, packages, images (root)
```

Each fix says what it will run and asks first (`-y` skips the question; a reboot asks you to type the word). Nothing new is trusted: a run goes to the node as an ordinary action, so root's allow list, `max_per_hour`, the hash-chained audit log (`nightgrid-node audit`) and the root helper apply, and the menu itself runs nothing privileged. With `sudo`, the menu offers to allow what isn't allowed yet and saves that in `/etc/nightgrid-node/fixes.toml`, next to `config.toml` (which it never rewrites). Fixes that write outside the helper's sandbox run as a short transient unit with their own limits.

Automation is off until root turns a preset on. `preview` only logs what it would do; `on` acts after the condition has held for a while (5 or 10 minutes), at most a few times an hour and a day per target, and gives up after a few tries until the condition clears. It's the same engine as the app's self-healing rules.

`fixes.toml` can also hold runbooks:

```toml
[[runbook]]
name = "renew-certs"
description = "Renew TLS certificates"
run = ["/usr/bin/certbot", "renew", "--quiet"]   # absolute path, no shell
timeout_secs = 300                                # 1-600
```

### Everything else

```
nightgrid-node run                 the service (systemd starts it)
nightgrid-node install [--user NAME]... [--no-start] [--json]
nightgrid-node uninstall [--purge] [--json]
nightgrid-node allow docker        let it read containers (root; root-equivalent)
nightgrid-node allow journal       let it read the journal for SSH logins (root)
nightgrid-node allow netwatch      start the listen-only ARP/DHCP helper (root; CAP_NET_RAW)
nightgrid-node allow helper        install the root helper for unit and power actions (root)
nightgrid-node allow health        install the read-only SMART/pools/updates/UPS helper (root)
nightgrid-node allow power         let it read the CPU energy counters for power draw (root)
nightgrid-node allow checks        start the check runner for the app's service checks (root)
nightgrid-node allow peers         start the node-to-node link for nodes the app pairs (root)
nightgrid-node allow statuspage    start the read-only status page and print its link (root)
nightgrid-node serve add|remove ping|ups|sync   what paired peers may ask this node (root; `sync` lets its hub read it)
nightgrid-node deny docker|journal|netwatch|helper|health|power|checks|peers|statuspage   take that away again
nightgrid-node allow | deny | serve menus of the above, with what's on
nightgrid-node netwatch            the helper (its unit starts it)
nightgrid-node upgrade FILE --sha256 HEX   verified upgrade that reverts itself (root)
nightgrid-node status              what the running node reports (--json: raw)
nightgrid-node doctor              check this host's install, end to end
nightgrid-node events              the last events (--count N)
nightgrid-node audit [verify]      the action log, or check its whole hash chain
nightgrid-node query METHOD [JSON] one protocol request, e.g. query alerts.active
nightgrid-node watch STREAM...     live frames: events, metrics, containers, usage
nightgrid-node bridge              stdin/stdout ⇄ the node's socket (used by the app over SSH)
nightgrid-node completions SHELL   bash, zsh or fish completions; `man` prints the manual page
```

Configuration: `/etc/nightgrid-node/config.toml` (root-owned; every setting is in the file `install` writes).

### Optional: one hub per network

`nightgrid-hub` is a separate, heavier binary for one always-on host per network (0.26.0, milestone H1). It pulls every member's history read-only over the node-to-node link, keeps it in one store, and raises "host offline" alerts while the app is closed. Lite nodes don't need it and nothing changes for them unless root runs `nightgrid-node serve add sync` and the app names the hub. Design and limits: [docs/HUB.md](docs/HUB.md).

On the one host that should be the hub, after Nightgrid Node is installed there (same `VER`, `ARCH` and `URL` as above):

```sh
curl -fL -O "$URL/nightgrid-hub-$VER-$ARCH-linux" -O "$URL/SHA256SUMS"
sha256sum -c --ignore-missing SHA256SUMS      # BusyBox: grep "hub-$VER-$ARCH-linux" SHA256SUMS | sha256sum -c
chmod +x nightgrid-hub-$VER-$ARCH-linux
sudo ./nightgrid-hub-$VER-$ARCH-linux install   # copies itself to /usr/local/bin, writes the nightgrid-hub and nightgrid-hub-collect units
sudo nightgrid-hub allow collect                 # prints the hub id and fingerprint the app pairs
# on each member that the hub should read (the hub host is a member too):
sudo nightgrid-node allow peers                  # the node-to-node link; the hub reads members over it
sudo nightgrid-node serve add sync
```

Both links listen on the host's tailnet address by default, so each host needs Tailscale (or Headscale) up: `tailscale ip -4` should print a `100.x` address.

The app pairs the hub with its members. Until it does, pair by hand. On the hub host:

```sh
j() { python3 -c "import json,sys; print(json.load(sys.stdin)$1)"; }
sudo nightgrid-hub allow collect --json > hub.json
HUB_ID=$(j '["hubId"]' < hub.json); HUB_FP=$(j '["fingerprint"]' < hub.json); HUB_ADDR=$(j '["listening"]' < hub.json)
echo "$HUB_ID $HUB_FP $HUB_ADDR"
```

On each member (with those three values copied over):

```sh
j() { python3 -c "import json,sys; print(json.load(sys.stdin)$1)"; }
sudo nightgrid-node query peers.set "{\"peers\":[{\"id\":\"$HUB_ID\",\"name\":\"hub\",\"address\":\"$HUB_ADDR\",\"fingerprint\":\"$HUB_FP\"}]}"
sudo nightgrid-node query hub.member.set "{\"hubId\":\"$HUB_ID\"}"
sudo nightgrid-node query node.info | j '["node"]["nodeId"]'      # this member's id
sudo nightgrid-node query peers.latest | j '["fingerprint"]'      # its fingerprint
sudo nightgrid-node query peers.latest | j '["listening"]'        # its address
```

`peers.set` replaces the node's whole peer list, so list any other peers there too. Then on the hub, list every member in one `fleet.members.set` (it also replaces the list) and check:

```sh
sudo nightgrid-hub query fleet.members.set '{"members":[{"id":"NODE_ID","name":"lumen","address":"NODE_ADDR","fingerprint":"NODE_FP"}]}'
nightgrid-hub status      # each member should show "ok" within a minute or two
```

Use the same version of `nightgrid-node` and `nightgrid-hub` on the hub host.

```
nightgrid-hub install [--user NAME]... [--json]   install next to Nightgrid Node (root)
nightgrid-hub                      menu (on a terminal)
nightgrid-hub allow | deny         menu: the collector, on or off
nightgrid-hub allow collect        start the collector; prints the hub id and fingerprint to pair (root)
nightgrid-hub deny collect         stop it (root)
nightgrid-hub status               the members and their state
nightgrid-hub query METHOD [JSON]  one request, e.g. query fleet.overview
nightgrid-hub uninstall [--purge]  remove the hub; the node stays (root)
```

## Troubleshooting

Start with `nightgrid-node doctor`. Any user can run it, it changes nothing, and it names the first thing that's wrong:

| doctor says | what to do |
| --- | --- |
| `FAIL config` | Fix the line it names in `/etc/nightgrid-node/config.toml`, then `sudo systemctl restart nightgrid-node`. |
| `FAIL service` | `journalctl -u nightgrid-node -n 50` shows why it stopped. |
| `WARN socket … Permission denied` | This user isn't in the node's group: `sudo nightgrid-node install --user $USER`, then log in again. |
| `WARN version … restart` | A new binary is installed but the old one still runs: `sudo systemctl restart nightgrid-node`. |
| `WARN allow X … is failed` | That grant's own unit stopped: `journalctl -u` the unit it names. |
| `WARN memory` | Over the 20 MB budget; please open an issue with `nightgrid-node status --json`. |

`nightgrid-node events` shows what the node noticed recently. `nightgrid-node audit verify` checks that nobody edited the action log. If the history database was damaged (after a power cut, for example), the node moves it to `node.db.damaged`, starts a fresh one under the same node id, and records a "history database replaced" event. The app refills its own history as usual.

## Docs

- [Design](docs/DESIGN.md): architecture, transport, storage, sync, security model, automatic actions, feature map
- [Protocol](docs/PROTOCOL.md): versioned NDJSON protocol with capability negotiation
- [Hub profile](docs/HUB.md): the optional `nightgrid-hub`, one per network
- [Milestones](docs/MILESTONES.md) and [Decisions](docs/DECISIONS.md)
- [App integration specs](docs/app-integration/): what the Nightgrid app needs to do
- [Security](SECURITY.md)

## Development

```sh
cargo test                       # unit + end-to-end tests (no root needed)
cargo clippy --all-targets -- -D warnings
cargo zigbuild --release --target x86_64-unknown-linux-musl   # static build (pip install ziglang; cargo install cargo-zigbuild)
cargo run -p nightgrid-node -- run --socket /tmp/ngn.sock --data-dir /tmp/ngn --config /dev/null   # run without installing
cargo run -p nightgrid-node -- query node.info --socket /tmp/ngn.sock
```

Releasing: bump `version` in `Cargo.toml` and add its `## X.Y.Z` section to CHANGELOG.md. When that lands on main, [auto-release.yml](.github/workflows/auto-release.yml) tags the commit `vX.Y.Z` and publishes the release (binaries, SHA256SUMS, provenance). A tag pushed by hand still publishes too.

Parts of the metrics, alert, forecast, timeline and secret-filter code are vendored from Nightgrid (MIT, attributed in the file headers).

## License

MIT
