//! The unix socket server: NDJSON requests in, one response each.
//!
//! Access control is the socket's file mode: `0660`, owner and group
//! `nightgrid-node`, inside a `0750` runtime directory. The app reaches it as
//! an SSH user who is a member of that group (through `nightgrid-node bridge`
//! or SSH streamlocal forwarding). Nothing listens on the network. Every
//! connection's peer uid is logged.

use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use nightgrid_node_proto::{
    cap, method, negotiate, resolution, AlertRules, Caps, CursorParams, DiskResult, ErrorCode, EventsResult, Hello,
    HistoryParams, HistoryResult, NodeInfo, Request, Response, Retention, SyncParams, SyncResult, UptimeResult,
    Welcome, MAX_LINE, MAX_POINTS, MAX_ROWS, PROTO_MIN, PROTO_VERSION,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;

use crate::daemon::{now_secs, rss_bytes, Shared};
use crate::store::{HOURLY_DAYS, ROLLUP_DAYS};

/// Binds the socket, replacing a stale one, with mode 0660.
pub fn bind(path: &Path) -> Result<UnixListener, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let l = UnixListener::bind(path).map_err(|e| format!("bind {}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    Ok(l)
}

/// Serves until `shutdown` completes.
pub async fn serve(listener: UnixListener, shared: Arc<Shared>, shutdown: impl Future<Output = ()>) {
    let slots = Arc::new(Semaphore::new(shared.config.server.max_connections as usize));
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let Ok(permit) = slots.clone().try_acquire_owned() else {
                    crate::warn!("connection refused: {} connections already open", shared.config.server.max_connections);
                    continue;
                };
                let shared = shared.clone();
                tokio::spawn(async move {
                    let uid = stream.peer_cred().map(|c| c.uid().to_string()).unwrap_or_else(|_| "?".into());
                    crate::debug!("client connected (uid {uid})");
                    if let Err(e) = connection(stream, &shared, uid.clone()).await {
                        crate::debug!("client (uid {uid}): {e}");
                    }
                    crate::debug!("client disconnected (uid {uid})");
                    drop(permit);
                });
            }
        }
    }
}

async fn connection(stream: UnixStream, shared: &Shared, uid: String) -> std::io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::with_capacity(16 * 1024, r);
    let mut session = Session { uid, ..Default::default() };
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = (&mut r).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        if buf.len() > MAX_LINE {
            send(&mut w, &Response::err(0, ErrorCode::TooLarge, "line too long")).await?;
            return Ok(());
        }
        let resp = handle_line(shared, &mut session, &buf);
        send(&mut w, &resp).await?;
    }
}

async fn send(w: &mut tokio::net::unix::OwnedWriteHalf, r: &Response) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(r).unwrap_or_default();
    line.push(b'\n');
    w.write_all(&line).await
}

/// Per-connection protocol state.
#[derive(Debug, Default)]
pub struct Session {
    /// The negotiated version, once `hello` succeeded.
    pub proto: Option<u32>,
    /// The peer's uid, for the log.
    pub uid: String,
}

/// What this node offers. Every capability here is on in N0; later ones
/// will depend on the root-owned config.
pub fn caps() -> Caps {
    [
        (cap::NODE, 1),
        (cap::METRICS, 1),
        (cap::METRICS_SYNC, 1),
        (cap::EVENTS, 1),
        (cap::UPTIME, 1),
        (cap::DISK, 1),
        (cap::ALERTS, 1),
        (cap::CONTAINERS, 1),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect()
}

fn params<T: DeserializeOwned>(v: &Value) -> Result<T, Response> {
    let v = if v.is_null() { Value::Object(Default::default()) } else { v.clone() };
    serde_json::from_value(v).map_err(|e| Response::err(0, ErrorCode::BadRequest, format!("params: {e}")))
}

/// Handles one request line.
pub fn handle_line(shared: &Shared, session: &mut Session, line: &[u8]) -> Response {
    let req: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => return Response::err(0, ErrorCode::BadRequest, format!("not a request: {e}")),
    };
    let id = req.id;
    let mut r = dispatch(shared, session, &req).unwrap_or_else(|e| e);
    r.id = id;
    r
}

fn internal(e: impl std::fmt::Display) -> Response {
    crate::warn!("request failed: {e}");
    Response::err(0, ErrorCode::Internal, "internal error (see the node's log)")
}

fn dispatch(shared: &Shared, session: &mut Session, req: &Request) -> Result<Response, Response> {
    if req.method == method::HELLO {
        let h: Hello = params(&req.params)?;
        let Some(v) = negotiate(h.proto_min, h.proto_max, PROTO_MIN, PROTO_VERSION) else {
            return Err(Response::err(
                0,
                ErrorCode::UnsupportedProto,
                format!("node speaks protocol {PROTO_MIN}-{PROTO_VERSION}"),
            ));
        };
        session.proto = Some(v);
        let client: String = h.client.chars().filter(|c| !c.is_control()).take(80).collect();
        crate::info!("hello from {client:?} as uid {} (protocol {v})", session.uid);
        return Ok(Response::ok(0, Welcome { proto: v, node: shared.identity.clone(), caps: caps() }));
    }
    if session.proto.is_none() {
        return Err(Response::err(0, ErrorCode::HelloRequired, "send hello first"));
    }
    match req.method.as_str() {
        method::NODE_INFO => {
            let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
            let m = &shared.config.metrics;
            Ok(Response::ok(
                0,
                NodeInfo {
                    node: shared.identity.clone(),
                    started: shared.started,
                    interval_secs: m.interval_secs,
                    retention: Retention {
                        minute_days: m.minute_retention_days,
                        rollup_days: ROLLUP_DAYS.max(m.minute_retention_days),
                        hourly_days: HOURLY_DAYS.max(m.minute_retention_days),
                    },
                    coverage: st.coverage().map_err(internal)?,
                    rss_bytes: rss_bytes(),
                    db_bytes: st.size(),
                },
            ))
        }
        method::METRICS_LATEST => {
            let latest = shared.latest.lock().unwrap_or_else(|e| e.into_inner()).clone();
            Ok(Response::ok(0, latest))
        }
        method::METRICS_HISTORY => {
            let p: HistoryParams = params(&req.params)?;
            if p.to < p.from {
                return Err(Response::err(0, ErrorCode::BadRequest, "to is before from"));
            }
            let res = p.res.unwrap_or_else(|| resolution(p.from, p.to));
            if !nightgrid_node_proto::is_tier(res) {
                return Err(Response::err(0, ErrorCode::BadRequest, "res must be 60, 900 or 3600"));
            }
            let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
            let mut points = st.history(res, p.from, p.to, MAX_POINTS + 1).map_err(internal)?;
            let truncated = points.len() > MAX_POINTS;
            points.truncate(MAX_POINTS);
            Ok(Response::ok(0, HistoryResult { res, points, truncated }))
        }
        method::METRICS_SYNC => {
            let p: SyncParams = params(&req.params)?;
            if !nightgrid_node_proto::is_tier(p.res) {
                return Err(Response::err(0, ErrorCode::BadRequest, "res must be 60, 900 or 3600"));
            }
            let limit = p.limit.unwrap_or(MAX_POINTS).clamp(1, MAX_POINTS);
            let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
            let mut points = st.sync(p.res, p.after, now_secs(), limit + 1).map_err(internal)?;
            let more = points.len() > limit;
            points.truncate(limit);
            let next = points.last().map_or(p.after, |x| x.ts as i64);
            let oldest = st.oldest(p.res).map_err(internal)?;
            Ok(Response::ok(0, SyncResult { res: p.res, points, next, more, oldest }))
        }
        method::EVENTS_SYNC => {
            let p: CursorParams = params(&req.params)?;
            let limit = p.limit.unwrap_or(MAX_ROWS).clamp(1, MAX_ROWS);
            let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
            let mut events = st.events_after(p.after, limit + 1).map_err(internal)?;
            let more = events.len() > limit;
            events.truncate(limit);
            let next = events.last().map_or(p.after, |e| e.seq);
            let oldest = st.events_oldest().map_err(internal)?;
            Ok(Response::ok(0, EventsResult { events, next, more, oldest }))
        }
        method::UPTIME_SYNC => {
            let p: CursorParams = params(&req.params)?;
            let limit = p.limit.unwrap_or(MAX_ROWS).clamp(1, MAX_ROWS);
            let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
            let mut hours = st.uptime_after(p.after, now_secs(), limit + 1).map_err(internal)?;
            let more = hours.len() > limit;
            hours.truncate(limit);
            let next = hours.last().map_or(p.after, |h| h.hour);
            Ok(Response::ok(0, UptimeResult { hours, next, more }))
        }
        method::DISK_SYNC => {
            let p: CursorParams = params(&req.params)?;
            let limit = p.limit.unwrap_or(MAX_ROWS).clamp(1, MAX_ROWS);
            let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
            // Whole hours only, so a page never splits an hour's mounts.
            let mut rows = st.disk_usage_after(p.after, now_secs(), limit + 1).map_err(internal)?;
            let more = rows.len() > limit;
            if more {
                let cut = rows[limit].ts;
                rows.retain(|r| r.ts < cut);
                if rows.is_empty() {
                    rows = st.disk_usage_after(cut - 1, now_secs(), usize::MAX >> 1).map_err(internal)?;
                    rows.retain(|r| r.ts == cut);
                }
            }
            let next = rows.last().map_or(p.after, |r| r.ts);
            Ok(Response::ok(0, DiskResult { rows, next, more }))
        }
        method::CONTAINERS_LIST => {
            let r = shared.containers.lock().unwrap_or_else(|e| e.into_inner()).result.clone();
            Ok(Response::ok(0, r))
        }
        method::ALERTS_ACTIVE => {
            let active = shared.active.lock().unwrap_or_else(|e| e.into_inner()).clone();
            Ok(Response::ok(0, active))
        }
        method::ALERTS_RULES_GET => {
            let rules = shared.rules.lock().unwrap_or_else(|e| e.into_inner()).clone();
            Ok(Response::ok(0, rules))
        }
        method::ALERTS_RULES_SET => {
            let rules: AlertRules = params(&req.params)?;
            rules.validate().map_err(|e| Response::err(0, ErrorCode::BadRequest, e))?;
            let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
            st.set_meta(crate::engine::META_RULES, &serde_json::to_string(&rules).unwrap_or_default())
                .map_err(internal)?;
            crate::info!("alert rules updated by uid {}", session.uid);
            *shared.rules.lock().unwrap_or_else(|e| e.into_inner()) = rules.clone();
            Ok(Response::ok(0, rules))
        }
        _ => Err(Response::err(0, ErrorCode::UnknownMethod, format!("unknown method {}", req.method))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::store::Store;
    use nightgrid_node_proto::{Point, MINUTE, ROLLUP};
    use serde_json::json;

    fn shared() -> Arc<Shared> {
        Shared::new(Config::default(), Store::open_in_memory().unwrap())
    }

    fn call(s: &Shared, ses: &mut Session, id: u64, m: &str, p: Value) -> Response {
        let line = serde_json::to_vec(&Request { id, method: m.into(), params: p }).unwrap();
        handle_line(s, ses, &line)
    }

    fn code(r: &Response) -> Option<ErrorCode> {
        r.error.as_ref().map(|e| e.code)
    }

    #[test]
    fn hello_first_and_negotiation() {
        let s = shared();
        let mut ses = Session::default();
        assert_eq!(code(&call(&s, &mut ses, 1, "node.info", Value::Null)), Some(ErrorCode::HelloRequired));
        let r = call(&s, &mut ses, 2, "hello", json!({"protoMin": 7, "protoMax": 9}));
        assert_eq!(code(&r), Some(ErrorCode::UnsupportedProto));
        let r = call(&s, &mut ses, 3, "hello", json!({"protoMin": 1, "protoMax": 9, "client": "test"}));
        assert_eq!(r.id, 3);
        let w: Welcome = serde_json::from_value(r.result.unwrap()).unwrap();
        assert_eq!(w.proto, PROTO_VERSION);
        assert_eq!(w.caps.get("metrics.sync"), Some(&1));
        assert_eq!(w.node.node_id.len(), 32);
        let r = call(&s, &mut ses, 4, "node.info", Value::Null);
        let info: NodeInfo = serde_json::from_value(r.result.unwrap()).unwrap();
        assert_eq!(info.retention.hourly_days, 400);
        assert_eq!(info.coverage.len(), 3);
        assert_eq!(code(&call(&s, &mut ses, 5, "nope", Value::Null)), Some(ErrorCode::UnknownMethod));
        assert_eq!(code(&handle_line(&s, &mut ses, b"{oops")), Some(ErrorCode::BadRequest));
    }

    #[test]
    fn sync_pages_through_history() {
        let s = shared();
        let now = now_secs();
        let start = (now - 3 * 3600).div_euclid(3600) * 3600;
        {
            let mut st = s.store.lock().unwrap();
            for i in 0..120 {
                st.insert_minute(&Point { ts: (start + i * 60) as f64, cpu: Some(i as f32), ..Default::default() })
                    .unwrap();
            }
        }
        let mut ses = Session::default();
        call(&s, &mut ses, 1, "hello", json!({"protoMin": 1, "protoMax": 1}));
        let mut after = 0;
        let mut got = Vec::new();
        loop {
            let r = call(&s, &mut ses, 2, "metrics.sync", json!({"res": MINUTE, "after": after, "limit": 50}));
            let page: SyncResult = serde_json::from_value(r.result.unwrap()).unwrap();
            assert_eq!(page.oldest, Some(start));
            got.extend(page.points.iter().map(|p| p.ts as i64));
            after = page.next;
            if !page.more {
                break;
            }
        }
        assert_eq!(got, (0..120).map(|i| start + i * 60).collect::<Vec<_>>());
        // Rollups: 8 closed 15-minute buckets.
        let r = call(&s, &mut ses, 3, "metrics.sync", json!({"res": ROLLUP, "after": 0}));
        let page: SyncResult = serde_json::from_value(r.result.unwrap()).unwrap();
        assert_eq!(page.points.len(), 8);
        assert_eq!(page.points[0].n, 15);
        // History picks the tier by range.
        let r = call(&s, &mut ses, 4, "metrics.history", json!({"from": start, "to": start + 3600}));
        let h: HistoryResult = serde_json::from_value(r.result.unwrap()).unwrap();
        assert_eq!((h.res, h.points.len(), h.truncated), (MINUTE, 61, false));
        let r = call(&s, &mut ses, 5, "metrics.sync", json!({"res": 61, "after": 0}));
        assert_eq!(code(&r), Some(ErrorCode::BadRequest));
    }

    #[test]
    fn events_uptime_disk_and_rules() {
        use crate::events::NewEvent;
        use nightgrid_node_proto::{EventKind, Level};
        let s = shared();
        {
            let st = s.store.lock().unwrap();
            for i in 0..5 {
                st.event_add(&NewEvent::new(100 + i, EventKind::Boot, Level::Info, "Booted", "")).unwrap();
            }
            let m = |mount: &str| nightgrid_node_proto::MountUsage {
                device: "d".into(),
                mount: mount.into(),
                fstype: "ext4".into(),
                total: 10,
                used: 5,
                avail: 5,
            };
            for h in 0..3 {
                st.disk_usage_record(&[m("/"), m("/srv")], 3600 * (10 + h)).unwrap();
            }
        }
        let mut ses = Session::default();
        call(&s, &mut ses, 1, "hello", json!({"protoMin": 1, "protoMax": 1}));
        let r: EventsResult = serde_json::from_value(
            call(&s, &mut ses, 2, "events.sync", json!({"after": 0, "limit": 3})).result.unwrap(),
        )
        .unwrap();
        assert_eq!((r.events.len(), r.next, r.more, r.oldest), (3, 3, true, Some(1)));
        let r: EventsResult =
            serde_json::from_value(call(&s, &mut ses, 3, "events.sync", json!({"after": 3})).result.unwrap()).unwrap();
        assert_eq!((r.events.len(), r.next, r.more), (2, 5, false));

        // Disk pages never split an hour: limit 3 gives one whole hour (2 rows).
        let r: DiskResult =
            serde_json::from_value(call(&s, &mut ses, 4, "disk.sync", json!({"after": 0, "limit": 3})).result.unwrap())
                .unwrap();
        assert_eq!((r.rows.len(), r.next, r.more), (2, 36000, true));
        let r: DiskResult =
            serde_json::from_value(call(&s, &mut ses, 5, "disk.sync", json!({"after": 0, "limit": 1})).result.unwrap())
                .unwrap();
        assert_eq!((r.rows.len(), r.next), (2, 36000));

        let r = call(&s, &mut ses, 6, "uptime.sync", json!({"after": 0}));
        assert!(r.error.is_none());

        let r = call(&s, &mut ses, 7, "alerts.rules.set", json!({"diskPercent": 20}));
        assert_eq!(code(&r), Some(ErrorCode::BadRequest));
        let r = call(&s, &mut ses, 8, "alerts.rules.set", json!({"diskPercent": 80, "webhook": "ntfy"}));
        assert!(r.error.is_none());
        let r = call(&s, &mut ses, 9, "alerts.rules.get", Value::Null);
        assert_eq!(r.result.unwrap()["diskPercent"], 80);
        assert_eq!(crate::engine::load_rules(&s.store.lock().unwrap()).disk_percent, 80);
        assert_eq!(call(&s, &mut ses, 10, "alerts.active", Value::Null).result, Some(json!([])));
        let r = call(&s, &mut ses, 11, "containers.list", Value::Null).result.unwrap();
        assert_eq!((r["access"].as_str(), r["containers"].as_array().map(Vec::len)), (Some("absent"), Some(0)));
    }

    #[test]
    fn latest_is_null_before_the_first_sample() {
        let s = shared();
        let mut ses = Session::default();
        call(&s, &mut ses, 1, "hello", json!({"protoMin": 1, "protoMax": 1}));
        let r = call(&s, &mut ses, 2, "metrics.latest", Value::Null);
        assert_eq!(r.result, Some(Value::Null));
    }
}
