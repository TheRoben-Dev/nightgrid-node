//! The unix socket server: NDJSON requests in, one response each.
//!
//! Access control is the socket's file mode: `0660`, owner and group
//! `nightgrid-node`, inside a `0750` runtime directory. The app reaches it as
//! an SSH user who is a member of that group (through `nightgrid-node bridge`
//! or SSH streamlocal forwarding). Nothing listens on the network. Every
//! connection's peer uid is logged.

use std::collections::BTreeSet;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use nightgrid_node_proto::{
    cap, method, negotiate, resolution, stream, ActionRequest, AlertRules, Caps, ChangeBegin, ChangeId,
    ContainersResult, CursorParams, DiskResult, ErrorCode, Event, EventsResult, Frame, HealRules, Hello, HistoryParams,
    HistoryResult, Lagged, NodeInfo, Request, Response, Retention, Sample, StreamsParams, StreamsResult, SyncParams,
    SyncResult, UptimeResult, Welcome, MAX_LINE, MAX_POINTS, MAX_ROWS, PROTO_MIN, PROTO_VERSION,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Semaphore};

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

/// Receivers for the streams a connection subscribed to.
#[derive(Default)]
struct Subs {
    events: Option<broadcast::Receiver<Event>>,
    metrics: Option<broadcast::Receiver<Sample>>,
    containers: Option<broadcast::Receiver<ContainersResult>>,
}

impl Subs {
    /// Opens or drops receivers to match what the session asked for.
    fn sync(&mut self, shared: &Shared, want: &BTreeSet<String>) {
        fn one<T: Clone>(slot: &mut Option<broadcast::Receiver<T>>, on: bool, tx: &broadcast::Sender<T>) {
            match (slot.is_some(), on) {
                (false, true) => *slot = Some(tx.subscribe()),
                (true, false) => *slot = None,
                _ => {}
            }
        }
        one(&mut self.events, want.contains(stream::EVENTS), &shared.bus.events);
        one(&mut self.metrics, want.contains(stream::METRICS), &shared.bus.metrics);
        one(&mut self.containers, want.contains(stream::CONTAINERS), &shared.bus.containers);
    }
}

/// The next item of a subscribed stream; pending forever when not subscribed.
async fn next<T: Clone>(rx: &mut Option<broadcast::Receiver<T>>) -> Result<T, broadcast::error::RecvError> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn frame(stream: &str, data: impl Serialize) -> Frame {
    Frame { stream: stream.to_owned(), data: serde_json::to_value(data).unwrap_or(Value::Null) }
}

/// What to push for one stream's receive result, if anything.
fn pushed<T: Serialize>(name: &str, r: Result<T, broadcast::error::RecvError>) -> Option<Frame> {
    match r {
        Ok(v) => Some(frame(name, v)),
        Err(broadcast::error::RecvError::Lagged(missed)) => {
            Some(frame(stream::LAGGED, Lagged { stream: name.to_owned(), missed }))
        }
        // The node is stopping.
        Err(broadcast::error::RecvError::Closed) => None,
    }
}

async fn connection(stream: UnixStream, shared: &Arc<Shared>, uid: String) -> std::io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::with_capacity(16 * 1024, r);
    let mut session = Session { uid, ..Default::default() };
    let mut subs = Subs::default();
    let mut buf = Vec::new();
    loop {
        // `read_until` keeps what it read in `buf` if another branch wins,
        // so a line split across a push is not lost.
        let room = (MAX_LINE + 1).saturating_sub(buf.len()) as u64;
        let mut line = (&mut r).take(room);
        let out = tokio::select! {
            n = line.read_until(b'\n', &mut buf) => {
                let eof = n? == 0;
                if eof && buf.is_empty() {
                    return Ok(());
                }
                if buf.len() > MAX_LINE {
                    send(&mut w, &Response::err(0, ErrorCode::TooLarge, "line too long")).await?;
                    return Ok(());
                }
                drop(line);
                let resp = if slow(&buf) {
                    // Off the (single) async thread, so other clients and
                    // this connection's pushes don't wait on the engine.
                    let (sh, mut ses, line) = (shared.clone(), session.clone(), buf.clone());
                    let (resp, ses) = tokio::task::spawn_blocking(move || {
                        let r = handle_line(&sh, &mut ses, &line);
                        (r, ses)
                    })
                    .await
                    .map_err(std::io::Error::other)?;
                    session = ses;
                    resp
                } else {
                    handle_line(shared, &mut session, &buf)
                };
                buf.clear();
                subs.sync(shared, &session.streams);
                send(&mut w, &resp).await?;
                if eof {
                    return Ok(());
                }
                continue;
            }
            e = next(&mut subs.events) => pushed(stream::EVENTS, e),
            m = next(&mut subs.metrics) => pushed(stream::METRICS, m),
            c = next(&mut subs.containers) => pushed(stream::CONTAINERS, c),
        };
        if let Some(f) = out {
            send(&mut w, &f).await?;
        }
    }
}

async fn send(w: &mut tokio::net::unix::OwnedWriteHalf, r: &impl Serialize) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(r).unwrap_or_default();
    line.push(b'\n');
    w.write_all(&line).await
}

/// Requests that may block for seconds (they talk to the container engine).
fn slow(line: &[u8]) -> bool {
    #[derive(serde::Deserialize)]
    struct M<'a> {
        #[serde(borrow, default)]
        method: std::borrow::Cow<'a, str>,
    }
    serde_json::from_slice::<M>(line).is_ok_and(|m| {
        matches!(
            m.method.as_ref(),
            method::ACTIONS_RUN | method::CHANGES_BEGIN | method::CHANGES_CONFIRM | method::CHANGES_REVERT
        )
    })
}

/// Per-connection protocol state.
#[derive(Debug, Default, Clone)]
pub struct Session {
    /// The negotiated version, once `hello` succeeded.
    pub proto: Option<u32>,
    /// The peer's uid, for the log.
    pub uid: String,
    /// Streams subscribed to (protocol 2).
    pub streams: BTreeSet<String>,
}

/// Optional access root has or hasn't granted (`node.info`).
fn access(shared: &Shared) -> std::collections::BTreeMap<String, String> {
    let docker = if shared.config.docker.enabled {
        let a = shared.containers.lock().unwrap_or_else(|e| e.into_inner()).result.access;
        serde_json::to_value(a).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
    } else {
        "off".into()
    };
    let journal = if shared.config.logins.enabled { crate::logins::journal_access() } else { "off" };
    let netwatch = if !shared.config.network.enabled {
        "off"
    } else if shared.dhcp.lock().unwrap_or_else(|e| e.into_inner()).watching(crate::daemon::now_secs()) {
        "ok"
    } else {
        "denied"
    };
    [
        ("docker".to_owned(), docker),
        ("journal".to_owned(), journal.to_owned()),
        ("netwatch".to_owned(), netwatch.to_owned()),
        ("helper".to_owned(), if crate::helper::installed(&shared.helper_socket) { "ok" } else { "denied" }.to_owned()),
    ]
    .into_iter()
    .collect()
}

/// Takes a report from the network watch helper.
fn net_report(shared: &Shared, r: nightgrid_node_proto::NetReport) {
    use crate::netwatch::{Inbox, Neigh};
    let now = crate::daemon::now_secs();
    let mac_ok = |m: &str| m.len() == 17 && m.bytes().all(|b| b.is_ascii_hexdigit() || b == b':');
    {
        let mut inbox = shared.net_inbox.lock().unwrap_or_else(|e| e.into_inner());
        let arp = r.arp.iter().map(|a| (a.ip.as_str(), a.mac.as_str(), a.iface.as_str()));
        // A DHCP ack tells us a client's new address before it ARPs.
        let acks = r
            .dhcp
            .iter()
            .filter(|d| d.msg == "ack")
            .filter_map(|d| Some((d.ip.as_deref()?, d.client_mac.as_str(), d.iface.as_str())));
        for (ip, mac, iface) in arp.chain(acks) {
            if inbox.arp.len() < Inbox::MAX && ip.parse::<std::net::Ipv4Addr>().is_ok() && mac_ok(mac) {
                inbox.arp.push(Neigh {
                    ip: ip.to_owned(),
                    mac: mac.to_ascii_lowercase(),
                    iface: iface.chars().take(16).collect(),
                });
            }
        }
        for d in &r.dhcp {
            if let Some(h) = d.hostname.as_deref() {
                let h: String =
                    h.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')).take(63).collect();
                if !h.is_empty() && mac_ok(&d.client_mac) && inbox.names.len() < Inbox::MAX {
                    inbox.names.push((d.client_mac.to_ascii_lowercase(), h));
                }
            }
        }
    }
    let mut dhcp = shared.dhcp.lock().unwrap_or_else(|e| e.into_inner());
    let events = dhcp.observe(now, &r.dhcp);
    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    for e in &events {
        if let Err(err) = shared.bus.record(&st, e) {
            crate::warn!("could not record a DHCP event: {err}");
        }
    }
    if let Err(e) = dhcp.save(&st) {
        crate::warn!("could not save DHCP servers: {e}");
    }
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
        (cap::LOGINS, 1),
        (cap::FILES, 1),
        (cap::NET, 1),
        (cap::NET_DHCP, 1),
        (cap::ACTIONS, 2),
        (cap::HEAL, 1),
        (cap::CHANGES, 1),
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
                    access: access(shared),
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
        method::SUBSCRIBE | method::UNSUBSCRIBE => {
            if session.proto < Some(2) {
                return Err(Response::err(0, ErrorCode::UnknownMethod, "streams need protocol 2"));
            }
            let p: StreamsParams = params(&req.params)?;
            for name in &p.streams {
                if !stream::ALL.contains(&name.as_str()) {
                    return Err(Response::err(0, ErrorCode::BadRequest, format!("no stream {name:?}")));
                }
            }
            for name in p.streams {
                if req.method == method::SUBSCRIBE {
                    session.streams.insert(name);
                } else {
                    session.streams.remove(&name);
                }
            }
            Ok(Response::ok(0, StreamsResult { streams: session.streams.iter().cloned().collect() }))
        }
        method::CONTAINERS_LIST => {
            let r = shared.containers.lock().unwrap_or_else(|e| e.into_inner()).result.clone();
            Ok(Response::ok(0, r))
        }
        method::NET_DHCP => {
            let r = shared.dhcp.lock().unwrap_or_else(|e| e.into_inner()).result(crate::daemon::now_secs());
            Ok(Response::ok(0, r))
        }
        method::NET_REPORT => {
            // Only the helper, which runs as the service's own account.
            // SAFETY: geteuid has no preconditions.
            let me = unsafe { libc::geteuid() };
            if session.uid.parse::<u32>().ok() != Some(me) {
                return Err(Response::err(0, ErrorCode::Forbidden, "only the network watch helper may report"));
            }
            let r: nightgrid_node_proto::NetReport = params(&req.params)?;
            net_report(shared, r);
            Ok(Response::ok(0, Value::Null))
        }
        method::NET_DEVICES => {
            let r = shared.net.lock().unwrap_or_else(|e| e.into_inner()).clone();
            Ok(Response::ok(0, r))
        }
        method::ACTIONS_POLICY => Ok(Response::ok(0, crate::actions::policy(&shared.config))),
        method::ACTIONS_RUN => {
            let r: ActionRequest = params(&req.params)?;
            let source = format!("app (uid {})", session.uid);
            Ok(Response::ok(0, crate::actions::run(shared, &r, &source)))
        }
        method::ACTIONS_LOG => {
            let p: CursorParams = params(&req.params)?;
            let limit = p.limit.unwrap_or(crate::actions::MAX_LOG).clamp(1, crate::actions::MAX_LOG);
            let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
            Ok(Response::ok(0, crate::actions::log(&st, p.after, limit).map_err(internal)?))
        }
        method::HEAL_RULES_GET => {
            let r = shared.heal.lock().unwrap_or_else(|e| e.into_inner()).rules();
            Ok(Response::ok(0, r))
        }
        method::HEAL_RULES_SET => {
            let rules: HealRules = params(&req.params)?;
            rules.validate().map_err(|e| Response::err(0, ErrorCode::BadRequest, e))?;
            let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
            st.set_meta(crate::heal::META_RULES, &serde_json::to_string(&rules).unwrap_or_default())
                .map_err(internal)?;
            let on = rules.rules.iter().filter(|r| r.enabled).count();
            crate::info!("self-healing rules updated by uid {} ({} rules, {on} on)", session.uid, rules.rules.len());
            shared.heal.lock().unwrap_or_else(|e| e.into_inner()).set_rules(rules.clone());
            Ok(Response::ok(0, rules))
        }
        method::CHANGES_BEGIN => {
            let p: ChangeBegin = params(&req.params)?;
            let source = format!("app (uid {})", session.uid);
            crate::rollback::begin(shared, &p, &source)
                .map(|c| Response::ok(0, c))
                .map_err(|e| Response::err(0, ErrorCode::BadRequest, e))
        }
        method::CHANGES_CONFIRM | method::CHANGES_REVERT => {
            let p: ChangeId = params(&req.params)?;
            let source = format!("app (uid {})", session.uid);
            let r = if req.method == method::CHANGES_CONFIRM {
                crate::rollback::confirm(shared, &p.id, &source)
            } else {
                crate::rollback::revert(shared, &p.id, &source)
            };
            r.map(|c| Response::ok(0, c)).map_err(|e| Response::err(0, ErrorCode::BadRequest, e))
        }
        method::CHANGES_LIST => {
            let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
            Ok(Response::ok(0, crate::rollback::list(&st).map_err(internal)?))
        }
        method::HEAL_STATUS => {
            let r = shared.heal.lock().unwrap_or_else(|e| e.into_inner()).status();
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
