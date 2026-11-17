# Nightgrid Node: Hub profile (design proposal)

Status: **proposed** (H0, design only). Nothing here is built. Decisions N-051 to N-059 in [DECISIONS.md](DECISIONS.md) record the choices, milestones H1 to H7 are in [MILESTONES.md](MILESTONES.md#hub-profile-proposed), and the app side is the future spec [app-integration/0020](app-integration/0020-hub-fleet-sync.md).

Roben, 2026-11-17: keep Lite and Hub separate, with the Hub profile recommended as one system per network.

## 0. Summary

Nightgrid Node today is one small agent per host ("Lite"). It watches its own host, keeps history, opens no ports by default and does nothing unless the app or root allows it. That stays exactly as it is.

The **Hub** is a second, optional and heavier component that runs on one always-on host per network. It does two things first:

1. **Fleet aggregation.** The hub pulls history, events and alerts from the Lite nodes on its network, over the existing node-to-node link, and keeps a copy. The app can then sync the whole network from one place, and the network is watched as a whole while the app is closed.
2. **Direct notifications.** The hub sends alerts to ntfy, Discord, a generic webhook or email, with no Mac or PC running.

Later it can host LAN service checks, SNMP collection, anomaly detection, a log index, runbooks, backup verification and patch windows (sections 9 to 15).

The rules that make this safe:

- **Lite does not change.** A Lite node behaves the same with or without a hub. It never depends on one and never pushes to one.
- **The hub pulls, read-only.** A Lite node only answers what root on that host allows (`[peers] serve`), to the one peer the app named as its hub. The hub can't run anything on a Lite node; it can at most ask a node to *hold* an automatic action it was about to take (section 7.3).
- **Hub down means business as usual.** Lite nodes keep working alone and the app syncs each host directly, as today, without gaps or duplicates.
- **Frozen interfaces stay frozen.** No method, field, frame or link request that exists today changes. Everything new is additive, versioned and opt-in (section 16).

## 1. Lite and Hub

| | Lite (today) | Hub (proposed) |
|---|---|---|
| Where | Every Linux host that wants it | One always-on host per network (recommended) |
| Binary | `nightgrid-node`, ~4.7 MB static | `nightgrid-hub`, separate static binary from the same workspace and release |
| Account | `nightgrid-node` | `nightgrid-hub` (own account, own state directory) |
| Memory | ~2.5 MB RSS, `MemoryMax=64M` on the main unit | 100 to 200 MB target for the core, hard caps per unit (section 4) |
| Network | None on the main service; opt-in units for checks, link, status page | Tailnet egress to its Lite nodes; the notifier may reach the internet when a channel needs it |
| Watches | Its own host | The network: its nodes, and later services and devices that can't run Node |
| Acts | Typed, root-allowed actions on its own host | Nothing on other hosts. Only "hold" signals (section 7.3) |
| Needed by | Nothing; everything works over SSH without it | Nothing; everything works with Lite alone |

A hub host also runs Lite for itself. The hub reads its own host through the local Lite socket like the app does, so there is one way to collect a host's data and the hub never samples anything itself.

### 1.1 Why a separate binary (N-051)

Two options were weighed:

- **One binary with a profile flag** (`nightgrid-node run --profile hub`). One artifact, one upgrade path. But every Lite host would carry the hub's code (SMTP, HTTP client for webhooks, SNMP, an indexer later), which grows the binary on a Pi and widens what an attacker on that host can reach. A config edit could also turn a light agent into a heavy one, which is the opposite of "keep them separate".
- **A separate binary** (`nightgrid-hub`) built from the same Cargo workspace, sharing the `proto` crate and the parts of the node crate it needs, released with the same version number in the same release. Lite stays the size it is, the hub gets its own account, units and sandbox, and installing the hub is a deliberate act on one host.

**Recommendation: a separate binary.** Same version number as Lite (a 0.30.0 hub talks to 0.30.0 nodes best, and negotiates down with capabilities like the app does), same release workflow, same checksums and provenance (N-014), and the same verified upgrade with automatic revert (N-042).

Install, on the chosen host, after Lite:

```sh
sudo ./nightgrid-hub-*-linux install     # needs nightgrid-node on this host, same or newer minor
sudo nightgrid-hub allow notify          # optional: the notifier unit (section 8)
nightgrid-hub status
```

The app will do this for you (future spec 0020). `nightgrid-hub uninstall [--purge]` removes it and leaves Lite alone.

## 2. One hub per network

### 2.1 What "network" means

A network is a set of hosts the person thinks of as one place: a home LAN, a rack, a parents' house. In the app it is a host group the person marks as a site. A tailnet can span several sites; each site may have its own hub, and the app syncs each hub. A hub never pulls from hosts outside its site, even when the tailnet reaches them, because the person chose the members (section 2.3).

### 2.2 Designation, not election (N-052)

The hub is **chosen by the person in the app**, not elected by the nodes.

- An automatic election (Raft, bully, lowest id) needs every node to talk to every other node, adds split-brain cases and a quorum that a two-host homelab never has, and moves a decision about which machine does more work away from the person who knows which machine is always on and on a UPS.
- The app is already the trust anchor: it reaches every host over SSH as an admin and pairs nodes (N-043). Designating the hub is one more pairing step with a role.

The app suggests a host (always-on, on a UPS, with free memory, preferring the one that already reads the UPS) and the person confirms.

### 2.3 Discovery

No broadcast discovery (no mDNS, no UDP announce). Lite hosts never listen on the LAN by default, and nothing here changes that. Membership is set by the app:

1. The app installs the hub on the chosen host and turns on its link client (`allow collect`).
2. For each member host, the app pairs it with the hub over the existing `peers.set` (unchanged: the hub is a normal peer with its own pinned certificate), and then names the hub on that host with the new, additive `hub.member.set {hubId}` (capability `hub.member` 1, section 16).
3. Root on each member allows serving sync to that hub by adding `sync` to `[peers] serve` (the app does this through its sudo flow, like `allow peers`). Without root's consent, the hub gets nothing.
4. The app gives the hub its member list with `fleet.members.set` on the hub's own socket.

### 2.4 Keeping it to one

- The app refuses to designate a second hub in the same site and says which host is the hub.
- If two hubs end up pulling the same node anyway (a restored backup, a manual install), nothing breaks: pulls are read-only and idempotent. The node answers sync requests only from the peer `hub.member` names, so the second hub gets `"not this node's hub"` and raises a `hub` warning event ("nas also lists hub pi as its hub; only one hub per network").
- A **standby hub** (later, N-059) is the only supported second hub: it pulls the same members but keeps notifications muted while the primary answers link pings. If the primary misses three pings in a row (about three minutes), the standby sends its notifications; when the primary is back, it goes quiet again. Primary wins by designation, so there is nothing to vote on.

## 3. Shape

```
                Nightgrid app ──SSH──> hub host
                                         ├ nightgrid-node (Lite, for this host)        unchanged
                                         ├ nightgrid-hub            (account nightgrid-hub, no network)
                                         │    store: /var/lib/nightgrid-hub/hub.db
                                         │    socket: /run/nightgrid-hub/hub.sock (0660, group nightgrid-hub)
                                         ├ nightgrid-hub collect    (tailnet egress only, link client key)
                                         └ nightgrid-hub notify     (opt-in; egress to channel endpoints)

  Lite host A ── nightgrid-node-peer (existing link unit, mTLS, tailnet) <── pull ── collect
  Lite host B ── nightgrid-node-peer                                    <── pull ── collect
```

Units, all as the `nightgrid-hub` account with no capabilities, `NoNewPrivileges`, `ProtectSystem=strict`, a syscall filter, and their own `MemoryMax`:

| Unit | Does | Network | Why separate |
|---|---|---|---|
| `nightgrid-hub` | Store, rollups, alert routing, the hub socket for the app | None (`IPAddressDeny=any`, `AF_UNIX` only) | Holds all the data; keeps it offline like the Lite main service (N-039) |
| `nightgrid-hub-collect` | Pulls members over the link, hands batches to the main unit over its socket. Also listens on the tailnet address on its own port (7420 by default, since the host's Lite link has 7419), answering only `peer.ping`, so members' existing pings see the hub | `IPAddressAllow` from root's `[collect] networks` (tailnet ranges by default) | Holds the link key and the only LAN or tailnet connections |
| `nightgrid-hub-notify` | Sends rendered messages to channels | Only what root's `[notify] networks` allows; `any` only if root says so for a public endpoint | The only unit that may reach the internet, and it never sees raw data |
| later: `-checks`, `-snmp`, `-index` | Sections 9, 10, 12 | Their own lists | Each new kind of exposure gets its own unit and budget |

This mirrors what Lite already does: the process with the history never has the network, and each piece that needs a network has only the one it needs.

## 4. Resource budget (N-053)

Targets for a network of **25 Lite nodes** on a Raspberry Pi 4 (4 GB) or any small x86 box. They are design targets; H1 adds a CI budget test that runs a hub against 25 simulated nodes and fails if a unit exceeds its line.

| Unit | Steady RSS target | `MemoryHigh` | `MemoryMax` | CPU |
|---|---|---|---|---|
| `nightgrid-hub` (store, rollups, routing, socket) | 40 to 80 MB | 150M | 200M | `CPUWeight=20`, `CPUQuota=50%` |
| `nightgrid-hub-collect` | 10 to 20 MB | 48M | 64M | `CPUWeight=20` |
| `nightgrid-hub-notify` | 5 to 10 MB | 24M | 32M | `CPUWeight=20` |
| **Core total (H1, H2)** | **55 to 110 MB** | | | < 2% of one Pi 4 core averaged |
| later: anomaly detection (in the main unit) | +20 to 40 MB | main unit raised to 200M / 256M | | bursts every 15 min |
| later: `-checks` / `-snmp` | 10 to 20 MB each | 48M | 64M | |
| later: `-index` (log search) | 60 to 120 MB | 200M | 256M | `IOWeight=20` |
| **Everything on** | **150 to 250 MB** | | | |

The core stays inside the 100 to 200 MB the proposal named; the full set goes a little past it, which is why each later feature is its own opt-in unit and can be left off.

**Disk.** A Lite node's minute tier is about 0.5 MB per host per day (more with many disks and interfaces), so 14 days of minutes, 90 of 15-minute points and 400 of hourly points come to roughly 15 to 25 MB per host. 25 hosts with events and uptime: about 0.5 to 1 GB. Root's `[store] max_gb` (default 4) caps it: past 90% the hub prunes the oldest minute data first, then raises a `hub` warning. The log index has its own cap (`[index] max_gb`, default 1).

**Writes.** The hub batches each pull into one transaction per member per minute, in WAL mode. On a Pi it should live on an SSD, not the SD card; `install` warns when the state directory is on an MMC device.

**Pull cost on Lite nodes.** One pull a minute per node, of closed buckets and new events only, is a few KB. The Lite node's work is a cursor query it already does for the app; its budget doesn't change.

## 5. Fleet aggregation (H1, N-054)

### 5.1 Pull, don't push

The hub is just another client of each Lite node, the way the app is: it keeps one cursor per member per stream and asks for what's new. This was chosen over Lite nodes pushing to the hub because:

- Lite stays stateless toward clients (DESIGN §5). A pushing node would need a queue, retries and to know where its hub is, and would behave differently when the hub is down. A pulled node doesn't notice.
- The hub controls its own load: it pulls at its pace and backs off when busy.
- Read-only is enforced on the Lite side: the link answers only the read methods in section 5.2, so the hub can't ask for anything else even if it is compromised.

### 5.2 What is pulled

Over the existing link (`nightgrid-node-peer`, mutual TLS 1.3, pinned certificates, tailnet only), with a new `serve` entry `sync` that root adds per host. Each request names an existing socket method, and the link passes it to the main service only if it is on this fixed, read-only list:

| Stream | Socket method it mirrors | Interval |
|---|---|---|
| metrics | `metrics.sync` (minute, 15-minute and hourly tiers) | 60 s |
| events | `events.sync` | 60 s (30 s while a member has active alerts) |
| uptime | `uptime.sync` | 5 min |
| disk | `disk.sync` | 15 min |
| alerts | `alerts.active` | 60 s |
| node | `node.info` (capabilities, version) | on connect and every hour |
| later | `health.latest`, `jobs.latest`, `checks.latest`, `usage.history`, `summary.daily` | per feature |

Request and answer shapes are the socket's own, unchanged, so the hub reuses the app's parsing and the Lite node reuses its handlers. Link frames are still one JSON line; the existing 4 KiB limit applies to requests, and sync answers are paged to at most 1 MiB per line (smaller pages than the socket's 5000 points if needed). `actions.*`, `heal.*`, `changes.*`, `*.set`, `firewall.*` and anything not on the list are refused with `"not served to the hub"`.

### 5.3 Same data, same cursors

The hub stores each member's data under its `node_id`, **keeping the node's own `seq` numbers and bucket timestamps**. So a cursor the app holds for host A means the same thing whether the app syncs A directly or through the hub. The app can switch between the two at any time, in either direction, with no gaps and no duplicates (spec 0020). This is the single most important property of the design: the hub is a cache of the nodes, never a different source of truth.

If a member's `node_id` changes (purged reinstall), the hub starts a new series, as the app does today.

### 5.4 What the hub adds on top

- **Host down while the app is closed.** The collector already pings members over the link. When a member misses pulls for 2 minutes (the app's host-offline grace is 120 s), the hub raises `hostOffline` for it, using the app's own `Tracker` semantics (vendored as Lite already does for disk alerts). Today nobody notices a dead host until the app opens; this is the first notification most people will want.
- **One view.** A fleet summary on the hub socket (`fleet.overview`): every member's state, active alerts, last contact, and version.
- **A fleet status page** (later, H3): the N8d page, but for the whole network, on the hub host's tailnet address, same rules (read-only, secret link, no scripts).

### 5.5 Clock skew

Data keeps the node's timestamps. The hub compares each answer's `at` with its own clock and raises a `hub` warning when a member is more than 60 s off, since skew bends graphs and grace periods.

## 6. Failure modes (N-055)

| What fails | What happens | What the person sees |
|---|---|---|
| **Hub down** (stopped, crashed, host off) | Lite nodes carry on alone: they don't push, so nothing queues and nothing changes on them. The app syncs each host directly as before; cursors carry over (section 5.3). | The app shows "hub pi unreachable since 14:02" and falls back without a gap. Members' link pings record "peer pi unreachable" in their timelines. Notifications pause (section 6.1). |
| **Hub back** | It pulls each member's backlog from its cursors, oldest first, a page per member per round, so a long outage doesn't spike memory. | Nothing to do. History is complete if the outage was shorter than the members' retention (14 days of minutes). |
| **A member down** | The hub raises `hostOffline` after the grace period and notifies. | A notification "nas is offline", and a matching "back online". |
| **Hub can't reach some members** (partition) | Same as member down from the hub's side; members' own link pings tell a dead host from a broken path, as today (N-043). | Notifications say "unreachable from the hub", not "down", unless a second witness (another member's link reading) agrees it is down. |
| **Link not allowed on a member** (`sync` not in `serve`, or `hub.member` names another hub) | The hub skips that member and keeps trying hourly. | The app's hub page lists it under "needs consent" with the fix. |
| **Hub over memory** | `MemoryHigh` slows it, `MemoryMax` restarts it; ingestion is bounded per round so a restart resumes from cursors. | A `hub` warning event if it restarts more than 3 times an hour. |
| **Hub disk full or at cap** | Prunes oldest minute data first, then stops ingesting minutes (keeps events and alerts). | A `hub` warning: "store at 4 GB cap, keeping 9 days of minutes". |
| **Notification channel down** | Retries with backoff (30 s, 2, 10, 30 min), queue capped at 500 messages or 24 h, then one "N alerts were not delivered" summary when the channel works again. | Undelivered messages are listed in the app's hub page. |
| **Version skew** | The collector reads each member's capabilities and pulls only listed streams. An old member without `sync` serve is listed as "needs Node 0.30 or newer". | |
| **Two hubs** | Section 2.4. | A warning on both. |
| **Hub compromised** | Section 7.1: it can read members' telemetry, not change or run anything. Unpair it in the app (removes its fingerprint everywhere) and it is cut off at once. | |
| **Power cut** | The hub host should be on the UPS and shut down last. If it reads the UPS (N-041), it notifies "on battery" before anything else. | |

### 6.1 Who watches the hub

A hub that is down sends nothing, so silence can mean "all fine" or "hub dead". Three answers, all opt-in:

1. **The app**, whenever it is open, shows the hub's state first.
2. **A standby hub** (N-059) takes over notifications.
3. **A heartbeat URL**: the notifier calls a URL the person gives (a self-hosted Uptime Kuma push monitor, Healthchecks, or similar) every few minutes. If the hub stops, that service alerts. Nothing is sent unless the person sets one.

## 7. Security model (N-056)

### 7.1 What a compromised hub can and can't do

The design aims at this: **taking over the hub host gives an attacker a copy of the network's monitoring data, and nothing more.**

| Can | Can't |
|---|---|
| Read the telemetry members serve it: metrics, events, alerts, uptime, (later) health, jobs, and logs if the person enabled the log index on that member | Run actions, heal rules, rollbacks, blocks, upgrades or shutdowns on any member: the link refuses everything but the read list |
| Send notifications with what it knows | Change any member's config, alert rules, peers or `serve` list |
| Ask members to hold an automatic action (section 7.3) | Make a member do anything it wasn't already going to do |
| | Reach members over SSH: the hub holds no SSH keys and no sudo passwords |
| | Reach hosts that aren't members, or the internet, outside root's network lists for each unit |

Revocation is unpairing: the app removes the hub's fingerprint from each member's `peers.set`, and `deny`s `sync`.

### 7.2 Consent per host

- Root on each **member** decides whether it serves the hub at all (`sync` in `[peers] serve`) and, later, which extra streams (`logs` for the index).
- Root on the **hub host** decides where each hub unit may connect (`[collect] networks`, `[notify] networks`) and whether the notifier exists at all (`allow notify`).
- The person in the app decides membership and channels. Nothing on any socket can widen root's policy, as today.

### 7.3 The hold rule: the hub may ask for less, never more

Some later features need coordination across hosts: patch windows that stop rolling out after a failure (section 15), runbooks that shouldn't run while the network is degraded. The only control message the hub may ever send a member is `hold {kind, until, reason}`: "don't start automatic actions of this kind until then". A hold can only prevent something the member would have done on its own; it can never start one, and it expires by itself (at most 24 h). Each hold is an `action` event on the member. A member accepts holds only from its named hub, only if root lists `hold` in `serve`, and only for kinds root allowed. A compromised hub can, at worst, delay automatic actions, which is the safe direction.

### 7.4 Data at rest and in transit

- In transit: the existing link (TLS 1.3, pinned mutual certificates, tailnet only) between hub and members; SSH between app and hub; TLS to notification channels (HTTPS with the platform verifier, SMTP with STARTTLS or implicit TLS required, never plain).
- At rest: the hub's state directory is 0700, owned by `nightgrid-hub`. It holds the same kind of data each Lite node already holds about itself, now in one place, so `install` recommends full-disk encryption on the hub host and says so in its report.
- Secrets (channel URLs, SMTP passwords, tokens) are stored with `systemd-creds` (encrypted, TPM-bound where available), readable only by the notifier unit, and set only by root (`nightgrid-hub notify set-secret NAME`, which the app runs through its sudo flow). They are never on the socket, in a log or in an event, and are shown back only as scheme and host, like the app's webhook (D-051).
- The secret filter (`redact::line_has_secret`) runs on the member before anything leaves it (log lines, event text), so the hub never receives what the member would have dropped.

### 7.5 No cloud, no telemetry

The hub contacts nothing on its own. The only outbound traffic beyond the tailnet is to channels the person sets up, to the endpoints they name; with no channel set, the notifier unit isn't even installed. A public ntfy server or Discord is a third party the person chose, and the app says so when it is picked; a self-hosted ntfy or SMTP relay on the LAN keeps everything local. A `minimal` message mode sends only "2 alerts on home, open Nightgrid" with no host names or details, for people who use a public push service.

## 8. Notifications (H2, N-057)

### 8.1 Channels

The app already delivers alerts to ntfy, Discord and a generic JSON webhook (D-051). The hub uses the same three formats, vendored from the app, so a person moves their existing webhook to the hub without changing the receiver. Email is new:

| Channel | Format | Notes |
|---|---|---|
| ntfy | Plain body, `Title`/`Priority`/`Tags` headers, as the app | Self-hosted or public; priority follows severity |
| Discord | `content`, mentions disabled, as the app | |
| Webhook | `{source, state, alert, headline}`, as the app | `source` is `nightgrid-hub` |
| Email | Plain-text mail through an SMTP submission server the person names (port 465 or 587 with STARTTLS required), with a login | No local MTA, no direct-to-MX delivery |

Push to phones is ntfy (or any push service that takes a webhook). Native APNs or FCM would need an Apple or Google developer account and a cloud relay, so they are out.

### 8.2 What is sent

- Every alert a member fires or resolves (from its events), plus the hub's own `hostOffline` and `hub` warnings, through the app's alert keys and severities.
- **Folding**, as the app does (the cause is shown, its consequences folded under it): when a host is offline, its service and container alerts aren't sent separately.
- **Grouping**: more than three in one round become one summary message, like the app.
- **Quiet hours** per channel, in the hub host's time zone, with the app's rule: alerts still record, and messages are held, except for those the app sends through quiet hours (a UPS shutdown countdown).
- **Rate limit** per channel (default 30 messages an hour) with a summary of what was held.
- **Resolved** messages per channel, on or off, as the app's `webhook_resolved`.
- **Test** button in the app, which sends one message through each channel.

### 8.3 Who notifies: the app or the hub

Both delivering the same alert would double every message. With a hub, the app gets a per-site choice (spec 0020):

- **Hub delivers** (recommended when a hub exists): the hub sends, and the app's own native notifications stay on only while the app is open, for hosts the hub doesn't cover. The app's webhook is moved to the hub.
- **App delivers**: today's behaviour; the hub keeps data and the overview but sends nothing.

### 8.4 Rendering stays in the main unit

The main unit renders each message to its final text and passes only that to the notifier over a unix socket. The notifier holds the secrets and the network, but no history, so a bug in it can leak at most a message it was given.

## 9. LAN service checks (H3)

Lite already runs the app's HTTP(S) and TCP checks with certificate expiry from a host, in a network-limited unit (N-039). The hub adds:

- **One place to run them for a network**, so the person doesn't pick a host per check. Same check code (vendored from the app), same alert keys, so the app can drop its own result for a service the hub checks, as with Lite.
- **DNS checks**: resolve a name against a given resolver (the LAN's Pi-hole or AdGuard, the router, a public one), compare with an expected address set, alert on failure, NXDOMAIN, change or slow answers. Catches a dead local DNS, which today shows up as "everything is down".
- **Certificate expiry for many endpoints**, including internal CAs, with the warn and critical days the app uses.

Runs in `nightgrid-hub-checks`, a copy of the Lite check runner's sandbox with `IPAddressAllow` from root's `[checks] networks`. Lite's own checker stays; a person can keep checks on a specific host where the vantage point matters.

## 10. SNMP for devices that can't run Node (H4)

Switches, routers, printers, NAS appliances, UPS network cards and access points speak SNMP. The hub polls them as **devices** (not nodes), in `nightgrid-hub-snmp`:

- SNMPv3 with authPriv (SHA-2 and AES) by default; v2c only when the person picks it and the app warns that the community string travels in clear text on the LAN.
- A fixed set of tables, not arbitrary OIDs: `SNMPv2-MIB` (uptime, name), `IF-MIB` (`ifXTable` 64-bit counters, oper status, errors), `HOST-RESOURCES-MIB` (CPU, storage), `ENTITY-SENSOR-MIB` (temperatures), `Printer-MIB` (supplies), `UPS-MIB`. Profiles per device kind pick from these.
- Every 60 s by default, with the hub's tiers and retention. A device has an id the hub makes (`dev-…`), and its history is served by new methods on the hub socket, so nothing about node data changes.
- Alerts: device unreachable, interface down or flapping, error rate, link speed dropped (a gigabit port negotiating 100 Mb), printer supply low, temperature, UPS on battery for devices with a network card but no NUT.
- Read-only: GET and GETBULK only, never SET. Credentials are `systemd-creds` secrets like the notifier's.
- Network: `IPAddressAllow` from root's `[snmp] networks`, UDP 161 out only.

## 11. Anomaly detection and forecasting (H5)

Lite forecasts disk fill per mount ("full in about N days"). With history from the whole network, the hub can do more, deterministically and explainably (no trained model, nothing downloaded):

- **Baselines per series and hour of the week.** For each member's CPU, memory, load, network and disk I/O, the hub keeps a median and a median absolute deviation per hour-of-week bucket from the 15-minute tier over the last 8 weeks. A point that stays more than 4 MADs from its bucket for 15 minutes is an `anomaly` event ("nas: network out 6× usual for a Tuesday evening").
- **Forecasts.** Linear and Holt (trend) fits over the hourly tier for disks, ZFS and Btrfs pools (from `health.latest`), and memory growth (a leak). "Pool tank full in about 23 days", "memory on web has grown 2% a day for 9 days".
- **Change points.** A level shift that stays (a backup that now takes three times as long, a service whose memory doubled after an update), reported once, with the time it started.

Anomalies are `info` by default and become alerts only when the person turns a kind on, so a new feature doesn't start paging anyone. Each event says what the usual range was. Runs in the main unit every 15 minutes, within the budget in section 4.

## 12. Log index and search (H6)

Today Lite keeps log lines in memory only, drops secret-looking ones and reports findings, not lines (N-035). A searchable index means copying log lines off each host, which is a real change in what leaves a host, so:

- **Opt-in per member**, by root on that member: `logs` in `[peers] serve`, and `[logs] ship = ["journal", "containers"]` naming the sources. Off by default.
- **Filtered on the member**: the secret filter drops lines before they leave; root can add units or containers to never ship.
- **Index on the hub** in `nightgrid-hub-index`, with SQLite FTS5 (already in the bundled SQLite, no new search engine), its own database, its own cap (`[index] max_gb`, default 1) and retention (default 14 days).
- **Search from the app** through the hub socket: full text, host, unit or container, priority and time range; results link to the member's timeline at that minute. Correlation means "show everything from all hosts in the 5 minutes around this alert".
- Lines are never sent to a notification channel, only the alert that a search matched.

## 13. Runbooks (H7)

Self-healing on Lite is "condition → one action", rate-limited, dry-run first and audited (DESIGN §7a). Runbooks extend that **on each host**, not across hosts:

- A runbook is a named sequence of the typed actions Lite already has (restart a container or unit, prune, rollback-wrapped change, wait for a check), with conditions between steps ("restart web, wait 60 s, if the check still fails restart db, then alert").
- Each step is an existing action through the existing path, so root's policy, the helper, rate limits, dry-run and the hash-chained audit log all apply unchanged. No shell steps.
- **Approval**: a runbook runs automatically only if marked so per runbook; otherwise the hub's notification says "runbook X is ready to run on nas" and the person starts it from the app. Approving from the notification itself would need an inbound endpoint, so it is out.
- The hub's only role is the hold rule: it can tell members to hold runbooks while the network is degraded (more than N hosts down), so a power blip doesn't start ten restarts at once.

Runbooks are a Lite feature with a new capability on the Lite socket, so they wait until the freeze allows additive Lite methods (section 16).

## 14. Backup verification and restore tests (H7)

Lite watches backup jobs and their freshness (N-040). Test restores stay in the app today because they need the repository password. With a hub:

- **Verify** jobs: `restic check --read-data-subset=N%` or `borg check --verify-data` on a schedule the app sets, as a `nightgrid-job-*` timer on the host that owns the repository (the existing pattern), with the password in `systemd-creds` for that job. Lite reports the result as a `job` event, as today.
- **Restore tests**: restore a few chosen paths (or a random sample) to a scratch directory, compare hashes with the source where it is local, delete the scratch copy, report. Also a `nightgrid-job-*` timer, run by systemd, so neither the hub nor Lite needs new privileges to run it.
- The hub's part is the fleet view ("every repository verified in the last 7 days, restore test on 2 of 3"), alerts on a verification that failed or is overdue, and staggering so two big checks don't hit the same NAS at once (by suggesting non-overlapping windows to the app, not by commanding hosts).

## 15. Patch windows with health check and rollback (H7)

Per host, through the root helper (a new typed op root must allow), on a schedule the app sets:

1. Before: snapshot where possible: `rpm-ostree` keeps the old deployment by itself; Btrfs or ZFS root gets a snapshot; a Proxmox guest gets a vzdump or snapshot from its host. Hosts with nothing to roll back to (plain apt on ext4) get "update and verify, alert on failure" without automatic rollback, and the app says so when the window is set up.
2. Update with the distribution's tool, non-interactively, held packages respected.
3. Reboot if required and allowed.
4. Health check: Lite's own (service active, its checks, the app's service checks for this host, containers that were running are running), the same idea as `upgrade`'s (N-042).
5. Fail: roll back (`rpm-ostree rollback`, snapshot revert) and alert; succeed: record the versions changed.

The hub adds **canary and stop**: windows across the network are staggered by the app; when a host's window fails, the hub sends `hold {kind: "patch"}` to the members, so the remaining windows that night are skipped until the person looks. A hold can only stop windows, never start one (section 7.3).

## 16. Frozen interfaces and what is new

Since 2026-11-17 the app-facing interfaces are frozen while Nightgrid's compatibility work runs: the socket RPC methods, the NDJSON protocol and schemas (`crates/proto`), the app-integration specs 0001 to 0019, and the peer link protocol.

**This proposal changes none of them.** Specifically:

- `hello`, every existing method, field, stream frame and event kind: unchanged.
- `peers.set`, `peers.get`, `peers.latest` and their fields: unchanged. The hub is paired as a normal peer.
- Existing link requests `peer.ping` and `peer.ups`: unchanged.
- Specs 0001 to 0019: unchanged.

What would be **added**, each opt-in, versioned and ignorable by older clients:

| Where | Addition | Versioning |
|---|---|---|
| Lite socket | `hub.member.get` / `hub.member.set` | New capability `hub.member` 1; absent on old nodes |
| Lite socket | (H7) `runbooks.*`, patch window methods | New capabilities, each at 1 |
| Lite link | Requests `peer.sync` (wraps the read list in section 5.2) and `peer.hold` | New `serve` entries `sync`, `hold`, `logs`; a node without them answers with today's "isn't served here" error |
| Lite config and CLI | `[peers] serve` gains the values `sync`, `hold`, `logs`; `[logs] ship`; `nightgrid-node serve add\|remove NAME` (root) so the app can change `serve` through its sudo flow | Root-only, off by default |
| Hub socket | A new socket for a new product, same framing and `hello` rules as Lite: capabilities `fleet` 1, `notify` 1, later `devices` 1, `anomaly` 1, `logsearch` 1 | Starts at protocol 2 (streams) |
| App | Future spec [0020](app-integration/0020-hub-fleet-sync.md) | Marked future; the app keeps working unchanged without a hub |

`crates/node/tests/frozen-protocol.txt` pins every Lite method, capability and result field; the Lite additions above will show up there as new lines only, never as changed ones (regenerate with `FROZEN_UPDATE=1 cargo test -p nightgrid-node frozen` and check the diff is purely added lines).

The hub can be built (H1, H2) without touching Lite's app-facing surface at all, except for the additive Lite pieces above. Those Lite pieces wait for Roben's go once the compatibility work allows additive changes; until then H1 can be developed and tested against a Lite build on a branch.

## 17. Milestones

| | Scope | Lite changes |
|---|---|---|
| **H0** | This design (docs only) | None |
| **H1** | `nightgrid-hub` binary, install/uninstall/upgrade, store, collector, pull over the link, `fleet.*` on the hub socket, host-offline alerts, budget test in CI | `hub.member`, `peer.sync`, `sync` serve (additive) |
| **H2** | Notifier: ntfy, Discord, webhook, email; folding, grouping, quiet hours, rate limits; heartbeat URL | None |
| **H3** | Fleet status page; hub checks (HTTP, TLS, TCP, DNS) | None |
| **H4** | SNMP devices | None |
| **H5** | Baselines, anomalies, forecasts | None |
| **H6** | Log index and search | `logs` serve, `[logs] ship` (additive) |
| **H7** | Runbooks, backup verification and restore tests, patch windows, holds, standby hub | New Lite capabilities, `peer.hold` (additive) |

Each milestone gets its app spec in `docs/app-integration/` (0020 covers H1 and H2), its decisions, its CI tests (systemd containers for install, simulated members for the collector, a local ntfy and SMTP sink for the notifier) and a release, like N0 to N9.

## 18. Open questions

- **Name.** "Hub" in the app and docs; `nightgrid-hub` as the binary. The app may prefer "site hub".
- **Email.** Plain text only, or a small HTML summary as well? Plain text is proposed.
- **Minimum hardware.** The budget targets a Pi 4 with 4 GB and an SSD. A Pi 3 or 1 GB board could run the core (H1, H2) but not the index.
- **Standby hub.** Proposed for H7; it could move up if hub availability turns out to matter more than the later features.
