//! The service: a sampling thread that writes history, and the socket server.

use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nightgrid_node_proto::{Alert, AlertRules, NodeIdentity, Sample};

use crate::config::Config;
use crate::metrics::{collect::Reader, point_from_sample, Collector, MinuteAverager};
use crate::store::Store;

/// How often old history is pruned.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);
/// Sampling interval while a `metrics` subscriber is connected.
pub const LIVE_INTERVAL: Duration = Duration::from_secs(2);

pub fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn now_millis() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

/// What the sampler and the server share.
pub struct Shared {
    pub config: Config,
    pub identity: NodeIdentity,
    pub started: i64,
    pub store: Mutex<Store>,
    pub latest: Mutex<Option<Sample>>,
    /// Alert rules from the app (`alerts.rules.set`), or the defaults.
    pub rules: Mutex<AlertRules>,
    /// Active alerts, as the engine last saw them.
    pub active: Mutex<Vec<Alert>>,
    /// What the container watcher last saw.
    pub containers: Mutex<crate::containers::ContainerState>,
    /// Live streams for protocol 2 subscribers.
    pub bus: crate::bus::Bus,
    /// The LAN inventory, as the network watcher last saw it.
    pub net: Mutex<nightgrid_node_proto::NetDevicesResult>,
    /// Sightings from the network watch helper, for the next read.
    pub net_inbox: Mutex<crate::netwatch::Inbox>,
    /// DHCP servers seen by the helper.
    pub dhcp: Mutex<crate::dhcp::Dhcp>,
    /// Self-healing rules and what they are tracking.
    pub heal: Mutex<crate::heal::Heal>,
    /// The root helper's socket (tests point it elsewhere).
    pub helper_socket: PathBuf,
    /// Host health readings from the health helper.
    pub health: Mutex<crate::health::watch::HealthState>,
    /// The health helper's socket (tests point it elsewhere).
    pub health_socket: PathBuf,
    /// The log watcher's state, saved searches and alert conditions.
    pub logs: Mutex<crate::logs::LogsState>,
    /// The latest usage sample.
    pub usage: Mutex<Option<nightgrid_node_proto::UsageSample>>,
    /// Recent failed logins, for the login-burst alert.
    pub logins: Mutex<crate::logins::Recent>,
    /// The latest read of listening sockets.
    pub ports: Mutex<nightgrid_node_proto::PortsResult>,
    /// Addresses blocked through `firewall.block`.
    pub blocks: Mutex<crate::firewall::Blocks>,
    /// Where the login watcher sends addresses past `[block] threshold`.
    pub autoblock: Mutex<Option<std::sync::mpsc::Sender<String>>>,
    /// The app's service checks and the runner's latest results.
    pub checks: Mutex<crate::checks::ChecksState>,
    /// The app's backup jobs and the latest read of them and its scheduled jobs.
    pub jobs: Mutex<crate::jobs::JobsState>,
    /// The UPS shutdown guard (`[ups] shutdown`).
    pub ups_guard: Mutex<crate::upsguard::Guard>,
}

impl Shared {
    pub fn new(config: Config, store: Store) -> Arc<Self> {
        let identity = NodeIdentity {
            node_id: store.node_id(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            hostname: std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
        };
        let rules = crate::engine::load_rules(&store);
        let dhcp = crate::dhcp::Dhcp::load(&store, &config.network.dhcp_servers);
        let heal = crate::heal::Heal::load(&store);
        let logs = crate::logs::LogsState::load(&store);
        let docker_access = if config.docker.enabled {
            nightgrid_node_proto::ContainerAccess::Absent
        } else {
            nightgrid_node_proto::ContainerAccess::Off
        };
        Arc::new(Self {
            config,
            identity,
            started: now_secs(),
            store: Mutex::new(store),
            latest: Mutex::new(None),
            rules: Mutex::new(rules),
            active: Mutex::new(Vec::new()),
            containers: Mutex::new(crate::containers::ContainerState::new(docker_access)),
            bus: crate::bus::Bus::default(),
            net: Mutex::new(nightgrid_node_proto::NetDevicesResult { at: 0, gateway: None, devices: Vec::new() }),
            net_inbox: Mutex::new(crate::netwatch::Inbox::default()),
            dhcp: Mutex::new(dhcp),
            heal: Mutex::new(heal),
            helper_socket: PathBuf::from(crate::helper::SOCKET),
            health: Mutex::new(crate::health::watch::HealthState::default()),
            health_socket: PathBuf::from(crate::health::watch::SOCKET),
            logs: Mutex::new(logs),
            usage: Mutex::new(None),
            logins: Mutex::new(crate::logins::Recent::default()),
            ports: Mutex::new(nightgrid_node_proto::PortsResult::default()),
            blocks: Mutex::new(crate::firewall::Blocks::default()),
            autoblock: Mutex::new(None),
            checks: Mutex::new(crate::checks::ChecksState::default()),
            jobs: Mutex::new(crate::jobs::JobsState::default()),
            ups_guard: Mutex::new(crate::upsguard::Guard::default()),
        })
    }
}

/// Where the service keeps its files.
#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
}

impl Paths {
    /// systemd's `StateDirectory`, else the default.
    pub fn from_env() -> Self {
        let data_dir = std::env::var_os("STATE_DIRECTORY")
            .map(|v| PathBuf::from(v.to_string_lossy().split(':').next().unwrap_or_default().to_owned()))
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| PathBuf::from(crate::config::DEFAULT_DATA_DIR));
        Self { data_dir }
    }

    pub fn db(&self) -> PathBuf {
        self.data_dir.join("node.db")
    }
}

/// Handle to the sampling thread.
pub struct Sampler {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Sampler {
    /// Starts sampling with `reader` every `interval`.
    pub fn start(shared: Arc<Shared>, mut reader: Reader, interval: Duration) -> std::io::Result<Self> {
        let (stop, rx) = mpsc::channel::<()>();
        let minute_days = shared.config.metrics.minute_retention_days;
        let thread = std::thread::Builder::new().name("sampler".into()).spawn(move || {
            let mut collector = Collector::new();
            let mut minutes = MinuteAverager::default();
            let mut last_prune: Option<std::time::Instant> = None;
            let mut engine = match crate::engine::Engine::start(&shared, now_secs()) {
                Ok(e) => Some(e),
                Err(e) => {
                    crate::error!("uptime and alerts are off: {e}");
                    None
                }
            };
            loop {
                let started = std::time::Instant::now();
                let raw = crate::metrics::parse(&reader.read());
                let sample = collector.sample(raw, now_millis());
                // The first sample has no CPU % or rates; don't let it into history.
                if sample.cpu.is_some() {
                    if let Some(done) = minutes.push(&point_from_sample(&sample)) {
                        store_minute(&shared, &done);
                    }
                }
                if let Some(e) = engine.as_mut() {
                    e.sample(&shared, &sample, now_secs());
                }
                if shared.bus.metrics_watched() {
                    let _ = shared.bus.metrics.send(sample.clone());
                }
                *shared.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(sample);
                if last_prune.map_or(true, |t| t.elapsed() >= PRUNE_EVERY) {
                    last_prune = Some(std::time::Instant::now());
                    let mut st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
                    match st.prune(now_secs(), minute_days) {
                        Ok(0) => {}
                        Ok(n) => crate::debug!("pruned {n} old history rows"),
                        Err(e) => crate::warn!("prune failed: {e}"),
                    }
                }
                // Sample faster while someone watches live metrics; check
                // every second so a new subscriber doesn't wait a full interval.
                let stop = loop {
                    let every = if shared.bus.metrics_watched() { interval.min(LIVE_INTERVAL) } else { interval };
                    let left = every.saturating_sub(started.elapsed());
                    if left.is_zero() {
                        break false;
                    }
                    match rx.recv_timeout(left.min(Duration::from_secs(1))) {
                        Err(RecvTimeoutError::Timeout) => continue,
                        _ => break true,
                    }
                };
                if stop {
                    break;
                }
            }
            if let Some(p) = minutes.flush() {
                store_minute(&shared, &p);
            }
            if let Some(e) = engine.as_mut() {
                e.stop(&shared, now_secs());
            }
        })?;
        Ok(Self { stop, thread: Some(thread) })
    }

    /// Stops sampling and writes the partial minute.
    pub fn stop(mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn store_minute(shared: &Shared, p: &nightgrid_node_proto::Point) {
    let mut st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = st.insert_minute(p) {
        crate::warn!("could not store the minute at {}: {e}", p.ts);
    }
}

/// Resident set size of this process, bytes.
pub fn rss_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    pages * u64::try_from(page).unwrap_or(4096)
}

/// Tells systemd (`Type=notify`) something, if it's listening.
pub fn sd_notify(state: &str) {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else { return };
    let path = path.to_string_lossy().into_owned();
    let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else { return };
    let r = if let Some(name) = path.strip_prefix('@') {
        use std::os::linux::net::SocketAddrExt;
        std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            .and_then(|addr| sock.send_to_addr(state.as_bytes(), &addr))
    } else {
        sock.send_to(state.as_bytes(), &path)
    };
    if let Err(e) = r {
        crate::debug!("sd_notify: {e}");
    }
}

/// Runs the service until SIGTERM/SIGINT.
pub fn run(config: Config, paths: Paths) -> Result<(), String> {
    std::fs::create_dir_all(&paths.data_dir).map_err(|e| format!("{}: {e}", paths.data_dir.display()))?;
    let store = Store::open(&paths.db())?;
    let shared = Shared::new(config.clone(), store);
    crate::info!(
        "nightgrid-node {} starting (node {}, every {} s, socket {})",
        shared.identity.version,
        shared.identity.node_id,
        config.metrics.interval_secs,
        config.server.socket.display()
    );
    let sampler =
        Sampler::start(shared.clone(), Reader::system(), Duration::from_secs(config.metrics.interval_secs.into()))
            .map_err(|e| format!("sampler: {e}"))?;
    let files = if config.files.enabled && !config.files.watch.is_empty() {
        Some(crate::files::FileWatcher::start(shared.clone()).map_err(|e| format!("file watcher: {e}"))?)
    } else {
        None
    };
    crate::firewall::load(&shared);
    {
        let list = crate::checks::load(&shared.store.lock().unwrap_or_else(|e| e.into_inner()));
        shared.checks.lock().unwrap_or_else(|e| e.into_inner()).list = list;
        let jobs = crate::jobs::load(&shared.store.lock().unwrap_or_else(|e| e.into_inner()));
        shared.jobs.lock().unwrap_or_else(|e| e.into_inner()).list = jobs;
    }
    let autoblock = if crate::firewall::auto_enabled(&shared) && config.logins.enabled {
        let (b, tx) = crate::firewall::AutoBlocker::start(shared.clone()).map_err(|e| format!("auto-block: {e}"))?;
        *shared.autoblock.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        crate::info!(
            "auto-block is on: {} failed logins in 5 minutes block an address for {} minutes",
            config.block.threshold,
            config.block.minutes
        );
        Some(b)
    } else {
        None
    };
    let logins = if config.logins.enabled {
        Some(crate::logins::LoginWatcher::start(shared.clone()).map_err(|e| format!("login watcher: {e}"))?)
    } else {
        None
    };
    let network = if config.network.enabled {
        Some(
            crate::netwatch::NetWatcher::start(shared.clone(), crate::netwatch::Sources::default())
                .map_err(|e| format!("network watcher: {e}"))?,
        )
    } else {
        None
    };
    let healer = crate::heal::Healer::start(shared.clone()).map_err(|e| format!("self-healing: {e}"))?;
    let usage = if config.usage.enabled {
        Some(
            crate::usage::UsageWatcher::start(shared.clone(), crate::usage::UsageReader::system())
                .map_err(|e| format!("usage watcher: {e}"))?,
        )
    } else {
        None
    };
    let logs = if config.logs.enabled {
        Some(crate::logs::LogWatcher::start(shared.clone()).map_err(|e| format!("log watcher: {e}"))?)
    } else {
        None
    };
    let health = if config.health.enabled {
        Some(crate::health::watch::HealthWatcher::start(shared.clone()).map_err(|e| format!("health watcher: {e}"))?)
    } else {
        None
    };
    let ports = if config.ports.enabled {
        Some(
            crate::ports::PortWatcher::start(shared.clone(), PathBuf::from("/"))
                .map_err(|e| format!("port watcher: {e}"))?,
        )
    } else {
        None
    };
    let jobs = if config.jobs.enabled {
        Some(crate::jobs::JobsWatcher::start(shared.clone()).map_err(|e| format!("jobs watcher: {e}"))?)
    } else {
        None
    };
    let watcher = if config.docker.enabled {
        Some(crate::containers::Watcher::start(shared.clone()).map_err(|e| format!("container watcher: {e}"))?)
    } else {
        None
    };

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let result = rt.block_on(async {
        let listener = crate::server::bind(&config.server.socket)?;
        sd_notify("READY=1");
        let shutdown = async {
            use tokio::signal::unix::{signal, SignalKind};
            let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
                return std::future::pending::<()>().await;
            };
            tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
        };
        crate::server::serve(listener, shared.clone(), shutdown).await;
        Ok::<(), String>(())
    });
    sd_notify("STOPPING=1");
    sampler.stop();
    healer.stop();
    if let Some(w) = watcher {
        w.stop();
    }
    if let Some(w) = files {
        w.stop();
    }
    if let Some(w) = logins {
        w.stop();
    }
    if let Some(b) = autoblock {
        b.stop();
    }
    if let Some(w) = network {
        w.stop();
    }
    if let Some(w) = health {
        w.stop();
    }
    if let Some(w) = logs {
        w.stop();
    }
    if let Some(w) = usage {
        w.stop();
    }
    if let Some(w) = ports {
        w.stop();
    }
    if let Some(w) = jobs {
        w.stop();
    }
    let _ = std::fs::remove_file(&config.server.socket);
    crate::info!("stopped");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampler_writes_minutes_and_latest() {
        let shared = Shared::new(Config::default(), Store::open_in_memory().unwrap());
        let s = Sampler::start(shared.clone(), Reader::system(), Duration::from_millis(80)).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        s.stop();
        let latest = shared.latest.lock().unwrap().clone().expect("a sample");
        assert!(latest.mem_total > 0);
        // The partial minute was flushed on stop.
        let cov = shared.store.lock().unwrap().coverage().unwrap();
        assert!((1..=2).contains(&cov[0].points), "{cov:?}");
    }

    #[test]
    fn rss_is_measured() {
        assert!(rss_bytes() > 0);
    }
}
