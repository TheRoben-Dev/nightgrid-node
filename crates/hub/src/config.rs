//! `/etc/nightgrid-hub/config.toml`: root's policy for the hub. Nothing on
//! the socket can change it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_CONFIG: &str = "/etc/nightgrid-hub/config.toml";
pub const DEFAULT_DATA_DIR: &str = "/var/lib/nightgrid-hub";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub store: StoreConfig,
    pub collect: CollectConfig,
    pub alerts: AlertsConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub socket: PathBuf,
    /// Most connections served at once.
    pub max_connections: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { socket: PathBuf::from(nightgrid_node_proto::hub::DEFAULT_SOCKET), max_connections: 16 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StoreConfig {
    /// The store's cap in GB (1-1000). Past 90% of it the oldest minute
    /// points go first.
    pub max_gb: u32,
    /// Days of minute points kept per member (1-365), like a node's.
    pub minute_days: u32,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self { max_gb: 4, minute_days: 14 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CollectConfig {
    /// Where the collector answers members' pings: `tailnet`, `lan`,
    /// `ip:port`, empty (pull only; members then show the hub as
    /// unreachable), or a list of those (0.28.0), e.g. `["tailnet",
    /// "192.168.1.10:7420"]` for members on the tailnet and on the LAN.
    pub listen: Listen,
    /// Port for `tailnet` and `lan`.
    pub port: u16,
    /// The only addresses the collector may talk to, in and out.
    pub networks: Vec<String>,
    /// Seconds between pulls of each member (30-3600).
    pub interval_secs: u32,
}

impl Default for CollectConfig {
    fn default() -> Self {
        Self {
            listen: Listen::One("tailnet".into()),
            port: 7420,
            networks: nightgrid_node::config::DEFAULT_PEER_NETWORKS.iter().map(|s| (*s).to_owned()).collect(),
            interval_secs: 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AlertsConfig {
    /// A member that hasn't answered for this long is offline (60-3600; the
    /// app's own grace is 120 s).
    pub offline_secs: u32,
}

impl Default for AlertsConfig {
    fn default() -> Self {
        Self { offline_secs: 120 }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let c: Self = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(1..=1000).contains(&self.store.max_gb) {
            return Err("store.max_gb must be 1-1000".into());
        }
        if !(1..=365).contains(&self.store.minute_days) {
            return Err("store.minute_days must be 1-365".into());
        }
        if !(30..=3600).contains(&self.collect.interval_secs) {
            return Err("collect.interval_secs must be 30-3600".into());
        }
        if !(60..=3600).contains(&self.alerts.offline_secs) {
            return Err("alerts.offline_secs must be 60-3600".into());
        }
        if !(1..=256).contains(&self.server.max_connections) {
            return Err("server.max_connections must be 1-256".into());
        }
        let entries = self.collect.listen.entries();
        if entries.len() > 8 {
            return Err("collect.listen takes at most 8 addresses".into());
        }
        for e in &entries {
            match *e {
                "" | "tailnet" | "lan" => {}
                a => match a.parse::<SocketAddr>() {
                    Ok(s) if !s.ip().is_unspecified() && s.port() != 0 => {}
                    _ => {
                        return Err(format!(
                            "collect.listen: {a:?} must be \"tailnet\", \"lan\", \"\" or ip:port (not 0.0.0.0 or ::)"
                        ))
                    }
                },
            }
        }
        if self.collect.port == 0 {
            return Err("collect.port must not be 0".into());
        }
        for n in &self.collect.networks {
            if nightgrid_node::firewall::parse_cidr(n).is_none() {
                return Err(format!("collect.networks: {n:?} is not an address or CIDR"));
            }
        }
        Ok(())
    }

    /// Where the collector answers pings now: each entry resolved, in
    /// order, without duplicates.
    pub fn listen_addrs(&self) -> Vec<SocketAddr> {
        let mut out: Vec<SocketAddr> = Vec::new();
        for e in self.collect.listen.entries() {
            if let Some(a) = nightgrid_node::peers::resolve_listen(e, self.collect.port, &self.collect.networks) {
                if !out.contains(&a) {
                    out.push(a);
                }
            }
        }
        out
    }

    /// The first of them.
    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.listen_addrs().into_iter().next()
    }
}

/// `[collect] listen`: one entry, as before 0.28.0, or a list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Listen {
    One(String),
    Many(Vec<String>),
}

impl Listen {
    /// The non-empty entries.
    pub fn entries(&self) -> Vec<&str> {
        match self {
            Self::One(s) => vec![s.as_str()],
            Self::Many(v) => v.iter().map(String::as_str).collect(),
        }
        .into_iter()
        .filter(|e| !e.is_empty())
        .collect()
    }
    /// Whether any entry follows the host's addresses (`tailnet`, `lan`).
    pub fn follows(&self) -> bool {
        self.entries().iter().any(|e| matches!(*e, "tailnet" | "lan"))
    }
}

impl From<&str> for Listen {
    fn from(s: &str) -> Self {
        Self::One(s.to_owned())
    }
}

pub const TEMPLATE: &str = r#"# Nightgrid Hub configuration (root-owned). Written by `nightgrid-hub install`
# if missing; edit and run `systemctl restart nightgrid-hub` (and
# `nightgrid-hub allow collect` again after changing [collect] networks).
# See https://github.com/TheRoben-Dev/nightgrid-node/blob/main/docs/HUB.md

[server]
# The app reaches the hub through this socket over SSH, like a node's.
socket = "/run/nightgrid-hub/hub.sock"
max_connections = 16

[store]
# Cap on the hub's copy of its members' history, in GB. Past 90% of it the
# oldest minute points go first.
max_gb = 4
# Days of minute points kept per member (15-minute and hourly points are
# kept 90 and 400 days, like a node's).
minute_days = 14

[collect]
# The collector pulls from the nodes the app made members, over their
# node-to-node link (mutual TLS, pinned certificates), read-only. It also
# answers the members' pings here so they see the hub as a linked peer:
# "tailnet" (this host's tailnet address), "lan" (this host's LAN address,
# followed when DHCP changes it), "ip:port", "" (pull only), or a list of
# them when members reach the hub on different networks, for example
# listen = ["tailnet", "192.168.1.10:7420"]. Never every interface.
listen = "tailnet"
# Port for "tailnet" and "lan".
port = 7420
# The only addresses the collector may talk to (tailnet ranges by default).
networks = ["100.64.0.0/10", "fd7a:115c:a1e0::/48"]
# Seconds between pulls of each member (30-3600).
interval_secs = 60

[alerts]
# A member that hasn't answered the hub for this long is offline (60-3600).
offline_secs = 120
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_takes_one_address_or_a_list() {
        let c: Config = toml::from_str("[collect]\nlisten = \"127.0.0.1:17420\"\n").unwrap();
        assert_eq!(c.collect.listen.entries(), ["127.0.0.1:17420"]);
        let c: Config =
            toml::from_str("[collect]\nlisten = [\"127.0.0.1:17420\", \"127.0.0.2:17420\", \"127.0.0.1:17420\"]\n")
                .unwrap();
        c.validate().unwrap();
        assert_eq!(c.listen_addrs().len(), 2, "duplicates once");
        assert_eq!(c.listen_addr(), Some("127.0.0.1:17420".parse().unwrap()));
        let c: Config = toml::from_str("[collect]\nlisten = [\"tailnet\", \"lan\", \"\"]\n").unwrap();
        c.validate().unwrap();
        assert!(c.collect.listen.follows());
        let bad: Config = toml::from_str("[collect]\nlisten = [\"tailnet\", \"0.0.0.0:7420\"]\n").unwrap();
        assert!(bad.validate().unwrap_err().contains("0.0.0.0:7420"));
        assert!(toml::from_str::<Config>("[collect]\nlisten = 7420\n").is_err());
    }

    #[test]
    fn template_is_the_default_and_valid() {
        let c: Config = toml::from_str(TEMPLATE).unwrap();
        c.validate().unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn refuses_every_interface_and_typos() {
        let mut c = Config::default();
        c.collect.listen = "0.0.0.0:7420".into();
        assert!(c.validate().is_err());
        c.collect.listen = "127.0.0.1:17420".into();
        assert!(c.validate().is_ok());
        c.collect.networks = vec!["everywhere".into()];
        assert!(c.validate().is_err());
        assert!(toml::from_str::<Config>("[collect]\nlisten_on = \"x\"\n").is_err());
    }
}
