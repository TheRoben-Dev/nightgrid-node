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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActionsConfig {
    /// Action types the node may run (manually from the app, or from
    /// self-healing rules). Empty: none, the default.
    pub allow: Vec<String>,
    /// Real (not dry) runs per hour, all sources together (1-1000).
    pub max_per_hour: u32,
}

impl Default for ActionsConfig {
    fn default() -> Self {
        Self { allow: Vec::new(), max_per_hour: 20 }
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
}

impl Default for LoginsConfig {
    fn default() -> Self {
        Self { enabled: true }
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
    /// Seconds between checks (2-300).
    pub interval_secs: u32,
}

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
        Self { enabled: true, watch: DEFAULT_WATCH.iter().map(PathBuf::from).collect(), interval_secs: 10 }
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
        if let Some(a) = self.actions.allow.iter().find(|a| !nightgrid_node_proto::action::ALL.contains(&a.as_str())) {
            return Err(format!(
                "actions.allow: unknown action {a:?} (known: {})",
                nightgrid_node_proto::action::ALL.join(", ")
            ));
        }
        if !(1..=1000).contains(&self.actions.max_per_hour) {
            return Err("actions.max_per_hour must be 1-1000".into());
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
# Seconds between checks (2-300).
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
allow = []
# Most real runs per hour, all rules and the app together (1-1000).
max_per_hour = 20
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
