//! `nightgrid-hub collect`: pulls each member over its node-to-node link,
//! read-only, and hands every page to the hub's main unit as it arrives.
//!
//! The collector is a normal link peer to the members: its own Ed25519 key,
//! its certificate pinned by each member, mutual TLS 1.3. It asks only
//! `peer.ping` and `peer.sync` (which a member serves only to the peer its
//! `hub.member` names). It also answers the members' pings, so their
//! "linked hosts" show the hub. It may reach only `[collect] networks`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use nightgrid_node::peers::{client_config, identity, server_config, Answer, Identity, Pong};
use nightgrid_node_proto::hub::{method as hm, Ingest, IngestBatch, Member, Plan, PlanMember};
use nightgrid_node_proto::method;
use rustls::pki_types::ServerName;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::config::Config;

/// Longest answer the collector reads (a node caps `peer.sync` at 1 MiB).
const MAX_ANSWER: usize = 2 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);
/// Members pulled at once.
const AT_ONCE: usize = 4;

/// Pages per stream per round, so a long backlog spreads over a few rounds
/// and memory stays flat (each page is handed over before the next).
pub struct Pages {
    pub minutes: usize,
    pub other: usize,
}

const PAGES: Pages = Pages { minutes: 20, other: 5 };
const MINUTE_LIMIT: usize = 300;
const ROW_LIMIT: usize = 500;

fn state_dir() -> PathBuf {
    std::env::var_os("STATE_DIRECTORY")
        .map(|s| PathBuf::from(s.to_string_lossy().split(':').next().unwrap_or_default().to_owned()))
        .unwrap_or_else(|| PathBuf::from("/var/lib/nightgrid-hub-collect"))
}

async fn read_line<R: tokio::io::AsyncRead + Unpin>(r: R, max: usize) -> Result<String, String> {
    let mut buf = Vec::new();
    let mut r = BufReader::new(r).take(max as u64 + 1);
    let n = r.read_until(b'\n', &mut buf).await.map_err(|e| e.to_string())?;
    if n == 0 || n > max {
        return Err("no answer or an oversized line".into());
    }
    String::from_utf8(buf).map_err(|_| "not UTF-8".into())
}

fn describe(s: String) -> String {
    if s.contains("ApplicationVerificationFailure") || s.contains("certificate") {
        "the member's certificate isn't the paired one (add it to the hub again in the app)".into()
    } else if s.contains("eof") || s.contains("reset") || s.contains("AccessDenied") || s.contains("BadCertificate") {
        "the member refused this node (pair it with the hub on the member first)".into()
    } else {
        s
    }
}

/// One request to a member over the link; its `result`, or why not.
pub async fn request(id: &Identity, m: &Member, req: &Value) -> Result<Value, String> {
    let addr: SocketAddr = m.address.parse().map_err(|_| "bad address".to_string())?;
    let fut = async {
        let tcp = tokio::net::TcpStream::connect(addr).await.map_err(|e| match e.kind() {
            std::io::ErrorKind::ConnectionRefused => "connection refused (is the link on there?)".to_string(),
            std::io::ErrorKind::PermissionDenied => "not allowed by [collect] networks".to_string(),
            _ => e.to_string(),
        })?;
        let conn = tokio_rustls::TlsConnector::from(Arc::new(client_config(id, &m.fingerprint)?));
        let name = ServerName::try_from("nightgrid-node").map_err(|e| e.to_string())?;
        let mut tls = conn.connect(name, tcp).await.map_err(|e| describe(e.to_string()))?;
        let mut line = req.to_string();
        line.push('\n');
        tls.write_all(line.as_bytes()).await.map_err(|e| describe(e.to_string()))?;
        let line = read_line(&mut tls, MAX_ANSWER).await.map_err(describe)?;
        let v: Value = serde_json::from_str(&line).map_err(|_| "a garbled answer".to_string())?;
        if v["ok"].as_bool() != Some(true) {
            return Err(v["error"].as_str().unwrap_or("refused").to_owned());
        }
        Ok(v["result"].clone())
    };
    tokio::time::timeout(TIMEOUT, fut).await.map_err(|_| "timed out".to_string())?
}

/// Where answers go: the hub's socket in the real unit, a vector in tests.
pub trait Sink: Send + Sync {
    fn send(&self, batch: IngestBatch) -> Result<(), String>;
}

pub struct SocketSink(pub PathBuf);

impl Sink for SocketSink {
    fn send(&self, batch: IngestBatch) -> Result<(), String> {
        let mut c = nightgrid_node::client::Client::connect(&self.0, "nightgrid-hub collect")
            .map_err(|e| format!("{}: {e}", self.0.display()))?;
        c.call(hm::INGEST, serde_json::to_value(&batch).unwrap_or_default()).map(|_| ()).map_err(|e| e.to_string())
    }
}

/// What the collector says about itself with each batch.
#[derive(Clone, Default)]
pub struct Me {
    pub fingerprint: String,
    pub listening: Option<String>,
    /// Every address it answers on, `listening` first.
    pub listen_all: Vec<String>,
}

async fn hand_over(sink: &Arc<dyn Sink>, me: &Me, answers: Vec<Ingest>) {
    let sink = sink.clone();
    let batch = IngestBatch {
        fingerprint: me.fingerprint.clone(),
        listening: me.listening.clone(),
        listen_all: me.listen_all.clone(),
        answers,
    };
    match tokio::task::spawn_blocking(move || sink.send(batch)).await {
        Ok(Err(e)) => nightgrid_node::warn!("can't hand answers to the hub: {e}"),
        Err(e) => nightgrid_node::warn!("can't hand answers to the hub: {e}"),
        Ok(Ok(())) => {}
    }
}

fn version_ok(v: &str) -> bool {
    let mut it = v.split(['.', '-']).map(|x| x.parse::<u32>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0)) >= crate::fleet::MIN_NODE
}

/// Pulls one member for one round. `round` picks the slower streams.
pub async fn pull(id: &Identity, pm: &PlanMember, round: u64, sink: &Arc<dyn Sink>, me: &Me) {
    let m = &pm.member;
    let now = crate::now_secs;
    let one = |method: &str, params: Value, r: Result<Value, String>| {
        let (result, error) = match r {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        Ingest { id: m.id.clone(), at: now(), method: method.into(), params, result, error }
    };
    let pong = request(id, m, &json!({"method": "peer.ping"})).await.and_then(|v| {
        let p: Pong = serde_json::from_value(v.clone()).map_err(|_| "a garbled answer".to_string())?;
        if p.id != m.id {
            return Err(format!("answered as node {}, not {}", p.id, m.id));
        }
        Ok(v)
    });
    let ok = pong.as_ref().ok().and_then(|v| v["version"].as_str()).is_some_and(version_ok);
    hand_over(sink, me, vec![one("peer.ping", Value::Null, pong)]).await;
    if !ok {
        return;
    }
    let cur = |k: &str| pm.cursors.get(k).copied().unwrap_or(0);
    let mut streams: Vec<(&str, Value, usize, Option<&str>)> = vec![
        (method::ALERTS_ACTIVE, Value::Null, 1, None),
        (method::EVENTS_SYNC, json!({"after": cur("events"), "limit": ROW_LIMIT}), PAGES.other, Some("events")),
        (
            method::METRICS_SYNC,
            json!({"res": 60, "after": cur("metrics.60"), "limit": MINUTE_LIMIT}),
            PAGES.minutes,
            Some("m"),
        ),
        (
            method::METRICS_SYNC,
            json!({"res": 900, "after": cur("metrics.900"), "limit": ROW_LIMIT}),
            PAGES.other,
            Some("m"),
        ),
    ];
    // node.info is small but changes rarely; history every 5 rounds.
    if round.is_multiple_of(60) || !pm.cursors.contains_key("events") {
        streams.insert(0, (method::NODE_INFO, Value::Null, 1, None));
    }
    if round.is_multiple_of(5) {
        streams.push((
            method::METRICS_SYNC,
            json!({"res": 3600, "after": cur("metrics.3600"), "limit": ROW_LIMIT}),
            PAGES.other,
            Some("m"),
        ));
        streams.push((
            method::UPTIME_SYNC,
            json!({"after": cur("uptime"), "limit": ROW_LIMIT}),
            PAGES.other,
            Some("uptime"),
        ));
        streams.push((method::DISK_SYNC, json!({"after": cur("disk"), "limit": ROW_LIMIT}), PAGES.other, Some("disk")));
    }
    for (meth, mut params, pages, paged) in streams {
        for _ in 0..pages {
            let mut r =
                request(id, m, &json!({"method": "peer.sync", "params": {"method": meth, "params": params}})).await;
            if matches!(&r, Err(e) if e.contains("too large")) {
                if let Some(l) = params["limit"].as_u64() {
                    params["limit"] = (l / 4).max(1).into();
                    r = request(id, m, &json!({"method": "peer.sync", "params": {"method": meth, "params": params}}))
                        .await;
                }
            }
            let next = r.as_ref().ok().and_then(|v| v["next"].as_i64());
            let more = r.as_ref().ok().and_then(|v| v["more"].as_bool()).unwrap_or(false);
            let failed = r.is_err();
            let refused = r.as_ref().err().is_some_and(|e| crate::fleet::refused(e));
            hand_over(sink, me, vec![one(meth, params.clone(), r)]).await;
            if refused {
                // It won't serve this hub; nothing else will be answered either.
                return;
            }
            if failed || paged.is_none() || !more {
                break;
            }
            match next {
                Some(n) => params["after"] = n.into(),
                None => break,
            }
        }
    }
}

/// One round over every member, a few at a time.
pub async fn round(id: &Arc<Identity>, plan: &Plan, n: u64, sink: &Arc<dyn Sink>, me: &Me) {
    let slots = Arc::new(tokio::sync::Semaphore::new(AT_ONCE));
    let mut set = tokio::task::JoinSet::new();
    for pm in &plan.members {
        let (pm, id, sink, me, slots) = (pm.clone(), id.clone(), sink.clone(), me.clone(), slots.clone());
        set.spawn(async move {
            let _permit = slots.acquire_owned().await;
            pull(&id, &pm, n, &sink, &me).await;
        });
    }
    while set.join_next().await.is_some() {}
    // An empty batch keeps the collector's own state fresh with no members.
    if plan.members.is_empty() {
        hand_over(sink, me, Vec::new()).await;
    }
}

/// Answers members' pings: `peer.ping` only, to paired certificates only.
async fn answer_pings(l: tokio::net::TcpListener, acceptor: tokio_rustls::TlsAcceptor, me: Arc<RwLock<Pong>>) {
    let slots = Arc::new(tokio::sync::Semaphore::new(16));
    loop {
        let Ok((tcp, _)) = l.accept().await else { continue };
        let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
        let (acceptor, me) = (acceptor.clone(), me.clone());
        tokio::spawn(async move {
            let fut = async {
                let mut tls = acceptor.accept(tcp).await.map_err(|e| e.to_string())?;
                let line = read_line(&mut tls, 4096).await?;
                let pong = Pong { uptime: uptime(), ..me.read().map(|p| p.clone()).unwrap_or_default() };
                let v = match nightgrid_node::peers::answer(&line, &["ping".to_owned()], &pong) {
                    Answer::Now(v) => v,
                    Answer::Node(..) => json!({"ok": false, "error": "a hub serves only ping"}),
                };
                let mut out = v.to_string();
                out.push('\n');
                tls.write_all(out.as_bytes()).await.map_err(|e| e.to_string())?;
                tls.shutdown().await.map_err(|e| e.to_string())
            };
            if let Ok(Err(e)) = tokio::time::timeout(Duration::from_secs(10), fut).await {
                nightgrid_node::debug!("ping from a member: {e}");
            }
            drop(permit);
        });
    }
}

fn uptime() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split('.').next().and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

fn fetch_plan(socket: &Path) -> Result<Plan, String> {
    let mut c = nightgrid_node::client::Client::connect(socket, "nightgrid-hub collect")
        .map_err(|e| format!("{}: {e}", socket.display()))?;
    let v = c.call(hm::PLAN, Value::Null).map_err(|e| e.to_string())?;
    serde_json::from_value(v).map_err(|e| e.to_string())
}

/// The collector unit's loop.
pub fn run(config_path: &Path) -> Result<(), String> {
    let cfg = Config::load(config_path).map_err(|e| format!("config: {e}"))?;
    let id = Arc::new(identity(&state_dir())?);
    nightgrid_node::info!("hub collector, fingerprint {}", id.fingerprint);
    let allowed: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(Vec::new()));
    let pong = Arc::new(RwLock::new(Pong { version: env!("CARGO_PKG_VERSION").into(), ..Default::default() }));
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config(&id, allowed.clone())?));
    let socket = cfg.server.socket.clone();
    let sink: Arc<dyn Sink> = Arc::new(SocketSink(socket.clone()));
    let interval = Duration::from_secs(cfg.collect.interval_secs.into());
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let mut me = Me { fingerprint: id.fingerprint.clone(), ..Default::default() };
        // One accept task per address; `tailnet` and `lan` entries are looked
        // up again each round, so a new DHCP lease moves the listener.
        let mut bound: Vec<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> = Vec::new();
        let mut n: u64 = 0;
        loop {
            let started = Instant::now();
            let want = cfg.listen_addrs();
            bound.retain(|(a, t)| {
                let keep = want.contains(a);
                if !keep {
                    nightgrid_node::info!("no longer answering pings on {a}");
                    t.abort();
                }
                keep
            });
            for addr in &want {
                if bound.iter().any(|(a, _)| a == addr) {
                    continue;
                }
                match tokio::net::TcpListener::bind(*addr).await {
                    Ok(l) => {
                        nightgrid_node::info!("answering members' pings on {addr}");
                        bound.push((*addr, tokio::spawn(answer_pings(l, acceptor.clone(), pong.clone()))));
                    }
                    Err(e) => nightgrid_node::warn!("can't listen on {addr}: {e}"),
                }
            }
            // In the configured order, so `listening` is the first entry's.
            me.listen_all = want.iter().filter(|a| bound.iter().any(|(b, _)| b == *a)).map(|a| a.to_string()).collect();
            me.listening = me.listen_all.first().cloned();
            let s = socket.clone();
            match tokio::task::spawn_blocking(move || fetch_plan(&s)).await.map_err(|e| e.to_string()).and_then(|r| r) {
                Ok(plan) => {
                    if let Ok(mut a) = allowed.write() {
                        *a = plan.members.iter().map(|m| m.member.fingerprint.clone()).collect();
                    }
                    if let Ok(mut p) = pong.write() {
                        p.id = plan.hub_id.clone();
                        p.hostname = plan.hostname.clone();
                    }
                    round(&id, &plan, n, &sink, &me).await;
                    n += 1;
                }
                Err(e) => nightgrid_node::warn!("can't read the hub's plan: {e}"),
            }
            let wait = interval.saturating_sub(started.elapsed()).max(Duration::from_secs(5));
            // Addresses that follow the host are looked for again sooner.
            let wait = if cfg.collect.listen.follows() || me.listen_all.len() < cfg.collect.listen.entries().len() {
                wait.min(Duration::from_secs(30))
            } else {
                wait
            };
            tokio::time::sleep(wait).await;
        }
    })
}
