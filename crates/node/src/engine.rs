//! What runs on each sample besides metric history: the uptime heartbeat,
//! disk usage per mount, and alert evaluation. Writes happen once a minute.

use nightgrid_node_proto::{AlertRules, EventKind, Level, Sample, Severity};

use crate::alerts::{self, Change, Condition, Saved, Tracker};
use crate::daemon::Shared;
use crate::diskusage::{self, MountForecast};
use crate::events::NewEvent;
use crate::store::Store;
use crate::uptime::{span_words, Clocks, Heartbeat};

/// Seconds between writes (heartbeat, disk usage, alert evaluation).
pub const MINUTE: i64 = 60;
const META_ALERTS: &str = "alerts";
pub const META_RULES: &str = "alert_rules";

pub struct Engine {
    hb: Heartbeat,
    tracker: Tracker,
    forecasts: Vec<MountForecast>,
    forecast_hour: i64,
    last_write: i64,
}

/// The alert rules stored by `alerts.rules.set`, or the defaults.
pub fn load_rules(store: &Store) -> AlertRules {
    store
        .meta(META_RULES)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<AlertRules>(&v).ok())
        .filter(|r| r.validate().is_ok())
        .unwrap_or_default()
}

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Store> {
    shared.store.lock().unwrap_or_else(|e| e.into_inner())
}

impl Engine {
    pub fn start(shared: &Shared, now: i64) -> Result<Self, String> {
        let mut st = lock(shared);
        let hb = Heartbeat::start(&mut st, &Clocks::read(now)).map_err(|e| format!("uptime: {e}"))?;
        let saved: Saved =
            st.meta(META_ALERTS).ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
        let tracker = Tracker::restore(saved);
        *shared.active.lock().unwrap_or_else(|e| e.into_inner()) = tracker.active();
        Ok(Self { hb, tracker, forecasts: Vec::new(), forecast_hour: i64::MIN, last_write: now })
    }

    /// Called after every sample.
    pub fn sample(&mut self, shared: &Shared, s: &Sample, now: i64) {
        self.hb.beat(now);
        if now - self.last_write >= MINUTE {
            self.last_write = now;
            if let Err(e) = self.minute(shared, s, now) {
                crate::warn!("could not record uptime, disk usage or alerts: {e}");
            }
        }
    }

    fn minute(&mut self, shared: &Shared, s: &Sample, now: i64) -> crate::store::Result<()> {
        let rules = shared.rules.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut st = lock(shared);
        self.hb.flush(&mut st, &Clocks::read(now))?;
        st.disk_usage_record(&s.mounts, now)?;
        let hour = now.div_euclid(3600);
        if hour != self.forecast_hour {
            self.forecast_hour = hour;
            self.forecasts = st.disk_forecasts(now)?;
            st.prune_uptime(now)?;
            st.prune_disk_usage(now)?;
        }
        let host = shared.identity.hostname.as_str();
        let mut conds: Vec<Condition> = Vec::new();
        if rules.enabled {
            if rules.disk_full {
                conds.extend(alerts::disk_full(host, &s.mounts, rules.disk_percent));
            }
            if rules.disk_forecast {
                conds.extend(diskusage::conditions(host, &self.forecasts, rules.forecast_days, now));
            }
            if rules.container_down {
                let c = shared.containers.lock().unwrap_or_else(|e| e.into_inner());
                conds.extend(c.conditions.iter().flatten().cloned());
            }
        }
        let changes = self.tracker.report(conds, now);
        if !changes.is_empty() {
            for c in &changes {
                st.event_add(&event_for(c, now))?;
            }
            st.set_meta(META_ALERTS, &serde_json::to_string(&self.tracker.save()).unwrap_or_default())?;
            *shared.active.lock().unwrap_or_else(|e| e.into_inner()) = self.tracker.active();
        }
        Ok(())
    }

    /// A clean stop: write the last heartbeat and mark it clean.
    pub fn stop(&mut self, shared: &Shared, now: i64) {
        let mut st = lock(shared);
        if let Err(e) = self.hb.stop(&mut st, &Clocks::read(now)) {
            crate::warn!("could not record a clean stop: {e}");
        }
    }
}

fn level(s: Severity) -> Level {
    match s {
        Severity::Warn => Level::Warn,
        Severity::Critical => Level::Critical,
    }
}

/// The event for one alert change (same headline shape as the app's: `DISK FULL · server01`).
pub fn event_for(c: &Change, now: i64) -> NewEvent {
    let (kind, lvl, a, detail) = match c {
        Change::Fired(a) => (EventKind::Alert, level(a.severity), a, a.detail.clone()),
        Change::Updated { alert, worse: true } => {
            (EventKind::Alert, level(alert.severity), alert, alert.detail.clone())
        }
        Change::Updated { alert, worse: false } => (EventKind::Alert, Level::Info, alert, alert.detail.clone()),
        Change::Resolved(a) => (
            EventKind::Recovered,
            Level::Ok,
            a,
            format!("resolved after {}", span_words(a.resolved.unwrap_or(now) - a.since)),
        ),
    };
    let mut e = NewEvent::new(now, kind, lvl, format!("{} · {}", alerts::label(a.kind), a.title), detail);
    e.alert = Some(a.clone());
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use nightgrid_node_proto::{AlertKind, MountUsage};

    fn sample(used: u64, avail: u64) -> Sample {
        Sample {
            ts: 0.0,
            hostname: "server01".into(),
            kernel: String::new(),
            uptime_secs: 0.0,
            cpu: None,
            cores: vec![],
            load: [0.0; 3],
            mem_total: 0,
            mem_used: 0,
            swap_total: 0,
            swap_used: 0,
            mounts: vec![MountUsage {
                device: "d".into(),
                mount: "/".into(),
                fstype: "ext4".into(),
                total: used + avail,
                used,
                avail,
            }],
            net: vec![],
            net_rx: 0.0,
            net_tx: 0.0,
            disks: vec![],
            disk_read: 0.0,
            disk_write: 0.0,
            temps: vec![],
            cpu_temp: None,
            gpus: vec![],
        }
    }

    #[test]
    fn disk_alert_fires_is_recorded_and_resolves() {
        let shared = Shared::new(Config::default(), Store::open_in_memory().unwrap());
        let t0 = 1_700_000_000;
        let mut e = Engine::start(&shared, t0).unwrap();
        e.sample(&shared, &sample(95, 5), t0 + 60);
        assert_eq!(shared.active.lock().unwrap().len(), 1);
        // Lower the threshold rule off: rules come from the app.
        shared.rules.lock().unwrap().disk_full = false;
        e.sample(&shared, &sample(95, 5), t0 + 120);
        e.sample(&shared, &sample(95, 5), t0 + 480);
        assert!(shared.active.lock().unwrap().is_empty());
        let st = shared.store.lock().unwrap();
        let ev = st.events_after(0, 10).unwrap();
        let kinds: Vec<_> = ev.iter().map(|e| (e.kind, e.level)).collect();
        assert_eq!(kinds, [(EventKind::Alert, Level::Warn), (EventKind::Recovered, Level::Ok)]);
        assert_eq!(ev[0].title, format!("DISK FULL · {}", shared.identity.hostname));
        assert_eq!(ev[0].alert.as_ref().unwrap().kind, AlertKind::DiskFull);
        assert_eq!(ev[1].detail, "resolved after 7m");
        // Up time and disk usage were written.
        assert!(!st.uptime_after(0, i64::MAX, 10).unwrap().is_empty());
        assert_eq!(st.disk_usage_after(0, i64::MAX, 10).unwrap().len(), 1);
    }
}
