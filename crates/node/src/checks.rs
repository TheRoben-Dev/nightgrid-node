//! Service checks from inside the LAN (N7a): the app's HTTP(S) checks
//! (status, latency, certificate trust and expiry) and plain TCP connects,
//! run with the app closed, so "down" can be told from "only unreachable
//! from where the app is".
//!
//! The main service never opens an IP socket. `allow checks` installs a
//! separate runner unit (`nightgrid-node checker`, the service's own
//! account, no capabilities) whose only network access is to the ranges root
//! lists in `[checks] networks` (systemd `IPAddressAllow`, private ranges by
//! default). It reads the list over the node's socket (`checks.get`) and
//! hands results back (`checks.report`, accepted only from the service's own
//! account), like the network watch helper.
//!
//! The URL parsing, status matching, TLS capture and alert conditions are
//! vendored from the Nightgrid app (`core/src/services/{mod,check}.rs` and
//! `core/src/alerts.rs`, commit 8eedcf2). NODE: the request is a minimal
//! HTTP/1.1 GET that reads only the status line, instead of hyper.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nightgrid_node_proto::{
    method, AlertKind, CheckReading, CheckResult, ChecksList, ChecksReport, ServiceCheck, Severity, TlsInfo,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::alerts::Condition;

pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Checks the app may push, at most.
pub const MAX_CHECKS: usize = 200;
/// Checks running at the same time.
const PARALLEL: usize = 8;
const META_CHECKS: &str = "checks";

// ---- vendored from the app (core/src/services/mod.rs, 8eedcf2) ----

pub fn status_matches(spec: &str, code: u16) -> bool {
    let spec = spec.trim();
    if spec.is_empty() {
        return (200..400).contains(&code);
    }
    spec.split(',').map(str::trim).filter(|p| !p.is_empty()).any(|part| {
        if let Some(d) = part.strip_suffix("xx").and_then(|d| d.parse::<u16>().ok()) {
            return code / 100 == d;
        }
        match part.split_once('-') {
            Some((a, b)) => match (a.trim().parse::<u16>(), b.trim().parse::<u16>()) {
                (Ok(a), Ok(b)) => (a..=b).contains(&code),
                _ => false,
            },
            None => part.parse::<u16>().is_ok_and(|c| c == code),
        }
    })
}

pub fn valid_status_spec(spec: &str) -> bool {
    spec.trim().is_empty()
        || spec.split(',').map(str::trim).all(|p| {
            p.strip_suffix("xx").is_some_and(|d| matches!(d, "1" | "2" | "3" | "4" | "5"))
                || p.parse::<u16>().is_ok_and(|c| (100..600).contains(&c))
                || p.split_once('-').is_some_and(|(a, b)| {
                    matches!((a.trim().parse::<u16>(), b.trim().parse::<u16>()), (Ok(a), Ok(b)) if a <= b && (100..600).contains(&a) && b < 600)
                })
        })
}

// ---- vendored from the app (core/src/services/check.rs, 8eedcf2) ----

/// Scheme, host (without IPv6 brackets), port, and request target.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    /// NODE: `http`, `https` or `tcp`.
    pub scheme: &'static str,
    pub host: String,
    pub port: u16,
    /// `host[:port]` exactly as written, for the Host header.
    pub authority: String,
    pub path: String,
}

pub fn parse_target(url: &str) -> Result<Target, String> {
    let (scheme, rest) = url.split_once("://").ok_or("not an http(s) or tcp URL")?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "https" => "https",
        "http" => "http",
        "tcp" => "tcp",
        _ => return Err("not an http(s) or tcp URL".into()),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let mut path = rest[end..].split('#').next().unwrap_or("").to_owned();
    if path.is_empty() || path.starts_with('?') {
        path.insert(0, '/');
    }
    let default_port = match scheme {
        "https" => 443,
        "http" => 80,
        _ => 0,
    };
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, after) = v6.split_once(']').ok_or("bad IPv6 address")?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| "bad port")?,
            None if after.is_empty() => default_port,
            None => return Err("bad IPv6 address".into()),
        };
        (h.to_owned(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_owned(), p.parse().map_err(|_| "bad port")?),
            None => (authority.to_owned(), default_port),
        }
    };
    if host.is_empty() {
        return Err("missing host".into());
    }
    if port == 0 {
        return Err("tcp:// needs a port".into());
    }
    // NODE: nothing that could split the request line or a header.
    if host.chars().chain(path.chars()).any(|c| c.is_control() || c == ' ') || authority.contains(['@', ' ']) {
        return Err("bad characters in the URL".into());
    }
    Ok(Target { scheme, host, port, authority: authority.to_owned(), path })
}

fn failed(error: impl Into<String>, tls: Option<TlsInfo>) -> CheckResult {
    CheckResult { up: false, status: None, latency_ms: None, error: Some(error.into()), tls }
}

fn connect_error(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "connection refused".into(),
        // NODE: systemd's IPAddressDeny makes connect fail with EPERM.
        std::io::ErrorKind::PermissionDenied => "address not allowed by [checks] networks".into(),
        _ => e.to_string(),
    }
}

fn expect_label(spec: &str) -> &str {
    if spec.trim().is_empty() {
        "200-399"
    } else {
        spec.trim()
    }
}

fn finish(res: Result<u16, String>, started: Instant, expect_status: &str, tls: Option<TlsInfo>) -> CheckResult {
    match res {
        Ok(status) => {
            let up = status_matches(expect_status, status);
            CheckResult {
                up,
                status: Some(status),
                latency_ms: Some(u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX)),
                error: (!up).then(|| format!("HTTP {status} isn't in {}", expect_label(expect_status))),
                tls,
            }
        }
        Err(e) => failed(e, tls),
    }
}

#[derive(Default)]
struct Captured {
    leaf: Option<Vec<u8>>,
    problem: Option<String>,
    verified: bool,
}

impl std::fmt::Debug for Captured {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Captured").field("verified", &self.verified).finish()
    }
}

impl Captured {
    fn info(&self) -> Option<TlsInfo> {
        let leaf = self.leaf.as_ref()?;
        let (expires, subject, issuer) = match x509_parser::parse_x509_certificate(leaf) {
            Ok((_, cert)) => (
                Some(cert.validity().not_after.timestamp()),
                Some(cert.subject().to_string()),
                Some(cert.issuer().to_string()),
            ),
            Err(_) => (None, None, None),
        };
        Some(TlsInfo { trusted: self.verified, problem: self.problem.clone(), expires, subject, issuer })
    }
}

/// Wraps the OS verifier: records the leaf and the verdict, then lets the
/// handshake continue so the HTTP status can still be read. Handshake
/// signatures are still checked.
#[derive(Debug)]
struct CapturingVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    captured: Arc<Mutex<Captured>>,
}

impl ServerCertVerifier for CapturingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let verdict = self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now);
        if let Ok(mut c) = self.captured.lock() {
            c.leaf = Some(end_entity.to_vec());
            c.verified = verdict.is_ok();
            c.problem = verdict.err().map(|e| describe_tls_error(&e));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn describe_tls_error(e: &rustls::Error) -> String {
    use rustls::CertificateError as C;
    match e {
        rustls::Error::InvalidCertificate(c) => match c {
            C::Expired | C::ExpiredContext { .. } => "expired".into(),
            C::NotValidYet | C::NotValidYetContext { .. } => "not valid yet".into(),
            C::UnknownIssuer => "unknown issuer or self-signed".into(),
            C::NotValidForName | C::NotValidForNameContext { .. } => "doesn't match the host name".into(),
            C::Revoked => "revoked".into(),
            other => format!("{other:?}"),
        },
        other => other.to_string(),
    }
}

fn tls_connector(captured: Arc<Mutex<Captured>>) -> Result<tokio_rustls::TlsConnector, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let inner: Arc<dyn ServerCertVerifier> = Arc::new(rustls_platform_verifier::Verifier::new(provider.clone())?);
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(CapturingVerifier { inner, captured }))
        .with_no_client_auth();
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

/// A failed service check, and its certificate if it expires within `days`.
pub fn conditions(
    c: &ServiceCheck,
    r: &CheckResult,
    rules: &nightgrid_node_proto::AlertRules,
    now: i64,
) -> Vec<Condition> {
    let mut out = Vec::new();
    if !r.up && rules.service_down {
        out.push(Condition {
            key: format!("svc:{}", c.id),
            kind: AlertKind::ServiceDown,
            severity: Severity::Critical,
            title: c.name.clone(),
            detail: r.error.clone().unwrap_or_else(|| "check failed".into()),
        });
    }
    let left = r.tls.as_ref().and_then(|t| t.expires).map(|e| (e - now).div_euclid(86_400));
    if let (Some(left), true) = (left, rules.cert_expiring) {
        if left < i64::from(rules.cert_days) {
            out.push(Condition {
                key: format!("svc:{}:cert", c.id),
                kind: AlertKind::CertExpiring,
                severity: if left <= 3 { Severity::Critical } else { Severity::Warn },
                title: c.name.clone(),
                detail: match left {
                    l if l < 0 => "certificate has expired".into(),
                    0 => "certificate expires today".into(),
                    1 => "certificate expires tomorrow".into(),
                    l => format!("certificate expires in {l} days"),
                },
            });
        }
    }
    out
}

// ---- end of vendored code ----

/// Checks a pushed list: ids, names and URLs the runner will accept.
pub fn validate(list: &[ServiceCheck]) -> Result<(), String> {
    if list.len() > MAX_CHECKS {
        return Err(format!("at most {MAX_CHECKS} checks"));
    }
    let mut ids = std::collections::HashSet::new();
    for c in list {
        if c.id.is_empty()
            || c.id.len() > 64
            || !c.id.chars().all(|ch| ch.is_ascii_alphanumeric() || "-_.".contains(ch))
        {
            return Err(format!("bad check id {:?}", c.id));
        }
        if !ids.insert(&c.id) {
            return Err(format!("duplicate check id {:?}", c.id));
        }
        if c.name.chars().count() > 200 || c.url.len() > 2048 {
            return Err(format!("check {}: name or URL too long", c.id));
        }
        parse_target(&c.url).map_err(|e| format!("check {}: {e}", c.id))?;
        if !valid_status_spec(&c.expect_status) {
            return Err(format!("check {}: bad expected status {:?}", c.id, c.expect_status));
        }
    }
    Ok(())
}

/// Whether `ip` is inside root's `[checks] networks`.
pub fn allowed(networks: &[String], ip: IpAddr) -> bool {
    let ip = crate::firewall::canonical(ip);
    networks
        .iter()
        .filter_map(|n| crate::firewall::parse_cidr(n))
        .any(|(net, bits)| crate::firewall::in_cidr(ip, (net, bits)))
}

/// Addresses of `host` inside root's networks, in resolver order.
async fn resolve(host: &str, port: u16, networks: &[String]) -> Result<Vec<SocketAddr>, String> {
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => {
            tokio::net::lookup_host((host, port)).await.map_err(|e| format!("can't resolve {host}: {e}"))?.collect()
        }
    };
    let ok: Vec<SocketAddr> = addrs.iter().filter(|a| allowed(networks, a.ip())).copied().collect();
    if ok.is_empty() {
        return Err(match addrs.first() {
            Some(a) => format!("{} is outside [checks] networks", a.ip()),
            None => format!("{host} has no address"),
        });
    }
    Ok(ok)
}

/// Connects to the first address that answers (`localhost` may be ::1 first
/// while the service listens on 127.0.0.1 only).
async fn connect(addrs: &[SocketAddr]) -> std::io::Result<tokio::net::TcpStream> {
    let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "no address");
    for a in addrs {
        match tokio::net::TcpStream::connect(a).await {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Reads up to the end of the status line and returns the code.
async fn request<S: AsyncRead + AsyncWrite + Unpin>(mut io: S, t: &Target) -> Result<u16, String> {
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: Nightgrid-Node/{} (health check)\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        t.path,
        t.authority,
        env!("CARGO_PKG_VERSION")
    );
    io.write_all(req.as_bytes()).await.map_err(|e| format!("HTTP error: {e}"))?;
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 512];
    while !buf.windows(2).any(|w| w == b"\r\n") {
        let n = io.read(&mut chunk).await.map_err(|e| format!("HTTP error: {e}"))?;
        if n == 0 {
            return Err("HTTP error: connection closed before a response".into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 8192 {
            return Err("HTTP error: no status line".into());
        }
    }
    let line = String::from_utf8_lossy(&buf);
    let line = line.lines().next().unwrap_or_default();
    let mut parts = line.split_whitespace();
    match (parts.next(), parts.next().and_then(|c| c.parse::<u16>().ok())) {
        (Some(v), Some(code)) if v.starts_with("HTTP/") && (100..600).contains(&code) => Ok(code),
        _ => Err(format!("HTTP error: not an HTTP response ({:?})", line.chars().take(40).collect::<String>())),
    }
}

/// Runs one check. Never fails: problems are a down result.
pub async fn run_one(c: &ServiceCheck, networks: &[String], timeout: Duration) -> CheckResult {
    let target = match parse_target(&c.url) {
        Ok(t) => t,
        Err(e) => return failed(e, None),
    };
    let started = Instant::now();
    let addrs = match tokio::time::timeout(timeout, resolve(&target.host, target.port, networks)).await {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => return failed(e, None),
        Err(_) => return failed("timed out resolving", None),
    };
    let tcp = match tokio::time::timeout(timeout.saturating_sub(started.elapsed()), connect(&addrs)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return failed(connect_error(&e), None),
        Err(_) => return failed("timed out connecting", None),
    };
    let _ = tcp.set_nodelay(true);
    if target.scheme == "tcp" {
        return CheckResult {
            up: true,
            status: None,
            latency_ms: Some(u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX)),
            error: None,
            tls: None,
        };
    }
    let remaining = timeout.saturating_sub(started.elapsed());
    if target.scheme == "http" {
        let r = tokio::time::timeout(remaining, request(tcp, &target))
            .await
            .unwrap_or_else(|_| Err("timed out waiting for a response".into()));
        return finish(r, started, &c.expect_status, None);
    }
    let captured = Arc::new(Mutex::new(Captured::default()));
    let connector = match tls_connector(captured.clone()) {
        Ok(c) => c,
        Err(e) => return failed(format!("TLS setup failed: {e}"), None),
    };
    let Ok(name) = ServerName::try_from(target.host.clone()) else {
        return failed("host name isn't valid for TLS", None);
    };
    let tls_stream = tokio::time::timeout(remaining, connector.connect(name, tcp)).await;
    let tls = captured.lock().ok().and_then(|c| c.info());
    let stream = match tls_stream {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return failed(format!("TLS handshake failed: {e}"), tls),
        Err(_) => return failed("timed out during TLS handshake", tls),
    };
    let remaining = timeout.saturating_sub(started.elapsed());
    let r = tokio::time::timeout(remaining, request(stream, &target))
        .await
        .unwrap_or_else(|_| Err("timed out waiting for a response".into()));
    let mut result = finish(r, started, &c.expect_status, tls);
    if let Some(t) = &result.tls {
        if !t.trusted && !c.allow_untrusted && result.up {
            result.up = false;
            result.error = Some(format!(
                "certificate not trusted ({}); tick \"allow untrusted\" for self-signed or internal CAs",
                t.problem.as_deref().unwrap_or("unknown reason")
            ));
        }
    }
    result
}

/// One round over every check, a few at a time.
pub async fn run_round(list: &[ServiceCheck], networks: &[String], timeout: Duration) -> Vec<CheckReading> {
    let sem = Arc::new(tokio::sync::Semaphore::new(PARALLEL));
    let mut handles = Vec::new();
    for c in list.iter().cloned() {
        let sem = sem.clone();
        let networks = networks.to_vec();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await;
            let result = run_one(&c, &networks, timeout).await;
            CheckReading { id: c.id, at: crate::daemon::now_secs(), result }
        }));
    }
    let mut out = Vec::new();
    for h in handles {
        if let Ok(r) = h.await {
            out.push(r);
        }
    }
    out
}

/// The service's side: the pushed list and the latest readings.
#[derive(Debug, Default)]
pub struct ChecksState {
    pub list: Vec<ServiceCheck>,
    pub readings: Vec<CheckReading>,
    /// When the runner last reported (unix seconds).
    pub reported: i64,
}

impl ChecksState {
    /// The runner reported within three rounds.
    pub fn fresh(&self, interval: u32, now: i64) -> bool {
        self.reported > 0 && now - self.reported <= 3 * i64::from(interval) + 30
    }

    /// Alert conditions for checks still in the list; none while the
    /// runner is silent (unknown is not down).
    pub fn conditions(&self, rules: &nightgrid_node_proto::AlertRules, interval: u32, now: i64) -> Vec<Condition> {
        if !self.fresh(interval, now) {
            return Vec::new();
        }
        self.readings
            .iter()
            .filter_map(|r| self.list.iter().find(|c| c.id == r.id).map(|c| conditions(c, &r.result, rules, now)))
            .flatten()
            .collect()
    }

    /// Takes a report: keeps readings of listed checks only.
    pub fn report(&mut self, r: ChecksReport, now: i64) {
        self.readings = r.readings.into_iter().filter(|x| self.list.iter().any(|c| c.id == x.id)).collect();
        self.reported = now;
    }
}

pub fn load(store: &crate::store::Store) -> Vec<ServiceCheck> {
    store.meta(META_CHECKS).ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

pub fn save(store: &crate::store::Store, list: &[ServiceCheck]) -> crate::store::Result<()> {
    store.set_meta(META_CHECKS, &serde_json::to_string(list).unwrap_or_default())
}

/// `nightgrid-node checker`: the runner unit's loop. Reads the list from
/// the node, runs it, reports, sleeps `[checks] interval_secs`.
pub fn checker(config_path: &Path) -> Result<(), String> {
    checker_until(config_path, &|| false)
}

/// [`checker`] until `stop` says so (the Windows service's stop request).
pub fn checker_until(config_path: &Path, stop: &dyn Fn() -> bool) -> Result<(), String> {
    let config = crate::config::Config::load(config_path).map_err(|e| format!("config: {e}"))?;
    if !config.checks.enabled {
        return Err("[checks] is off".into());
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let interval = Duration::from_secs(config.checks.interval_secs.into());
    loop {
        let started = Instant::now();
        match round(&config, &rt) {
            Ok(n) => crate::debug!("checked {n} services"),
            Err(e) => crate::warn!("check round: {e}"),
        }
        let until = (started + interval).max(Instant::now() + Duration::from_secs(5));
        while Instant::now() < until {
            if stop() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(500).min(until.saturating_duration_since(Instant::now())));
        }
        if stop() {
            return Ok(());
        }
    }
}

fn round(config: &crate::config::Config, rt: &tokio::runtime::Runtime) -> Result<usize, String> {
    let mut client =
        crate::client::Client::connect(&config.server.socket, "nightgrid-node checker").map_err(|e| e.to_string())?;
    let list: ChecksList =
        serde_json::from_value(client.call(method::CHECKS_GET, serde_json::Value::Null).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let readings = rt.block_on(run_round(&list.checks, &config.checks.networks, TIMEOUT));
    let n = readings.len();
    let report = serde_json::to_value(ChecksReport { readings }).map_err(|e| e.to_string())?;
    client.call(method::CHECKS_REPORT, report).map_err(|e| e.to_string())?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightgrid_node_proto::AlertRules;
    use tokio::net::TcpListener;

    fn chk(id: &str, url: &str) -> ServiceCheck {
        ServiceCheck {
            id: id.into(),
            name: format!("svc {id}"),
            url: url.into(),
            expect_status: String::new(),
            allow_untrusted: false,
        }
    }

    #[test]
    fn targets_and_validation() {
        let t = parse_target("https://[fd00::1]:8443/x?y#z").unwrap();
        assert_eq!((t.scheme, t.host.as_str(), t.port, t.path.as_str()), ("https", "fd00::1", 8443, "/x?y"));
        let t = parse_target("tcp://nas.lan:22").unwrap();
        assert_eq!((t.scheme, t.port), ("tcp", 22));
        assert!(parse_target("tcp://nas.lan").is_err());
        assert!(parse_target("ftp://x").is_err());
        assert!(parse_target("http://a/b c").is_err());
        assert!(parse_target("http://a/\r\nX: y").is_err());
        assert!(parse_target("http://user@host/").is_err());
        assert!(validate(&[chk("1", "http://a"), chk("2", "tcp://b:1")]).is_ok());
        assert!(validate(&[chk("1", "http://a"), chk("1", "http://b")]).is_err());
        assert!(validate(&[chk("a b", "http://a")]).is_err());
        let mut c = chk("1", "http://a");
        c.expect_status = "200-x".into();
        assert!(validate(&[c]).is_err());
        assert!(status_matches("", 301) && !status_matches("", 404) && status_matches("2xx,401", 401));
    }

    #[test]
    fn networks_limit_where_checks_go() {
        let nets = vec!["192.168.0.0/16".to_owned(), "127.0.0.0/8".to_owned()];
        assert!(allowed(&nets, "192.168.1.5".parse().unwrap()));
        assert!(allowed(&nets, "::ffff:127.0.0.1".parse().unwrap()));
        assert!(!allowed(&nets, "8.8.8.8".parse().unwrap()));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let r = rt.block_on(run_one(&chk("1", "http://8.8.8.8/"), &nets, Duration::from_secs(2)));
        assert_eq!(r.error.as_deref(), Some("8.8.8.8 is outside [checks] networks"));
    }

    #[test]
    fn http_and_tcp_against_a_local_server() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            tokio::spawn(async move {
                loop {
                    let (mut s, _) = l.accept().await.unwrap();
                    let mut buf = [0u8; 1024];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let code = if req.starts_with("GET /missing ") { 404 } else { 204 };
                    assert!(req.contains(&format!("Host: 127.0.0.1:{port}\r\n")), "{req}");
                    let _ = s.write_all(format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\n\r\n").as_bytes()).await;
                }
            });
            let nets = vec!["127.0.0.0/8".to_owned()];
            let list = vec![
                chk("ok", &format!("http://127.0.0.1:{port}/")),
                chk("404", &format!("http://127.0.0.1:{port}/missing")),
                chk("tcp", &format!("tcp://127.0.0.1:{port}")),
                chk("closed", "tcp://127.0.0.1:1"),
            ];
            let r = run_round(&list, &nets, Duration::from_secs(3)).await;
            let by = |id: &str| r.iter().find(|x| x.id == id).unwrap().result.clone();
            assert!(by("ok").up && by("ok").status == Some(204));
            assert_eq!(by("404").error.as_deref(), Some("HTTP 404 isn't in 200-399"));
            assert!(by("tcp").up && by("tcp").latency_ms.is_some());
            assert_eq!(by("closed").error.as_deref(), Some("connection refused"));
        });
    }

    #[test]
    fn https_to_a_closed_port_and_conditions() {
        let rules = AlertRules::default();
        let c = chk("7", "https://x");
        let down = failed("connection refused", None);
        let cs = conditions(&c, &down, &rules, 0);
        assert_eq!(
            (cs[0].key.as_str(), cs[0].kind, cs[0].severity),
            ("svc:7", AlertKind::ServiceDown, Severity::Critical)
        );
        let tls = TlsInfo { trusted: true, problem: None, expires: Some(5 * 86_400 + 10), subject: None, issuer: None };
        let up = CheckResult { up: true, status: Some(200), latency_ms: Some(3), error: None, tls: Some(tls) };
        let cs = conditions(&c, &up, &rules, 0);
        assert_eq!((cs[0].key.as_str(), cs[0].detail.as_str()), ("svc:7:cert", "certificate expires in 5 days"));
        let mut st = ChecksState { list: vec![c.clone()], ..Default::default() };
        st.report(
            ChecksReport {
                readings: vec![
                    CheckReading { id: "7".into(), at: 100, result: down.clone() },
                    CheckReading { id: "gone".into(), at: 100, result: down },
                ],
            },
            100,
        );
        assert_eq!(st.readings.len(), 1);
        assert_eq!(st.conditions(&rules, 60, 150).len(), 1);
        assert!(st.conditions(&rules, 60, 100 + 300).is_empty(), "silent runner: unknown, not down");
    }
}
