//! Listening sockets (N6b): what listens on this host, read from
//! `/proc/net/{tcp,tcp6,udp,udp6}` (no `ss`, no privileges), with the
//! containers that publish host ports. A socket that starts or stops
//! listening is recorded as a `port` event once it has been seen (or missed)
//! in two reads in a row, so short-lived sockets don't make noise.
//!
//! The well-known service table and `scope_of` are vendored from the
//! Nightgrid app, `core/src/system/security.rs` (commit 8eedcf2), so both
//! judge a socket the same way.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use nightgrid_node_proto::{ContainerInfo, EventKind, Level, Listener, PortsResult, Scope};

use crate::daemon::{now_secs, Shared};
use crate::events::NewEvent;

const META_BASELINE: &str = "ports_baseline";
/// Sockets kept per read; a host with more is unusual and the rest are cut.
const MAX_LISTENERS: usize = 1000;

// ---- vendored from the app (core/src/system/security.rs, 8eedcf2) ----

/// Well-known ports. Databases and caches are flagged when they listen on
/// every interface.
const SERVICES: &[(u16, &str, bool)] = &[
    (21, "ftp", false),
    (22, "ssh", false),
    (23, "telnet", false),
    (25, "smtp", false),
    (53, "dns", false),
    (80, "http", false),
    (111, "rpcbind", false),
    (139, "netbios", false),
    (443, "https", false),
    (445, "smb", false),
    (631, "cups", false),
    (873, "rsync", false),
    (1883, "mqtt", false),
    (2049, "nfs", false),
    (2375, "docker api (no TLS)", true),
    (3000, "web app", false),
    (3306, "mysql", true),
    (3389, "rdp", false),
    (5432, "postgres", true),
    (5900, "vnc", false),
    (5984, "couchdb", true),
    (6379, "redis", true),
    (8006, "proxmox", false),
    (8080, "http-alt", false),
    (8086, "influxdb", true),
    (8443, "https-alt", false),
    (9000, "portainer/minio", false),
    (9090, "prometheus", false),
    (9100, "node-exporter", false),
    (9200, "elasticsearch", true),
    (11211, "memcached", true),
    (27017, "mongodb", true),
    (32400, "plex", false),
];

pub fn service_name(port: u16) -> Option<&'static str> {
    SERVICES.iter().find(|(p, _, _)| *p == port).map(|(_, n, _)| *n)
}

fn is_sensitive(port: u16) -> bool {
    SERVICES.iter().any(|(p, _, s)| *p == port && *s)
}

pub fn scope_of(addr: &str) -> Scope {
    let a = addr.trim_matches(|c| c == '[' || c == ']');
    match a {
        "0.0.0.0" | "::" | "*" | "" => return Scope::All,
        "::1" | "localhost" => return Scope::Local,
        _ => {}
    }
    if a.starts_with("127.") || a.starts_with("::ffff:127.") {
        return Scope::Local;
    }
    let v4: Vec<u8> = a.trim_start_matches("::ffff:").split('.').filter_map(|p| p.parse().ok()).collect();
    if v4.len() == 4 && v4[0] == 100 && (64..128).contains(&v4[1]) {
        return Scope::Vpn;
    }
    // Tailscale's IPv6 range.
    if a.to_ascii_lowercase().starts_with("fd7a:115c:a1e0:") {
        return Scope::Vpn;
    }
    Scope::Address
}

// ---- end of vendored code ----

/// `0100007F:0035` (IPv4) or 32 hex digits (IPv6), as the kernel prints
/// them: each 32-bit word in host byte order.
fn parse_addr(hex: &str) -> Option<(String, u16)> {
    let (a, p) = hex.split_once(':')?;
    let port = u16::from_str_radix(p, 16).ok()?;
    let addr = match a.len() {
        8 => {
            // The kernel prints the in-memory (network order) word as a
            // native integer: on little-endian hosts the bytes come reversed.
            let n = u32::from_str_radix(a, 16).ok()?;
            Ipv4Addr::from(n.to_ne_bytes()).to_string()
        }
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                let n = u32::from_str_radix(&a[i * 8..i * 8 + 8], 16).ok()?;
                b[i * 4..i * 4 + 4].copy_from_slice(&n.to_ne_bytes());
            }
            let v6 = Ipv6Addr::from(b);
            match v6.to_ipv4_mapped() {
                Some(v4) => v4.to_string(),
                None => v6.to_string(),
            }
        }
        _ => return None,
    };
    Some((addr, port))
}

/// Listening sockets in one `/proc/net/*` table. TCP: state `0A` (LISTEN).
/// UDP: unconnected bound sockets (state `07`, remote port 0).
pub fn parse_table(text: &str, proto: &str) -> Vec<(String, String, u16)> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 4 {
            continue;
        }
        let listening = match proto {
            "tcp" => f[3] == "0A",
            _ => f[3] == "07" && f[2].ends_with(":0000"),
        };
        if !listening {
            continue;
        }
        if let Some((addr, port)) = parse_addr(f[1]) {
            out.push((proto.to_owned(), addr, port));
        }
    }
    out
}

/// Listeners from the socket tables plus containers' published ports
/// (when the engine forwards with iptables only there is no socket to see).
pub fn merge(raw: Vec<(String, String, u16)>, containers: &[ContainerInfo]) -> Vec<Listener> {
    let mut out: Vec<Listener> = Vec::new();
    for (proto, addr, port) in raw {
        let l = Listener {
            scope: scope_of(&addr),
            proto,
            addr,
            port,
            process: None,
            container: None,
            service: service_name(port).map(str::to_owned),
        };
        // `0.0.0.0` and `::` for the same port are one service (as the app).
        if !out.iter().any(|o| o.proto == l.proto && o.port == l.port && o.scope == l.scope) {
            out.push(l);
        }
        if out.len() >= MAX_LISTENERS {
            break;
        }
    }
    for c in containers.iter().filter(|c| c.state == "running") {
        for p in &c.ports {
            let scope = scope_of(&p.addr);
            match out.iter_mut().find(|o| o.proto == p.proto && o.port == p.port && o.scope == scope) {
                Some(o) => o.container = Some(c.name.clone()),
                None => out.push(Listener {
                    proto: p.proto.clone(),
                    addr: p.addr.clone(),
                    port: p.port,
                    scope,
                    process: None,
                    container: Some(c.name.clone()),
                    service: service_name(p.port).map(str::to_owned),
                }),
            }
        }
    }
    out.sort_by(|a, b| (a.port, &a.proto, a.scope).cmp(&(b.port, &b.proto, b.scope)));
    out
}

/// The lowest port of the kernel's ephemeral range. Unconnected UDP sockets
/// there are mostly clients (resolvers, NTP), so they're listed but not
/// reported when they come and go.
fn ephemeral_floor(root: &Path) -> u16 {
    std::fs::read_to_string(root.join("proc/sys/net/ipv4/ip_local_port_range"))
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|n| n.parse().ok()))
        .unwrap_or(32768)
}

/// Reads the socket tables under `root` (`/` normally).
pub fn read(root: &Path, containers: &[ContainerInfo]) -> Vec<Listener> {
    let mut raw = Vec::new();
    for (file, proto) in [("tcp", "tcp"), ("tcp6", "tcp"), ("udp", "udp"), ("udp6", "udp")] {
        if let Ok(t) = std::fs::read_to_string(root.join("proc/net").join(file)) {
            raw.extend(parse_table(&t, proto));
        }
    }
    merge(raw, containers)
}

/// A listener's identity for change tracking.
pub type Key = (String, u16, Scope);

fn key(l: &Listener) -> Key {
    (l.proto.clone(), l.port, l.scope)
}

fn reportable(l: &Listener, floor: u16) -> bool {
    !(l.proto == "udp" && l.port >= floor && l.container.is_none())
}

fn where_(l: &Listener) -> String {
    let scope = match l.scope {
        Scope::All => "every interface".to_owned(),
        Scope::Local => "this host only".to_owned(),
        Scope::Vpn => format!("the VPN address {}", l.addr),
        Scope::Address => l.addr.clone(),
    };
    let mut s = format!("on {scope}");
    if let Some(c) = &l.container {
        s.push_str(&format!(", published by container {c}"));
    }
    s
}

fn name(l: &Listener) -> String {
    match &l.service {
        Some(s) => format!("{}/{} ({s})", l.port, l.proto),
        None => format!("{}/{}", l.port, l.proto),
    }
}

pub fn opened_event(l: &Listener, at: i64, note: &str) -> NewEvent {
    let reachable = matches!(l.scope, Scope::All | Scope::Address);
    let level = if reachable { Level::Warn } else { Level::Info };
    let mut detail = where_(l);
    if reachable && is_sensitive(l.port) {
        detail.push_str("; this service is usually kept off the network");
    }
    if !note.is_empty() {
        detail.push(' ');
        detail.push_str(note);
    }
    NewEvent::new(at, EventKind::Port, level, format!("new listener {}", name(l)), detail)
}

pub fn closed_event(l: &Listener, at: i64, note: &str) -> NewEvent {
    let mut detail = where_(l);
    if !note.is_empty() {
        detail.push(' ');
        detail.push_str(note);
    }
    NewEvent::new(at, EventKind::Port, Level::Info, format!("stopped listening {}", name(l)), detail)
}

/// Debounced change tracking: a listener counts as opened or closed once two
/// reads in a row agree.
#[derive(Debug, Default)]
pub struct Tracker {
    /// What the last report was based on.
    pub stable: BTreeMap<Key, Listener>,
    /// Seen once but not yet stable (opened), or missed once (closed).
    pending_open: BTreeSet<Key>,
    pending_close: BTreeSet<Key>,
}

impl Tracker {
    pub fn new(stable: BTreeMap<Key, Listener>) -> Self {
        Self { stable, ..Default::default() }
    }

    /// Takes one read; returns events for what changed for sure.
    pub fn update(&mut self, now: &[Listener], floor: u16, at: i64) -> Vec<NewEvent> {
        let seen: BTreeMap<Key, &Listener> = now.iter().map(|l| (key(l), l)).collect();
        let mut events = Vec::new();
        let mut open = BTreeSet::new();
        for (k, l) in &seen {
            if self.stable.contains_key(k) {
                // Keep the newest details (a container name can appear later).
                self.stable.insert(k.clone(), (*l).clone());
            } else if self.pending_open.contains(k) {
                if reportable(l, floor) {
                    events.push(opened_event(l, at, ""));
                }
                self.stable.insert(k.clone(), (*l).clone());
            } else {
                open.insert(k.clone());
            }
        }
        let mut close = BTreeSet::new();
        let gone: Vec<Key> = self.stable.keys().filter(|k| !seen.contains_key(*k)).cloned().collect();
        for k in gone {
            if self.pending_close.contains(&k) {
                if let Some(l) = self.stable.remove(&k) {
                    if reportable(&l, floor) {
                        events.push(closed_event(&l, at, ""));
                    }
                }
            } else {
                close.insert(k);
            }
        }
        self.pending_open = open;
        self.pending_close = close;
        events
    }
}

/// Events for the difference between the saved list and the first read
/// after a start.
pub fn while_away(saved: &[Listener], now: &[Listener], floor: u16, at: i64) -> Vec<NewEvent> {
    const NOTE: &str = "(while the node was not running)";
    let old: BTreeMap<Key, &Listener> = saved.iter().map(|l| (key(l), l)).collect();
    let new: BTreeMap<Key, &Listener> = now.iter().map(|l| (key(l), l)).collect();
    let mut out = Vec::new();
    for (k, l) in &new {
        if !old.contains_key(k) && reportable(l, floor) {
            out.push(opened_event(l, at, NOTE));
        }
    }
    for (k, l) in &old {
        if !new.contains_key(k) && reportable(l, floor) {
            out.push(closed_event(l, at, NOTE));
        }
    }
    out
}

/// Handle to the port watcher thread.
pub struct PortWatcher {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PortWatcher {
    pub fn start(shared: Arc<Shared>, root: PathBuf) -> std::io::Result<Self> {
        let (stop, rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new().name("ports".into()).spawn(move || run(&shared, &root, &rx))?;
        Ok(Self { stop, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn containers(shared: &Shared) -> Vec<ContainerInfo> {
    shared.containers.lock().unwrap_or_else(|e| e.into_inner()).result.containers.clone()
}

fn record(shared: &Shared, events: &[NewEvent]) {
    if events.is_empty() {
        return;
    }
    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    for e in events {
        if let Err(err) = shared.bus.record(&st, e) {
            crate::warn!("could not record a port change: {err}");
        }
    }
}

fn save(shared: &Shared, list: &[Listener]) {
    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = st.set_meta(META_BASELINE, &serde_json::to_string(list).unwrap_or_default()) {
        crate::warn!("could not save the listening ports: {e}");
    }
}

fn publish(shared: &Shared, list: &[Listener]) {
    *shared.ports.lock().unwrap_or_else(|e| e.into_inner()) = PortsResult { at: now_secs(), listeners: list.to_vec() };
}

fn run(shared: &Shared, root: &Path, stop: &mpsc::Receiver<()>) {
    let interval = Duration::from_secs(shared.config.ports.interval_secs.into());
    let floor = ephemeral_floor(root);
    // Give the container watcher a moment to list the engine's containers,
    // so published ports aren't reported as anonymous sockets first.
    if let Ok(()) | Err(RecvTimeoutError::Disconnected) = stop.recv_timeout(Duration::from_secs(3)) {
        return;
    }
    let saved: Option<Vec<Listener>> = {
        let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        st.meta(META_BASELINE).ok().flatten().and_then(|v| serde_json::from_str(&v).ok())
    };
    let first = read(root, &containers(shared));
    publish(shared, &first);
    if let Some(old) = &saved {
        record(shared, &while_away(old, &first, floor, now_secs()));
    }
    let mut tracker = Tracker::new(first.iter().map(|l| (key(l), l.clone())).collect());
    save(shared, &first);
    while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(interval) {
        let now = read(root, &containers(shared));
        publish(shared, &now);
        let events = tracker.update(&now, floor, now_secs());
        if !events.is_empty() {
            record(shared, &events);
            let stable: Vec<Listener> = tracker.stable.values().cloned().collect();
            save(shared, &stable);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightgrid_node_proto::PublishedPort;

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1001 1 0000000000000000 100 0 0 10 0
   1: 3500007F:0035 00000000:0000 0A 00000000:00000000 00:00000000 00000000   101        0 1002 1 0000000000000000 100 0 0 10 0
   2: 0100007F:0016 0100007F:D431 01 00000000:00000000 00:00000000 00000000     0        0 1003 1 0000000000000000 100 0 0 10 0
";
    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:0016 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 2001 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 2002 1 0000000000000000 100 0 0 10 0
   2: 0000000000000000FFFF00000501A8C0:01BB 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 2003 1 0000000000000000 100 0 0 10 0
";
    const UDP: &str = "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
  1: 00000000:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000   104        0 3001 2 0000000000000000 0
  2: 0A01A8C0:A1B2 08080808:0035 01 00000000:00000000 00:00000000 00000000  1000        0 3002 2 0000000000000000 0
  3: 00000000:9C40 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 3003 2 0000000000000000 0
";

    fn le() -> bool {
        cfg!(target_endian = "little")
    }

    #[test]
    fn reads_proc_tables() {
        if !le() {
            return; // the fixtures are little-endian kernel output
        }
        let t = parse_table(TCP, "tcp");
        assert_eq!(t, [("tcp".into(), "0.0.0.0".into(), 22), ("tcp".into(), "127.0.0.53".into(), 53)]);
        let t6 = parse_table(TCP6, "tcp");
        assert_eq!(
            t6,
            [
                ("tcp".into(), "::".into(), 22),
                ("tcp".into(), "::1".into(), 8080),
                ("tcp".into(), "192.168.1.5".into(), 443)
            ]
        );
        let u = parse_table(UDP, "udp");
        assert_eq!(u, [("udp".into(), "0.0.0.0".into(), 5353), ("udp".into(), "0.0.0.0".into(), 40000)]);

        let web = ContainerInfo {
            id: "a".into(),
            name: "web".into(),
            image: "nginx".into(),
            state: "running".into(),
            status: "Up".into(),
            health: None,
            project: None,
            ports: vec![
                PublishedPort { proto: "tcp".into(), addr: "0.0.0.0".into(), port: 22, private_port: 2222 },
                PublishedPort { proto: "tcp".into(), addr: "0.0.0.0".into(), port: 9000, private_port: 9000 },
            ],
            compose_files: vec![],
        };
        let all: Vec<_> = t.into_iter().chain(t6).chain(u).collect();
        let l = merge(all, std::slice::from_ref(&web));
        let short: Vec<(u16, &str, Scope, Option<&str>)> =
            l.iter().map(|l| (l.port, l.proto.as_str(), l.scope, l.container.as_deref())).collect();
        assert_eq!(
            short,
            [
                (22, "tcp", Scope::All, Some("web")),
                (53, "tcp", Scope::Local, None),
                (443, "tcp", Scope::Address, None),
                (5353, "udp", Scope::All, None),
                (8080, "tcp", Scope::Local, None),
                (9000, "tcp", Scope::All, Some("web")),
                (40000, "udp", Scope::All, None),
            ]
        );
        assert_eq!(l[0].service.as_deref(), Some("ssh"));
    }

    fn lis(proto: &str, port: u16, addr: &str) -> Listener {
        Listener {
            proto: proto.into(),
            addr: addr.into(),
            port,
            scope: scope_of(addr),
            process: None,
            container: None,
            service: service_name(port).map(str::to_owned),
        }
    }

    #[test]
    fn reports_after_two_reads_and_skips_ephemeral_udp() {
        let ssh = lis("tcp", 22, "0.0.0.0");
        let mut t = Tracker::new([(key(&ssh), ssh.clone())].into());
        let redis = lis("tcp", 6379, "0.0.0.0");
        let local = lis("tcp", 8080, "127.0.0.1");
        let dns = lis("udp", 41000, "0.0.0.0");
        let blip = lis("tcp", 7000, "0.0.0.0");
        // A socket seen once only is never reported.
        assert!(t.update(&[ssh.clone(), blip], 32768, 10).is_empty());
        assert!(t.update(&[ssh.clone(), redis.clone(), local.clone(), dns.clone()], 32768, 20).is_empty());
        let ev = t.update(&[ssh.clone(), redis.clone(), local.clone(), dns.clone()], 32768, 30);
        let got: Vec<(&str, Level)> = ev.iter().map(|e| (e.title.as_str(), e.level)).collect();
        assert_eq!(
            got,
            [("new listener 6379/tcp (redis)", Level::Warn), ("new listener 8080/tcp (http-alt)", Level::Info)]
        );
        assert!(ev[0].detail.contains("every interface") && ev[0].detail.contains("kept off the network"));
        assert!(t.stable.contains_key(&key(&dns)), "listed even though not reported");
        // Gone once: nothing; gone twice: closed.
        assert!(t.update(&[ssh.clone(), local.clone(), dns.clone()], 32768, 40).is_empty());
        let ev = t.update(&[ssh.clone(), local.clone()], 32768, 50);
        assert_eq!(ev.len(), 1);
        assert_eq!((ev[0].title.as_str(), ev[0].level), ("stopped listening 6379/tcp (redis)", Level::Info));
        // Back after one miss: no event either way.
        assert!(t.update(std::slice::from_ref(&ssh), 32768, 60).is_empty());
        assert!(t.update(&[ssh.clone(), local.clone()], 32768, 70).is_empty());
        assert!(t.update(&[ssh, local], 32768, 80).is_empty());

        let ev = while_away(&[lis("tcp", 22, "0.0.0.0")], &[lis("tcp", 3306, "192.168.1.5")], 32768, 90);
        let titles: Vec<&str> = ev.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["new listener 3306/tcp (mysql)", "stopped listening 22/tcp (ssh)"]);
        assert!(ev[0].detail.ends_with("(while the node was not running)"));
    }

    #[test]
    fn reads_this_machine() {
        let l = read(Path::new("/"), &[]);
        // Nothing to assert about a CI runner's sockets beyond sanity.
        assert!(l.iter().all(|l| l.port > 0 && (l.proto == "tcp" || l.proto == "udp")));
    }
}
