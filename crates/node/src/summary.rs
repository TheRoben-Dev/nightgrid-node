//! Daily summary (N9): what happened on this host over the last day, as
//! short plain facts for the app's DRIFT helper, a small local model.
//!
//! The node keeps the timeline the app would otherwise have missed while it
//! was closed, so it can say what changed (files, ports, devices, logins),
//! what broke (outages, alerts, crashes, failed jobs) and what is filling up
//! (disk forecasts) without the app reading anything over SSH. It is made on
//! request from the node's own history and current state, and nothing is
//! sent anywhere: the app asks for it like any other read.
//!
//! The lines follow the app's own daily summary facts (`summaryFacts`):
//! each starts with what it is about, times are relative ("3 h ago"), lists
//! are capped, and repeated events are counted instead of listed.

use std::collections::BTreeMap;

use nightgrid_node_proto::{Alert, DailySummary, Event, EventKind, Level, OutageState, Severity};

use crate::diskusage::MountForecast;
use crate::health::packages::UpdatesReport;

/// Most events read for one summary.
pub const MAX_EVENTS: usize = 5000;
const LIST: usize = 6;

/// What a summary is made from.
#[derive(Debug, Default)]
pub struct Input<'a> {
    pub host: &'a str,
    pub now: i64,
    pub from: i64,
    /// The oldest event the node still has (or when it started).
    pub watched_since: i64,
    /// Events in the window, oldest first.
    pub events: &'a [Event],
    pub active: &'a [Alert],
    pub forecasts: &'a [MountForecast],
    pub updates: Option<&'a UpdatesReport>,
}

fn ago(secs: i64) -> String {
    let s = secs.max(0);
    if s < 3600 {
        format!("{} min ago", (s / 60).max(1))
    } else if s < 2 * 86_400 {
        format!("{} h ago", s / 3600)
    } else {
        format!("{} days ago", s / 86_400)
    }
}

fn lasted(secs: i64) -> String {
    let s = secs.max(0);
    if s < 3600 {
        format!("{} min", (s / 60).max(1))
    } else if s < 2 * 86_400 {
        let (h, m) = (s / 3600, (s % 3600) / 60);
        if m == 0 {
            format!("{h} h")
        } else {
            format!("{h} h {m} min")
        }
    } else {
        format!("{} days", s / 86_400)
    }
}

/// "a (×3), b, c and 2 more", in order of first appearance.
fn counted<'a>(titles: impl IntoIterator<Item = &'a str>) -> String {
    let mut order: Vec<&str> = Vec::new();
    let mut n: BTreeMap<&str, usize> = BTreeMap::new();
    for t in titles {
        let c = n.entry(t).or_insert(0);
        if *c == 0 {
            order.push(t);
        }
        *c += 1;
    }
    let mut parts: Vec<String> =
        order.iter().take(LIST).map(|t| if n[t] > 1 { format!("{t} (×{})", n[t]) } else { (*t).to_owned() }).collect();
    if order.len() > LIST {
        parts.push(format!("{} more", order.len() - LIST));
    }
    parts.join("; ")
}

fn of(events: &[Event], kind: EventKind) -> Vec<&Event> {
    events.iter().filter(|e| e.kind == kind).collect()
}

fn bad(e: &Event) -> bool {
    matches!(e.level, Level::Warn | Level::Critical)
}

/// The summary. Pure, so it can be tested.
pub fn summarize(i: &Input<'_>) -> DailySummary {
    let mut facts = Vec::new();
    let mut worst = Level::Ok;
    let mut raise = |l: Level| {
        let rank = |l: Level| match l {
            Level::Critical => 3,
            Level::Warn => 2,
            _ => 1,
        };
        if rank(l) > rank(worst) {
            worst = l;
        }
    };
    let hours = ((i.now - i.from) as f64 / 3600.0).round() as i64;
    let complete = i.watched_since <= i.from;
    let window = format!("the last {hours} h");
    if !complete {
        facts.push(format!(
            "Coverage: the node has only watched this host since {}, so earlier events are missing.",
            ago(i.now - i.watched_since)
        ));
    }
    let ev = i.events;

    // Up and down.
    let outages: Vec<String> = of(ev, EventKind::Outage)
        .into_iter()
        .filter_map(|e| e.outage.as_ref())
        .map(|o| {
            let how = match o.state {
                OutageState::Down => "down (crash or power loss)",
                OutageState::Asleep => "off or asleep",
            };
            format!("{how} for {} from {}", lasted(o.end - o.start), ago(i.now - o.start))
        })
        .collect();
    let boots = of(ev, EventKind::Boot).len();
    if !outages.is_empty() {
        if outages.iter().any(|o| o.starts_with("down")) {
            raise(Level::Warn);
        }
        facts.push(format!("Uptime: {}.", outages.join("; ")));
    }
    if boots > 0 {
        facts.push(format!("Restarts: booted {boots} time{} in {window}.", if boots == 1 { "" } else { "s" }));
    }
    if outages.is_empty() && boots == 0 && complete {
        facts.push(format!("Uptime: up the whole of {window}."));
    }

    // Alerts.
    if i.active.is_empty() {
        facts.push("Active alerts: none.".into());
    } else {
        let mut a: Vec<&Alert> = i.active.iter().collect();
        a.sort_by_key(|a| (a.severity != Severity::Critical, a.since));
        raise(if a.iter().any(|a| a.severity == Severity::Critical) { Level::Critical } else { Level::Warn });
        let list: Vec<String> = a
            .iter()
            .take(LIST)
            .map(|a| {
                format!(
                    "{}{} since {}",
                    if a.severity == Severity::Critical { "CRITICAL " } else { "" },
                    a.title,
                    ago(i.now - a.since)
                )
            })
            .collect();
        facts.push(format!(
            "Active alerts ({}): {}{}.",
            a.len(),
            list.join("; "),
            if a.len() > LIST { format!("; {} more", a.len() - LIST) } else { String::new() }
        ));
    }
    let resolved: Vec<String> = of(ev, EventKind::Recovered)
        .into_iter()
        .filter_map(|e| e.alert.as_ref())
        .map(|a| format!("{} (lasted {})", a.title, lasted(a.resolved.unwrap_or(i.now) - a.since)))
        .collect();
    if !resolved.is_empty() {
        facts.push(format!(
            "Resolved in {window} ({}): {}.",
            resolved.len(),
            counted(resolved.iter().map(String::as_str))
        ));
    }

    // What broke.
    let containers = of(ev, EventKind::Container);
    let broken: Vec<&str> = containers.iter().filter(|e| bad(e)).map(|e| e.title.as_str()).collect();
    if !broken.is_empty() {
        raise(Level::Warn);
        facts.push(format!("Containers: {}.", counted(broken)));
    }
    let jobs = of(ev, EventKind::Job);
    let failed: Vec<&str> = jobs.iter().filter(|e| bad(e)).map(|e| e.title.as_str()).collect();
    let ok = jobs.len() - failed.len();
    if !failed.is_empty() {
        raise(Level::Warn);
        facts.push(format!(
            "Jobs and backups: {}{}.",
            counted(failed),
            if ok > 0 { format!("; {ok} other run{} fine", if ok == 1 { "" } else { "s" }) } else { String::new() }
        ));
    } else if ok > 0 {
        facts.push(format!("Jobs and backups: {ok} run{} finished fine.", if ok == 1 { "" } else { "s" }));
    }
    let actions = of(ev, EventKind::Action);
    if !actions.is_empty() {
        facts.push(format!(
            "Actions run (self-healing, the app, the UPS guard): {}.",
            counted(actions.iter().map(|e| e.title.as_str()))
        ));
    }

    // What changed.
    let logins = of(ev, EventKind::Login);
    let (failed_logins, good): (Vec<&Event>, Vec<&Event>) =
        logins.iter().partition(|e| e.title.contains("failed login"));
    if !good.is_empty() {
        facts.push(format!("Logins: {}.", counted(good.iter().map(|e| e.title.as_str()))));
    }
    if !failed_logins.is_empty() {
        // Failed logins are summed per minute: "12 failed logins from 3 addresses".
        let n: u64 = failed_logins
            .iter()
            .map(|e| e.title.split(' ').next().and_then(|w| w.parse::<u64>().ok()).unwrap_or(1))
            .sum();
        facts.push(format!("Failed logins: {n} in {window}."));
    }
    for (kind, label) in
        [(EventKind::File, "Files changed"), (EventKind::Port, "Listening ports"), (EventKind::Network, "Network")]
    {
        let list = of(ev, kind);
        if !list.is_empty() {
            if list.iter().any(|e| bad(e)) {
                raise(Level::Warn);
            }
            facts.push(format!("{label}: {}.", counted(list.iter().map(|e| e.title.as_str()))));
        }
    }
    let other: Vec<&str> = of(ev, EventKind::Other)
        .into_iter()
        .filter(|e| bad(e) || e.title.starts_with("upgraded"))
        .map(|e| e.title.as_str())
        .collect();
    if !other.is_empty() {
        facts.push(format!("Other: {}.", counted(other)));
    }

    // What is filling up.
    let mut filling: Vec<(f64, String)> = i
        .forecasts
        .iter()
        .filter_map(|f| {
            let pct = if f.size > 0 { f.used as f64 * 100.0 / f.size as f64 } else { 0.0 };
            match f.days_left {
                Some(d) if d < 30.0 => {
                    Some((d, format!("{} {pct:.0}% full, full in about {}", f.mount, crate::diskusage::days_words(d))))
                }
                _ if pct >= 85.0 => Some((1e9 - pct, format!("{} {pct:.0}% full", f.mount))),
                _ => None,
            }
        })
        .collect();
    filling.sort_by(|a, b| a.0.total_cmp(&b.0));
    if filling.is_empty() {
        if !i.forecasts.is_empty() {
            facts.push("Disks: nothing filling up.".into());
        }
    } else {
        raise(if filling.iter().any(|(d, _)| *d < 7.0) { Level::Critical } else { Level::Warn });
        facts.push(format!(
            "Disks: {}.",
            filling.iter().take(LIST).map(|(_, s)| s.as_str()).collect::<Vec<_>>().join("; ")
        ));
    }
    if let Some(u) = i.updates {
        let reboot = if u.reboot_required { "; a reboot is needed" } else { "" };
        facts.push(if u.updates.is_empty() {
            format!("Updates: none pending (from the package cache){reboot}.")
        } else {
            format!("Updates: {} pending (from the package cache){reboot}.", u.updates.len())
        });
    }

    DailySummary {
        host: i.host.to_owned(),
        from: i.from,
        to: i.now,
        level: match worst {
            Level::Critical => "critical",
            Level::Warn => "warn",
            _ => "ok",
        }
        .into(),
        complete,
        facts,
    }
}

/// Reads what the summary needs and makes it.
pub fn daily(shared: &crate::daemon::Shared, hours: u32) -> Result<DailySummary, String> {
    let now = crate::daemon::now_secs();
    let from = now - i64::from(hours.clamp(1, 168)) * 3600;
    let (events, oldest, forecasts) = {
        let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        let events = st.events_between(from, now, MAX_EVENTS).map_err(|e| e.to_string())?;
        let oldest = st.events_oldest_at().map_err(|e| e.to_string())?;
        let forecasts = st.disk_forecasts(now).map_err(|e| e.to_string())?;
        (events, oldest, forecasts)
    };
    let active = shared.active.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let updates =
        shared.health.lock().unwrap_or_else(|e| e.into_inner()).result.updates.as_ref().and_then(|r| r.report.clone());
    Ok(summarize(&Input {
        host: &shared.identity.hostname,
        now,
        from,
        watched_since: oldest.unwrap_or(shared.started).min(shared.started),
        events: &events,
        active: &active,
        forecasts: &forecasts,
        updates: updates.as_ref(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightgrid_node_proto::{AlertKind, OutageSpan};

    fn ev(at: i64, kind: EventKind, level: Level, title: &str, detail: &str) -> Event {
        Event { seq: at, at, kind, level, title: title.into(), detail: detail.into(), alert: None, outage: None }
    }

    #[test]
    fn a_quiet_day() {
        let s = summarize(&Input {
            host: "nas",
            now: 100_000,
            from: 100_000 - 86_400,
            watched_since: 0,
            ..Default::default()
        });
        assert_eq!(s.level, "ok");
        assert!(s.complete);
        assert_eq!(s.facts, ["Uptime: up the whole of the last 24 h.", "Active alerts: none."]);
    }

    #[test]
    fn a_busy_day() {
        let now = 1_000_000;
        let mut events = vec![
            ev(now - 20_000, EventKind::Boot, Level::Info, "booted", ""),
            ev(now - 3000, EventKind::Container, Level::Warn, "jellyfin restarted", ""),
            ev(now - 2000, EventKind::Container, Level::Warn, "jellyfin restarted", ""),
            ev(now - 1000, EventKind::Container, Level::Info, "jellyfin healthy", ""),
            ev(now - 900, EventKind::Job, Level::Critical, "backup nightly failed (exit 1)", ""),
            ev(now - 800, EventKind::Job, Level::Info, "job sync finished", ""),
            ev(now - 700, EventKind::Login, Level::Info, "robin logged in from 100.64.0.2", "ssh, publickey"),
            ev(now - 650, EventKind::Login, Level::Info, "failed login for admin from 203.0.113.9", ""),
            ev(
                now - 600,
                EventKind::Login,
                Level::Warn,
                "11 failed logins from 1 address",
                "users: root; from: 203.0.113.9 ×11",
            ),
            ev(now - 500, EventKind::File, Level::Warn, "sshd_config changed", ""),
            ev(now - 400, EventKind::Other, Level::Info, "upgraded to 0.22.0", ""),
            ev(now - 300, EventKind::Other, Level::Info, "peer pi reachable again", ""),
        ];
        let mut out = ev(now - 20_000, EventKind::Outage, Level::Warn, "down", "");
        out.outage = Some(OutageSpan { start: now - 27_200, end: now - 20_000, state: OutageState::Down });
        events.insert(0, out);
        let alert = Alert {
            id: 1,
            key: "disk:/".into(),
            kind: AlertKind::DiskFull,
            severity: Severity::Critical,
            title: "Disk / 96% full on nas".into(),
            detail: String::new(),
            since: now - 7200,
            resolved: None,
        };
        let fc = [
            MountForecast { mount: "/".into(), size: 100, used: 96, at: now, per_day: Some(1.0), days_left: Some(4.0) },
            MountForecast { mount: "/srv".into(), size: 100, used: 10, at: now, per_day: None, days_left: None },
        ];
        let s = summarize(&Input {
            host: "nas",
            now,
            from: now - 86_400,
            watched_since: now - 40_000,
            events: &events,
            active: std::slice::from_ref(&alert),
            forecasts: &fc,
            updates: None,
        });
        assert_eq!(s.level, "critical");
        assert!(!s.complete);
        let f = s.facts.join("\n");
        for want in [
            "Coverage: the node has only watched this host since 11 h ago",
            "Uptime: down (crash or power loss) for 2 h from 7 h ago.",
            "Restarts: booted 1 time in the last 24 h.",
            "Active alerts (1): CRITICAL Disk / 96% full on nas since 2 h ago.",
            "Containers: jellyfin restarted (×2).",
            "Jobs and backups: backup nightly failed (exit 1); 1 other run fine.",
            "Logins: robin logged in from 100.64.0.2.",
            "Failed logins: 12 in the last 24 h.",
            "Files changed: sshd_config changed.",
            "Other: upgraded to 0.22.0.",
            "Disks: / 96% full, full in about 4 days.",
        ] {
            assert!(f.contains(want), "missing {want:?} in\n{f}");
        }
        assert!(!f.contains("reachable again"), "good news from other events stays out");
    }

    #[test]
    fn counts_and_caps() {
        assert_eq!(counted(["a", "b", "a"]), "a (×2); b");
        let many: Vec<String> = (0..9).map(|i| format!("t{i}")).collect();
        assert!(counted(many.iter().map(String::as_str)).ends_with("; 3 more"));
    }
}
