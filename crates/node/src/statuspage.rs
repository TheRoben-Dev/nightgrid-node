//! Read-only status page (N8d), opt-in: `nightgrid-node allow statuspage`
//! installs a separate unit (`nightgrid-node status-page`) that serves one
//! small HTML page about this host for a phone on the tailnet.
//!
//! - **Where:** this host's tailnet address (or loopback), never a LAN or
//!   public one, because it is plain HTTP; WireGuard encrypts the tailnet.
//!   systemd lets in only `[statuspage] networks`.
//! - **Who:** the page lives at a secret path, `/<token>`, 256 random bits
//!   that `allow statuspage` writes to `/etc/nightgrid-node/status-token`
//!   (root and the service's group) and prints as a link. Anything else is
//!   a plain 404. `deny statuspage` removes the token, so the link dies.
//! - **What:** `GET` only, no scripts, no forms, nothing that changes the
//!   host. It reads the node's socket as the service account, like the app.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nightgrid_node_proto::{method, Alert, NodeInfo, PeersLatest, PeersList, Sample, Severity, UpsGuard, UpsPhase};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::Config;

pub const TOKEN_FILE: &str = "/etc/nightgrid-node/status-token";
const MAX_HEAD: usize = 8192;
const TIMEOUT: Duration = Duration::from_secs(5);
/// How long one read of the node is shown before reading again.
const FRESH: Duration = Duration::from_secs(10);
const MAX_CONNECTIONS: usize = 8;

/// A new token: 32 random bytes as hex.
pub fn new_token() -> std::io::Result<String> {
    let mut b = [0u8; 32];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

pub fn token_ok(t: &str) -> bool {
    t.len() == 64 && t.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Compares without stopping at the first difference.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Where to listen, per `[statuspage] listen`.
pub fn listen_addr(cfg: &Config) -> Option<SocketAddr> {
    match cfg.statuspage.listen.as_str() {
        "tailnet" => crate::peers::tailnet_address().map(|ip| SocketAddr::new(ip, cfg.statuspage.port)),
        a => a.parse().ok(),
    }
}

/// The address a person opens, for `allow statuspage`.
pub fn url(cfg: &Config, token: &str) -> String {
    match listen_addr(cfg) {
        Some(a) => format!("http://{a}/{token}"),
        None => format!("http://<this host's tailnet address>:{}/{token}", cfg.statuspage.port),
    }
}

// ---- what the page shows -----------------------------------------------------------

/// One read of the node.
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    pub at: i64,
    pub error: Option<String>,
    pub info: Option<NodeInfo>,
    pub sample: Option<Sample>,
    pub alerts: Vec<Alert>,
    pub ups: Option<UpsGuard>,
    pub peers: Option<(PeersList, PeersLatest)>,
}

fn read_node(socket: &Path) -> Snapshot {
    let now = crate::daemon::now_secs();
    let mut c = match crate::client::Client::connect(socket, "nightgrid-node status-page") {
        Ok(c) => c,
        Err(e) => return Snapshot { at: now, error: Some(e.to_string()), ..Default::default() },
    };
    let mut get = |m: &str| c.call(m, Value::Null).ok();
    let info = get(method::NODE_INFO).and_then(|v| serde_json::from_value(v).ok());
    let sample = get(method::METRICS_LATEST).and_then(|v| serde_json::from_value(v).ok());
    let alerts = get(method::ALERTS_ACTIVE).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    let ups = get(method::UPS_GUARD).and_then(|v| serde_json::from_value::<UpsGuard>(v).ok()).filter(|g| g.enabled);
    let list = get(method::PEERS_GET).and_then(|v| serde_json::from_value::<PeersList>(v).ok());
    let latest = get(method::PEERS_LATEST).and_then(|v| serde_json::from_value::<PeersLatest>(v).ok());
    let peers = match (list, latest) {
        (Some(l), Some(r)) if !l.peers.is_empty() => Some((l, r)),
        _ => None,
    };
    Snapshot { at: now, error: None, info, sample, alerts, ups, peers }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c if c.is_control() => {}
            c => o.push(c),
        }
    }
    o
}

fn bytes(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

/// "3 min ago", "2 h ago", "in 40 s".
fn ago(t: i64, now: i64) -> String {
    let d = now - t;
    let (n, future) = if d < 0 { (-d, true) } else { (d, false) };
    let s = match n {
        0..=59 => format!("{n} s"),
        60..=3599 => format!("{} min", n / 60),
        3600..=172_799 => format!("{} h", n / 3600),
        _ => format!("{} d", n / 86_400),
    };
    if future {
        format!("in {s}")
    } else if n < 5 {
        "just now".into()
    } else {
        format!("{s} ago")
    }
}

fn span(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..=3599 => format!("{} min", s / 60),
        3600..=86_399 => format!("{} h {} min", s / 3600, (s % 3600) / 60),
        _ => format!("{} d {} h", s / 86_400, (s % 86_400) / 3600),
    }
}

fn level(p: f64) -> &'static str {
    if p >= 90.0 {
        "bad"
    } else if p >= 75.0 {
        "warn"
    } else {
        "ok"
    }
}

fn bar(label: &str, pct: f64, text: &str) -> String {
    let p = pct.clamp(0.0, 100.0);
    format!(
        "<div class=\"bar\"><div class=\"bl\"><span>{}</span><span>{}</span></div>\
         <div class=\"track\"><div class=\"fill {}\" style=\"width:{p:.1}%\"></div></div></div>",
        esc(label),
        esc(text),
        level(p)
    )
}

const STYLE: &str = "\
:root{--bg:#f4f5f7;--card:#fff;--fg:#1b1f24;--mute:#5b6470;--line:#e2e5ea;--ok:#1f9d55;--warn:#c27c0e;--bad:#d03b3b;--acc:#3b6fd0}\
@media (prefers-color-scheme:dark){:root{--bg:#0d1014;--card:#161b21;--fg:#e6e9ee;--mute:#8b95a3;--line:#252c35;--ok:#3ccf7e;--warn:#f0b43c;--bad:#ff6b6b;--acc:#7aa7ff}}\
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.45 system-ui,-apple-system,Segoe UI,Roboto,sans-serif}\
main{max-width:720px;margin:0 auto;padding:16px}header{display:flex;align-items:center;justify-content:space-between;gap:12px;margin:8px 0 16px}\
h1{font-size:22px;margin:0;font-weight:650;letter-spacing:-.01em}h2{font-size:13px;text-transform:uppercase;letter-spacing:.06em;color:var(--mute);margin:0 0 10px;font-weight:600}\
.sub{color:var(--mute);font-size:13px}.pill{padding:4px 10px;border-radius:999px;font-weight:600;font-size:13px;white-space:nowrap}\
.pill.ok{background:color-mix(in srgb,var(--ok) 15%,transparent);color:var(--ok)}.pill.warn{background:color-mix(in srgb,var(--warn) 18%,transparent);color:var(--warn)}\
.pill.bad{background:color-mix(in srgb,var(--bad) 15%,transparent);color:var(--bad)}\
section{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:14px 16px;margin-bottom:12px}\
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(130px,1fr));gap:10px}.stat b{display:block;font-size:20px;font-weight:650;font-variant-numeric:tabular-nums}\
.stat span{color:var(--mute);font-size:12px}.bar{margin:8px 0}.bl{display:flex;justify-content:space-between;font-size:13px;gap:8px}.bl span:first-child{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}\
.bl span:last-child{color:var(--mute);font-variant-numeric:tabular-nums;white-space:nowrap}.track{height:6px;background:var(--line);border-radius:3px;overflow:hidden;margin-top:4px}\
.fill{height:100%;border-radius:3px}.fill.ok{background:var(--ok)}.fill.warn{background:var(--warn)}.fill.bad{background:var(--bad)}\
ul{list-style:none;margin:0;padding:0}li{padding:8px 0;border-top:1px solid var(--line);display:flex;gap:10px;align-items:baseline}li:first-child{border-top:0}\
.dot{width:8px;height:8px;border-radius:50%;flex:none;transform:translateY(-1px)}.dot.ok{background:var(--ok)}.dot.warn{background:var(--warn)}.dot.bad{background:var(--bad)}\
.t{font-weight:550}.d{color:var(--mute);font-size:13px;overflow-wrap:anywhere}.r{margin-left:auto;color:var(--mute);font-size:12px;white-space:nowrap}\
footer{color:var(--mute);font-size:12px;text-align:center;margin:16px 0}.empty{color:var(--mute);font-size:14px}";

/// The page. Pure, so it can be tested.
pub fn render(s: &Snapshot, now: i64) -> String {
    let host = s
        .info
        .as_ref()
        .map(|i| i.node.hostname.clone())
        .or_else(|| s.sample.as_ref().map(|x| x.hostname.clone()))
        .unwrap_or_else(|| "this host".into());
    let version = s.info.as_ref().map(|i| i.node.version.clone()).unwrap_or_default();
    let crit = s.alerts.iter().filter(|a| a.severity == Severity::Critical).count();
    let (cls, word) = match (&s.error, crit, s.alerts.len()) {
        (Some(_), _, _) => ("bad", "node not answering".to_owned()),
        (None, 0, 0) => ("ok", "all good".to_owned()),
        (None, 0, n) => ("warn", format!("{n} warning{}", if n == 1 { "" } else { "s" })),
        (None, c, _) => ("bad", format!("{c} critical")),
    };
    let mut b = String::with_capacity(8192);
    b.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    b.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    b.push_str("<meta http-equiv=\"refresh\" content=\"30\"><meta name=\"robots\" content=\"noindex\">");
    b.push_str(&format!("<title>{} · Nightgrid</title><style>{STYLE}</style></head><body><main>", esc(&host)));
    b.push_str(&format!(
        "<header><div><h1>{}</h1><div class=\"sub\">Nightgrid Node {}</div></div><span class=\"pill {cls}\">{}</span></header>",
        esc(&host),
        esc(&version),
        esc(&word)
    ));
    if let Some(e) = &s.error {
        b.push_str(&format!(
            "<section><h2>Node</h2><p class=\"empty\">The node didn't answer: {}</p></section>",
            esc(e)
        ));
    }
    if let Some(x) = &s.sample {
        b.push_str("<section><h2>Now</h2><div class=\"grid\">");
        let stat = |v: String, l: &str| format!("<div class=\"stat\"><b>{}</b><span>{}</span></div>", esc(&v), esc(l));
        b.push_str(&stat(x.cpu.map_or("–".into(), |c| format!("{c:.0}%")), "CPU"));
        let mem = if x.mem_total > 0 { x.mem_used as f64 * 100.0 / x.mem_total as f64 } else { 0.0 };
        b.push_str(&stat(format!("{mem:.0}%"), &format!("memory of {}", bytes(x.mem_total))));
        b.push_str(&stat(format!("{:.2}", x.load[0]), "load"));
        b.push_str(&stat(span(x.uptime_secs), "up"));
        if let Some(t) = x.cpu_temp {
            b.push_str(&stat(format!("{t:.0} °C"), "CPU temperature"));
        }
        b.push_str("</div>");
        for m in x.mounts.iter().take(12) {
            let pct = if m.total > 0 { m.used as f64 * 100.0 / m.total as f64 } else { 0.0 };
            b.push_str(&bar(&m.mount, pct, &format!("{} free of {}", bytes(m.avail), bytes(m.total))));
        }
        b.push_str("</section>");
    }
    b.push_str("<section><h2>Alerts</h2>");
    if s.alerts.is_empty() {
        b.push_str("<p class=\"empty\">No active alerts.</p>");
    } else {
        b.push_str("<ul>");
        let mut alerts: Vec<&Alert> = s.alerts.iter().collect();
        alerts.sort_by_key(|a| (a.severity != Severity::Critical, -a.since));
        for a in alerts.iter().take(30) {
            let c = if a.severity == Severity::Critical { "bad" } else { "warn" };
            b.push_str(&format!(
                "<li><span class=\"dot {c}\"></span><div><div class=\"t\">{}</div><div class=\"d\">{}</div></div><span class=\"r\">{}</span></li>",
                esc(&a.title),
                esc(&a.detail),
                esc(&ago(a.since, now))
            ));
        }
        b.push_str("</ul>");
    }
    b.push_str("</section>");
    if let Some(g) = &s.ups {
        let (c, what) = match g.phase {
            UpsPhase::Idle => ("ok", "on line power".to_owned()),
            UpsPhase::OnBattery => (
                "warn",
                format!("on battery{}", g.since.map(|t| format!(" since {}", ago(t, now))).unwrap_or_default()),
            ),
            UpsPhase::Countdown => {
                ("bad", format!("shutting down {}", g.deadline.map(|d| ago(d, now)).unwrap_or_else(|| "soon".into())))
            }
            UpsPhase::ShuttingDown => ("bad", "powering off".to_owned()),
            UpsPhase::Cancelled => ("warn", "shutdown cancelled for this outage".to_owned()),
        };
        let detail = [g.reason.clone(), g.led_by.as_ref().map(|l| format!("following {l}")), g.problem.clone()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        b.push_str(&format!(
            "<section><h2>UPS shutdown</h2><ul><li><span class=\"dot {c}\"></span><div><div class=\"t\">{}</div><div class=\"d\">{}</div></div><span class=\"r\">{}</span></li></ul></section>",
            esc(&what),
            esc(&detail),
            esc(g.target.as_deref().unwrap_or(""))
        ));
    }
    if let Some((list, latest)) = &s.peers {
        b.push_str("<section><h2>Linked hosts</h2><ul>");
        for p in &list.peers {
            let r = latest.readings.iter().find(|r| r.id == p.id);
            let (c, d, right) = match r {
                Some(r) if r.ok => (
                    "ok",
                    format!("reachable{}", r.version.as_ref().map(|v| format!(" · node {v}")).unwrap_or_default()),
                    r.rtt_ms.map(|m| format!("{m} ms")).unwrap_or_default(),
                ),
                Some(r) => ("bad", r.error.clone().unwrap_or_else(|| "unreachable".into()), ago(r.at, now)),
                None => ("warn", "not checked yet".to_owned(), String::new()),
            };
            b.push_str(&format!(
                "<li><span class=\"dot {c}\"></span><div><div class=\"t\">{}</div><div class=\"d\">{}</div></div><span class=\"r\">{}</span></li>",
                esc(&p.name),
                esc(&d),
                esc(&right)
            ));
        }
        b.push_str("</ul></section>");
    }
    b.push_str(&format!(
        "<footer>Read-only · read {} · refreshes every 30 s</footer></main></body></html>",
        esc(&ago(s.at, now))
    ));
    b
}

// ---- serving -------------------------------------------------------------------------

struct Cache {
    at: Option<Instant>,
    snap: Snapshot,
}

/// What one request gets: status line and body.
pub fn route(head: &str, token: &str, page: impl FnOnce() -> String) -> (&'static str, String) {
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split(' ');
    let (m, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if m != "GET" && m != "HEAD" {
        return ("405 Method Not Allowed", "only GET\n".into());
    }
    let p = path.split('?').next().unwrap_or("").trim_start_matches('/').trim_end_matches('/');
    if same(p.as_bytes(), token.as_bytes()) {
        ("200 OK", page())
    } else {
        ("404 Not Found", "not found\n".into())
    }
}

async fn serve_one(mut tcp: tokio::net::TcpStream, token: Arc<String>, socket: Arc<PathBuf>, cache: Arc<Mutex<Cache>>) {
    let fut = async {
        let mut buf = Vec::with_capacity(1024);
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") && !buf.windows(2).any(|w| w == b"\n\n") {
            let n = tcp.read(&mut chunk).await.map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("closed".to_string());
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > MAX_HEAD {
                return Err("request too large".to_string());
            }
        }
        let head = String::from_utf8_lossy(&buf).into_owned();
        let need = {
            let c = cache.lock().unwrap_or_else(|e| e.into_inner());
            c.at.map_or(true, |t| t.elapsed() >= FRESH)
        };
        let ok_path = route(&head, &token, String::new).0.starts_with("200");
        if ok_path && need {
            let s = socket.clone();
            let snap = tokio::task::spawn_blocking(move || read_node(&s)).await.unwrap_or_default();
            let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
            *c = Cache { at: Some(Instant::now()), snap };
        }
        let (status, body) = route(&head, &token, || {
            let c = cache.lock().unwrap_or_else(|e| e.into_inner());
            render(&c.snap, crate::daemon::now_secs())
        });
        if !status.starts_with("200") {
            // Guessing gets slow.
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let ctype = if status.starts_with("200") { "text/html; charset=utf-8" } else { "text/plain; charset=utf-8" };
        let head_only = head.starts_with("HEAD ");
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\
             Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\n\
             X-Frame-Options: DENY\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; \
             frame-ancestors 'none'\r\n\r\n",
            body.len()
        )
        .into_bytes();
        if !head_only {
            out.extend_from_slice(body.as_bytes());
        }
        tcp.write_all(&out).await.map_err(|e| e.to_string())?;
        tcp.shutdown().await.map_err(|e| e.to_string())
    };
    if let Ok(Err(e)) = tokio::time::timeout(TIMEOUT, fut).await {
        crate::debug!("status page: {e}");
    }
}

/// `nightgrid-node status-page`: the unit's loop.
pub fn serve(config_path: &Path) -> Result<(), String> {
    let cfg = Config::load(config_path).map_err(|e| format!("config: {e}"))?;
    let token_path =
        std::env::var_os("NIGHTGRID_STATUS_TOKEN").map_or_else(|| PathBuf::from(TOKEN_FILE), PathBuf::from);
    let token = std::fs::read_to_string(&token_path)
        .map_err(|e| format!("{}: {e} (run `sudo nightgrid-node allow statuspage`)", token_path.display()))?
        .trim()
        .to_owned();
    if !token_ok(&token) {
        return Err(format!(
            "{} doesn't hold a token; run `sudo nightgrid-node allow statuspage` again",
            token_path.display()
        ));
    }
    let token = Arc::new(token);
    let socket = Arc::new(cfg.server.socket.clone());
    let cache = Arc::new(Mutex::new(Cache { at: None, snap: Snapshot::default() }));
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let listener = loop {
            match listen_addr(&cfg) {
                Some(addr) => match tokio::net::TcpListener::bind(addr).await {
                    Ok(l) => {
                        crate::info!("status page on http://{addr}/ (secret path)");
                        break l;
                    }
                    Err(e) => crate::warn!("can't listen on {addr}: {e}"),
                },
                None => crate::debug!("no tailnet address yet"),
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        };
        loop {
            let Ok((tcp, _)) = listener.accept().await else { continue };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                drop(tcp);
                continue;
            };
            let (t, s, c) = (token.clone(), socket.clone(), cache.clone());
            tokio::spawn(async move {
                serve_one(tcp, t, s, c).await;
                drop(permit);
            });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_only_the_secret_path() {
        let t = "a".repeat(64);
        let page = || "PAGE".to_string();
        assert_eq!(route(&format!("GET /{t} HTTP/1.1\r\n\r\n"), &t, page), ("200 OK", "PAGE".into()));
        assert_eq!(route(&format!("GET /{t}/?x=1 HTTP/1.1\r\n\r\n"), &t, page).0, "200 OK");
        assert_eq!(route("GET / HTTP/1.1\r\n\r\n", &t, page).0, "404 Not Found");
        assert_eq!(route(&format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(63)), &t, page).0, "404 Not Found");
        assert_eq!(route(&format!("POST /{t} HTTP/1.1\r\n\r\n"), &t, page).0, "405 Method Not Allowed");
        assert!(token_ok(&new_token().unwrap()));
        assert!(!token_ok("x"));
    }

    #[test]
    fn renders_escaped_and_summarised() {
        let mut s = Snapshot { at: 1000, ..Default::default() };
        s.alerts.push(Alert {
            id: 1,
            key: "disk:/".into(),
            kind: nightgrid_node_proto::AlertKind::DiskFull,
            severity: Severity::Critical,
            title: "<script>alert(1)</script>".into(),
            detail: "95% used".into(),
            since: 400,
            resolved: None,
        });
        let html = render(&s, 1000);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("1 critical"));
        assert!(html.contains("10 min ago"));
        let ok = render(&Snapshot { at: 1000, ..Default::default() }, 1003);
        assert!(ok.contains("all good") && ok.contains("No active alerts") && ok.contains("just now"));
        let down = render(&Snapshot { at: 1000, error: Some("no socket".into()), ..Default::default() }, 1000);
        assert!(down.contains("node not answering"));
    }
}
