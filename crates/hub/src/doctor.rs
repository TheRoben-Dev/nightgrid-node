//! `nightgrid-hub doctor`: a read-only check of this host's hub, in the same
//! shape as `nightgrid-node doctor` (`{version, findings: [{check, level,
//! detail}], arch, libc, init, kernel, ...}`; levels ok/info/warn/fail).
//! With `--config PATH` it checks that file instead of the installed one, so
//! the app can refuse a bad hub config before saving and restarting.

use std::path::Path;
use std::process::Command;

use nightgrid_node::doctor::{Level, Report};
use nightgrid_node_proto::hub::{MemberState, Overview};

use crate::config::Config;
use crate::install::{Layout, COLLECT_SERVICE, SERVICE};

/// What the doctor learns from outside its own process.
pub trait Probe {
    /// `systemctl is-active UNIT`, or `None` without systemd.
    fn unit_state(&mut self, unit: &str) -> Option<String>;
    /// The running hub's `fleet.overview`, or why it could not be asked.
    fn overview(&mut self, socket: &Path) -> Result<Overview, String>;
}

pub struct RealProbe;

impl Probe for RealProbe {
    fn unit_state(&mut self, unit: &str) -> Option<String> {
        if !Path::new("/run/systemd/system").is_dir() {
            return None;
        }
        let out = Command::new("systemctl").args(["is-active", unit]).output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (!s.is_empty()).then_some(s)
    }
    fn overview(&mut self, socket: &Path) -> Result<Overview, String> {
        let mut c =
            nightgrid_node::client::Client::connect(socket, "nightgrid-hub doctor").map_err(|e| e.to_string())?;
        let v =
            c.call(nightgrid_node_proto::hub::method::OVERVIEW, serde_json::Value::Null).map_err(|e| e.to_string())?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }
}

/// Runs every check.
pub fn run(layout: &Layout, config_path: &Path, socket: Option<&Path>, probe: &mut dyn Probe) -> Report {
    let mut r = Report { version: env!("CARGO_PKG_VERSION"), ..Default::default() };
    let node_layout = nightgrid_node::install::Layout { root: layout.root.clone() };
    let facts = node_layout.facts();
    r.add("host", Level::Info, format!("{} ({}), {}, kernel {}", facts.arch, facts.libc, facts.init, facts.kernel));
    let systemd = facts.init == "systemd";
    r.host = Some(facts);

    let installed = layout.bin().exists();
    if installed {
        r.add("install", Level::Ok, format!("{} is installed", layout.bin().display()));
    } else {
        r.add("install", Level::Info, "the hub is not installed as a service (root can run: nightgrid-hub install)");
    }
    if layout.node_bin().exists() {
        r.add("node", Level::Ok, "Nightgrid Node is installed (the hub reads this host through it)");
    } else {
        let level = if installed { Level::Fail } else { Level::Info };
        r.add(
            "node",
            level,
            format!("{} is missing: the hub needs Nightgrid Node on its host", layout.node_bin().display()),
        );
    }
    if !systemd {
        let level = if installed { Level::Fail } else { Level::Info };
        r.add("init", level, "the hub needs systemd");
    }

    // The config: the given file, or the installed one.
    let cfg = if config_path.exists() {
        match Config::load(config_path) {
            Ok(c) => {
                r.add("config", Level::Ok, format!("{} is valid", config_path.display()));
                Some(c)
            }
            Err(e) => {
                r.add("config", Level::Fail, e);
                None
            }
        }
    } else {
        let level = if installed { Level::Warn } else { Level::Info };
        r.add("config", level, format!("{} is missing; defaults apply", config_path.display()));
        Some(Config::default())
    };

    // Units.
    let collect_allowed = crate::install::collect_allowed(layout);
    for (unit, needed) in [(SERVICE, installed), (COLLECT_SERVICE, collect_allowed)] {
        let check = format!("unit {unit}");
        match probe.unit_state(unit) {
            None => r.add(check, Level::Info, "systemd not reachable; skipped"),
            Some(s) if s == "active" => r.add(check, Level::Ok, "active"),
            Some(s) if needed => r.add(check, Level::Fail, format!("{s} (journalctl -u {unit})")),
            Some(_) if unit == COLLECT_SERVICE => {
                r.add(check, Level::Info, "off (root can run: nightgrid-hub allow collect)")
            }
            Some(s) => r.add(check, Level::Info, s),
        }
    }

    // The socket and what the running hub says.
    let socket = socket
        .map(Path::to_path_buf)
        .or_else(|| cfg.as_ref().map(|c| c.server.socket.clone()))
        .unwrap_or_else(|| nightgrid_node_proto::hub::DEFAULT_SOCKET.into());
    let overview = match probe.overview(&socket) {
        Ok(o) => {
            r.add("socket", Level::Ok, format!("{} answers", socket.display()));
            Some(o)
        }
        Err(e) => {
            let hint = if e.contains("ermission denied") {
                " (this user is not in the nightgrid-hub group; root can run: nightgrid-hub install --user NAME)"
            } else {
                ""
            };
            let level = if installed { Level::Warn } else { Level::Info };
            r.add("socket", level, format!("{}: {e}{hint}", socket.display()));
            None
        }
    };

    // The collector: reporting, and on every configured address.
    if let Some(o) = &overview {
        match o.collector.as_str() {
            "ok" => {
                let all =
                    if o.listen_all.is_empty() { o.listening.iter().cloned().collect() } else { o.listen_all.clone() };
                let wanted = cfg.as_ref().map_or(0, |c| c.collect.listen.entries().len());
                if all.len() < wanted {
                    r.add(
                        "collector",
                        Level::Warn,
                        format!(
                            "reports, but answers pings on {} of the {wanted} [collect] listen addresses ({}); \
                             members that use the others show the hub as unreachable",
                            all.len(),
                            if all.is_empty() { "none".to_owned() } else { all.join(", ") }
                        ),
                    );
                } else {
                    r.add("collector", Level::Ok, format!("reports; answers pings on {}", all.join(", ")));
                }
            }
            "off" if !collect_allowed => r.add("collector", Level::Info, "off"),
            other => r.add("collector", Level::Warn, format!("collector is {other} (journalctl -u {COLLECT_SERVICE})")),
        }
        let stuck: Vec<String> = o
            .members
            .iter()
            .filter(|m| matches!(m.state, MemberState::Unreachable | MemberState::Consent | MemberState::Version))
            .map(|m| {
                format!(
                    "{} ({})",
                    m.name,
                    serde_json::to_value(m.state).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
                )
            })
            .collect();
        let level = if stuck.is_empty() { Level::Ok } else { Level::Warn };
        let detail = if stuck.is_empty() {
            format!("{} member(s)", o.members.len())
        } else {
            format!("{} member(s); not pulled: {}", o.members.len(), stuck.join(", "))
        };
        r.add("members", level, detail);
    }

    // The store: under its cap, with room on its disk.
    let data = layout.data_dir();
    if data.is_dir() || overview.is_some() {
        let max = cfg.as_ref().map_or(4, |c| c.store.max_gb);
        let cap = u64::from(max) * 1_000_000_000;
        let used = overview.as_ref().map(|o| o.store_bytes);
        let free = free_bytes(&data);
        let mut detail = match used {
            Some(u) => format!("{} of its {max} GB cap", nightgrid_node::cli::bytes(u)),
            None => format!("{} (size unknown while the hub doesn't answer)", data.display()),
        };
        if let Some(f) = free {
            detail.push_str(&format!(", {} free on its disk", nightgrid_node::cli::bytes(f)));
        }
        let level = if used.is_some_and(|u| u * 10 >= cap * 9) || free.is_some_and(|f| f < 512 * 1024 * 1024) {
            Level::Warn
        } else {
            Level::Ok
        };
        r.add("store", level, detail);
    } else if installed {
        r.add("store", Level::Warn, format!("{} is missing", data.display()));
    }
    r
}

/// Bytes available to unprivileged users on `path`'s filesystem.
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` a writable statvfs.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)] // c_ulong is 32-bit on 32-bit targets.
    let frsize = if st.f_frsize > 0 { st.f_frsize as u64 } else { st.f_bsize as u64 };
    #[allow(clippy::unnecessary_cast)]
    Some((st.f_bavail as u64).saturating_mul(frsize))
}

/// Plain text for a terminal.
pub fn render(r: &Report) -> String {
    nightgrid_node::doctor::render(r).replacen("nightgrid-node", "nightgrid-hub", 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Fake {
        units: HashMap<&'static str, &'static str>,
        overview: Result<Overview, String>,
    }
    impl Probe for Fake {
        fn unit_state(&mut self, unit: &str) -> Option<String> {
            self.units.get(unit).map(|s| s.to_string())
        }
        fn overview(&mut self, _: &Path) -> Result<Overview, String> {
            self.overview.clone()
        }
    }

    fn level(r: &Report, check: &str) -> Level {
        r.findings.iter().find(|f| f.check == check).unwrap_or_else(|| panic!("{check}: {r:?}")).level
    }

    fn overview(listen_all: &[&str]) -> Overview {
        Overview {
            hub_id: "h".into(),
            fingerprint: Some("f".into()),
            listening: listen_all.first().map(|s| (*s).to_owned()),
            listen_all: listen_all.iter().map(|s| (*s).to_owned()).collect(),
            collector: "ok".into(),
            store_bytes: 1_000_000,
            members: vec![],
        }
    }

    #[test]
    fn checks_config_units_socket_collector_and_store() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout { root: d.path().into() };
        std::fs::create_dir_all(d.path().join("run/systemd/system")).unwrap();
        for p in [layout.bin(), layout.node_bin(), layout.config(), layout.unit(COLLECT_SERVICE)] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
        }
        std::fs::create_dir_all(layout.data_dir()).unwrap();
        std::fs::write(layout.config(), "[collect]\nlisten = [\"127.0.0.1:17420\", \"127.0.0.2:17420\"]\n").unwrap();
        let units = HashMap::from([(SERVICE, "active"), (COLLECT_SERVICE, "active")]);
        let mut p = Fake { units: units.clone(), overview: Ok(overview(&["127.0.0.1:17420", "127.0.0.2:17420"])) };
        let r = run(&layout, &layout.config(), None, &mut p);
        assert!(r.worst() <= Level::Info, "{}", render(&r));
        assert_eq!((level(&r, "collector"), level(&r, "store")), (Level::Ok, Level::Ok));
        assert!(render(&r).starts_with("nightgrid-hub "), "{}", render(&r));
        let j = serde_json::to_value(&r).unwrap();
        assert!(j["findings"][0]["check"].is_string() && j["init"].is_string() && j["arch"].is_string(), "{j}");

        // One address not bound: named.
        let mut p = Fake { units: units.clone(), overview: Ok(overview(&["127.0.0.1:17420"])) };
        assert_eq!(level(&run(&layout, &layout.config(), None, &mut p), "collector"), Level::Warn);

        // A bad config given with --config fails, as the app's check needs.
        let bad = d.path().join("new.toml");
        std::fs::write(&bad, "[store]\nmax_gb = 0\n").unwrap();
        let mut p = Fake { units, overview: Ok(overview(&["127.0.0.1:17420"])) };
        let r = run(&layout, &bad, None, &mut p);
        assert_eq!((level(&r, "config"), r.worst()), (Level::Fail, Level::Fail));
        std::fs::write(&bad, "[collect]\nlisten_on = 1\n").unwrap();
        assert_eq!(level(&run(&layout, &bad, None, &mut p), "config"), Level::Fail);

        // The hub stopped.
        let mut p = Fake {
            units: HashMap::from([(SERVICE, "failed"), (COLLECT_SERVICE, "inactive")]),
            overview: Err("Connection refused".into()),
        };
        let r = run(&layout, &layout.config(), None, &mut p);
        assert_eq!(level(&r, "unit nightgrid-hub"), Level::Fail);
    }

    #[test]
    fn a_host_without_the_hub_is_only_informed() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout { root: d.path().into() };
        std::fs::create_dir_all(d.path().join("run/systemd/system")).unwrap();
        let mut p = Fake { units: HashMap::new(), overview: Err("No such file or directory".into()) };
        let r = run(&layout, &layout.config(), None, &mut p);
        assert_eq!(r.worst(), Level::Info, "{}", render(&r));
    }
}
