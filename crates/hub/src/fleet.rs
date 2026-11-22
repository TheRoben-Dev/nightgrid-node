//! What the hub knows about its members: the plan for each round, taking in
//! the collector's answers, serving them back in the node's own shapes, and
//! noticing a member that stops answering.

use std::sync::Mutex;

use nightgrid_node_proto::hub::{
    Ingest, IngestBatch, Member, MemberState, MemberStatus, Overview, Plan, PlanMember, SyncParams,
};
use nightgrid_node_proto::{
    method, Alert, AlertKind, CursorParams, ErrorCode, EventKind, Level, NodeIdentity, Peer, Severity, MAX_POINTS,
    MAX_ROWS,
};
use serde_json::value::RawValue;
use serde_json::{json, Value};

use crate::config::Config;
use crate::store::{MemberRow, Store};

/// Most members one hub takes, like a node's peer list.
pub const MAX_MEMBERS: usize = 64;
/// The first node release that can serve a hub.
pub const MIN_NODE: (u32, u32) = (0, 26);

pub struct Hub {
    pub config: Config,
    pub store: Mutex<Store>,
    pub identity: NodeIdentity,
    pub collector: Mutex<CollectorState>,
    /// Pushes per profile this minute (`profile` 1).
    pub profile_rate: crate::profile::RateLimit,
}

#[derive(Debug, Default, Clone)]
pub struct CollectorState {
    pub fingerprint: Option<String>,
    pub listening: Option<String>,
    pub listen_all: Vec<String>,
    pub reported: i64,
}

impl Hub {
    pub fn new(config: Config, store: Store) -> Self {
        let identity = NodeIdentity {
            node_id: store.hub_id(),
            version: env!("CARGO_PKG_VERSION").into(),
            hostname: hostname(),
            arch: std::env::consts::ARCH.into(),
        };
        Self {
            config,
            store: Mutex::new(store),
            identity,
            collector: Mutex::default(),
            profile_rate: Default::default(),
        }
    }

    pub fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_owned()).unwrap_or_default()
}

/// Checks `fleet.members.set` with the node's own rules for peers.
pub fn validate(list: &[Member]) -> Result<(), String> {
    if list.len() > MAX_MEMBERS {
        return Err(format!("at most {MAX_MEMBERS} members"));
    }
    let peers: Vec<Peer> = list
        .iter()
        .map(|m| Peer {
            id: m.id.clone(),
            name: m.name.clone(),
            address: m.address.clone(),
            fingerprint: m.fingerprint.clone(),
            follow_ups: false,
        })
        .collect();
    nightgrid_node::peers::validate(&peers).map_err(|e| e.replace("peer ", "member "))
}

pub fn plan(hub: &Hub) -> Result<Plan, String> {
    let st = hub.store();
    let members = st.members().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for m in members {
        let cursors = st.cursors(&m.id).map_err(|e| e.to_string())?;
        out.push(PlanMember { member: m, cursors });
    }
    Ok(Plan {
        hub_id: hub.identity.node_id.clone(),
        hostname: hub.identity.hostname.clone(),
        interval_secs: hub.config.collect.interval_secs,
        members: out,
    })
}

/// An error that means the member answered but won't serve this hub.
pub fn refused(e: &str) -> bool {
    ["isn't served here", "not this node's hub", "not paired", "not served to the hub", "refused this node"]
        .iter()
        .any(|x| e.contains(x))
}

fn version_ok(v: &str) -> bool {
    let mut it = v.split(['.', '-']).map(|x| x.parse::<u32>().unwrap_or(0));
    let (a, b) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    (a, b) >= MIN_NODE
}

fn json_str(v: &Value) -> String {
    v.to_string()
}

/// Takes one batch from the collector.
pub fn ingest(hub: &Hub, batch: IngestBatch, now: i64) -> Result<(), String> {
    {
        let mut c = hub.collector.lock().unwrap_or_else(|e| e.into_inner());
        c.fingerprint = Some(batch.fingerprint.clone()).filter(|f| !f.is_empty());
        c.listening = batch.listening.clone();
        c.listen_all = if batch.listen_all.is_empty() {
            batch.listening.iter().cloned().collect()
        } else {
            batch.listen_all.clone()
        };
        c.reported = now;
    }
    let mut st = hub.store();
    let members = st.members().map_err(|e| e.to_string())?;
    for a in batch.answers {
        if !members.iter().any(|m| m.id == a.id) {
            continue;
        }
        if let Err(e) = take(&mut st, &a) {
            nightgrid_node::warn!("hub: answer from {} ({}) not stored: {e}", a.id, a.method);
        }
    }
    Ok(())
}

fn take(st: &mut Store, a: &Ingest) -> Result<(), String> {
    let mut row = st.member_row(&a.id).map_err(|e| e.to_string())?;
    row.last_try = Some(a.at);
    let Some(r) = &a.result else {
        let e = a.error.clone().unwrap_or_else(|| "no answer".into());
        row.refused = refused(&e);
        row.error = Some(e);
        return st.set_member_row(&a.id, &row).map_err(|e| e.to_string());
    };
    row.last_contact = Some(a.at.max(row.last_contact.unwrap_or(0)));
    let e = |x: rusqlite::Error| x.to_string();
    match a.method.as_str() {
        "peer.ping" => {
            row.version = r["version"].as_str().map(str::to_owned);
            row.hostname = r["hostname"].as_str().map(str::to_owned);
            if !row.refused {
                row.error = None;
            }
        }
        method::NODE_INFO => {
            row.info = Some(r.clone());
            row.version = r["node"]["version"].as_str().map(str::to_owned).or(row.version);
            row.hostname = r["node"]["hostname"].as_str().map(str::to_owned).or(row.hostname);
        }
        method::ALERTS_ACTIVE => {
            row.alerts = Some(r.clone());
        }
        method::METRICS_SYNC => {
            let res = a.params["res"].as_i64().ok_or("no res")?;
            let points: Vec<(i64, String)> = r["points"]
                .as_array()
                .map(|p| p.iter().filter_map(|x| Some((x["ts"].as_f64()? as i64, json_str(x)))).collect())
                .unwrap_or_default();
            let next = r["next"].as_i64().unwrap_or(0);
            st.put_metrics(&a.id, res, &points, next).map_err(e)?;
        }
        method::EVENTS_SYNC => {
            let events: Vec<(i64, i64, String)> = r["events"]
                .as_array()
                .map(|v| v.iter().filter_map(|x| Some((x["seq"].as_i64()?, x["at"].as_i64()?, json_str(x)))).collect())
                .unwrap_or_default();
            st.put_events(&a.id, &events, r["next"].as_i64().unwrap_or(0)).map_err(e)?;
        }
        method::UPTIME_SYNC => {
            let hours: Vec<(i64, String)> = r["hours"]
                .as_array()
                .map(|v| v.iter().filter_map(|x| Some((x["hour"].as_i64()?, json_str(x)))).collect())
                .unwrap_or_default();
            st.put_uptime(&a.id, &hours, r["next"].as_i64().unwrap_or(0)).map_err(e)?;
        }
        method::DISK_SYNC => {
            let rows: Vec<(i64, String, String)> = r["rows"]
                .as_array()
                .map(|v| {
                    v.iter()
                        .filter_map(|x| Some((x["ts"].as_i64()?, x["mount"].as_str()?.to_owned(), json_str(x))))
                        .collect()
                })
                .unwrap_or_default();
            st.put_disk(&a.id, &rows, r["next"].as_i64().unwrap_or(0)).map_err(e)?;
        }
        m => return Err(format!("unexpected method {m:?}")),
    }
    if a.method != "peer.ping" {
        row.refused = false;
        row.error = None;
    }
    st.set_member_row(&a.id, &row).map_err(|e| e.to_string())
}

fn state(row: &MemberRow, interval: u32, now: i64) -> MemberState {
    let fresh = row.last_contact.is_some_and(|t| now - t <= 2 * i64::from(interval) + 30);
    if row.last_try.is_none() {
        MemberState::New
    } else if row.version.as_deref().is_some_and(|v| !version_ok(v)) {
        MemberState::Version
    } else if row.refused && fresh {
        MemberState::Consent
    } else if fresh {
        MemberState::Ok
    } else if row.error.is_some() {
        MemberState::Unreachable
    } else {
        MemberState::Stale
    }
}

pub fn overview(hub: &Hub, now: i64) -> Result<Overview, String> {
    let st = hub.store();
    let interval = hub.config.collect.interval_secs;
    let mut members = Vec::new();
    for m in st.members().map_err(|e| e.to_string())? {
        let row = st.member_row(&m.id).map_err(|e| e.to_string())?;
        members.push(MemberStatus {
            state: state(&row, interval, now),
            alerts: row.alerts.as_ref().and_then(|a| a.as_array()).map_or(0, |a| a.len() as u32),
            id: m.id,
            name: m.name,
            last_contact: row.last_contact,
            version: row.version,
            hostname: row.hostname,
            error: row.error,
        });
    }
    let c = hub.collector.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let collector = if c.reported == 0 {
        "off"
    } else if now - c.reported <= 3 * i64::from(interval) + 60 {
        "ok"
    } else {
        "denied"
    };
    Ok(Overview {
        hub_id: hub.identity.node_id.clone(),
        fingerprint: c.fingerprint,
        listening: c.listening,
        listen_all: c.listen_all,
        collector: collector.into(),
        store_bytes: st.size(),
        members,
    })
}

fn bad(m: impl Into<String>) -> (ErrorCode, String) {
    (ErrorCode::BadRequest, m.into())
}

/// Stored rows are the member's own JSON; they are passed on as text, never
/// parsed into `Value` trees (which take several times the memory).
fn parse_rows(rows: Vec<(i64, String)>) -> Vec<Box<RawValue>> {
    rows.into_iter().filter_map(|(_, s)| RawValue::from_string(s).ok()).collect()
}

/// One page of rows plus its cursor fields, as JSON text.
#[derive(Debug)]
pub struct Answer(Box<RawValue>);

impl Answer {
    fn page(items: (&str, Vec<Box<RawValue>>), fields: Value) -> Self {
        let mut m: std::collections::BTreeMap<String, AnswerField> = std::collections::BTreeMap::new();
        if let Value::Object(o) = fields {
            for (k, v) in o {
                m.insert(k, AnswerField::Value(v));
            }
        }
        m.insert(items.0.to_owned(), AnswerField::Rows(items.1));
        Self(serde_json::value::to_raw_value(&m).unwrap_or_else(|_| RawValue::from_string("null".into()).unwrap()))
    }

    fn value(v: &Value) -> Self {
        Self(serde_json::value::to_raw_value(v).unwrap_or_else(|_| RawValue::from_string("null".into()).unwrap()))
    }

    /// The answer as a `Value` (tests and small answers).
    pub fn to_value(&self) -> Value {
        serde_json::from_str(self.0.get()).unwrap_or(Value::Null)
    }
}

impl serde::Serialize for Answer {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum AnswerField {
    Value(Value),
    Rows(Vec<Box<RawValue>>),
}

/// `fleet.sync`: one member's read method, from the hub's copy, in exactly
/// the shape the node answers with.
pub fn sync(hub: &Hub, p: &SyncParams) -> Result<Answer, (ErrorCode, String)> {
    let st = hub.store();
    let internal = |e: rusqlite::Error| (ErrorCode::Internal, e.to_string());
    if st.added(&p.node_id).map_err(internal)?.is_none() {
        return Err(bad(format!("{} is not a member of this hub", p.node_id)));
    }
    let params = if p.params.is_null() { json!({}) } else { p.params.clone() };
    let cursor = || -> Result<CursorParams, (ErrorCode, String)> {
        serde_json::from_value(params.clone()).map_err(|e| bad(format!("params: {e}")))
    };
    let id = p.node_id.as_str();
    match p.method.as_str() {
        method::NODE_INFO => {
            let row = st.member_row(id).map_err(internal)?;
            row.info
                .map(|v| Answer::value(&v))
                .ok_or_else(|| (ErrorCode::Internal, "not pulled from that member yet".to_owned()))
        }
        method::ALERTS_ACTIVE => {
            Ok(Answer::value(&st.member_row(id).map_err(internal)?.alerts.unwrap_or_else(|| json!([]))))
        }
        method::METRICS_SYNC => {
            let sp: nightgrid_node_proto::SyncParams =
                serde_json::from_value(params).map_err(|e| bad(format!("params: {e}")))?;
            if !nightgrid_node_proto::is_tier(sp.res) {
                return Err(bad("res must be 60, 900 or 3600"));
            }
            let limit = sp.limit.unwrap_or(MAX_POINTS).clamp(1, MAX_POINTS);
            let mut rows = st.metrics_after(id, sp.res, sp.after, limit + 1).map_err(internal)?;
            let more = rows.len() > limit;
            rows.truncate(limit);
            let next = rows.last().map_or(sp.after, |r| r.0);
            let oldest = st.metrics_oldest(id, sp.res).map_err(internal)?;
            Ok(Answer::page(
                ("points", parse_rows(rows)),
                json!({"res": sp.res, "next": next, "more": more, "oldest": oldest}),
            ))
        }
        method::EVENTS_SYNC => {
            let c = cursor()?;
            let limit = c.limit.unwrap_or(MAX_ROWS).clamp(1, MAX_ROWS);
            let kinds = c.kinds.as_deref().filter(|k| !k.is_empty());
            let mut rows = st.events_after_kinds(id, c.after, limit + 1, kinds).map_err(internal)?;
            let more = rows.len() > limit;
            rows.truncate(limit);
            let mut next = rows.last().map_or(c.after, |r| r.0);
            if kinds.is_some() && !more {
                // Past the events the filter skipped, as the node does.
                next = st.events_newest(id).map_err(internal)?.unwrap_or(c.after).max(c.after);
            }
            let oldest = st.events_oldest(id).map_err(internal)?;
            Ok(Answer::page(("events", parse_rows(rows)), json!({"next": next, "more": more, "oldest": oldest})))
        }
        method::UPTIME_SYNC => {
            let c = cursor()?;
            let limit = c.limit.unwrap_or(MAX_ROWS).clamp(1, MAX_ROWS);
            let mut rows = st.uptime_after(id, c.after, limit + 1).map_err(internal)?;
            let more = rows.len() > limit;
            rows.truncate(limit);
            let next = rows.last().map_or(c.after, |r| r.0);
            Ok(Answer::page(("hours", parse_rows(rows)), json!({"next": next, "more": more})))
        }
        method::DISK_SYNC => {
            let c = cursor()?;
            let limit = c.limit.unwrap_or(MAX_ROWS).clamp(1, MAX_ROWS);
            let mut rows = st.disk_after(id, c.after, limit + 1).map_err(internal)?;
            let more = rows.len() > limit;
            if more {
                // Whole hours only, as the node does.
                let cut = rows[limit].0;
                rows.retain(|r| r.0 < cut);
                if rows.is_empty() {
                    rows = st.disk_after(id, cut - 1, usize::MAX >> 1).map_err(internal)?;
                    rows.retain(|r| r.0 == cut);
                }
            }
            let next = rows.last().map_or(c.after, |r| r.0);
            Ok(Answer::page(("rows", parse_rows(rows)), json!({"next": next, "more": more})))
        }
        m => Err((
            ErrorCode::BadRequest,
            format!("{m:?} is not kept by the hub (one of {})", nightgrid_node_proto::HUB_READS.join(", ")),
        )),
    }
}

/// `fleet.events.sync`: the hub's own timeline, shaped like `events.sync`.
pub fn hub_events(hub: &Hub, c: &CursorParams) -> Result<Answer, String> {
    let st = hub.store();
    let limit = c.limit.unwrap_or(MAX_ROWS).clamp(1, MAX_ROWS);
    let mut rows = st.hub_events_after(c.after, limit + 1).map_err(|e| e.to_string())?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    let next = rows.last().map_or(c.after, |r| r.0);
    let oldest = st.hub_events_oldest().map_err(|e| e.to_string())?;
    Ok(Answer::page(("events", parse_rows(rows)), json!({"next": next, "more": more, "oldest": oldest})))
}

fn alert_id(id: &str) -> u32 {
    u32::from_str_radix(id.get(..8).unwrap_or("0"), 16).unwrap_or(0)
}

/// Records "offline" for members that stopped answering and "back online"
/// for those that answer again. Returns the titles, for the log and tests.
pub fn check_offline(hub: &Hub, now: i64) -> Result<Vec<String>, String> {
    let st = hub.store();
    let grace = i64::from(hub.config.alerts.offline_secs);
    let mut out = Vec::new();
    for m in st.members().map_err(|e| e.to_string())? {
        let added = st.added(&m.id).map_err(|e| e.to_string())?.unwrap_or(now);
        let mut row = st.member_row(&m.id).map_err(|e| e.to_string())?;
        let last = row.last_contact.unwrap_or(added);
        let silent = now - last > grace;
        if silent == row.offline {
            continue;
        }
        let since = if silent { last } else { now };
        let (kind, level, title, detail, resolved) = if silent {
            let why = row.error.clone().unwrap_or_else(|| "no answer".into());
            (
                EventKind::Alert,
                Level::Critical,
                format!("{} offline", m.name),
                format!("no answer to the hub for {} min: {why}", (now - last) / 60),
                None,
            )
        } else {
            (
                EventKind::Recovered,
                Level::Ok,
                format!("{} back online", m.name),
                "answers the hub again".into(),
                Some(now),
            )
        };
        let alert = Alert {
            id: alert_id(&m.id),
            key: format!("hostOffline:{}", m.id),
            kind: AlertKind::HostOffline,
            severity: Severity::Critical,
            title: format!("{} offline", m.name),
            detail: detail.clone(),
            since,
            resolved,
        };
        let ev = json!({"seq": 0, "at": now, "kind": kind, "level": level, "title": title, "detail": detail,
                         "alert": alert, "nodeId": m.id});
        st.hub_event(now, ev).map_err(|e| e.to_string())?;
        row.offline = silent;
        st.set_member_row(&m.id, &row).map_err(|e| e.to_string())?;
        nightgrid_node::info!("hub: {title}");
        out.push(title);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> Hub {
        Hub::new(Config::default(), Store::open_in_memory().unwrap())
    }

    fn member(c: char) -> Member {
        Member {
            id: c.to_string().repeat(32),
            name: format!("host-{c}"),
            address: "100.64.0.9:7419".into(),
            fingerprint: c.to_string().repeat(64).replace(c, "1"),
        }
    }

    fn ans(id: &str, at: i64, m: &str, params: Value, result: Option<Value>, error: Option<&str>) -> Ingest {
        Ingest { id: id.into(), at, method: m.into(), params, result, error: error.map(str::to_owned) }
    }

    #[test]
    fn serves_back_what_it_pulled_in_the_nodes_shape() {
        let h = hub();
        let mut a = member('a');
        a.fingerprint = "1".repeat(64);
        h.store().set_members(std::slice::from_ref(&a), 100).unwrap();
        let ev = json!({"events": [{"seq": 7, "at": 100, "kind": "boot", "level": "info", "title": "booted", "detail": ""},
                                   {"seq": 8, "at": 160, "kind": "other", "level": "warn", "title": "x", "detail": "y"}],
                        "next": 8, "more": false, "oldest": 1});
        let pts = json!({"res": 60, "points": [{"ts": 60.0, "n": 6}, {"ts": 120.0, "n": 6}], "next": 120, "more": false, "oldest": 60});
        let batch = IngestBatch {
            fingerprint: "f".repeat(64),
            listening: Some("100.64.0.1:7420".into()),
            listen_all: vec![],
            answers: vec![
                ans(&a.id, 200, "peer.ping", Value::Null, Some(json!({"version": "0.26.0", "hostname": "nas"})), None),
                ans(&a.id, 200, "events.sync", json!({"after": 0}), Some(ev.clone()), None),
                ans(&a.id, 200, "metrics.sync", json!({"res": 60, "after": 0}), Some(pts), None),
                ans(&a.id, 200, "alerts.active", Value::Null, Some(json!([{"id": 1}])), None),
            ],
        };
        ingest(&h, batch, 200).unwrap();
        let got =
            sync(&h, &SyncParams { node_id: a.id.clone(), method: "events.sync".into(), params: json!({"after": 7}) })
                .unwrap()
                .to_value();
        assert_eq!(got["events"].as_array().unwrap().len(), 1);
        assert_eq!((got["next"].as_i64(), got["oldest"].as_i64()), (Some(8), Some(7)));
        let r: nightgrid_node_proto::EventsResult = serde_json::from_value(got).unwrap();
        assert_eq!(r.events[0].seq, 8, "the node's own seq, so the app's cursor fits");
        let only_boot = |after: i64| {
            sync(
                &h,
                &SyncParams {
                    node_id: a.id.clone(),
                    method: "events.sync".into(),
                    params: json!({"after": after, "kinds": ["boot"]}),
                },
            )
            .unwrap()
            .to_value()
        };
        let got = only_boot(0);
        assert_eq!((got["events"][0]["seq"].as_i64(), got["next"].as_i64()), (Some(7), Some(8)), "{got}");
        assert_eq!(only_boot(7)["events"].as_array().unwrap().len(), 0);
        assert_eq!(only_boot(7)["next"].as_i64(), Some(8), "past the skipped event");
        let m = sync(
            &h,
            &SyncParams {
                node_id: a.id.clone(),
                method: "metrics.sync".into(),
                params: json!({"res": 60, "after": 0, "limit": 1}),
            },
        )
        .unwrap()
        .to_value();
        assert_eq!((m["next"].as_i64(), m["more"].as_bool()), (Some(60), Some(true)));
        assert_eq!(plan(&h).unwrap().members[0].cursors["events"], 8);
        let o = overview(&h, 210).unwrap();
        assert_eq!(o.members[0].state, MemberState::Ok);
        assert_eq!((o.members[0].alerts, o.collector.as_str()), (1, "ok"));
        let e = sync(
            &h,
            &SyncParams { node_id: "b".repeat(32), method: "events.sync".into(), params: json!({"after": 0}) },
        );
        assert!(e.is_err(), "not a member");
        let e = sync(&h, &SyncParams { node_id: a.id.clone(), method: "actions.log".into(), params: Value::Null });
        assert!(e.unwrap_err().1.contains("not kept"));
    }

    #[test]
    fn offline_after_the_grace_and_back_once() {
        let h = hub();
        let a = Member { fingerprint: "1".repeat(64), ..member('a') };
        h.store().set_members(std::slice::from_ref(&a), 1000).unwrap();
        assert!(check_offline(&h, 1100).unwrap().is_empty(), "within the grace after being added");
        let fail = |at| IngestBatch {
            answers: vec![ans(&a.id, at, "peer.ping", Value::Null, None, Some("timed out"))],
            ..Default::default()
        };
        ingest(&h, fail(1150), 1150).unwrap();
        assert_eq!(check_offline(&h, 1130 + 120).unwrap(), vec!["host-a offline"]);
        assert!(check_offline(&h, 1300).unwrap().is_empty(), "only once");
        assert_eq!(overview(&h, 1300).unwrap().members[0].state, MemberState::Unreachable);
        let ok = IngestBatch {
            answers: vec![ans(&a.id, 1310, "peer.ping", Value::Null, Some(json!({"version": "0.26.0"})), None)],
            ..Default::default()
        };
        ingest(&h, ok, 1310).unwrap();
        assert_eq!(check_offline(&h, 1320).unwrap(), vec!["host-a back online"]);
        let ev = hub_events(&h, &CursorParams { after: 0, limit: None, kinds: None }).unwrap();
        let r: nightgrid_node_proto::EventsResult = serde_json::from_value(ev.to_value()).unwrap();
        assert_eq!(r.events.len(), 2);
        assert_eq!(r.events[0].alert.as_ref().unwrap().kind, AlertKind::HostOffline);
        assert!(r.events[1].alert.as_ref().unwrap().resolved.is_some());
    }

    #[test]
    fn consent_and_version_states() {
        let h = hub();
        let a = Member { fingerprint: "1".repeat(64), ..member('a') };
        h.store().set_members(std::slice::from_ref(&a), 0).unwrap();
        let batch = IngestBatch {
            answers: vec![
                ans(&a.id, 10, "peer.ping", Value::Null, Some(json!({"version": "0.26.0"})), None),
                ans(
                    &a.id,
                    10,
                    "events.sync",
                    json!({"after": 0}),
                    None,
                    Some("\"peer.sync\" isn't served here ([peers] serve)"),
                ),
            ],
            ..Default::default()
        };
        ingest(&h, batch, 10).unwrap();
        assert_eq!(overview(&h, 20).unwrap().members[0].state, MemberState::Consent);
        let old = IngestBatch {
            answers: vec![ans(&a.id, 30, "peer.ping", Value::Null, Some(json!({"version": "0.25.1"})), None)],
            ..Default::default()
        };
        ingest(&h, old, 30).unwrap();
        assert_eq!(overview(&h, 40).unwrap().members[0].state, MemberState::Version);
        assert!(version_ok("0.26.0") && version_ok("1.0.0") && !version_ok("0.9.9"));
    }

    #[test]
    fn validates_members_like_peers() {
        let ok = Member { fingerprint: "1".repeat(64), ..member('a') };
        assert!(validate(std::slice::from_ref(&ok)).is_ok());
        let bad = Member { address: "0.0.0.0:7419".into(), ..ok.clone() };
        assert!(validate(&[bad]).unwrap_err().starts_with("member"));
        assert!(validate(&[ok.clone(), ok]).is_err());
    }
}
