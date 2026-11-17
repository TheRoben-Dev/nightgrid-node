//! Node-to-node link (N8b), opt-in: `nightgrid-node allow peers` installs a
//! separate unit (`nightgrid-node peer`) that talks to the nodes the app
//! paired with this one, over mutual TLS, on the tailnet.
//!
//! - **Identity:** the link unit makes an Ed25519 key and a self-signed
//!   certificate on first start, in its own state directory. Its SHA-256 is
//!   the node's fingerprint (`peers.latest`).
//! - **Pairing:** the app, which already reaches every host over SSH, gives
//!   each node the other's fingerprint and address (`peers.set`). A peer is
//!   accepted, in either direction, only if its certificate has a pinned
//!   fingerprint: no certificate authority, no trust on first use.
//! - **Requests:** one typed request per connection, answered only if root
//!   lists it in `[peers] serve`. `ping` returns version, host name and uptime.
//! - **Reach:** the unit may use IP only to `[peers] networks` (tailnet
//!   ranges by default), enforced by systemd; it listens on one address,
//!   never every interface. The main service stays offline and only takes
//!   the unit's reports over its socket, like the check runner (N-043).

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use nightgrid_node_proto::{EventKind, Level, Peer, PeerReading, PeersLatest, PeersList, PeersReport};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::Config;
use crate::events::NewEvent;

const META_PEERS: &str = "peers";
pub const MAX_PEERS: usize = 64;
/// Longest request or answer line on the link.
const MAX_LINE: usize = 4096;
const TIMEOUT: Duration = Duration::from_secs(10);
/// Consecutive failed probes before a peer counts as unreachable.
const FAILS: u32 = 2;
/// How often the link looks for a changed peer list.
const LIST_CHECK: Duration = Duration::from_secs(10);
/// How soon a peer that didn't answer is tried again.
const RETRY: Duration = Duration::from_secs(15);

// ---- the main service's side -------------------------------------------------

pub fn hex_ok(s: &str, len: usize) -> bool {
    s.len() == len && s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Checks `peers.set`.
pub fn validate(list: &[Peer]) -> Result<(), String> {
    if list.len() > MAX_PEERS {
        return Err(format!("at most {MAX_PEERS} peers"));
    }
    for (i, p) in list.iter().enumerate() {
        let who = if p.name.is_empty() { p.id.clone() } else { p.name.clone() };
        if !hex_ok(&p.id, 32) {
            return Err(format!("peer {who}: id must be the peer's node id (32 lowercase hex digits)"));
        }
        if p.name.trim().is_empty() || p.name.chars().count() > 80 || p.name.chars().any(char::is_control) {
            return Err(format!("peer {who}: name must be 1-80 characters"));
        }
        if p.address.parse::<SocketAddr>().map_or(true, |a| a.ip().is_unspecified() || a.port() == 0) {
            return Err(format!("peer {who}: address must be ip:port or [ipv6]:port"));
        }
        if !hex_ok(&p.fingerprint, 64) {
            return Err(format!("peer {who}: fingerprint must be 64 lowercase hex digits"));
        }
        if list[..i].iter().any(|q| q.id == p.id || q.fingerprint == p.fingerprint) {
            return Err(format!("peer {who} is listed twice"));
        }
    }
    Ok(())
}

pub fn load(store: &crate::store::Store) -> Vec<Peer> {
    store.meta(META_PEERS).ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

pub fn save(store: &crate::store::Store, list: &[Peer]) -> crate::store::Result<()> {
    store.set_meta(META_PEERS, &serde_json::to_string(list).unwrap_or_default())
}

/// What the main service knows about the link.
#[derive(Debug, Default)]
pub struct PeersState {
    pub list: Vec<Peer>,
    pub report: PeersReport,
    pub reported: i64,
    /// Consecutive failures and whether the peer is counted as down, by id.
    pub fails: std::collections::BTreeMap<String, (u32, bool)>,
}

impl PeersState {
    /// A new list from the app; readings of removed peers go.
    pub fn set(&mut self, list: Vec<Peer>) {
        self.report.readings.retain(|r| list.iter().any(|p| p.id == r.id));
        self.fails.retain(|id, _| list.iter().any(|p| &p.id == id));
        self.list = list;
    }

    pub fn fresh(&self, interval: u32, now: i64) -> bool {
        self.reported > 0 && now - self.reported <= 3 * i64::from(interval) + 30
    }

    pub fn latest(&self, cfg: &Config, now: i64) -> PeersLatest {
        let fresh = self.fresh(cfg.peers.interval_secs, now);
        PeersLatest {
            access: if fresh { "ok" } else { "denied" }.into(),
            fingerprint: (!self.report.fingerprint.is_empty()).then(|| self.report.fingerprint.clone()),
            listening: self.report.listening.clone(),
            readings: self.report.readings.clone(),
        }
    }

    /// Takes a report; returns events for peers that went down or came back.
    pub fn take(&mut self, r: PeersReport, now: i64) -> Vec<NewEvent> {
        let mut out = Vec::new();
        let readings: Vec<PeerReading> =
            r.readings.into_iter().filter(|x| self.list.iter().any(|p| p.id == x.id)).collect();
        for x in &readings {
            let name = self.list.iter().find(|p| p.id == x.id).map_or(x.id.clone(), |p| p.name.clone());
            let e = self.fails.entry(x.id.clone()).or_insert((0, false));
            if x.ok {
                if e.1 {
                    out.push(NewEvent::new(
                        now,
                        EventKind::Other,
                        Level::Ok,
                        format!("peer {name} reachable again"),
                        x.rtt_ms.map(|r| format!("over the node-to-node link, {r} ms")).unwrap_or_default(),
                    ));
                }
                *e = (0, false);
            } else {
                e.0 += 1;
                if e.0 >= FAILS && !e.1 {
                    e.1 = true;
                    out.push(NewEvent::new(
                        now,
                        EventKind::Other,
                        Level::Warn,
                        format!("peer {name} unreachable"),
                        x.error.clone().unwrap_or_else(|| "no answer over the node-to-node link".into()),
                    ));
                }
            }
        }
        let list = &self.list;
        self.fails.retain(|id, _| list.iter().any(|p| &p.id == id));
        self.report = PeersReport { readings, ..r };
        self.reported = now;
        out
    }
}

// ---- identity -----------------------------------------------------------------

pub fn fingerprint(cert: &[u8]) -> String {
    Sha256::digest(cert).iter().map(|b| format!("{b:02x}")).collect()
}

pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: Vec<u8>,
    pub fingerprint: String,
}

/// Loads the link's key and certificate from `dir`, making them on first use.
pub fn identity(dir: &Path) -> Result<Identity, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let (cert_p, key_p) = (dir.join("cert.der"), dir.join("key.pk8"));
    if !cert_p.exists() || !key_p.exists() {
        let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).map_err(|e| e.to_string())?;
        let mut params = rcgen::CertificateParams::new(vec!["nightgrid-node".into()]).map_err(|e| e.to_string())?;
        params.not_before = rcgen::date_time_ymd(2025, 1, 1);
        params.not_after = rcgen::date_time_ymd(4000, 1, 1);
        params.distinguished_name.push(rcgen::DnType::CommonName, "Nightgrid Node link");
        let cert = params.self_signed(&kp).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let write = |p: &Path, b: &[u8]| -> Result<(), String> {
            let tmp = p.with_extension("tmp");
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| format!("{}: {e}", tmp.display()))?;
            std::io::Write::write_all(&mut f, b).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, p).map_err(|e| e.to_string())
        };
        write(&key_p, &kp.serialize_der())?;
        write(&cert_p, cert.der())?;
        crate::info!("made the node-to-node link key in {}", dir.display());
    }
    let cert = std::fs::read(&cert_p).map_err(|e| format!("{}: {e}", cert_p.display()))?;
    let key = std::fs::read(&key_p).map_err(|e| format!("{}: {e}", key_p.display()))?;
    Ok(Identity { fingerprint: fingerprint(&cert), cert: CertificateDer::from(cert), key })
}

// ---- pinned verification ---------------------------------------------------------

/// Accepts exactly the certificates whose fingerprints are listed, and
/// checks the handshake signatures with them.
#[derive(Debug)]
pub struct Pinned {
    pub allowed: Arc<RwLock<Vec<String>>>,
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl Pinned {
    pub fn new(allowed: Arc<RwLock<Vec<String>>>) -> Self {
        Self { allowed, algs: rustls::crypto::ring::default_provider().signature_verification_algorithms }
    }

    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let fp = fingerprint(cert);
        if self.allowed.read().map(|a| a.contains(&fp)).unwrap_or(false) {
            Ok(())
        } else {
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure))
        }
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity).map(|_| ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not used on the link".into()))
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.algs)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

impl ClientCertVerifier for Pinned {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity).map(|_| ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not used on the link".into()))
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.algs)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn key(id: &Identity) -> PrivateKeyDer<'static> {
    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(id.key.clone()))
}

pub fn server_config(id: &Identity, allowed: Arc<RwLock<Vec<String>>>) -> Result<rustls::ServerConfig, String> {
    rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .with_client_cert_verifier(Arc::new(Pinned::new(allowed)))
        .with_single_cert(vec![id.cert.clone()], key(id))
        .map_err(|e| e.to_string())
}

pub fn client_config(id: &Identity, peer_fp: &str) -> Result<rustls::ClientConfig, String> {
    rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned::new(Arc::new(RwLock::new(vec![peer_fp.to_owned()])))))
        .with_client_auth_cert(vec![id.cert.clone()], key(id))
        .map_err(|e| e.to_string())
}

// ---- the link protocol ------------------------------------------------------------

/// What this node tells a peer that pings it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Pong {
    pub id: String,
    pub hostname: String,
    pub version: String,
    pub uptime: u64,
}

/// Answers one request line. `serve` is root's list.
pub fn answer(line: &str, serve: &[String], me: &Pong) -> String {
    let v: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
    let method = v["method"].as_str().unwrap_or("");
    let r = match method.strip_prefix("peer.") {
        Some("ping") if serve.iter().any(|s| s == "ping") => serde_json::json!({"ok": true, "result": me}),
        Some(m) if serve.iter().any(|s| s == m) => {
            serde_json::json!({"ok": false, "error": format!("{method} isn't available on this node yet")})
        }
        _ => serde_json::json!({"ok": false, "error": format!("{method:?} isn't served here ([peers] serve)")}),
    };
    r.to_string()
}

async fn read_line<R: tokio::io::AsyncRead + Unpin>(r: R) -> Result<String, String> {
    let mut buf = Vec::new();
    let mut r = BufReader::new(r).take(MAX_LINE as u64 + 1);
    use tokio::io::AsyncReadExt;
    let n = r.read_until(b'\n', &mut buf).await.map_err(|e| e.to_string())?;
    if n == 0 || n > MAX_LINE {
        return Err("no answer or an oversized line".into());
    }
    String::from_utf8(buf).map_err(|_| "not UTF-8".into())
}

/// Pings one peer: connect, mutual TLS with its pinned fingerprint, one request.
pub async fn ping(id: &Identity, peer: &Peer) -> Result<(Pong, u32), String> {
    let addr: SocketAddr = peer.address.parse().map_err(|_| "bad address".to_string())?;
    let started = Instant::now();
    let fut = async {
        let tcp = tokio::net::TcpStream::connect(addr).await.map_err(|e| describe_io(&e))?;
        let conn = tokio_rustls::TlsConnector::from(Arc::new(client_config(id, &peer.fingerprint)?));
        let name = ServerName::try_from("nightgrid-node").map_err(|e| e.to_string())?;
        let mut tls = conn.connect(name, tcp).await.map_err(|e| describe_tls(&e))?;
        tls.write_all(b"{\"method\":\"peer.ping\"}\n").await.map_err(|e| e.to_string())?;
        let line = read_line(&mut tls).await?;
        let v: serde_json::Value = serde_json::from_str(&line).map_err(|_| "a garbled answer".to_string())?;
        if v["ok"].as_bool() != Some(true) {
            return Err(v["error"].as_str().unwrap_or("refused").to_owned());
        }
        serde_json::from_value::<Pong>(v["result"].clone()).map_err(|_| "a garbled answer".to_string())
    };
    let pong = tokio::time::timeout(TIMEOUT, fut).await.map_err(|_| "timed out".to_string())??;
    if pong.id != peer.id {
        return Err(format!("answered as node {}, not {}", pong.id, peer.id));
    }
    Ok((pong, started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32))
}

fn describe_io(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "connection refused (is the link on there?)".into(),
        std::io::ErrorKind::PermissionDenied => "not allowed by [peers] networks".into(),
        _ => e.to_string(),
    }
}

fn describe_tls(e: &std::io::Error) -> String {
    let s = e.to_string();
    if s.contains("ApplicationVerificationFailure") || s.contains("certificate") {
        "the peer's certificate isn't the paired one (pair the nodes again in the app)".into()
    } else if s.contains("eof") || s.contains("reset") {
        "the peer closed the connection (it may not have this node paired)".into()
    } else {
        s
    }
}

/// Serves one connection.
async fn serve_one(
    tcp: tokio::net::TcpStream,
    acceptor: tokio_rustls::TlsAcceptor,
    serve: Arc<Vec<String>>,
    me: Arc<RwLock<Pong>>,
) {
    let fut = async {
        let mut tls = acceptor.accept(tcp).await.map_err(|e| e.to_string())?;
        let line = read_line(&mut tls).await?;
        let pong = me.read().map(|p| p.clone()).unwrap_or_default();
        let mut out = answer(&line, &serve, &Pong { uptime: uptime(), ..pong });
        out.push('\n');
        tls.write_all(out.as_bytes()).await.map_err(|e| e.to_string())?;
        tls.shutdown().await.map_err(|e| e.to_string())
    };
    if let Ok(Err(e)) = tokio::time::timeout(TIMEOUT, fut).await {
        crate::debug!("peer link: {e}");
    }
}

fn uptime() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split('.').next().and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

/// This host's first address in 100.64.0.0/10 (Tailscale and Headscale).
pub fn tailnet_address() -> Option<IpAddr> {
    let mut out = None;
    // SAFETY: getifaddrs fills a list we walk read-only and free once.
    unsafe {
        let mut ifa: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifa) != 0 {
            return None;
        }
        let mut cur = ifa;
        while !cur.is_null() {
            let a = (*cur).ifa_addr;
            if !a.is_null() && i32::from((*a).sa_family) == libc::AF_INET {
                let sin = &*(a as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                let o = ip.octets();
                if o[0] == 100 && (64..128).contains(&o[1]) {
                    out = Some(IpAddr::V4(ip));
                    break;
                }
            }
            cur = (*cur).ifa_next;
        }
        libc::freeifaddrs(ifa);
    }
    out
}

/// Where to listen, per `[peers] listen`.
pub fn listen_addr(cfg: &Config) -> Option<SocketAddr> {
    match cfg.peers.listen.as_str() {
        "" => None,
        "tailnet" => tailnet_address().map(|ip| SocketAddr::new(ip, cfg.peers.port)),
        a => a.parse().ok(),
    }
}

fn state_dir() -> PathBuf {
    std::env::var_os("STATE_DIRECTORY")
        .map(|s| PathBuf::from(s.to_string_lossy().split(':').next().unwrap_or_default().to_owned()))
        .unwrap_or_else(|| PathBuf::from("/var/lib/nightgrid-node-peer"))
}

/// `nightgrid-node peer`: the link unit's loop.
pub fn link(config_path: &Path) -> Result<(), String> {
    let cfg = Config::load(config_path).map_err(|e| format!("config: {e}"))?;
    let id = Arc::new(identity(&state_dir())?);
    crate::info!("node-to-node link, fingerprint {}", id.fingerprint);
    let allowed: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(Vec::new()));
    let me = Arc::new(RwLock::new(Pong { version: env!("CARGO_PKG_VERSION").into(), ..Default::default() }));
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let mut listening: Option<SocketAddr> = None;
    let mut listener: Option<tokio::net::TcpListener> = None;
    let serve = Arc::new(cfg.peers.serve.clone());
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config(&id, allowed.clone())?));
    let interval = Duration::from_secs(cfg.peers.interval_secs.into());
    let socket = cfg.server.socket.clone();
    rt.block_on(async {
        let mut next_round = Instant::now();
        let mut next_check = Instant::now();
        let mut known: Option<PeersList> = None;
        loop {
            if listener.is_none() {
                if let Some(addr) = listen_addr(&cfg) {
                    match tokio::net::TcpListener::bind(addr).await {
                        Ok(l) => {
                            crate::info!("listening for peers on {addr}");
                            listening = Some(addr);
                            listener = Some(l);
                            // Report the address now.
                            next_round = Instant::now();
                        }
                        Err(e) => crate::warn!("can't listen on {addr}: {e}"),
                    }
                }
            }
            // A cheap look at the list, so a pairing from the app takes
            // effect in seconds rather than at the next round.
            if Instant::now() >= next_check {
                next_check = Instant::now() + LIST_CHECK;
                match fetch(&socket).await {
                    Ok((node, host, list, missing)) => {
                        if let Ok(mut m) = me.write() {
                            m.id = node;
                            m.hostname = host;
                        }
                        if let Ok(mut a) = allowed.write() {
                            *a = list.peers.iter().map(|p| p.fingerprint.clone()).collect();
                        }
                        if known.as_ref() != Some(&list) || missing {
                            next_round = Instant::now();
                            known = Some(list);
                        }
                    }
                    Err(e) => crate::warn!("can't read the peer list: {e}"),
                }
            }
            if Instant::now() >= next_round {
                next_round = Instant::now() + interval;
                if let Some(list) = &known {
                    match round(&socket, &id, list, listening).await {
                        // A peer that didn't answer is tried again sooner,
                        // so an outage (two misses) or a recovery shows quickly.
                        Ok(false) => next_round = Instant::now() + interval.min(RETRY),
                        Ok(true) => {}
                        Err(e) => crate::warn!("peer round: {e}"),
                    }
                }
            }
            let wait = next_round.min(next_check).saturating_duration_since(Instant::now());
            match &listener {
                Some(l) => {
                    if let Ok(Ok((tcp, _))) = tokio::time::timeout(wait, l.accept()).await {
                        tokio::spawn(serve_one(tcp, acceptor.clone(), serve.clone(), me.clone()));
                    }
                }
                None => tokio::time::sleep(wait).await,
            }
        }
    })
}

/// This node's id and host name, and the paired peers.
async fn fetch(socket: &Path) -> Result<(String, String, PeersList, bool), String> {
    let socket = socket.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut c = crate::client::Client::connect(&socket, "nightgrid-node peer").map_err(|e| e.to_string())?;
        let v = c.call(nightgrid_node_proto::method::PEERS_GET, serde_json::Value::Null).map_err(|e| e.to_string())?;
        let list: PeersList = serde_json::from_value(v).map_err(|e| e.to_string())?;
        // After the node restarts it has no report yet: send one now.
        let l =
            c.call(nightgrid_node_proto::method::PEERS_LATEST, serde_json::Value::Null).map_err(|e| e.to_string())?;
        let missing = l["fingerprint"].is_null();
        Ok::<_, String>((c.welcome.node.node_id.clone(), c.welcome.node.hostname.clone(), list, missing))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Pings every peer and reports.
async fn round(
    socket: &Path,
    id: &Arc<Identity>,
    list: &PeersList,
    listening: Option<SocketAddr>,
) -> Result<bool, String> {
    let socket = socket.to_path_buf();
    // All at once, so one dead peer doesn't hold up the rest.
    let mut set = tokio::task::JoinSet::new();
    for (i, p) in list.peers.iter().cloned().enumerate() {
        let id = id.clone();
        set.spawn(async move {
            let at = crate::daemon::now_secs();
            let r = match ping(&id, &p).await {
                Ok((pong, rtt)) => PeerReading {
                    id: p.id.clone(),
                    at,
                    ok: true,
                    rtt_ms: Some(rtt),
                    version: Some(pong.version),
                    hostname: Some(pong.hostname),
                    error: None,
                },
                Err(e) => PeerReading {
                    id: p.id.clone(),
                    at,
                    ok: false,
                    rtt_ms: None,
                    version: None,
                    hostname: None,
                    error: Some(e),
                },
            };
            (i, r)
        });
    }
    let mut readings: Vec<(usize, PeerReading)> = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(r) = r {
            readings.push(r);
        }
    }
    readings.sort_by_key(|(i, _)| *i);
    let readings: Vec<PeerReading> = readings.into_iter().map(|(_, r)| r).collect();
    let all_ok = readings.iter().all(|r| r.ok);
    let report =
        PeersReport { fingerprint: id.fingerprint.clone(), listening: listening.map(|a| a.to_string()), readings };
    tokio::task::spawn_blocking(move || {
        let mut c = crate::client::Client::connect(&socket, "nightgrid-node peer").map_err(|e| e.to_string())?;
        c.call(nightgrid_node_proto::method::PEERS_REPORT, serde_json::to_value(&report).unwrap_or_default())
            .map_err(|e| e.to_string())
            .map(|_| all_ok)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: char, fp: &str, addr: &str) -> Peer {
        Peer { id: id.to_string().repeat(32), name: "nas".into(), address: addr.into(), fingerprint: fp.into() }
    }

    #[test]
    fn validates_peers() {
        let a = peer('a', &"1".repeat(64), "100.64.0.2:7419");
        assert!(validate(std::slice::from_ref(&a)).is_ok());
        assert!(validate(&[peer('b', &"1".repeat(64), "nas:7419")]).unwrap_err().contains("ip:port"));
        assert!(validate(&[peer('b', &"1".repeat(64), "0.0.0.0:7419")]).is_err());
        assert!(validate(&[peer('b', "xyz", "100.64.0.2:1")]).unwrap_err().contains("fingerprint"));
        let mut bad = a.clone();
        bad.id = "nope".into();
        assert!(validate(&[bad]).is_err());
        assert!(validate(&[a.clone(), a]).unwrap_err().contains("twice"));
    }

    #[test]
    fn unreachable_after_two_fails_and_back() {
        let mut s = PeersState { list: vec![peer('a', &"1".repeat(64), "100.64.0.2:7419")], ..Default::default() };
        let r = |ok| PeersReport {
            fingerprint: "f".into(),
            listening: None,
            readings: vec![PeerReading {
                id: "a".repeat(32),
                at: 0,
                ok,
                rtt_ms: ok.then_some(3),
                version: None,
                hostname: None,
                error: (!ok).then(|| "timed out".into()),
            }],
        };
        assert!(s.take(r(false), 1).is_empty());
        let e = s.take(r(false), 2);
        assert_eq!(e[0].title, "peer nas unreachable");
        assert!(s.take(r(false), 3).is_empty(), "only once");
        assert_eq!(s.take(r(true), 4)[0].title, "peer nas reachable again");
        // Readings for unknown peers are dropped.
        s.list.clear();
        s.take(r(true), 5);
        assert!(s.report.readings.is_empty() && s.fails.is_empty());
    }

    #[test]
    fn answers_only_what_is_served() {
        let me = Pong { id: "a".repeat(32), hostname: "nas".into(), version: "0.20.0".into(), uptime: 5 };
        let serve = vec!["ping".to_owned()];
        let ok: serde_json::Value = serde_json::from_str(&answer(r#"{"method":"peer.ping"}"#, &serve, &me)).unwrap();
        assert_eq!(ok["result"]["hostname"], "nas");
        let off: serde_json::Value = serde_json::from_str(&answer(r#"{"method":"peer.ping"}"#, &[], &me)).unwrap();
        assert_eq!(off["ok"], false, "root took ping out of [peers] serve");
        let no: serde_json::Value = serde_json::from_str(&answer(r#"{"method":"peer.restart"}"#, &[], &me)).unwrap();
        assert_eq!(no["ok"], false);
        let junk: serde_json::Value = serde_json::from_str(&answer("not json", &[], &me)).unwrap();
        assert_eq!(junk["ok"], false);
    }

    #[test]
    fn mutual_tls_with_pinned_fingerprints() {
        let d = tempfile::tempdir().unwrap();
        let a = identity(&d.path().join("a")).unwrap();
        let b = identity(&d.path().join("b")).unwrap();
        let mallory = identity(&d.path().join("m")).unwrap();
        assert_eq!(identity(&d.path().join("a")).unwrap().fingerprint, a.fingerprint, "kept across starts");
        assert_ne!(a.fingerprint, b.fingerprint);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            // B serves and has paired A only.
            let allowed = Arc::new(RwLock::new(vec![a.fingerprint.clone()]));
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config(&b, allowed).unwrap()));
            let me = Arc::new(RwLock::new(Pong {
                id: "b".repeat(32),
                hostname: "b".into(),
                version: "x".into(),
                uptime: 0,
            }));
            let serve = Arc::new(vec!["ping".to_owned()]);
            tokio::spawn(async move {
                loop {
                    let (tcp, _) = l.accept().await.unwrap();
                    tokio::spawn(serve_one(tcp, acceptor.clone(), serve.clone(), me.clone()));
                }
            });
            let to_b = Peer {
                id: "b".repeat(32),
                name: "b".into(),
                address: addr.to_string(),
                fingerprint: b.fingerprint.clone(),
            };
            let (pong, _) = ping(&a, &to_b).await.unwrap();
            assert_eq!(pong.hostname, "b");
            // Mallory isn't paired on B.
            assert!(ping(&mallory, &to_b).await.is_err());
            // A pinned to the wrong fingerprint refuses B.
            let wrong = Peer { fingerprint: mallory.fingerprint.clone(), ..to_b.clone() };
            assert!(ping(&a, &wrong).await.unwrap_err().contains("certificate"));
            // Right certificate, wrong node id.
            let other = Peer { id: "c".repeat(32), ..to_b };
            assert!(ping(&a, &other).await.unwrap_err().contains("answered as node"));
        });
    }
}
