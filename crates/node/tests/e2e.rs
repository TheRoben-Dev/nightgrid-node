//! Runs the real binary as a service on a temporary socket and talks to it
//! the way the app does: directly, and through `nightgrid-node bridge`.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nightgrid_node::client::{CallError, Client};
use nightgrid_node_proto::{ErrorCode, NodeInfo, Response, Sample};
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_nightgrid-node");

struct Service {
    child: Child,
    _dir: tempfile::TempDir,
    socket: std::path::PathBuf,
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start() -> Service {
    start_with("[metrics]\ninterval_secs = 2\n")
}

fn start_with(config_text: &str) -> Service {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("run/node.sock");
    let config = dir.path().join("config.toml");
    std::fs::write(&config, config_text).unwrap();
    let child = Command::new(BIN)
        .args(["run", "--config"])
        .arg(&config)
        .arg("--socket")
        .arg(&socket)
        .arg("--data-dir")
        .arg(dir.path().join("data"))
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&socket);
    Service { child, _dir: dir, socket }
}

fn wait_for(socket: &Path) {
    let t = Instant::now();
    while Client::connect(socket, "e2e").is_err() {
        assert!(t.elapsed() < Duration::from_secs(10), "service didn't come up");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn serves_info_latest_and_history() {
    let svc = start();
    let mut c = Client::connect(&svc.socket, "e2e").unwrap();
    assert_eq!(c.welcome.proto, 2);
    assert!(c.welcome.caps.contains_key("metrics.sync"));

    let info: NodeInfo = serde_json::from_value(c.call("node.info", Value::Null).unwrap()).unwrap();
    assert_eq!(info.interval_secs, 2);
    assert_eq!(info.node.node_id, c.welcome.node.node_id);
    assert!(info.rss_bytes > 0);

    let t = Instant::now();
    let latest: Sample = loop {
        let v = c.call("metrics.latest", Value::Null).unwrap();
        if !v.is_null() {
            break serde_json::from_value(v).unwrap();
        }
        assert!(t.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(latest.mem_total > 0 && latest.kernel.starts_with("Linux"));

    let now = (latest.ts / 1000.0) as i64;
    let h = c.call("metrics.history", json!({"from": now - 3600, "to": now})).unwrap();
    assert_eq!(h["res"], 60);
    let s = c.call("metrics.sync", json!({"res": 60, "after": 0})).unwrap();
    assert_eq!(s["more"], false);

    match c.call("metrics.sync", json!({"res": 7})) {
        Err(CallError::Rpc(e)) => assert_eq!(e.code, ErrorCode::BadRequest),
        other => panic!("{other:?}"),
    }

    // N1: events, uptime, disk usage and alert rules.
    for cap in ["events", "uptime", "disk", "alerts"] {
        assert!(c.welcome.caps.contains_key(cap), "{cap}");
    }
    let ev = c.call("events.sync", json!({"after": 0})).unwrap();
    assert!(ev["events"].is_array() && ev["more"] == false);
    assert!(c.call("uptime.sync", json!({"after": 0})).unwrap()["hours"].is_array());
    assert!(c.call("disk.sync", json!({"after": 0})).unwrap()["rows"].is_array());
    assert!(c.call("alerts.active", Value::Null).unwrap().is_array());
    let mut rules = c.call("alerts.rules.get", Value::Null).unwrap();
    assert_eq!(rules["diskPercent"], 90);
    rules["diskPercent"] = json!(85);
    c.call("alerts.rules.set", rules).unwrap();
    assert_eq!(c.call("alerts.rules.get", Value::Null).unwrap()["diskPercent"], 85);
    // N3a: the LAN inventory answers (empty or not, depending on the machine).
    assert!(c.welcome.caps.contains_key("net"));
    let net = c.call("net.devices", Value::Null).unwrap();
    assert!(net["devices"].is_array() && net["at"].is_i64(), "{net}");
    match c.call("alerts.rules.set", json!({"enabled": true, "diskPercent": 5})) {
        Err(CallError::Rpc(e)) => assert_eq!(e.code, ErrorCode::BadRequest),
        other => panic!("{other:?}"),
    }
}

#[test]
fn bridge_relays_the_protocol() {
    let svc = start();
    let mut bridge = Command::new(BIN)
        .arg("bridge")
        .arg("--socket")
        .arg(&svc.socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = bridge.stdin.take().unwrap();
    let mut out = BufReader::new(bridge.stdout.take().unwrap());
    let mut ask = |line: &str| -> Response {
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
        let mut l = String::new();
        out.read_line(&mut l).unwrap();
        serde_json::from_str(&l).unwrap()
    };
    let r = ask(r#"{"id":1,"method":"node.info"}"#);
    assert_eq!(r.error.unwrap().code, ErrorCode::HelloRequired);
    let r = ask(r#"{"id":2,"method":"hello","params":{"protoMin":1,"protoMax":1,"client":"bridge-test"}}"#);
    assert_eq!(r.result.unwrap()["proto"], 1);
    let r = ask(r#"{"id":3,"method":"node.info"}"#);
    assert_eq!(r.id, 3);
    assert!(r.result.unwrap()["coverage"].is_array());
    drop(stdin);
    let st = bridge.wait().unwrap();
    assert!(st.success());
}

#[test]
fn stops_cleanly_on_sigterm() {
    let mut svc = start();
    // SAFETY: plain kill(2) on our own child.
    unsafe { libc::kill(svc.child.id() as i32, libc::SIGTERM) };
    let t = Instant::now();
    let status = loop {
        if let Some(s) = svc.child.try_wait().unwrap() {
            break s;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "didn't stop");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success());
    assert!(!svc.socket.exists(), "socket removed on exit");
}

#[test]
fn version_and_help() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("nightgrid-node "));
    let out = Command::new(BIN).arg("frobnicate").output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn pushes_live_metrics_and_keeps_protocol_1() {
    let svc = start();
    let mut c = Client::connect(&svc.socket, "e2e").unwrap();
    let r = c.call("subscribe", json!({"streams": ["metrics", "events"]})).unwrap();
    assert_eq!(r["streams"], json!(["events", "metrics"]));
    // Frames don't confuse request/response matching.
    // Events (e.g. "network watch started") may arrive in between.
    let next_metrics = |c: &mut Client| loop {
        let f = c.next_frame().unwrap();
        if f.stream == "metrics" {
            break f;
        }
        assert_eq!(f.stream, "events");
    };
    let t = Instant::now();
    let f = next_metrics(&mut c);
    assert!(f.data["memTotal"].as_u64().unwrap() > 0);
    assert!(c.call("node.info", Value::Null).is_ok());
    next_metrics(&mut c);
    assert!(t.elapsed() < Duration::from_secs(8), "{:?}", t.elapsed());
    match c.call("subscribe", json!({"streams": ["nope"]})) {
        Err(CallError::Rpc(e)) => assert_eq!(e.code, ErrorCode::BadRequest),
        other => panic!("{other:?}"),
    }
    let r = c.call("unsubscribe", json!({"streams": ["metrics", "events"]})).unwrap();
    assert_eq!(r["streams"], json!([]));

    // A protocol 1 client gets no streams.
    use std::io::{BufRead, BufReader, Write};
    let mut s = std::os::unix::net::UnixStream::connect(&svc.socket).unwrap();
    let mut rd = BufReader::new(s.try_clone().unwrap());
    let mut line = String::new();
    s.write_all(b"{\"id\":1,\"method\":\"hello\",\"params\":{\"protoMin\":1,\"protoMax\":1}}\n").unwrap();
    rd.read_line(&mut line).unwrap();
    assert!(line.contains("\"proto\":1"), "{line}");
    line.clear();
    s.write_all(b"{\"id\":2,\"method\":\"subscribe\",\"params\":{\"streams\":[\"metrics\"]}}\n").unwrap();
    rd.read_line(&mut line).unwrap();
    assert!(line.contains("unknown_method"), "{line}");
}

#[test]
fn takes_netwatch_reports_from_its_own_account() {
    let svc = start_with("[network]\ninterval_secs = 5\n[docker]\nenabled = false\n");
    let mut c = Client::connect(&svc.socket, "e2e").unwrap();
    assert!(c.welcome.caps.contains_key("net.dhcp"));
    let info = c.call("node.info", Value::Null).unwrap();
    assert_eq!(info["access"]["netwatch"], "denied");
    let empty = c.call("net.dhcp", Value::Null).unwrap();
    assert_eq!(empty, json!({"watching": false, "servers": []}));

    // The test runs as the service's own uid, like the helper does.
    let offer = |server: &str, dns: &str| {
        json!({"at": 1, "msg": "offer", "iface": "eth0", "clientMac": "aa:bb:cc:00:00:02",
               "server": server, "serverMac": "02:00:00:00:00:01", "ip": "10.99.0.50",
               "router": server, "dns": [dns], "subnet": "255.255.255.0", "leaseSecs": 3600})
    };
    let report = json!({
        "arp": [{"ip": "10.99.0.1", "mac": "02:00:00:00:00:01", "iface": "eth0"}],
        "dhcp": [
            offer("10.99.0.1", "10.99.0.1"),
            {"at": 2, "msg": "request", "iface": "eth0", "clientMac": "aa:bb:cc:00:00:02",
             "server": "10.99.0.1", "hostname": "ngn-phone"},
            {"at": 3, "msg": "ack", "iface": "eth0", "clientMac": "aa:bb:cc:00:00:02",
             "server": "10.99.0.1", "ip": "10.99.0.50"}
        ]
    });
    c.call("net.report", report).unwrap();
    c.call("net.report", json!({"dhcp": [offer("10.99.0.66", "10.99.0.66")]})).unwrap();

    let d = c.call("net.dhcp", Value::Null).unwrap();
    assert_eq!(d["watching"], true);
    let ips: Vec<&str> = d["servers"].as_array().unwrap().iter().map(|s| s["ip"].as_str().unwrap()).collect();
    assert!(ips.contains(&"10.99.0.1") && ips.contains(&"10.99.0.66"), "{d}");
    assert_eq!(c.call("node.info", Value::Null).unwrap()["access"]["netwatch"], "ok");
    let ev = c.call("events.sync", json!({"after": 0})).unwrap();
    let titles: Vec<&str> = ev["events"].as_array().unwrap().iter().map(|e| e["title"].as_str().unwrap()).collect();
    assert!(titles.contains(&"DHCP server 10.99.0.1"), "{titles:?}");
    assert!(titles.contains(&"another DHCP server on the LAN: 10.99.0.66"), "{titles:?}");

    // The device and its DHCP name reach the inventory at the next read.
    let t = Instant::now();
    loop {
        let net = c.call("net.devices", Value::Null).unwrap();
        let dev = net["devices"].as_array().unwrap().iter().find(|d| d["mac"] == "aa:bb:cc:00:00:02").cloned();
        if let Some(dev) = dev {
            assert_eq!((dev["ip"].as_str(), dev["name"].as_str()), (Some("10.99.0.50"), Some("ngn-phone")), "{dev}");
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(15), "device never listed: {net}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn actions_need_roots_allow_and_are_logged() {
    let svc = start_with("[actions]\nallow = [\"container.restart\"]\n[docker]\nenabled = false\n");
    let mut c = Client::connect(&svc.socket, "e2e").unwrap();
    assert!(c.welcome.caps.contains_key("actions") && c.welcome.caps.contains_key("heal"));
    let p = c.call("actions.policy", Value::Null).unwrap();
    assert_eq!(p["allowed"], json!(["container.restart"]));

    // Not allowed by root, then allowed but no engine: both refused, both logged.
    let r = c.call("actions.run", json!({"action": "container.stop", "target": "web"})).unwrap();
    assert_eq!(r["outcome"], "refused", "{r}");
    let r = c.call("actions.run", json!({"action": "container.restart", "target": "web", "dryRun": true})).unwrap();
    assert_eq!(r["outcome"], "refused", "{r}");
    let log = c.call("actions.log", json!({"after": 0})).unwrap();
    assert_eq!(log["records"].as_array().unwrap().len(), 2, "{log}");
    assert_eq!(log["verified"], true);
    let ev = c.call("events.sync", json!({"after": 0})).unwrap();
    assert!(ev["events"].as_array().unwrap().iter().any(|e| e["kind"] == "action"), "{ev}");

    // Self-healing rules: validated, stored, off by default.
    let bad = c.call("heal.rules.set", json!({"rules": [{"id": "x", "action": "rm"}]}));
    assert!(bad.is_err());
    let rules = json!({"rules": [{"id": "web-unhealthy", "target": "web"}]});
    let set = c.call("heal.rules.set", rules).unwrap();
    assert_eq!((set["rules"][0]["enabled"].as_bool(), set["rules"][0]["dryRun"].as_bool()), (Some(false), Some(true)));
    assert_eq!(c.call("heal.rules.get", Value::Null).unwrap(), set);
    assert_eq!(c.call("heal.status", Value::Null).unwrap(), json!({"enabled": 0, "tracking": []}));
}

struct Link(Child);

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Runs `nightgrid-node peer` for a service, keeping its key in `dir`.
fn link(svc: &Service, dir: &Path, listen: &str) -> Link {
    let config = dir.join("peer.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nsocket = {:?}\n[peers]\nlisten = {listen:?}\nnetworks = [\"127.0.0.0/8\"]\ninterval_secs = 30\n",
            svc.socket
        ),
    )
    .unwrap();
    let child = Command::new(BIN)
        .args(["peer", "--config"])
        .arg(&config)
        .env("STATE_DIRECTORY", dir)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    Link(child)
}

fn latest(svc: &Service, until: impl Fn(&Value) -> bool) -> Value {
    let t = Instant::now();
    loop {
        let mut c = Client::connect(&svc.socket, "e2e").unwrap();
        let v = c.call("peers.latest", Value::Null).unwrap();
        if until(&v) {
            return v;
        }
        assert!(t.elapsed() < Duration::from_secs(40), "peers.latest never got there: {v}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn paired_nodes_reach_each_other_over_the_link() {
    let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    let (addr_a, addr_b) = (free(), free());
    let a = start();
    let b = start();
    let mut c = Client::connect(&a.socket, "e2e").unwrap();
    assert_eq!(c.welcome.caps.get("peers"), Some(&1));
    let v = c.call("peers.latest", Value::Null).unwrap();
    assert_eq!(v["access"], "off", "no link until root allows it: {v}");
    let id_a = c.welcome.node.node_id.clone();
    let id_b = Client::connect(&b.socket, "e2e").unwrap().welcome.node.node_id.clone();

    let keys = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(keys.path().join("a")).unwrap();
    std::fs::create_dir_all(keys.path().join("b")).unwrap();
    let _lb = link(&b, &keys.path().join("b"), &addr_b);
    let vb = latest(&b, |v| v["listening"].is_string());
    let _la = link(&a, &keys.path().join("a"), &addr_a);
    let va = latest(&a, |v| v["listening"].is_string());
    assert_eq!(va["access"], "ok");
    let (fp_a, fp_b) = (va["fingerprint"].as_str().unwrap(), vb["fingerprint"].as_str().unwrap());
    assert_ne!(fp_a, fp_b);

    // The app pairs them: each gets the other's fingerprint.
    let pair = |svc: &Service, id: &str, addr: &str, fp: &str| {
        let mut c = Client::connect(&svc.socket, "e2e").unwrap();
        c.call("peers.set", json!({"peers": [{"id": id, "name": "other", "address": addr, "fingerprint": fp}]}))
            .unwrap()
    };
    pair(&b, &id_a, &addr_a, fp_a);
    // A pinned the wrong certificate for B first: refused.
    pair(&a, &id_b, &addr_b, fp_a);
    let v = latest(&a, |v| v["readings"][0]["at"].is_number());
    assert_eq!(v["readings"][0]["ok"], false, "{v}");
    pair(&a, &id_b, &addr_b, fp_b);
    let v = latest(&a, |v| v["readings"][0]["ok"] == true);
    assert_eq!(v["readings"][0]["id"], id_b.as_str());
    assert_eq!(v["readings"][0]["version"], env!("CARGO_PKG_VERSION"));
    let v = latest(&b, |v| v["readings"][0]["ok"] == true);
    assert_eq!(v["readings"][0]["id"], id_a.as_str());
}

#[test]
fn follows_a_paired_peers_ups_shutdown() {
    use nightgrid_node::peers;
    let addr_b = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    let serve = "[peers]\nserve = [\"ping\", \"ups\"]\nnetworks = [\"127.0.0.0/8\"]\n";
    let b = start_with(&format!(
        "[actions]\nallow = [\"power.poweroff\"]\n[ups]\nshutdown = true\ndry_run = true\ncountdown_secs = 300\n{serve}"
    ));
    let keys = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(keys.path().join("b")).unwrap();
    let config = keys.path().join("peer.toml");
    std::fs::write(
        &config,
        format!("[server]\nsocket = {:?}\n{serve}listen = {addr_b:?}\ninterval_secs = 30\n", b.socket),
    )
    .unwrap();
    let _lb = Link(
        Command::new(BIN)
            .args(["peer", "--config"])
            .arg(&config)
            .env("STATE_DIRECTORY", keys.path().join("b"))
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let vb = latest(&b, |v| v["listening"].is_string());
    let fp_b = vb["fingerprint"].as_str().unwrap().to_owned();

    // A is the test itself, with its own link identity.
    let a = peers::identity(&keys.path().join("a")).unwrap();
    let id_a = "a".repeat(32);
    let mut c = Client::connect(&b.socket, "e2e").unwrap();
    let pair = |c: &mut Client, follow: bool| {
        c.call(
            "peers.set",
            json!({"peers": [{"id": id_a, "name": "nas", "address": "127.0.0.1:9", "fingerprint": a.fingerprint, "followUps": follow}]}),
        )
        .unwrap();
    };
    let to_b = nightgrid_node_proto::Peer {
        id: String::new(),
        name: "b".into(),
        address: addr_b.clone(),
        fingerprint: fp_b,
        follow_ups: false,
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let tell = |phase: &str| {
        rt.block_on(peers::request(
            &a,
            &to_b,
            &json!({"method": "peer.ups", "params": {"phase": phase, "reason": "on battery for 5 min", "target": "ups@nas"}}),
        ))
    };
    // Paired, but not followed: refused. (B learns the list within 10 s.)
    pair(&mut c, false);
    let t = Instant::now();
    let err = loop {
        let e = tell("countdown").unwrap_err();
        if e != "not paired" && !e.contains("certificate") && !e.contains("refused this node") {
            break e;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "B never took the pairing: {e}");
        std::thread::sleep(Duration::from_millis(500));
    };
    assert!(err.contains("doesn't follow"), "{err}");
    pair(&mut c, true);
    std::thread::sleep(Duration::from_secs(11));
    let r = tell("countdown").unwrap();
    assert_eq!(r["phase"], "countdown");
    let g = c.call("ups.guard", Value::Null).unwrap();
    assert_eq!(g["ledBy"], "nas", "{g}");
    assert!(g["reason"].as_str().unwrap().starts_with("nas is shutting down for ups@nas"), "{g}");
    let ev = c.call("events.sync", json!({"after": 0})).unwrap().to_string();
    assert!(ev.contains("UPS shutdown in 5 min"), "{ev}");
    assert_eq!(tell("idle").unwrap()["phase"], "idle");
    assert_eq!(c.call("ups.guard", Value::Null).unwrap()["phase"], "idle");
}

fn http_get(addr: &str, path: &str) -> String {
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut out = String::new();
    std::io::Read::read_to_string(&mut s, &mut out).unwrap();
    out
}

#[test]
fn status_page_at_its_secret_path_only() {
    let svc = start();
    let dir = tempfile::tempdir().unwrap();
    let addr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    let token = "0123456789abcdef".repeat(4);
    std::fs::write(dir.path().join("token"), format!("{token}\n")).unwrap();
    let config = dir.path().join("page.toml");
    std::fs::write(&config, format!("[server]\nsocket = {:?}\n[statuspage]\nlisten = {addr:?}\n", svc.socket)).unwrap();
    let _page = Link(
        Command::new(BIN)
            .args(["status-page", "--config"])
            .arg(&config)
            .env("NIGHTGRID_STATUS_TOKEN", dir.path().join("token"))
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let t = Instant::now();
    while std::net::TcpStream::connect(&addr).is_err() {
        assert!(t.elapsed() < Duration::from_secs(10), "status page never listened");
        std::thread::sleep(Duration::from_millis(50));
    }
    let page = http_get(&addr, &format!("/{token}"));
    assert!(page.starts_with("HTTP/1.1 200 OK\r\n"), "{page}");
    assert!(page.contains("Content-Security-Policy: default-src 'none'"));
    assert!(page.contains("Nightgrid Node ") && page.contains("Alerts"), "{page}");
    assert!(http_get(&addr, "/").starts_with("HTTP/1.1 404"));
    assert!(http_get(&addr, &format!("/{}", &token[..63])).starts_with("HTTP/1.1 404"));
    let mut s = std::net::TcpStream::connect(&addr).unwrap();
    write!(s, "POST /{token} HTTP/1.1\r\n\r\n").unwrap();
    let mut out = String::new();
    std::io::Read::read_to_string(&mut s, &mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 405"), "{out}");
}

#[test]
fn daily_summary_for_drift() {
    let svc = start();
    let mut c = Client::connect(&svc.socket, "e2e").unwrap();
    assert_eq!(c.welcome.caps.get("summary"), Some(&1));
    let s = c.call("summary.daily", Value::Null).unwrap();
    assert_eq!(s["to"].as_i64().unwrap() - s["from"].as_i64().unwrap(), 86_400);
    let facts: Vec<String> = serde_json::from_value(s["facts"].clone()).unwrap();
    assert!(facts.iter().any(|f| f.starts_with("Active alerts")), "{facts:?}");
    assert!(
        facts.iter().any(|f| f.starts_with("Coverage:")),
        "a new node says it hasn't watched the whole day: {facts:?}"
    );
    let week = c.call("summary.daily", json!({"hours": 168})).unwrap();
    assert_eq!(week["to"].as_i64().unwrap() - week["from"].as_i64().unwrap(), 168 * 3600);
    assert!(c.call("summary.daily", json!({"hours": 0})).is_err());
}
