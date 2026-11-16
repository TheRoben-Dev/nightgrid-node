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
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("run/node.sock");
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "[metrics]\ninterval_secs = 2\n").unwrap();
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
    assert_eq!(c.welcome.proto, 1);
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
