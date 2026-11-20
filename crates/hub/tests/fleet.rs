//! The collector against simulated members over real mutual TLS: 25 nodes
//! with two days of minute history each, pulled in one round, served back
//! in the nodes' own shapes, inside the hub's memory budget (N-053).

use std::sync::{Arc, RwLock};

use nightgrid_hub::collect::{self, Me, Sink};
use nightgrid_hub::config::Config;
use nightgrid_hub::fleet::{self, Hub};
use nightgrid_hub::store::Store;
use nightgrid_node::peers::{identity, server_config, Identity};
use nightgrid_node_proto::hub::{IngestBatch, Member, MemberState, Plan, SyncParams};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const NOW: i64 = 1_763_000_000 / 60 * 60;
const MINUTES: i64 = 2 * 1440;
const EVENTS: i64 = 50;

struct HubSink(Arc<Hub>);

impl Sink for HubSink {
    fn send(&self, b: IngestBatch) -> Result<(), String> {
        fleet::ingest(&self.0, b, NOW)
    }
}

/// What a node answers to one `peer.sync` read, from synthetic history.
fn answer(method: &str, p: &Value) -> Value {
    let after = p["after"].as_i64().unwrap_or(0);
    let limit = p["limit"].as_u64().unwrap_or(5000) as i64;
    match method {
        "node.info" => json!({"node": {"nodeId": "x", "version": "0.26.0", "hostname": "sim", "arch": "x86_64"}}),
        "alerts.active" => json!([]),
        "metrics.sync" => {
            let res = p["res"].as_i64().unwrap();
            let first = (NOW - MINUTES * 60) / res * res;
            let start = (after / res * res + res).max(first);
            let ts: Vec<i64> =
                (0..).map(|i| start + i * res).take_while(|t| t + res <= NOW).take(limit as usize + 1).collect();
            let more = ts.len() as i64 > limit;
            let pts: Vec<Value> = ts
                .iter()
                .take(limit as usize)
                .map(|t| json!({"ts": *t as f64, "cpu": 12.5, "mem": 40.0, "load1": 0.3, "netRx": 1234.5, "n": 6}))
                .collect();
            let next = pts.last().and_then(|x| x["ts"].as_f64()).map_or(after, |t| t as i64);
            json!({"res": res, "points": pts, "next": next, "more": more, "oldest": first})
        }
        "events.sync" => {
            let ev: Vec<Value> = ((after + 1)..=EVENTS)
                .take(limit as usize)
                .map(|s| json!({"seq": s, "at": NOW - 1000 + s, "kind": "other", "level": "info", "title": format!("event {s}"), "detail": ""}))
                .collect();
            let next = ev.last().and_then(|e| e["seq"].as_i64()).unwrap_or(after);
            json!({"events": ev, "next": next, "more": next < EVENTS, "oldest": 1})
        }
        "uptime.sync" => json!({"hours": [], "next": after, "more": false}),
        "disk.sync" => json!({"rows": [], "next": after, "more": false}),
        m => panic!("the hub asked for {m}"),
    }
}

/// A simulated member: answers ping and sync to the hub's certificate only.
async fn member(id: Identity, node_id: String, hub_fp: String, refuse: bool) -> std::net::SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let acceptor =
        tokio_rustls::TlsAcceptor::from(Arc::new(server_config(&id, Arc::new(RwLock::new(vec![hub_fp]))).unwrap()));
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = l.accept().await else { continue };
            let (acceptor, node_id) = (acceptor.clone(), node_id.clone());
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                let mut line = String::new();
                let mut r = BufReader::new(&mut tls);
                if r.read_line(&mut line).await.is_err() {
                    return;
                }
                let v: Value = serde_json::from_str(&line).unwrap();
                let out = match v["method"].as_str() {
                    Some("peer.ping") => {
                        json!({"ok": true, "result": {"id": node_id, "hostname": "sim", "version": "0.26.0", "uptime": 5}})
                    }
                    Some("peer.sync") if refuse => json!({"ok": false, "error": "not this node's hub (hub.member)"}),
                    Some("peer.sync") => {
                        json!({"ok": true, "result": answer(v["params"]["method"].as_str().unwrap(), &v["params"]["params"])})
                    }
                    _ => json!({"ok": false, "error": "no"}),
                };
                let mut s = out.to_string();
                s.push('\n');
                let _ = tls.write_all(s.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    addr
}

fn peak_rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("VmHWM:")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

#[test]
fn pulls_25_members_within_budget_and_serves_their_cursors() {
    let d = tempfile::tempdir().unwrap();
    let hub_id = Arc::new(identity(&d.path().join("hub")).unwrap());
    let hub = Arc::new(Hub::new(Config::default(), Store::open(&d.path().join("hub.db")).unwrap()));
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let before = peak_rss_kb();
    let mut members = Vec::new();
    rt.block_on(async {
        for i in 0..25 {
            let id = identity(&d.path().join(format!("m{i}"))).unwrap();
            let fp = id.fingerprint.clone();
            let node_id = format!("{i:032x}");
            let addr = member(id, node_id.clone(), hub_id.fingerprint.clone(), i == 24).await;
            members.push(Member { id: node_id, name: format!("host{i}"), address: addr.to_string(), fingerprint: fp });
        }
    });
    // One member is paired to a key the node doesn't have.
    members[23].fingerprint = "ab".repeat(32);
    hub.store().set_members(&members, NOW - 10).unwrap();
    let sink: Arc<dyn Sink> = Arc::new(HubSink(hub.clone()));
    let me = Me { fingerprint: hub_id.fingerprint.clone(), ..Default::default() };
    let started = std::time::Instant::now();
    rt.block_on(async {
        let plan: Plan = fleet::plan(&hub).unwrap();
        collect::round(&hub_id, &plan, 0, &sink, &me).await;
    });
    let took = started.elapsed();
    let grew_mb = peak_rss_kb().saturating_sub(before) / 1024;
    eprintln!(
        "25 members, {MINUTES} minutes each: {took:?}, peak RSS grew {grew_mb} MB, store {} MB",
        hub.store().size() / 1_000_000
    );
    assert!(grew_mb < 80, "over the hub's budget: {grew_mb} MB");

    let o = fleet::overview(&hub, NOW).unwrap();
    for m in &o.members[..23] {
        assert_eq!(m.state, MemberState::Ok, "{m:?}");
    }
    assert_eq!(o.members[23].state, MemberState::Unreachable);
    assert!(o.members[23].error.as_deref().unwrap().contains("certificate"), "{:?}", o.members[23].error);
    assert_eq!(o.members[24].state, MemberState::Consent);

    // Everything pulled, with the node's own cursors.
    let cur = fleet::plan(&hub).unwrap().members[0].cursors.clone();
    assert_eq!(cur["events"], EVENTS);
    assert_eq!(cur["metrics.60"], NOW - 60);
    // What the app gets through the hub is what the node answers directly.
    let p = json!({"res": 60, "after": NOW - 3600, "limit": 10});
    let via_hub = fleet::sync(
        &hub,
        &SyncParams { node_id: members[0].id.clone(), method: "metrics.sync".into(), params: p.clone() },
    )
    .unwrap()
    .to_value();
    let direct = answer("metrics.sync", &p);
    assert_eq!(via_hub["points"], direct["points"]);
    assert_eq!((via_hub["next"].clone(), via_hub["more"].clone()), (direct["next"].clone(), direct["more"].clone()));
    let e = json!({"after": 40, "limit": 5});
    let via_hub = fleet::sync(
        &hub,
        &SyncParams { node_id: members[0].id.clone(), method: "events.sync".into(), params: e.clone() },
    )
    .unwrap()
    .to_value();
    assert_eq!(via_hub["events"], answer("events.sync", &e)["events"]);

    // A second round asks only for what's new.
    let before_rows = hub.store().size();
    rt.block_on(async {
        let plan: Plan = fleet::plan(&hub).unwrap();
        collect::round(&hub_id, &plan, 1, &sink, &me).await;
    });
    assert!(hub.store().size() <= before_rows + 64 * 1024, "nothing new to store");
}
