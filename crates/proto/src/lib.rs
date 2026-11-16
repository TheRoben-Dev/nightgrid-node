//! Wire protocol between the Nightgrid app and Nightgrid Node.
//!
//! Framing: one JSON object per line (NDJSON, UTF-8, `\n`-terminated, at most
//! [`MAX_LINE`] bytes). The client sends [`Request`]s; the node answers each
//! with exactly one [`Response`] carrying the same `id`. Answers may arrive out
//! of order once streams exist (protocol 2+), so clients match on `id`.
//!
//! The first request on a connection must be `hello` ([`Hello`]): it picks a
//! protocol version both sides speak and returns the node's capabilities
//! ([`Caps`]). A client only calls methods whose capability is listed, so an
//! older node, or one with a capability turned off, is handled by falling back
//! to plain SSH in the app.
//!
//! The metric types mirror `nightgrid-core`'s `metrics::Sample` and
//! `metrics::history::Point` field for field (camelCase JSON), so the app can
//! deserialize them straight into its own types.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Newest protocol version this crate speaks.
pub const PROTO_VERSION: u32 = 2;
/// Oldest protocol version this crate still speaks.
pub const PROTO_MIN: u32 = 1;
/// Longest accepted line, in bytes (requests and responses).
pub const MAX_LINE: usize = 4 * 1024 * 1024;
/// Most points one `metrics.sync` or `metrics.history` answer carries.
pub const MAX_POINTS: usize = 5000;

/// Default socket path of the system service.
pub const DEFAULT_SOCKET: &str = "/run/nightgrid-node/node.sock";

/// Method names.
pub mod method {
    pub const HELLO: &str = "hello";
    pub const NODE_INFO: &str = "node.info";
    pub const METRICS_LATEST: &str = "metrics.latest";
    pub const METRICS_HISTORY: &str = "metrics.history";
    pub const METRICS_SYNC: &str = "metrics.sync";
    pub const EVENTS_SYNC: &str = "events.sync";
    pub const UPTIME_SYNC: &str = "uptime.sync";
    pub const DISK_SYNC: &str = "disk.sync";
    pub const ALERTS_ACTIVE: &str = "alerts.active";
    pub const ALERTS_RULES_GET: &str = "alerts.rules.get";
    pub const ALERTS_RULES_SET: &str = "alerts.rules.set";
    pub const CONTAINERS_LIST: &str = "containers.list";
    /// Devices the node has seen on its LAN (neighbour table).
    pub const NET_DEVICES: &str = "net.devices";
    /// DHCP servers seen on the LAN (needs `allow netwatch`).
    pub const NET_DHCP: &str = "net.dhcp";
    /// Internal: the network watch helper's reports. Only the node's own
    /// service account may call it.
    pub const NET_REPORT: &str = "net.report";
    /// Which action types root allows, and the limits.
    pub const ACTIONS_POLICY: &str = "actions.policy";
    /// Run one typed action now (or dry-run it).
    pub const ACTIONS_RUN: &str = "actions.run";
    /// The hash-chained audit log of every action, by `seq`.
    pub const ACTIONS_LOG: &str = "actions.log";
    pub const HEAL_RULES_GET: &str = "heal.rules.get";
    pub const HEAL_RULES_SET: &str = "heal.rules.set";
    /// What each self-healing rule is doing right now.
    pub const HEAL_STATUS: &str = "heal.status";
    /// Snapshot files before a change and arm the dead-man timer.
    pub const CHANGES_BEGIN: &str = "changes.begin";
    /// The change worked: keep it, drop the snapshot.
    pub const CHANGES_CONFIRM: &str = "changes.confirm";
    /// Put the files back now.
    pub const CHANGES_REVERT: &str = "changes.revert";
    /// Pending and recent changes.
    pub const CHANGES_LIST: &str = "changes.list";
    /// Protocol 2: start pushing frames for streams.
    pub const SUBSCRIBE: &str = "subscribe";
    /// Protocol 2: stop pushing frames for streams.
    pub const UNSUBSCRIBE: &str = "unsubscribe";
}

/// Protocol 2 stream names (see [`Frame`]).
pub mod stream {
    /// Every new event as it is recorded: `data` is an [`super::Event`].
    pub const EVENTS: &str = "events";
    /// Samples, every couple of seconds while anyone subscribes: [`super::Sample`].
    pub const METRICS: &str = "metrics";
    /// The container list when a state or health changes: [`super::ContainersResult`].
    pub const CONTAINERS: &str = "containers";
    /// Not subscribable: the node dropped frames for a slow client: [`super::Lagged`].
    pub const LAGGED: &str = "lagged";
    pub const ALL: [&str; 3] = [EVENTS, METRICS, CONTAINERS];
}

/// Capability names. A capability's version bumps when its methods gain
/// fields a client may rely on; adding optional fields doesn't bump it.
pub mod cap {
    /// `node.info`.
    pub const NODE: &str = "node";
    /// `metrics.latest`, `metrics.history`.
    pub const METRICS: &str = "metrics";
    /// `metrics.sync`: gap-filling from local history.
    pub const METRICS_SYNC: &str = "metrics.sync";
    /// `events.sync`: alerts fired/resolved and outages, by sequence number.
    pub const EVENTS: &str = "events";
    /// `uptime.sync`: up/down/asleep seconds per hour for the host.
    pub const UPTIME: &str = "uptime";
    /// `disk.sync`: hourly usage per mount (for fill-up forecasts).
    pub const DISK: &str = "disk";
    /// `alerts.active`, `alerts.rules.get`, `alerts.rules.set`.
    pub const ALERTS: &str = "alerts";
    /// `containers.list`; container events in `events`; container-down alerts.
    pub const CONTAINERS: &str = "containers";
    /// Logins and failed logins in `events` (needs `allow journal`).
    pub const LOGINS: &str = "logins";
    /// Watched file changes in `events`.
    pub const FILES: &str = "files";
    /// `net.devices`; network events (new devices, IP changes and conflicts) in `events`.
    pub const NET: &str = "net";
    /// `net.dhcp`; DHCP events and device names (needs `allow netwatch`).
    pub const NET_DHCP: &str = "net.dhcp";
    /// `actions.policy`, `actions.run`, `actions.log`; `action` events.
    pub const ACTIONS: &str = "actions";
    /// `heal.rules.get`, `heal.rules.set`, `heal.status`.
    pub const HEAL: &str = "heal";
    /// `changes.begin`, `changes.confirm`, `changes.revert`, `changes.list`.
    pub const CHANGES: &str = "changes";
}

/// Typed actions (N4). Root lists the ones the node may run in
/// `[actions] allow`; nothing else ever runs.
pub mod action {
    /// Restart a container (engine socket; needs `allow docker`).
    pub const CONTAINER_RESTART: &str = "container.restart";
    pub const CONTAINER_START: &str = "container.start";
    pub const CONTAINER_STOP: &str = "container.stop";
    /// Remove dangling images (engine socket; needs `allow docker`).
    pub const IMAGES_PRUNE: &str = "images.prune";
    /// Restart, start or stop a systemd unit that root listed in
    /// `[actions] units` (root helper; needs `allow helper`).
    pub const UNIT_RESTART: &str = "unit.restart";
    pub const UNIT_START: &str = "unit.start";
    pub const UNIT_STOP: &str = "unit.stop";
    /// Reboot or power off, one minute after the request (root helper).
    pub const POWER_REBOOT: &str = "power.reboot";
    pub const POWER_POWEROFF: &str = "power.poweroff";
    /// Snapshot files under `[rollback] paths` and put them back unless the
    /// change is confirmed in time (`changes.*`, root helper).
    pub const FILES_ROLLBACK: &str = "files.rollback";
    pub const ALL: [&str; 10] = [
        CONTAINER_RESTART,
        CONTAINER_START,
        CONTAINER_STOP,
        IMAGES_PRUNE,
        UNIT_RESTART,
        UNIT_START,
        UNIT_STOP,
        POWER_REBOOT,
        POWER_POWEROFF,
        FILES_ROLLBACK,
    ];
    /// Whether the action works on one named container.
    pub fn takes_container(a: &str) -> bool {
        matches!(a, CONTAINER_RESTART | CONTAINER_START | CONTAINER_STOP)
    }
    /// Whether the action works on one named systemd unit.
    pub fn takes_unit(a: &str) -> bool {
        matches!(a, UNIT_RESTART | UNIT_START | UNIT_STOP)
    }
    /// Whether the root helper carries the action out.
    pub fn needs_helper(a: &str) -> bool {
        takes_unit(a) || matches!(a, POWER_REBOOT | POWER_POWEROFF | FILES_ROLLBACK)
    }
    /// Whether self-healing rules may run it (not units or power, for now).
    pub fn for_rules(a: &str) -> bool {
        takes_container(a) || a == IMAGES_PRUNE
    }
    /// A full systemd unit name such as `nginx.service`, and not one of the
    /// node's own units.
    pub fn valid_unit(name: &str) -> bool {
        const SUFFIXES: [&str; 3] = [".service", ".socket", ".timer"];
        let mut c = name.chars();
        c.next().is_some_and(|f| f.is_ascii_alphanumeric())
            && name.len() <= 255
            && name.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '_' | '.' | '@' | '-' | '\\'))
            && SUFFIXES.iter().any(|s| name.ends_with(s) && name.len() > s.len())
            && !name.starts_with("nightgrid-node")
    }
}

/// Capability name → version.
pub type Caps = BTreeMap<String, u32>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: u64, result: impl Serialize) -> Self {
        let result = serde_json::to_value(result).unwrap_or(Value::Null);
        Self { id, result: Some(result), error: None }
    }

    pub fn err(id: u64, code: ErrorCode, message: impl Into<String>) -> Self {
        Self { id, result: None, error: Some(RpcError { code, message: message.into() }) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The line wasn't a request, or its params didn't fit the method.
    BadRequest,
    /// No such method (or not in the negotiated protocol version).
    UnknownMethod,
    /// The first request wasn't `hello`.
    HelloRequired,
    /// No protocol version in common.
    UnsupportedProto,
    /// The method's capability is turned off on this node.
    CapabilityDisabled,
    /// The line was longer than [`MAX_LINE`].
    TooLarge,
    /// Something failed on the node (details are in the node's log).
    Internal,
    /// This caller may not use the method.
    Forbidden,
    /// Codes added by newer nodes.
    #[serde(other)]
    Unknown,
}

/// `hello` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    /// Oldest and newest protocol versions the client speaks.
    pub proto_min: u32,
    pub proto_max: u32,
    /// Free text for the node's log, e.g. `nightgrid/0.10.50`.
    #[serde(default)]
    pub client: String,
}

impl Hello {
    pub fn this_client(client: impl Into<String>) -> Self {
        Self { proto_min: PROTO_MIN, proto_max: PROTO_VERSION, client: client.into() }
    }
}

/// `hello` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Welcome {
    /// The version both sides now speak.
    pub proto: u32,
    pub node: NodeIdentity,
    pub caps: Caps,
}

/// Picks the newest version both ranges share.
pub fn negotiate(client_min: u32, client_max: u32, node_min: u32, node_max: u32) -> Option<u32> {
    let hi = client_max.min(node_max);
    let lo = client_min.max(node_min);
    (lo <= hi).then_some(hi)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeIdentity {
    /// Random id made when the node's database was created. A new id means
    /// the history was reset (purged reinstall), so sync cursors start over.
    pub node_id: String,
    pub version: String,
    pub hostname: String,
    /// `x86_64` or `aarch64`.
    pub arch: String,
}

/// `node.info` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub node: NodeIdentity,
    /// When the service started, unix seconds.
    pub started: i64,
    /// Seconds between metric samples.
    pub interval_secs: u32,
    /// How long each tier is kept, in days.
    pub retention: Retention,
    /// What each history tier holds right now.
    pub coverage: Vec<Coverage>,
    /// Resident memory of the node process, bytes (0 if unknown).
    pub rss_bytes: u64,
    /// Size of the history database, bytes.
    pub db_bytes: u64,
    /// Optional access root granted or not, by name (`docker`, `journal`):
    /// `ok`, `denied` (root hasn't run `nightgrid-node allow <name>`),
    /// `absent` (nothing to read on this host) or `off` (disabled in the config).
    #[serde(default)]
    pub access: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Retention {
    pub minute_days: u32,
    pub rollup_days: u32,
    pub hourly_days: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Coverage {
    /// 60, 900 or 3600.
    pub res: i64,
    pub points: u64,
    /// Oldest and newest bucket start, unix seconds (`None` when empty).
    pub oldest: Option<i64>,
    pub newest: Option<i64>,
}

// ---- metrics ----

/// History tiers, in seconds. Same as the app's `metrics::history`.
pub const MINUTE: i64 = 60;
pub const ROLLUP: i64 = 900;
pub const HOURLY: i64 = 3600;
/// Ranges up to this long are served from minute points.
pub const MINUTE_RANGE_SECS: i64 = 2 * 86400;
/// Ranges up to this long are served from 15-minute rollups; longer ones from hourly.
pub const ROLLUP_RANGE_SECS: i64 = 31 * 86400;

/// The tier a range is served from (same rule as the app).
pub fn resolution(from: i64, to: i64) -> i64 {
    match to - from {
        d if d <= MINUTE_RANGE_SECS => MINUTE,
        d if d <= ROLLUP_RANGE_SECS => ROLLUP,
        _ => HOURLY,
    }
}

pub fn is_tier(res: i64) -> bool {
    matches!(res, MINUTE | ROLLUP | HOURLY)
}

/// `metrics.history` params: points in `[from, to]` (unix seconds).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryParams {
    pub from: i64,
    pub to: i64,
    /// Force a tier; by default [`resolution`] picks one.
    #[serde(default)]
    pub res: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryResult {
    pub res: i64,
    pub points: Vec<Point>,
    /// More points were in range than [`MAX_POINTS`]; the newest were cut.
    pub truncated: bool,
}

/// `metrics.sync` params: closed buckets of one tier with `ts > after`, oldest first.
///
/// The app keeps one cursor per node and tier (the `next` it got back) and
/// calls again while `more` is set. Only closed buckets are returned (a
/// rollup still collecting minutes is left for the next sync), so every
/// point is final and inserting it is idempotent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncParams {
    pub res: i64,
    pub after: i64,
    /// At most this many points (capped at [`MAX_POINTS`]).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    pub res: i64,
    pub points: Vec<Point>,
    /// Cursor for the next call: the last point's `ts`, or `after` if none.
    pub next: i64,
    pub more: bool,
    /// Oldest bucket this tier still has. If it's newer than the app's
    /// cursor, the minutes in between are gone here and the app fills that
    /// span from a coarser tier.
    pub oldest: Option<i64>,
}

/// Usage of one mounted filesystem.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MountUsage {
    pub device: String,
    pub mount: String,
    pub fstype: String,
    pub total: u64,
    pub used: u64,
    pub avail: u64,
}

/// One temperature sensor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Temp {
    pub chip: String,
    pub label: String,
    pub celsius: f32,
}

/// One GPU. Fields a driver doesn't report are `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Gpu {
    pub name: String,
    pub util: Option<f32>,
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
    pub temp: Option<f32>,
    pub power: Option<f32>,
}

/// Throughput of one network interface or block device, bytes/s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rate {
    pub name: String,
    pub rx: f64,
    pub tx: f64,
}

/// One processed sample (`metrics.latest`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    /// Unix time, milliseconds.
    pub ts: f64,
    pub hostname: String,
    pub kernel: String,
    pub uptime_secs: f64,
    pub cpu: Option<f32>,
    pub cores: Vec<f32>,
    pub load: [f32; 3],
    pub mem_total: u64,
    pub mem_used: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub mounts: Vec<MountUsage>,
    pub net: Vec<Rate>,
    pub net_rx: f64,
    pub net_tx: f64,
    pub disks: Vec<Rate>,
    pub disk_read: f64,
    pub disk_write: f64,
    pub temps: Vec<Temp>,
    pub cpu_temp: Option<f32>,
    pub gpus: Vec<Gpu>,
}

/// One stored history point (averages over its bucket).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    /// Bucket start, unix seconds.
    pub ts: f64,
    pub cpu: Option<f32>,
    pub mem: Option<f32>,
    pub swap: Option<f32>,
    pub load1: Option<f32>,
    pub disk: Option<f32>,
    pub net_rx: Option<f64>,
    pub net_tx: Option<f64>,
    pub disk_read: Option<f64>,
    pub disk_write: Option<f64>,
    pub temp: Option<f32>,
    pub gpu: Option<f32>,
    pub gpu_mem: Option<f32>,
    pub gpu_temp: Option<f32>,
    pub gpu_power: Option<f32>,
    /// Samples (minute tier) or minutes (rollups) averaged into this point.
    /// Lets the app weight a rollup it merges. Absent in the app's own type.
    #[serde(default)]
    pub n: u32,
}

// ---- events, uptime, disk usage (N1) ----

/// Most events, hours or rows one sync answer carries.
pub const MAX_ROWS: usize = 2000;

/// Same kinds and strings as the app's `alerts::AlertKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AlertKind {
    HostOffline,
    DiskFull,
    ContainerDown,
    ServiceDown,
    CertExpiring,
    SmartWarning,
    BackupFailed,
    BackupStale,
    LogBurst,
    LogNewError,
    ExpirySoon,
    DiskFilling,
    PoolDegraded,
    UpsOnBattery,
    LogMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Severity {
    Warn,
    Critical,
}

/// An alert as the node tracks it. Same fields as the app's `alerts::Alert`
/// minus `hostId` (a node is one host) and `causedBy` (folding is the app's).
///
/// `key` is the app's key with the host segment left out: the node's
/// `disk:/srv` is the app's `disk:<hostId>:/srv`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Alert {
    /// Node-local id (stable across restarts).
    pub id: u32,
    pub key: String,
    pub kind: AlertKind,
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    /// When the condition was first seen.
    pub since: i64,
    /// When it resolved; `None` while active.
    pub resolved: Option<i64>,
}

/// One device in the node's LAN inventory (`net.devices`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetDevice {
    /// Lower-case `aa:bb:cc:dd:ee:ff`. The app adds the maker from its own
    /// OUI table; the node ships none.
    pub mac: String,
    /// Its latest IPv4 address.
    pub ip: String,
    /// The interface it was seen on.
    pub iface: String,
    pub first_seen: i64,
    pub last_seen: i64,
    /// In the neighbour table at the last look.
    pub online: bool,
    /// The host name the device asked DHCP for (needs `allow netwatch`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A device seen sending ARP on the LAN (network watch helper).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArpSeen {
    pub ip: String,
    pub mac: String,
    pub iface: String,
}

/// One DHCP message seen on the LAN (network watch helper).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhcpSeen {
    pub at: i64,
    /// `discover`, `offer`, `request`, `decline`, `ack`, `nak`, `release`, `inform`.
    pub msg: String,
    pub iface: String,
    /// The client's hardware address (`chaddr`).
    pub client_mac: String,
    /// The server's identifier (option 54).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// Ethernet source of a server's message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_mac: Option<String>,
    /// The address offered or acknowledged (`yiaddr`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subnet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_secs: Option<u32>,
    /// The client's host name (option 12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

/// `net.report` params (internal).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NetReport {
    pub arp: Vec<ArpSeen>,
    pub dhcp: Vec<DhcpSeen>,
}

/// A DHCP server the node has seen answering on the LAN.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhcpServer {
    /// Its server identifier (option 54).
    pub ip: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    pub first_seen: i64,
    pub last_seen: i64,
    /// What it hands out, from its last offer or ack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subnet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_secs: Option<u32>,
    /// Listed by root in `[network] dhcp_servers`.
    pub trusted: bool,
}

/// `net.dhcp` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhcpResult {
    /// The helper has reported in the last two minutes.
    pub watching: bool,
    pub servers: Vec<DhcpServer>,
}

/// `net.devices` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetDevicesResult {
    /// When the neighbour table was last read (0: not yet).
    pub at: i64,
    /// The default gateway's address, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    /// Most recently seen first.
    pub devices: Vec<NetDevice>,
}

/// Same strings as the app's `timeline::EventKind`, plus `outage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EventKind {
    /// An alert fired (or got worse).
    Alert,
    /// An alert cleared.
    Recovered,
    /// The host booted.
    Boot,
    /// A span the host was down or asleep, worked out at the next start.
    Outage,
    /// A container died, restarted, ran out of memory or turned unhealthy.
    Container,
    /// A login, or failed logins (SSH), from the journal.
    Login,
    /// A watched file or directory changed.
    File,
    /// The LAN changed: a new device, a device on a new address, an
    /// address answering from a new MAC, an IP conflict.
    Network,
    /// An action ran, was dry-run, failed or was refused (N4).
    Action,
    /// Kinds added by newer nodes.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Level {
    Info,
    Ok,
    Warn,
    Critical,
}

/// How the host spent a span the node couldn't watch live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutageState {
    /// Crashed or lost power (no clean shutdown before the reboot).
    Down,
    /// Suspended, or shut down cleanly and started again.
    Asleep,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    /// Increases by one per event on this node (cursor for `events.sync`).
    pub seq: i64,
    pub at: i64,
    pub kind: EventKind,
    pub level: Level,
    pub title: String,
    pub detail: String,
    /// For `alert` / `recovered`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert: Option<Alert>,
    /// For `outage`: the span and how it was spent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outage: Option<OutageSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutageSpan {
    pub start: i64,
    pub end: i64,
    pub state: OutageState,
}

/// `events.sync`, `uptime.sync`, `disk.sync` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorParams {
    pub after: i64,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventsResult {
    pub events: Vec<Event>,
    /// The last event's `seq`, or `after` if none.
    pub next: i64,
    pub more: bool,
    /// Oldest `seq` still kept. If it is above the client's cursor + 1,
    /// events in between were pruned.
    pub oldest: Option<i64>,
}

/// Seconds of one hour, for the host (like a row of the app's `uptime_hours`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UptimeHour {
    pub hour: i64,
    pub up: u32,
    pub down: u32,
    pub asleep: u32,
}

/// `uptime.sync`: closed hours with `hour > after`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UptimeResult {
    pub hours: Vec<UptimeHour>,
    pub next: i64,
    pub more: bool,
}

/// One mount's usage in one hour (like a row of the app's `disk_usage`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskUsageRow {
    pub mount: String,
    pub ts: i64,
    pub used: u64,
    pub size: u64,
}

/// `disk.sync`: closed hours with `ts > after`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskResult {
    pub rows: Vec<DiskUsageRow>,
    pub next: i64,
    pub more: bool,
}

/// The alert rules the node evaluates, pushed by the app (`alerts.rules.set`).
/// Field names and defaults match the app's `AlertSettings`; settings the node
/// has no source for yet are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AlertRules {
    pub enabled: bool,
    pub disk_full: bool,
    /// Percent used that counts as full, 50-99.
    pub disk_percent: u8,
    pub disk_forecast: bool,
    /// 1-90.
    pub forecast_days: u16,
    pub container_down: bool,
}

impl Default for AlertRules {
    fn default() -> Self {
        Self {
            enabled: true,
            disk_full: true,
            disk_percent: 90,
            disk_forecast: true,
            forecast_days: 14,
            container_down: true,
        }
    }
}

impl AlertRules {
    pub fn validate(&self) -> Result<(), String> {
        if !(50..=99).contains(&self.disk_percent) {
            return Err("diskPercent must be 50-99".into());
        }
        if !(1..=90).contains(&self.forecast_days) {
            return Err("forecastDays must be 1-90".into());
        }
        Ok(())
    }
}

/// `subscribe` / `unsubscribe` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamsParams {
    pub streams: Vec<String>,
}

/// `subscribe` / `unsubscribe` result: the streams now subscribed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamsResult {
    pub streams: Vec<String>,
}

/// A pushed line (protocol 2). It has no `id`, which is how a client tells
/// it from a response; frames can arrive between a request and its response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub stream: String,
    pub data: Value,
}

/// `data` of a `lagged` frame: this many frames of `stream` were dropped
/// because the client read too slowly. Re-sync with the `*.sync` methods.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lagged {
    pub stream: String,
    pub missed: u64,
}

/// Whether the node can read the container engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContainerAccess {
    /// Reading containers and events.
    Ok,
    /// Switched off in the node's config (`[docker] enabled = false`).
    Off,
    /// No engine socket on this host.
    Absent,
    /// The socket exists but the service account may not use it
    /// (root has not run `nightgrid-node allow docker`).
    Denied,
    /// The engine didn't answer properly; see `message`.
    Error,
}

/// One container, as `docker ps -a` would show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub image: String,
    /// created, running, paused, restarting, exited, removing, dead
    pub state: String,
    /// The engine's status line, e.g. "Up 3 hours (healthy)".
    pub status: String,
    /// healthy, unhealthy, starting; none without a healthcheck.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainersResult {
    pub access: ContainerAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// When the list was read (unix seconds); 0 if never.
    pub at: i64,
    pub containers: Vec<ContainerInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_newest_common_version() {
        assert_eq!(negotiate(1, 3, 1, 2), Some(2));
        assert_eq!(negotiate(2, 3, 1, 2), Some(2));
        assert_eq!(negotiate(3, 4, 1, 2), None);
        assert_eq!(negotiate(1, 1, 1, 5), Some(1));
    }

    #[test]
    fn request_and_response_lines() {
        let r: Request = serde_json::from_str(r#"{"id":7,"method":"node.info"}"#).unwrap();
        assert_eq!((r.id, r.method.as_str(), &r.params), (7, "node.info", &Value::Null));
        let line = serde_json::to_string(&Response::err(7, ErrorCode::HelloRequired, "say hello first")).unwrap();
        assert_eq!(line, r#"{"id":7,"error":{"code":"hello_required","message":"say hello first"}}"#);
        let e: RpcError = serde_json::from_str(r#"{"code":"from_the_future","message":""}"#).unwrap();
        assert_eq!(e.code, ErrorCode::Unknown);
    }

    #[test]
    fn point_matches_app_json() {
        // The app's Point has no `n`; it must still parse what the app writes.
        let p: Point = serde_json::from_str(r#"{"ts":60,"cpu":1.5,"netRx":2,"gpuMem":null}"#).unwrap();
        assert_eq!((p.ts, p.cpu, p.net_rx, p.n), (60.0, Some(1.5), Some(2.0), 0));
        let v = serde_json::to_value(Point { ts: 60.0, ..Default::default() }).unwrap();
        for k in ["netRx", "diskWrite", "gpuPower", "load1"] {
            assert!(v.get(k).is_some(), "{k}");
        }
    }

    #[test]
    fn alert_and_event_json_match_the_app() {
        assert_eq!(serde_json::to_string(&AlertKind::DiskFilling).unwrap(), r#""diskFilling""#);
        assert_eq!(serde_json::to_string(&Severity::Critical).unwrap(), r#""critical""#);
        let e: Event =
            serde_json::from_str(r#"{"seq":1,"at":5,"kind":"somethingNew","level":"info","title":"t","detail":""}"#)
                .unwrap();
        assert_eq!(e.kind, EventKind::Other);
        // The app's settings JSON (more fields than the node knows) is accepted.
        let r: AlertRules = serde_json::from_str(r#"{"diskPercent":80,"hostOffline":true,"webhook":"none"}"#).unwrap();
        assert_eq!((r.disk_percent, r.forecast_days, r.enabled), (80, 14, true));
        assert!(AlertRules { disk_percent: 20, ..Default::default() }.validate().is_err());
    }

    #[test]
    fn resolution_rule() {
        assert_eq!(resolution(0, 3600), MINUTE);
        assert_eq!(resolution(0, 7 * 86400), ROLLUP);
        assert_eq!(resolution(0, 90 * 86400), HOURLY);
    }
}

/// `actions.run` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRequest {
    /// One of [`action::ALL`].
    pub action: String,
    /// The container, for container actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Only record what would happen.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionOutcome {
    /// It ran and the engine (or helper) said it worked.
    Ok,
    /// Dry run: recorded, not done.
    DryRun,
    /// It ran and failed.
    Failed,
    /// Not run: not allowed by root, over the rate limit, or a bad target.
    Refused,
}

/// One entry of the audit log (also the `actions.run` result).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRecord {
    pub seq: i64,
    pub at: i64,
    /// Who asked: `app (uid 1000)`, or `rule <id>`.
    pub source: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub dry_run: bool,
    pub outcome: ActionOutcome,
    pub detail: String,
    /// SHA-256 over this entry and the previous entry's hash.
    pub hash: String,
}

/// `actions.log` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionsLogResult {
    pub records: Vec<ActionRecord>,
    pub next: i64,
    pub more: bool,
    /// Every returned entry's hash checks out and links to the one before.
    pub verified: bool,
}

/// `actions.policy` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionsPolicy {
    /// Action types root allows (`[actions] allow`). Empty: none.
    pub allowed: Vec<String>,
    /// Every action type this node knows.
    pub known: Vec<String>,
    /// Real (not dry) runs allowed per hour, all sources together.
    pub max_per_hour: u32,
    /// Units root lets `unit.*` act on (`[actions] units`).
    #[serde(default)]
    pub units: Vec<String>,
    /// Whether the root helper is installed (`allow helper`); unit and
    /// power actions need it.
    #[serde(default)]
    pub helper: bool,
}

/// What a self-healing rule waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HealWhen {
    /// A container's health check says unhealthy.
    ContainerUnhealthy,
    /// The container-down alert is active (it was running and stopped).
    ContainerDown,
    /// The disk-full alert is active for a mount.
    DiskFull,
}

/// One self-healing rule, pushed by the app. Off and dry-run by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HealRule {
    /// Short id: letters, digits, `-`, `_`.
    pub id: String,
    pub enabled: bool,
    /// Only record what it would do.
    pub dry_run: bool,
    pub when: HealWhen,
    /// Container name or mount the condition applies to; `*` for any.
    pub target: String,
    /// How long the condition must hold before acting, 1-1440.
    pub for_minutes: u32,
    /// What to do. Container actions act on the matching container.
    pub action: String,
    /// Most runs per hour (1-60) and per day (1-500) for one target.
    pub max_per_hour: u32,
    pub max_per_day: u32,
    /// Runs without the condition clearing before the rule gives up on a
    /// target (and raises a warning), 1-20.
    pub give_up_after: u32,
}

impl Default for HealRule {
    fn default() -> Self {
        Self {
            id: String::new(),
            enabled: false,
            dry_run: true,
            when: HealWhen::ContainerUnhealthy,
            target: "*".into(),
            for_minutes: 5,
            action: action::CONTAINER_RESTART.into(),
            max_per_hour: 3,
            max_per_day: 10,
            give_up_after: 3,
        }
    }
}

impl HealRule {
    pub fn validate(&self) -> Result<(), String> {
        let id_ok = !self.id.is_empty()
            && self.id.len() <= 40
            && self.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !id_ok {
            return Err(format!("rule id {:?}: 1-40 letters, digits, - or _", self.id));
        }
        if !action::ALL.contains(&self.action.as_str()) {
            return Err(format!("rule {}: unknown action {:?}", self.id, self.action));
        }
        if !action::for_rules(&self.action) {
            return Err(format!("rule {}: self-healing can't run {}", self.id, self.action));
        }
        let container_rule = matches!(self.when, HealWhen::ContainerUnhealthy | HealWhen::ContainerDown);
        if action::takes_container(&self.action) && !container_rule {
            return Err(format!("rule {}: {} needs a container condition", self.id, self.action));
        }
        if self.target.is_empty() || self.target.len() > 255 {
            return Err(format!("rule {}: target must be `*` or a name", self.id));
        }
        let ranges = [
            ("forMinutes", self.for_minutes, 1, 1440),
            ("maxPerHour", self.max_per_hour, 1, 60),
            ("maxPerDay", self.max_per_day, 1, 500),
            ("giveUpAfter", self.give_up_after, 1, 20),
        ];
        for (name, v, lo, hi) in ranges {
            if !(lo..=hi).contains(&v) {
                return Err(format!("rule {}: {name} must be {lo}-{hi}", self.id));
            }
        }
        Ok(())
    }
}

/// `heal.status`: one line per rule and target it is tracking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealStatus {
    pub rule: String,
    pub target: String,
    /// When the condition was first seen (it still holds).
    pub since: i64,
    /// Runs since the condition started.
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<i64>,
    /// No more runs until the condition clears.
    pub gave_up: bool,
}

/// Most self-healing rules a node keeps.
pub const MAX_HEAL_RULES: usize = 32;

/// `heal.rules.get` / `heal.rules.set`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HealRules {
    pub rules: Vec<HealRule>,
}

impl HealRules {
    pub fn validate(&self) -> Result<(), String> {
        if self.rules.len() > MAX_HEAL_RULES {
            return Err(format!("at most {MAX_HEAL_RULES} rules"));
        }
        let mut ids = std::collections::BTreeSet::new();
        for r in &self.rules {
            r.validate()?;
            if !ids.insert(r.id.as_str()) {
                return Err(format!("rule id {:?} is used twice", r.id));
            }
        }
        Ok(())
    }
}

/// `heal.status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealStatusResult {
    /// Rules that are on (not counting dry-run ones separately).
    pub enabled: u32,
    /// Conditions the rules are tracking now.
    pub tracking: Vec<HealStatus>,
}

/// Most files one change may snapshot, and the largest one.
pub const MAX_CHANGE_FILES: usize = 32;
pub const MAX_CHANGE_FILE_BYTES: u64 = 1 << 20;

/// `changes.begin`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ChangeBegin {
    /// Absolute paths of the files the change will touch, under root's
    /// `[rollback] paths`. A file that doesn't exist yet is removed on revert.
    pub paths: Vec<String>,
    /// Seconds until the node reverts unless confirmed, 30-3600.
    pub confirm_within_secs: u32,
    /// Run after the files are put back, e.g. restart the unit that reads them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_revert: Option<ActionRequest>,
    /// What the change is, for the log (at most 200 characters kept).
    pub note: String,
}

/// `changes.confirm` / `changes.revert`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeId {
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChangeState {
    /// Snapshot taken, timer running.
    Pending,
    Confirmed,
    /// Put back, by the timer or on request.
    Reverted,
    /// The snapshot or the revert failed (see `detail`).
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub id: String,
    pub created: i64,
    pub deadline: i64,
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_revert: Option<ActionRequest>,
    pub note: String,
    pub state: ChangeState,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<i64>,
}

/// `changes.list`: pending ones first, then the latest 50 others.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesResult {
    pub changes: Vec<Change>,
}
