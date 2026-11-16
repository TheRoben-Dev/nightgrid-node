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
