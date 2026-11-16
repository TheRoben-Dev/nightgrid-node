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
pub const PROTO_VERSION: u32 = 1;
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
}

impl Default for AlertRules {
    fn default() -> Self {
        Self { enabled: true, disk_full: true, disk_percent: 90, disk_forecast: true, forecast_days: 14 }
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
