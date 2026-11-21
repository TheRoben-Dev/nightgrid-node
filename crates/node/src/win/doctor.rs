//! `nightgrid-node doctor` on Windows: the same read-only self-check and
//! report as on Linux, with Windows sources: the Service Control Manager
//! instead of systemd, the pipe instead of the socket, the service
//! hardening rule instead of the unit's sandbox.

use std::path::Path;

use crate::config::Config;
use crate::doctor::{Level, Report, RSS_BUDGET};

pub fn run(config_path: &Path, socket: Option<&Path>) -> Report {
    let mut r =
        Report { version: env!("CARGO_PKG_VERSION"), host: Some(super::install::facts()), ..Default::default() };

    // Binary.
    let bin = super::installed_binary();
    if bin.is_file() {
        r.add("binary", Level::Ok, bin.display().to_string());
    } else {
        r.add("binary", Level::Fail, format!("{} is missing (run: nightgrid-node install)", bin.display()));
    }

    // Config.
    let config = match Config::load(config_path) {
        Ok(c) => {
            r.add("config", Level::Ok, format!("{} parses and validates", config_path.display()));
            Some(c)
        }
        Err(e) => {
            r.add("config", Level::Fail, format!("{}: {e}", config_path.display()));
            None
        }
    };

    // Service.
    match super::scm::status(super::SERVICE) {
        Ok(Some(s)) if s.running() => r.add("service", Level::Ok, format!("running (pid {})", s.pid)),
        Ok(Some(s)) => r.add(
            "service",
            Level::Fail,
            format!("{} (exit code {}); see the log in {}", s.word(), s.exit, super::data_dir().join("log").display()),
        ),
        Ok(None) => r.add("service", Level::Fail, "not installed (run: nightgrid-node install)"),
        Err(e) => r.add("service", Level::Warn, e),
    }
    match super::scm::command(super::SERVICE) {
        Ok(Some(cmd)) if cmd.contains(&*bin.to_string_lossy()) => {}
        Ok(Some(cmd)) => r.add("service", Level::Warn, format!("runs {cmd}, not {}", bin.display())),
        _ => {}
    }

    // Pipe.
    let pipe = socket
        .map(Path::to_path_buf)
        .or_else(|| config.as_ref().map(|c| c.server.socket.clone()))
        .unwrap_or_else(|| super::PIPE.into());
    match crate::client::Client::connect(&pipe, "nightgrid-node doctor") {
        Ok(mut c) => {
            let v = &c.welcome.node.version;
            if v == env!("CARGO_PKG_VERSION") {
                r.add("pipe", Level::Ok, format!("{} answers (node {v})", pipe.display()));
            } else {
                r.add(
                    "pipe",
                    Level::Warn,
                    format!("{} answers with node {v}, this binary is {}", pipe.display(), env!("CARGO_PKG_VERSION")),
                );
            }
            if let Ok(info) = c.call("node.info", serde_json::Value::Null) {
                if let Some(rss) = info["rssBytes"].as_u64() {
                    let mb = rss as f64 / 1_048_576.0;
                    let level = if rss > RSS_BUDGET { Level::Warn } else { Level::Ok };
                    r.add("memory", level, format!("{mb:.1} MB working set (budget {} MB)", RSS_BUDGET >> 20));
                }
            }
        }
        Err(e) => {
            let denied = e.to_string().contains("os error 5");
            let hint = if denied {
                format!(" (this account is not in {:?} yet, or needs a new login)", super::GROUP)
            } else {
                String::new()
            };
            r.add("pipe", Level::Fail, format!("{}: {e}{hint}", pipe.display()));
        }
    }

    // History.
    let db = super::data_dir().join("node.db");
    match std::fs::metadata(&db) {
        Ok(m) => r.add("history", Level::Ok, format!("{} ({:.1} MB)", db.display(), m.len() as f64 / 1_048_576.0)),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            r.add("history", Level::Info, "only the service and Administrators may read it")
        }
        Err(_) => r.add("history", Level::Info, format!("{} not created yet", db.display())),
    }

    // Event Log access (logins and logs).
    match super::install::eventlog_allowed() {
        Some(true) => r.add("eventlog", Level::Ok, "the service may read the Event Log (logins and logs)"),
        Some(false) => r.add(
            "eventlog",
            Level::Info,
            "logins and log errors are not read (an administrator can run: nightgrid-node allow eventlog)",
        ),
        None => {}
    }

    // Admin helper (actions).
    match super::scm::status(super::helper::SERVICE) {
        Ok(Some(s)) => {
            r.add("helper", Level::Ok, format!("installed, starts when the node needs it ({} now)", s.word()))
        }
        Ok(None) => r.add(
            "helper",
            Level::Info,
            "service and power actions are off (an administrator can run: nightgrid-node allow helper)",
        ),
        Err(_) => {}
    }

    match super::scm::status(super::helper::HEALTH.service) {
        Ok(Some(s)) => r.add(
            "health",
            Level::Ok,
            format!("health helper installed, starts when the node needs it ({} now)", s.word()),
        ),
        Ok(None) => r.add(
            "health",
            Level::Info,
            "disk, Storage Spaces and update health are off (an administrator can run: nightgrid-node allow health)",
        ),
        Err(_) => {}
    }

    // Check runner.
    match super::scm::status(super::checker::SERVICE) {
        Ok(Some(s)) => {
            let fenced = if crate::compat::am_root() { super::checker::restricted() } else { None };
            match (s.running(), fenced) {
                (_, Some(false)) => r.add(
                    "checks",
                    Level::Warn,
                    "the check runner's network limit is missing (run: nightgrid-node allow checks)",
                ),
                (true, _) => r.add("checks", Level::Ok, "check runner running, outbound only to [checks] networks"),
                (false, _) => r.add(
                    "checks",
                    Level::Warn,
                    format!("check runner {} (see {})", s.word(), super::checker::log_dir().display()),
                ),
            }
        }
        Ok(None) => r.add(
            "checks",
            Level::Info,
            "service checks from this host are off (an administrator can run: nightgrid-node allow checks)",
        ),
        Err(_) => {}
    }

    // Network restriction.
    if crate::compat::am_root() {
        match super::install::firewall_restricted() {
            Some(true) => {
                r.add("sandbox", Level::Ok, "all network traffic blocked for the service (Service Hardening)")
            }
            Some(false) => {
                r.add("sandbox", Level::Warn, "the service's network block is missing (run: nightgrid-node install)")
            }
            None => r.add("sandbox", Level::Info, "could not read the service hardening rules"),
        }
    } else {
        r.add("sandbox", Level::Info, "run as Administrator to check the service's network block");
    }

    for l in super::install::limits() {
        r.add("limits", Level::Info, l);
    }
    r
}
