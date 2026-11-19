//! `/etc/nightgrid-node/config.toml`: policy set by root. The service only
//! reads it (the file and its directory are root-owned), so nothing that
//! talks to the socket can widen what the node may do.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_CONFIG: &str = "/etc/nightgrid-node/config.toml";
pub const DEFAULT_DATA_DIR: &str = "/var/lib/nightgrid-node";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub metrics: MetricsConfig,
    pub server: ServerConfig,
    pub docker: DockerConfig,
    pub logins: LoginsConfig,
    pub files: FilesConfig,
    pub network: NetworkConfig,
    pub actions: ActionsConfig,
    pub rollback: RollbackConfig,
    pub health: HealthConfig,
    pub logs: LogsConfig,
    pub usage: UsageConfig,
    pub ports: PortsConfig,
    pub block: BlockConfig,
    pub checks: ChecksConfig,
    pub jobs: JobsConfig,
    pub ups: UpsConfig,
    pub peers: PeersConfig,
    pub statuspage: StatusPageConfig,
}

/// The optional read-only status page (`allow statuspage`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatusPageConfig {
    /// `tailnet` (this host's tailnet address) or an `ip:port` on the
    /// tailnet or loopback. Plain HTTP, so never a LAN or public address.
    pub listen: String,
    /// Port for `listen = "tailnet"`.
    pub port: u16,
    /// Who may open it (tailnet ranges by default).
    pub networks: Vec<String>,
}

impl Default for StatusPageConfig {
    fn default() -> Self {
        Self {
            listen: "tailnet".into(),
            port: 7420,
            networks: DEFAULT_PEER_NETWORKS.iter().map(|s| (*s).to_owned()).collect(),
        }
    }
}

/// Whether plain HTTP may listen on `ip`: tailnet (WireGuard-encrypted) or loopback.
pub fn statuspage_ip_ok(ip: std::net::IpAddr) -> bool {
    ip.is_loopback()
        || DEFAULT_PEER_NETWORKS
            .iter()
            .any(|n| crate::firewall::parse_cidr(n).is_some_and(|c| crate::firewall::in_cidr(ip, c)))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PeersConfig {
    /// Where the link listens: `tailnet` (this host's first 100.64.0.0/10
    /// address), `lan` (the source address of the route to the first
    /// non-tailnet `networks` entry, else of the default route; followed
    /// when DHCP changes it), a specific `ip:port`, or empty (connect out
    /// only). Never every interface.
    pub listen: String,
    /// Port for `listen = "tailnet"`.
    pub port: u16,
    /// The only addresses the link may talk to, in and out (tailnet ranges by default).
    pub networks: Vec<String>,
    /// What paired peers may ask this node (`ping`, `ups`, `sync`).
    pub serve: Vec<String>,
    /// Seconds between probes of each peer (30-3600).
    pub interval_secs: u32,
}

pub const DEFAULT_PEER_NETWORKS: &[&str] = &["100.64.0.0/10", "fd7a:115c:a1e0::/48"];
/// What `[peers] serve` may list.
pub const PEER_SERVES: &[&str] = &["ping", "ups", "sync"];

impl Default for PeersConfig {
    fn default() -> Self {
        Self {
            listen: "tailnet".into(),
            port: 7419,
            networks: DEFAULT_PEER_NETWORKS.iter().map(|s| (*s).to_owned()).collect(),
            serve: vec!["ping".into()],
            interval_secs: 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpsConfig {
    /// Power this host off when the UPS has been on battery too long. Also
    /// needs `power.poweroff` in `[actions] allow`, the root helper, `allow
    /// health` and the UPS in `[health] ups`. Off by default.
    pub shutdown: bool,
    /// The `[health] ups` entry to follow (empty: the first).
    pub target: String,
    /// Minutes on battery before the countdown starts (0: no time limit; 0-1440).
    pub on_battery_minutes: u32,
    /// Battery charge below which the countdown starts (0: off; 0-95).
    pub below_percent: u8,
    /// Start the countdown when the UPS reports low battery (`LB`).
    pub low_battery: bool,
    /// Seconds between announcing the shutdown and requesting it (0-600).
    /// The power-off itself follows a minute later.
    pub countdown_secs: u32,
    /// Only log the power-off, to try the settings.
    pub dry_run: bool,
}

impl Default for UpsConfig {
    fn default() -> Self {
        Self {
            shutdown: false,
            target: String::new(),
            on_battery_minutes: 5,
            below_percent: 0,
            low_battery: true,
            countdown_secs: 60,
            dry_run: false,
        }
    }
}

impl UpsConfig {
    /// The UPS to follow.
    pub fn target<'a>(&'a self, health: &'a HealthConfig) -> Option<&'a str> {
        if self.target.is_empty() {
            health.ups.first().map(String::as_str)
        } else {
            health.ups.iter().find(|u| **u == self.target).map(String::as_str)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JobsConfig {
    /// Watch the app's backup jobs and scheduled jobs (read-only).
    pub enabled: bool,
    /// Seconds between reads (30-3600).
    pub interval_secs: u32,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self { enabled: true, interval_secs: 60 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChecksConfig {
    /// Run the app's service checks from this host (needs `allow checks`,
    /// which installs the runner unit).
    pub enabled: bool,
    /// Seconds between rounds (30-3600).
    pub interval_secs: u32,
    /// The only addresses checks may connect to (and DNS servers they may
    /// ask). `allow checks` turns this into the runner's IPAddressAllow.
    pub networks: Vec<String>,
}

pub const DEFAULT_CHECK_NETWORKS: &[&str] = &[
    "127.0.0.0/8",
    "::1/128",
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "100.64.0.0/10",
    "fc00::/7",
    "fe80::/10",
];

impl Default for ChecksConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 60,
            networks: DEFAULT_CHECK_NETWORKS.iter().map(|s| (*s).to_owned()).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BlockConfig {
    /// Block addresses that keep failing SSH logins, by themselves. Also
    /// needs `firewall.block` in `[actions] allow`, the root helper and
    /// journal access. Off by default.
    pub auto: bool,
    /// Failed logins from one address within 5 minutes that block it (3-1000).
    pub threshold: u32,
    /// How long a block lasts, manual or automatic (5-10080).
    pub minutes: u32,
    /// Addresses blocked at the same time by auto-block, at most (1-10000).
    pub max_active: u32,
    /// Allow blocking local, private, link-local and CGNAT/Tailscale
    /// addresses too. Off: those are never blocked.
    pub private: bool,
    /// Addresses or CIDR ranges never to block (your office, a monitor).
    pub never: Vec<String>,
}

impl Default for BlockConfig {
    fn default() -> Self {
        Self { auto: false, threshold: 10, minutes: 60, max_active: 200, private: false, never: Vec::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PortsConfig {
    /// Record sockets that start or stop listening, and answer `ports.list`.
    pub enabled: bool,
    /// Seconds between reads (5-300).
    pub interval_secs: u32,
}

impl Default for PortsConfig {
    fn default() -> Self {
        Self { enabled: true, interval_secs: 15 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsageConfig {
    /// Per-service, per-container and per-process usage, and power draw.
    pub enabled: bool,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogsConfig {
    /// Watch the journal and container logs for error bursts, new errors and
    /// the app's saved searches (needs `allow journal` and/or `allow docker`;
    /// the anomaly check also needs `logAnomaly` on in the app's alert rules).
    pub enabled: bool,
    /// Seconds between rounds (30-3600).
    pub interval_secs: u32,
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self { enabled: true, interval_secs: 600 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HealthConfig {
    /// Read disk SMART, storage pools, pending updates and UPS status through
    /// the read-only health helper (after `allow health`).
    pub enabled: bool,
    /// NUT targets (`ups@host`) the helper may query with `upsc`.
    pub ups: Vec<String>,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self { enabled: true, ups: Vec::new() }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RollbackConfig {
    /// Files or directories whose files `changes.begin` may snapshot and the
    /// root helper may put back (needs `files.rollback` in `[actions] allow`).
    pub paths: Vec<String>,
}

/// A path root may list in `[rollback] paths`.
pub fn rollback_path_ok(p: &str) -> bool {
    let path = Path::new(p);
    path.is_absolute()
        && p.len() <= 1024
        && p != "/"
        && !p.ends_with('/')
        && p.chars().all(|c| c.is_ascii_graphic() && !matches!(c, '"' | '\'' | '\\' | '%'))
        && path.components().all(|c| matches!(c, std::path::Component::RootDir | std::path::Component::Normal(_)))
        && !["/proc", "/sys", "/dev", "/run", "/etc/nightgrid-node"].iter().any(|bad| path.starts_with(bad))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActionsConfig {
    /// Action types the node may run (manually from the app, or from
    /// self-healing rules). Empty: none, the default.
    pub allow: Vec<String>,
    /// Real (not dry) runs per hour, all sources together (1-1000).
    pub max_per_hour: u32,
    /// systemd units `unit.restart`/`start`/`stop` may act on, by full name.
    pub units: Vec<String>,
}

impl Default for ActionsConfig {
    fn default() -> Self {
        Self { allow: Vec::new(), max_per_hour: 20, units: Vec::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    /// Keep an inventory of LAN devices from the kernel's neighbour table
    /// (no privileges, nothing sent) and record new devices, address
    /// changes and IP conflicts.
    pub enabled: bool,
    /// Seconds between reads (5-600).
    pub interval_secs: u32,
    /// Interfaces to ignore, besides container bridges (always ignored).
    pub ignore_ifaces: Vec<String>,
    /// DHCP servers (their IPv4 identifiers) that are expected: a second
    /// server listed here is noted, not warned about. Needs `allow netwatch`.
    pub dhcp_servers: Vec<String>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self { enabled: true, interval_secs: 30, ignore_ifaces: Vec::new(), dhcp_servers: Vec::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoginsConfig {
    /// Record SSH logins and failed logins from the journal. Reading the
    /// journal also needs `nightgrid-node allow journal`.
    pub enabled: bool,
    /// Failed logins within 5 minutes that make a login-burst alert (5-10000;
    /// the alert also needs `loginBurst` on in the app's rules).
    pub burst: u32,
}

impl Default for LoginsConfig {
    fn default() -> Self {
        Self { enabled: true, burst: 20 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesConfig {
    /// Record changes to the files and directories in `watch`.
    pub enabled: bool,
    /// Files, or directories (their direct entries), to watch. Contents are
    /// never stored: only size, owner, mode, time and a SHA-256 when readable.
    pub watch: Vec<PathBuf>,
    /// Also watch the built-in security baseline: more of `/etc` (see
    /// [`BASELINE_WATCH`]), every login user's `authorized_keys` and the
    /// compose files of running containers.
    pub baseline: bool,
    /// Seconds between checks (2-300).
    pub interval_secs: u32,
}

/// Watched when `[files] baseline` is on, besides `watch`: places where a
/// change can give someone a way in or keep one.
pub const BASELINE_WATCH: &[&str] = &[
    "/etc/ld.so.preload",
    "/etc/environment",
    "/etc/profile",
    "/etc/profile.d",
    "/etc/bash.bashrc",
    "/etc/pam.d",
    "/etc/security/access.conf",
    "/etc/nsswitch.conf",
    "/etc/login.defs",
    "/etc/hosts.allow",
    "/etc/hosts.deny",
    "/etc/rc.local",
    "/etc/cron.hourly",
    "/etc/cron.daily",
    "/etc/cron.weekly",
    "/etc/cron.monthly",
    "/var/spool/cron/crontabs",
    "/var/spool/cron",
    "/etc/systemd/system/multi-user.target.wants",
    "/etc/systemd/system/timers.target.wants",
    "/etc/sysctl.conf",
    "/etc/sysctl.d",
    "/etc/modprobe.d",
    "/etc/modules-load.d",
    "/etc/apt/sources.list",
    "/etc/apt/sources.list.d",
    "/etc/yum.repos.d",
    "/etc/docker/daemon.json",
    "/etc/sudo.conf",
    "/root/.ssh/authorized_keys",
    "/root/.ssh/authorized_keys2",
];

pub const DEFAULT_WATCH: &[&str] = &[
    "/etc/ssh/sshd_config",
    "/etc/ssh/sshd_config.d",
    "/etc/passwd",
    "/etc/group",
    "/etc/shadow",
    "/etc/sudoers",
    "/etc/sudoers.d",
    "/etc/crontab",
    "/etc/cron.d",
    "/etc/hosts",
    "/etc/fstab",
    "/etc/systemd/system",
];

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            watch: DEFAULT_WATCH.iter().map(PathBuf::from).collect(),
            baseline: true,
            interval_secs: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DockerConfig {
    /// Watch containers when the engine socket is reachable. Reaching it
    /// also needs `nightgrid-node allow docker` (root-equivalent access).
    pub enabled: bool,
    /// Docker, or Podman's compatible socket (`/run/podman/podman.sock`).
    pub socket: PathBuf,
    /// Seconds between reads (5-300).
    pub interval_secs: u32,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self { enabled: true, socket: PathBuf::from("/var/run/docker.sock"), interval_secs: 15 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Seconds between samples (2–60). Each minute point averages the samples in it.
    pub interval_secs: u32,
    /// Days minute points are kept (1–365). Rollups keep their own fixed tiers.
    pub minute_retention_days: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Unix socket the app reaches through SSH. Group `nightgrid-node` may connect.
    pub socket: PathBuf,
    /// Most connections served at once.
    pub max_connections: u32,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { interval_secs: 10, minute_retention_days: 14 }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { socket: PathBuf::from(nightgrid_node_proto::DEFAULT_SOCKET), max_connections: 8 }
    }
}

impl Config {
    /// Reads the file; a missing file means defaults.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::parse(&s).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        let c: Config = toml::from_str(s).map_err(|e| e.to_string())?;
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(2..=60).contains(&self.metrics.interval_secs) {
            return Err("metrics.interval_secs must be 2-60".into());
        }
        if !(1..=365).contains(&self.metrics.minute_retention_days) {
            return Err("metrics.minute_retention_days must be 1-365".into());
        }
        if !self.server.socket.is_absolute() {
            return Err("server.socket must be an absolute path".into());
        }
        if !self.docker.socket.is_absolute() {
            return Err("docker.socket must be an absolute path".into());
        }
        if !(5..=300).contains(&self.docker.interval_secs) {
            return Err("docker.interval_secs must be 5-300".into());
        }
        if !(30..=3600).contains(&self.checks.interval_secs) {
            return Err("checks.interval_secs must be 30-3600".into());
        }
        if self.ups.on_battery_minutes > 1440 {
            return Err("ups.on_battery_minutes must be 0-1440".into());
        }
        if self.ups.below_percent > 95 {
            return Err("ups.below_percent must be 0-95".into());
        }
        if self.ups.countdown_secs > 600 {
            return Err("ups.countdown_secs must be 0-600".into());
        }
        if !self.ups.target.is_empty() && !self.health.ups.contains(&self.ups.target) {
            return Err("ups.target must be one of [health] ups".into());
        }
        if self.ups.shutdown && self.ups.on_battery_minutes == 0 && self.ups.below_percent == 0 && !self.ups.low_battery
        {
            return Err("[ups] shutdown needs a limit: on_battery_minutes, below_percent or low_battery".into());
        }
        if !(30..=3600).contains(&self.peers.interval_secs) {
            return Err("peers.interval_secs must be 30-3600".into());
        }
        if self.peers.port == 0 {
            return Err("peers.port must be 1-65535".into());
        }
        match self.peers.listen.as_str() {
            "" | "tailnet" | "lan" => {}
            l => match l.parse::<std::net::SocketAddr>() {
                Ok(a) if a.ip().is_unspecified() => {
                    return Err("peers.listen must be one address, not every interface".into())
                }
                Ok(_) => {}
                Err(_) => return Err("peers.listen must be \"tailnet\", \"lan\", \"ip:port\" or empty".into()),
            },
        }
        if self.peers.networks.is_empty()
            || self.peers.networks.len() > 64
            || self.peers.networks.iter().any(|c| crate::firewall::parse_cidr(c).is_none())
        {
            return Err("peers.networks takes 1-64 addresses or CIDR ranges".into());
        }
        if let Some(s) = self.peers.serve.iter().find(|s| !PEER_SERVES.contains(&s.as_str())) {
            return Err(format!("peers.serve: unknown {s:?} (one of {})", PEER_SERVES.join(", ")));
        }
        if self.statuspage.listen != "tailnet" {
            match self.statuspage.listen.parse::<std::net::SocketAddr>() {
                Ok(a) if statuspage_ip_ok(a.ip()) && a.port() != 0 => {}
                _ => {
                    return Err("statuspage.listen must be \"tailnet\" or an ip:port on the tailnet or loopback \
                                (it is plain HTTP)"
                        .into())
                }
            }
        }
        if self.statuspage.networks.is_empty()
            || self.statuspage.networks.len() > 64
            || self.statuspage.networks.iter().any(|c| crate::firewall::parse_cidr(c).is_none())
        {
            return Err("statuspage.networks takes 1-64 addresses or CIDR ranges".into());
        }
        if !(30..=3600).contains(&self.jobs.interval_secs) {
            return Err("jobs.interval_secs must be 30-3600".into());
        }
        if self.checks.networks.len() > 64
            || self.checks.networks.iter().any(|c| crate::firewall::parse_cidr(c).is_none())
        {
            return Err("checks.networks takes at most 64 addresses or CIDR ranges".into());
        }
        if !(3..=1000).contains(&self.block.threshold) {
            return Err("block.threshold must be 3-1000".into());
        }
        if !(5..=10_080).contains(&self.block.minutes) {
            return Err("block.minutes must be 5-10080".into());
        }
        if !(1..=10_000).contains(&self.block.max_active) {
            return Err("block.max_active must be 1-10000".into());
        }
        if self.block.never.len() > 256 || self.block.never.iter().any(|c| crate::firewall::parse_cidr(c).is_none()) {
            return Err("block.never takes at most 256 addresses or CIDR ranges".into());
        }
        if !(5..=300).contains(&self.ports.interval_secs) {
            return Err("ports.interval_secs must be 5-300".into());
        }
        if !(5..=10_000).contains(&self.logins.burst) {
            return Err("logins.burst must be 5-10000".into());
        }
        if !(2..=300).contains(&self.files.interval_secs) {
            return Err("files.interval_secs must be 2-300".into());
        }
        if self.files.watch.len() > 64 || self.files.watch.iter().any(|p| !p.is_absolute()) {
            return Err("files.watch takes at most 64 absolute paths".into());
        }
        if !(5..=600).contains(&self.network.interval_secs) {
            return Err("network.interval_secs must be 5-600".into());
        }
        if self.network.ignore_ifaces.len() > 64 {
            return Err("network.ignore_ifaces takes at most 64 names".into());
        }
        if self.network.dhcp_servers.len() > 32
            || self.network.dhcp_servers.iter().any(|s| s.parse::<std::net::Ipv4Addr>().is_err())
        {
            return Err("network.dhcp_servers takes at most 32 IPv4 addresses".into());
        }
        if let Some(a) = self.actions.allow.iter().find(|a| !crate::fixes::known(a)) {
            return Err(format!(
                "actions.allow: unknown action {a:?} (known: {}, {})",
                nightgrid_node_proto::action::ALL.join(", "),
                crate::fixes::LOCAL.join(", ")
            ));
        }
        if !(1..=1000).contains(&self.actions.max_per_hour) {
            return Err("actions.max_per_hour must be 1-1000".into());
        }
        if self.rollback.paths.len() > 32 {
            return Err("rollback.paths takes at most 32 paths".into());
        }
        if let Some(p) = self.rollback.paths.iter().find(|p| !rollback_path_ok(p)) {
            return Err(format!(
                "rollback.paths: {p:?} must be an absolute path without spaces or quotes, not / and not under \
                 /proc, /sys, /dev, /run or /etc/nightgrid-node"
            ));
        }
        if self.actions.units.len() > 64 {
            return Err("actions.units takes at most 64 units".into());
        }
        if let Some(u) = self.actions.units.iter().find(|u| !nightgrid_node_proto::action::valid_unit(u)) {
            return Err(format!(
                "actions.units: {u:?} is not a full unit name like nginx.service (the node's own units can't be listed)"
            ));
        }
        if !(30..=3600).contains(&self.logs.interval_secs) {
            return Err("logs.interval_secs must be 30-3600".into());
        }
        if self.health.ups.len() > 16 {
            return Err("health.ups takes at most 16 targets".into());
        }
        if let Some(u) = self.health.ups.iter().find(|u| !crate::health::ups::valid_target(u)) {
            return Err(format!("health.ups: {u:?} is not a NUT target like ups@localhost"));
        }
        if !(1..=64).contains(&self.server.max_connections) {
            return Err("server.max_connections must be 1-64".into());
        }
        Ok(())
    }
}

/// The file `install` writes when none exists: every setting, commented.
pub const TEMPLATE: &str = r#"# Nightgrid Node configuration (owned by root; the service only reads it).
# Restart the service after a change: systemctl restart nightgrid-node

[metrics]
# Seconds between samples (2-60). Minute points average the samples in them.
interval_secs = 10
# Days minute points are kept (1-365). 15-minute rollups are kept 90 days
# and hourly ones 400 days, like the app.
minute_retention_days = 14

[server]
# Unix socket the app reaches through SSH. Members of the nightgrid-node
# group may connect; nothing listens on the network.
socket = "/run/nightgrid-node/node.sock"
max_connections = 8

[docker]
# Watch containers (states, restarts, crashes) when the engine is reachable.
# The service account can only reach it after root runs
#   nightgrid-node allow docker
# which is root-equivalent access: read SECURITY.md first.
enabled = true
# Docker, or Podman's compatible socket (/run/podman/podman.sock).
socket = "/var/run/docker.sock"
# Seconds between reads (5-300).
interval_secs = 15

[logins]
# Record SSH logins and failed logins from the journal. The service can only
# read the journal after root runs: nightgrid-node allow journal
enabled = true
# Failed logins within 5 minutes that raise a login-burst alert (5-10000).
# The app turns that alert on (loginBurst in its alert rules).
burst = 20

[files]
# Record changes to these files and directories (direct entries only).
# Contents are never stored: size, owner, mode, time and a SHA-256 when the
# service can read the file (for unreadable ones like /etc/shadow, only that
# they changed).
enabled = true
watch = [
  "/etc/ssh/sshd_config",
  "/etc/ssh/sshd_config.d",
  "/etc/passwd",
  "/etc/group",
  "/etc/shadow",
  "/etc/sudoers",
  "/etc/sudoers.d",
  "/etc/crontab",
  "/etc/cron.d",
  "/etc/hosts",
  "/etc/fstab",
  "/etc/systemd/system",
]
# Also watch a built-in security baseline: more of /etc (ld.so.preload, PAM,
# profile, cron, sysctl, package sources, ...), root's and every login user's
# authorized_keys, and the compose files of running containers. With
# `nightgrid-node allow health`, files the service can't read (homes, root's
# crontabs) are read by the read-only health helper.
baseline = true
# Seconds between checks (2-300; at least 30 through the health helper).
interval_secs = 10

[network]
# Keep an inventory of devices on the LAN from the kernel's neighbour (ARP)
# table, and record new devices, address changes and IP conflicts. Needs no
# privileges and sends nothing.
enabled = true
# Seconds between reads (5-600).
interval_secs = 30
# Interfaces to ignore. Container bridges (docker0, br-*, veth*, ...) are
# always ignored.
ignore_ifaces = []
# Watching DHCP (servers, rogue servers, device names) needs a separate
# helper that may read raw packets; root turns it on with
#   nightgrid-node allow netwatch
# DHCP servers you expect, by address. A server not listed here that shows
# up next to another one is reported as a possible rogue DHCP server.
dhcp_servers = []

[actions]
# Actions the node may carry out, from the app or from self-healing rules.
# None by default. Each one is typed (never a shell command) and every run,
# dry or real, goes into a tamper-evident audit log. Known actions:
#   container.restart, container.start, container.stop  (need allow docker)
#   images.prune                                         (dangling images)
#   unit.restart, unit.start, unit.stop      (need allow helper, and units)
#   power.reboot, power.poweroff             (need allow helper; one minute
#                                             later, cancel with shutdown -c)
# Host fixes for `nightgrid-node fix` (need allow helper; node-local):
#   fix.unit-reset, fix.journal-vacuum, fix.logrotate, fix.package-cache,
#   fix.ssh-permissions, fix.runbook
# `sudo nightgrid-node fix` can also allow these for you; it keeps its own
# list (and runbooks and autofix rules) in fixes.toml next to this file.
allow = []
# Most real runs per hour, all rules and the app together (1-1000).
max_per_hour = 20
# systemd units the unit.* actions may act on, by full name, e.g.
#   units = ["nginx.service", "smbd.service"]
units = []

[rollback]
# Files, or directories of files, that a change from the app may snapshot
# and that are put back unless the app confirms the change in time (needs
# files.rollback in [actions] allow and the root helper). Keep this narrow,
# e.g. ["/srv/compose", "/etc/ssh/sshd_config"]. After editing it, run
#   nightgrid-node allow helper
# again so the helper may write there.
paths = []

[health]
# Disk SMART, storage pools (ZFS, mdadm, Btrfs, LVM), pending updates and
# UPS status, read by a separate read-only helper that root turns on with
#   nightgrid-node allow health
# It runs the same checks as the app and changes nothing.
enabled = true
# NUT UPS targets to poll with upsc, e.g. ["ups@localhost"].
ups = []

[logs]
# Look through the journal (after allow journal) and running containers'
# logs (after allow docker) for error bursts and new kinds of errors, when
# the app turns that alert on, and for the app's saved searches with alerts.
# Lines are only kept in memory; ones that look like secrets are dropped.
enabled = true
# Seconds between rounds (30-3600); each reads what was logged since the last.
interval_secs = 600

[usage]
# CPU, memory, disk and network per systemd service and per container (from
# cgroups), the busiest processes, and power draw from the CPU's energy
# counters (RAPL, after root runs: nightgrid-node allow power). Every 10 s,
# every second while the app watches; one-minute averages are kept a day.
enabled = true

[ports]
# Record sockets that start or stop listening (from /proc/net, no privileges)
# and list them for the app, with the containers that publish them.
enabled = true
# Seconds between reads (5-300). A change is reported after two reads agree.
interval_secs = 15

[block]
# Block addresses that keep failing SSH logins, by themselves: all their
# traffic is dropped for `minutes` (an nftables table of the node's own, with
# timeouts, so blocks end even if the node stops). Also needs
# "firewall.block" in [actions] allow, `nightgrid-node allow helper` and
# `nightgrid-node allow journal`. Every block is in the action log.
auto = false
# Failed logins from one address within 5 minutes that block it (3-1000).
threshold = 10
# How long a block lasts, from auto-block or the app (5-10080).
minutes = 60
# Addresses auto-blocked at the same time, at most.
max_active = 200
# Local, private, link-local and CGNAT/Tailscale addresses are never blocked
# unless this is true.
private = false
# Addresses or ranges never to block, e.g. ["203.0.113.0/24"].
never = []

[checks]
# Run the app's service checks (HTTP(S) status and certificates, TCP
# connects) from this host with the app closed. The main service never opens
# an IP socket: `nightgrid-node allow checks` installs a separate runner with
# network access only to `networks` (run it again after changing them).
enabled = true
# Seconds between rounds (30-3600).
interval_secs = 60
# The only addresses checks may reach, DNS servers included. Add a public
# address or range to check something outside your LAN.
networks = [
  "127.0.0.0/8",
  "::1/128",
  "10.0.0.0/8",
  "172.16.0.0/12",
  "192.168.0.0/16",
  "100.64.0.0/10",
  "fc00::/7",
  "fe80::/10",
]

[jobs]
# Watch the backup jobs the app defines (restic, borg, rsync, rclone: a
# systemd unit or a path) and the app's scheduled jobs (nightgrid-job-*,
# nightgrid-power-*): failed and stale backups alert, and each finished run
# is recorded with the app closed. Read-only; it runs nothing.
enabled = true
# Seconds between reads (30-3600).
interval_secs = 60

[ups]
# Power this host off by itself when the UPS has been on battery too long,
# with the app closed. Each host decides alone from the UPS's own reading, so
# give the hosts that must stay up longest (storage, the NUT server) larger
# limits. Also needs "power.poweroff" in [actions] allow,
# `nightgrid-node allow helper`, `allow health` and the UPS in [health] ups.
# A host that can't read the UPS can follow a paired peer's shutdown instead
# ("ups" in [peers] serve); it then needs only the helper and the allow.
# Every step is in the timeline and the action log. Off by default.
shutdown = false
# The [health] ups entry to follow (empty: the first).
target = ""
# Minutes on battery before the countdown starts (0: no time limit).
on_battery_minutes = 5
# Battery charge (percent) below which the countdown starts (0: off).
below_percent = 0
# Start the countdown when the UPS itself reports low battery.
low_battery = true
# Seconds to announce the shutdown before requesting it (the app can cancel
# it; power coming back cancels it). The power-off follows a minute later.
countdown_secs = 60
# Only log the power-off, to try the settings.
dry_run = false

[peers]
# The node-to-node link, off until root runs `nightgrid-node allow peers`.
# Each node acts alone by default; the link lets nodes the app paired check
# each other. A separate unit runs it, with mutual TLS: a peer is accepted
# only if the app gave this node its certificate fingerprint. It may talk
# only to `networks` (run `allow peers` again after changing them).
#
# Where it listens: "tailnet" (this host's tailnet address), "lan" (this
# host's LAN address: the one it uses to reach the first non-tailnet entry
# of networks, else its default route; followed when DHCP changes it; add
# the LAN to networks too), "ip:port", or "" to only connect out. Never
# every interface.
listen = "tailnet"
# Port for "tailnet" and "lan".
port = 7419
networks = ["100.64.0.0/10", "fd7a:115c:a1e0::/48"]
# What paired peers may ask this node:
#   "ping"  version, host name and uptime (reachability);
#   "ups"   start this host's UPS shutdown when a peer the app marked
#           "follow" starts its own, and end it when that peer's power is
#           back. Also needs [ups] shutdown and its requirements here.
#   "sync"  let this node's hub (the peer the app set with hub.member) read
#           its history, events, uptime, disk usage and active alerts.
#           Read-only; nothing else is served to it. See docs/HUB.md.
serve = ["ping"]
# Seconds between probes of each peer (30-3600).
interval_secs = 60

[statuspage]
# A read-only status page for your phone, off until root runs
# `nightgrid-node allow statuspage`, which prints its address with a secret
# link. A separate unit serves it. Plain HTTP, so it listens only on the
# tailnet (WireGuard encrypts it) or loopback: "tailnet" or "ip:port".
listen = "tailnet"
# Port for "tailnet".
port = 7420
# Who may open it (run `allow statuspage` again after changing them).
networks = ["100.64.0.0/10", "fd7a:115c:a1e0::/48"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_is_the_default() {
        assert_eq!(Config::parse(TEMPLATE).unwrap(), Config::default());
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn rejects_bad_values_and_unknown_keys() {
        assert!(Config::parse("[metrics]\ninterval_secs = 1\n").is_err());
        assert!(Config::parse("[server]\nsocket = \"rel.sock\"\n").is_err());
        assert!(Config::parse("[metrics]\ninterval = 5\n").is_err());
        assert!(Config::parse("listen = \"0.0.0.0:80\"\n").is_err());
        let c = Config::parse("[metrics]\ninterval_secs = 5\n").unwrap();
        assert_eq!((c.metrics.interval_secs, c.metrics.minute_retention_days), (5, 14));
    }

    #[test]
    fn missing_file_is_defaults() {
        assert_eq!(Config::load(Path::new("/nonexistent/nightgrid-node.toml")).unwrap(), Config::default());
    }
}
