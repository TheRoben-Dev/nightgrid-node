//! `nightgrid-node doctor`: a read-only self-check of this host's install.
//!
//! It only reads files, asks systemd and asks the running node, so any user
//! may run it; parts that need more (the socket's group) say so instead of
//! failing. Nothing here is part of the app protocol.

use std::path::Path;
use std::process::Command;

use serde::Serialize;

use crate::config::Config;
use crate::install::{Grant, Layout};

/// The resident memory the service should stay under (DESIGN.md).
pub const RSS_BUDGET: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Level {
    Ok,
    Info,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub check: String,
    pub level: Level,
    pub detail: String,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub version: &'static str,
    pub findings: Vec<Finding>,
}

impl Report {
    fn add(&mut self, check: impl Into<String>, level: Level, detail: impl Into<String>) {
        self.findings.push(Finding { check: check.into(), level, detail: detail.into() });
    }
    /// The worst level found.
    pub fn worst(&self) -> Level {
        self.findings.iter().map(|f| f.level).max().unwrap_or(Level::Ok)
    }
}

/// What the doctor learns from outside its own process; a trait so tests can fake it.
pub trait Probe {
    /// `systemctl is-active UNIT`, or `None` without systemd.
    fn unit_state(&mut self, unit: &str) -> Option<String>;
    /// The running node's `node.info`, or why it could not be asked.
    fn node_info(&mut self, socket: &Path) -> Result<serde_json::Value, String>;
}

pub struct RealProbe;

impl Probe for RealProbe {
    fn unit_state(&mut self, unit: &str) -> Option<String> {
        let out = Command::new("systemctl").args(["is-active", unit]).output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (!s.is_empty()).then_some(s)
    }
    fn node_info(&mut self, socket: &Path) -> Result<serde_json::Value, String> {
        let mut c = crate::client::Client::connect(socket, "nightgrid-node doctor").map_err(|e| e.to_string())?;
        c.call("node.info", serde_json::Value::Null).map_err(|e| e.to_string())
    }
}

/// Runs every check.
pub fn run(layout: &Layout, config_path: &Path, socket: Option<&Path>, probe: &mut dyn Probe) -> Report {
    let mut r = Report { version: env!("CARGO_PKG_VERSION"), ..Default::default() };

    // The installed binary and its unit.
    let installed = layout.bin().exists();
    if installed {
        r.add("install", Level::Ok, format!("{} is installed", layout.bin().display()));
    } else {
        r.add("install", Level::Info, "not installed as a service (root can run: nightgrid-node install)");
    }

    // The config root owns.
    let cfg = if config_path.exists() {
        match Config::load(config_path) {
            Ok(c) => {
                r.add("config", Level::Ok, format!("{} is valid", config_path.display()));
                Some(c)
            }
            Err(e) => {
                r.add("config", Level::Fail, format!("{}: {e}", config_path.display()));
                None
            }
        }
    } else {
        let level = if installed { Level::Warn } else { Level::Info };
        r.add("config", level, format!("{} is missing; defaults apply", config_path.display()));
        None
    };

    // The service.
    match probe.unit_state(crate::install::SERVICE) {
        None => r.add("service", Level::Info, "systemd not reachable; skipped unit checks"),
        Some(s) if s == "active" => r.add("service", Level::Ok, "nightgrid-node is active"),
        Some(s) if installed => {
            r.add("service", Level::Fail, format!("nightgrid-node is {s} (journalctl -u nightgrid-node)"))
        }
        Some(s) => r.add("service", Level::Info, format!("nightgrid-node is {s}")),
    }

    // The socket and what the running node says.
    let socket = socket
        .map(Path::to_path_buf)
        .or_else(|| cfg.as_ref().map(|c| c.server.socket.clone()))
        .unwrap_or_else(|| nightgrid_node_proto::DEFAULT_SOCKET.into());
    let info = match probe.node_info(&socket) {
        Ok(v) => {
            r.add("socket", Level::Ok, format!("{} answers", socket.display()));
            Some(v)
        }
        Err(e) => {
            let hint = if e.contains("ermission denied") {
                " (this user is not in the nightgrid-node group; root can run: nightgrid-node install --user NAME)"
            } else {
                ""
            };
            let level = if installed { Level::Warn } else { Level::Info };
            r.add("socket", level, format!("{}: {e}{hint}", socket.display()));
            None
        }
    };
    if let Some(info) = &info {
        let running = info["node"]["version"].as_str().unwrap_or("?");
        if running == env!("CARGO_PKG_VERSION") {
            r.add("version", Level::Ok, format!("running {running}"));
        } else {
            r.add(
                "version",
                Level::Warn,
                format!(
                    "running {running}, this binary is {} (restart the service to switch)",
                    env!("CARGO_PKG_VERSION")
                ),
            );
        }
        let rss = info["rssBytes"].as_u64().unwrap_or(0);
        let mib = |b: u64| format!("{:.1} MiB", b as f64 / 1048576.0);
        if rss > RSS_BUDGET {
            r.add("memory", Level::Warn, format!("{} resident, over the {} budget", mib(rss), mib(RSS_BUDGET)));
        } else if rss > 0 {
            r.add("memory", Level::Ok, format!("{} resident (budget {})", mib(rss), mib(RSS_BUDGET)));
        }
        if let Some(db) = info["dbBytes"].as_u64() {
            r.add("history", Level::Info, format!("{} of history", mib(db)));
        }
    }

    // Each grant: what root allowed, whether its unit runs, what the node sees.
    for g in Grant::ALL {
        let allowed = g.is_allowed(layout);
        let seen = info.as_ref().and_then(|i| i["access"][g.name()].as_str()).map(str::to_owned);
        let name = format!("allow {}", g.name());
        if !allowed {
            r.add(name, Level::Info, "off");
            continue;
        }
        if let Some(unit) = g.unit() {
            match probe.unit_state(unit) {
                Some(s) if s == "active" || (unit.ends_with(".socket") && s == "listening") => {}
                Some(s) => {
                    r.add(name, Level::Warn, format!("allowed, but {unit} is {s} (journalctl -u {unit})"));
                    continue;
                }
                None => {}
            }
        }
        match seen.as_deref() {
            Some("ok") | None => r.add(name, Level::Ok, "on"),
            Some(other) => r.add(name, Level::Warn, format!("allowed, but the node reports {other}")),
        }
    }
    r
}

/// Plain text for a terminal.
pub fn render(r: &Report) -> String {
    let mut out = format!("nightgrid-node {} doctor\n", r.version);
    for f in &r.findings {
        let tag = match f.level {
            Level::Ok => "  ok ",
            Level::Info => "  -- ",
            Level::Warn => "WARN ",
            Level::Fail => "FAIL ",
        };
        out.push_str(&format!("{tag} {:<18} {}\n", f.check, f.detail));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Fake {
        units: HashMap<&'static str, &'static str>,
        info: Result<serde_json::Value, String>,
    }
    impl Probe for Fake {
        fn unit_state(&mut self, unit: &str) -> Option<String> {
            self.units.get(unit).map(|s| s.to_string())
        }
        fn node_info(&mut self, _: &Path) -> Result<serde_json::Value, String> {
            self.info.clone()
        }
    }

    fn level(r: &Report, check: &str) -> Level {
        r.findings.iter().find(|f| f.check == check).unwrap_or_else(|| panic!("{check}: {r:?}")).level
    }

    #[test]
    fn a_host_without_the_node_is_only_informed() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout { root: dir.path().into() };
        let mut p = Fake { units: HashMap::new(), info: Err("No such file or directory".into()) };
        let r = run(&layout, &layout.config(), None, &mut p);
        assert_eq!(r.worst(), Level::Info, "{}", render(&r));
        assert_eq!(level(&r, "allow docker"), Level::Info);
    }

    #[test]
    fn a_healthy_install_is_ok_and_problems_are_named() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout { root: dir.path().into() };
        for p in [layout.bin(), layout.config(), layout.system_unit("nightgrid-node-peer.service")] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        }
        std::fs::write(layout.bin(), "").unwrap();
        std::fs::write(layout.config(), crate::config::TEMPLATE).unwrap();
        std::fs::write(layout.system_unit("nightgrid-node-peer.service"), "").unwrap();
        let info = serde_json::json!({
            "node": {"version": env!("CARGO_PKG_VERSION")},
            "rssBytes": 9 * 1048576, "dbBytes": 1048576,
            "access": {"peers": "ok"},
        });
        let units = HashMap::from([("nightgrid-node", "active"), ("nightgrid-node-peer", "active")]);
        let mut p = Fake { units: units.clone(), info: Ok(info.clone()) };
        let r = run(&layout, &layout.config(), None, &mut p);
        assert_eq!(r.worst(), Level::Info, "{}", render(&r));
        assert_eq!(level(&r, "allow peers"), Level::Ok);

        // The link unit died, the node is over budget and an older build runs.
        let mut bad = info;
        bad["rssBytes"] = (25 * 1048576).into();
        bad["node"]["version"] = "0.1.0".into();
        let mut p = Fake {
            units: HashMap::from([("nightgrid-node", "active"), ("nightgrid-node-peer", "failed")]),
            info: Ok(bad),
        };
        let r = run(&layout, &layout.config(), None, &mut p);
        assert_eq!(
            (level(&r, "allow peers"), level(&r, "memory"), level(&r, "version")),
            (Level::Warn, Level::Warn, Level::Warn)
        );

        // A broken config and a stopped service fail.
        std::fs::write(layout.config(), "[metrics]\ninterval_secs = 1\n").unwrap();
        let mut p =
            Fake { units: HashMap::from([("nightgrid-node", "failed")]), info: Err("Connection refused".into()) };
        let r = run(&layout, &layout.config(), None, &mut p);
        assert_eq!((level(&r, "config"), level(&r, "service"), r.worst()), (Level::Fail, Level::Fail, Level::Fail));
        assert!(render(&r).contains("FAIL  config"), "{}", render(&r));
    }
}
