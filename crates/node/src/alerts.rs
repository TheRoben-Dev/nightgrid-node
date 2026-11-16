//! Alert tracking on the node.
//!
// `Tracker` (grace periods, clear holds, history-free) and `disk_full` are
// vendored from Nightgrid (https://github.com/TheRoben-Dev/nightgrid),
// core/src/alerts.rs at commit 8eedcf2, MIT License, (c) 2026 TheRoben.
// NODE: keys have no host segment; one report covers every node source
// (there is no partial scope); active alerts survive a restart.

use std::collections::{HashMap, HashSet};

use nightgrid_node_proto::{Alert, AlertKind, MountUsage, Severity};
use serde::{Deserialize, Serialize};

/// Seconds a condition must hold before it fires (the app's values).
pub fn fire_after(k: AlertKind) -> i64 {
    match k {
        AlertKind::HostOffline => 120,
        AlertKind::ServiceDown => 90,
        _ => 0,
    }
}

/// Seconds a condition must be gone before its alert resolves.
pub fn clear_after(k: AlertKind) -> i64 {
    match k {
        AlertKind::DiskFull => 300,
        AlertKind::DiskFilling => 3 * 3600,
        _ => 0,
    }
}

pub fn label(k: AlertKind) -> &'static str {
    match k {
        AlertKind::HostOffline => "HOST OFFLINE",
        AlertKind::DiskFull => "DISK FULL",
        AlertKind::ContainerDown => "CONTAINER DOWN",
        AlertKind::ServiceDown => "SERVICE DOWN",
        AlertKind::CertExpiring => "CERT EXPIRING",
        AlertKind::SmartWarning => "SMART WARNING",
        AlertKind::BackupFailed => "BACKUP FAILED",
        AlertKind::BackupStale => "BACKUP STALE",
        AlertKind::LogBurst => "ERROR BURST",
        AlertKind::LogNewError => "NEW ERROR",
        AlertKind::ExpirySoon => "EXPIRING",
        AlertKind::DiskFilling => "FILLING UP",
        AlertKind::PoolDegraded => "POOL DEGRADED",
        AlertKind::UpsOnBattery => "ON BATTERY",
        AlertKind::LogMatch => "LOG MATCH",
    }
}

/// Something wrong right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    /// Stable identity, e.g. `disk:/srv`, prefixed with the source.
    pub key: String,
    pub kind: AlertKind,
    pub severity: Severity,
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Fired(Alert),
    /// Severity or detail changed while active.
    Updated {
        alert: Alert,
        worse: bool,
    },
    Resolved(Alert),
}

/// What survives a restart.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Saved {
    pub active: Vec<Alert>,
    pub next_id: u32,
}

#[derive(Debug, Default)]
pub struct Tracker {
    pending: HashMap<String, (Condition, i64)>,
    active: HashMap<String, Alert>,
    clearing: HashMap<String, i64>,
    next_id: u32,
}

impl Tracker {
    pub fn restore(saved: Saved) -> Self {
        Self {
            active: saved.active.into_iter().map(|a| (a.key.clone(), a)).collect(),
            next_id: saved.next_id,
            ..Default::default()
        }
    }

    pub fn save(&self) -> Saved {
        Saved { active: self.active(), next_id: self.next_id }
    }

    /// Takes every current condition and returns what changed.
    pub fn report(&mut self, conditions: Vec<Condition>, now: i64) -> Vec<Change> {
        let mut changes = Vec::new();
        let seen: HashSet<String> = conditions.iter().map(|c| c.key.clone()).collect();
        for c in conditions {
            if let Some(a) = self.active.get_mut(&c.key) {
                self.clearing.remove(&c.key);
                if a.severity != c.severity || a.detail != c.detail || a.title != c.title {
                    let worse = c.severity > a.severity;
                    a.severity = c.severity;
                    a.detail = c.detail;
                    a.title = c.title;
                    changes.push(Change::Updated { alert: a.clone(), worse });
                }
                continue;
            }
            let first = self.pending.get(&c.key).map(|(_, t)| *t).unwrap_or(now);
            if now - first >= fire_after(c.kind) {
                self.pending.remove(&c.key);
                self.next_id = self.next_id.wrapping_add(1);
                let alert = Alert {
                    id: self.next_id,
                    key: c.key.clone(),
                    kind: c.kind,
                    severity: c.severity,
                    title: c.title,
                    detail: c.detail,
                    since: first,
                    resolved: None,
                };
                self.active.insert(c.key, alert.clone());
                changes.push(Change::Fired(alert));
            } else {
                self.pending.insert(c.key.clone(), (c, first));
            }
        }
        self.pending.retain(|k, _| seen.contains(k));
        let gone: Vec<String> = self.active.keys().filter(|k| !seen.contains(*k)).cloned().collect();
        for key in gone {
            let since = *self.clearing.entry(key.clone()).or_insert(now);
            let kind = self.active[&key].kind;
            if now - since >= clear_after(kind) {
                self.clearing.remove(&key);
                if let Some(mut a) = self.active.remove(&key) {
                    a.resolved = Some(now);
                    changes.push(Change::Resolved(a));
                }
            }
        }
        changes
    }

    /// Active alerts, worst and newest first.
    pub fn active(&self) -> Vec<Alert> {
        let mut v: Vec<Alert> = self.active.values().cloned().collect();
        v.sort_by(|a, b| b.severity.cmp(&a.severity).then(b.since.cmp(&a.since)).then(b.id.cmp(&a.id)));
        v
    }
}

/// Mounts at or above `threshold` percent used (critical from 98 %).
pub fn disk_full(host: &str, mounts: &[MountUsage], threshold: u8) -> Vec<Condition> {
    mounts
        .iter()
        .filter_map(|m| {
            let size = m.used + m.avail;
            if size == 0 {
                return None;
            }
            let pct = m.used as f64 * 100.0 / size as f64;
            (pct >= f64::from(threshold)).then(|| Condition {
                key: format!("disk:{}", m.mount),
                kind: AlertKind::DiskFull,
                severity: if pct >= 98.0 { Severity::Critical } else { Severity::Warn },
                title: host.to_owned(),
                detail: format!("{} is {:.0}% full ({} free)", m.mount, pct.floor(), human_bytes(m.avail)),
            })
        })
        .collect()
}

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 || v >= 10.0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(key: &str, kind: AlertKind, sev: Severity, detail: &str) -> Condition {
        Condition { key: key.into(), kind, severity: sev, title: "server01".into(), detail: detail.into() }
    }

    #[test]
    fn grace_hold_update_and_resolve() {
        let mut t = Tracker::default();
        let d = |sev, det: &str| cond("disk:/srv", AlertKind::DiskFull, sev, det);
        let ch = t.report(vec![d(Severity::Warn, "91%")], 100);
        assert!(matches!(&ch[..], [Change::Fired(a)] if a.since == 100 && a.id == 1));
        let ch = t.report(vec![d(Severity::Critical, "98%")], 160);
        assert!(matches!(&ch[..], [Change::Updated { worse: true, .. }]));
        // Gone, but disks hold for 5 minutes before resolving.
        assert!(t.report(vec![], 200).is_empty());
        assert!(t.report(vec![], 400).is_empty());
        let ch = t.report(vec![], 500);
        assert!(matches!(&ch[..], [Change::Resolved(a)] if a.resolved == Some(500)));
        assert!(t.active().is_empty());

        // Service down waits 90 s before firing; a blip never fires.
        let s = || cond("svc:web", AlertKind::ServiceDown, Severity::Critical, "timeout");
        assert!(t.report(vec![s()], 1000).is_empty());
        assert!(t.report(vec![], 1030).is_empty());
        assert!(t.report(vec![s()], 1060).is_empty());
        assert!(matches!(&t.report(vec![s()], 1150)[..], [Change::Fired(a)] if a.since == 1060));
    }

    #[test]
    fn active_alerts_survive_a_restart() {
        let mut t = Tracker::default();
        t.report(vec![cond("disk:/", AlertKind::DiskFull, Severity::Warn, "92%")], 10);
        let saved: Saved = serde_json::from_str(&serde_json::to_string(&t.save()).unwrap()).unwrap();
        let mut t = Tracker::restore(saved);
        // Still there: no new "fired".
        assert!(t.report(vec![cond("disk:/", AlertKind::DiskFull, Severity::Warn, "92%")], 20).is_empty());
        let ch = t.report(vec![cond("disk:/x", AlertKind::DiskFull, Severity::Warn, "92%")], 30);
        assert!(matches!(&ch[..], [Change::Fired(a)] if a.id == 2));
    }

    #[test]
    fn disk_full_rule() {
        let m = |mount: &str, used, avail| MountUsage {
            device: "d".into(),
            mount: mount.into(),
            fstype: "ext4".into(),
            total: used + avail,
            used,
            avail,
        };
        let c = disk_full("server01", &[m("/", 95, 5), m("/srv", 50, 50), m("/full", 99, 1)], 90);
        assert_eq!(
            c.iter().map(|c| (c.key.as_str(), c.severity)).collect::<Vec<_>>(),
            [("disk:/", Severity::Warn), ("disk:/full", Severity::Critical)]
        );
        assert_eq!(c[0].detail, "/ is 95% full (5 B free)");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
    }
}
