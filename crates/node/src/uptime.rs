//! Host uptime from the node's own heartbeat.
//!
//! While the node runs, the host is up: each sample adds the time since the
//! previous one (at most [`MAX_GAP`]) to the hour it falls in. What happened
//! while the node *wasn't* running is worked out at the next start:
//!
//! - **Different boot id:** the host restarted. The span from the last
//!   heartbeat to the boot is `down` when the node didn't stop cleanly before
//!   it (crash, power loss) and `asleep` when it did (a deliberate shutdown or
//!   reboot). An `outage` event and a `boot` event are recorded.
//! - **Same boot, suspend time grew** (`CLOCK_BOOTTIME - CLOCK_MONOTONIC`):
//!   the host was suspended that long: `asleep`, with an `outage` event.
//! - **Otherwise** only the node was stopped: that time stays unknown, as in
//!   the app (unknown is never counted as up).

use nightgrid_node_proto::{EventKind, Level, OutageSpan, OutageState, UptimeHour};
use rusqlite::params;

use crate::events::NewEvent;
use crate::store::{Result, Store};

/// The longest gap between two heartbeats that still counts as up.
pub const MAX_GAP: i64 = 60;
/// Suspends shorter than this are ignored.
const MIN_SLEEP: i64 = 30;
/// Hours older than this are dropped (the app keeps 400 days).
pub const KEEP_DAYS: i64 = 400;
const HOUR: i64 = 3600;

/// What the node knows about the host's clocks right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clocks {
    pub now: i64,
    /// `/proc/sys/kernel/random/boot_id`.
    pub boot_id: String,
    /// Boot time, unix seconds (`btime` in `/proc/stat`).
    pub btime: i64,
    /// Seconds spent suspended since boot.
    pub suspended: i64,
}

impl Clocks {
    #[cfg(windows)]
    pub fn read(now: i64) -> Self {
        let btime = now - (crate::win::sys::ticks_ms() / 1000) as i64;
        // Without the boot counter, boot time to the minute stands in (the
        // tick count and the wall clock drift apart by seconds, not minutes).
        let boot_id = crate::win::sys::boot_id().unwrap_or_else(|| format!("windows-btime-{}", btime / 60));
        Self { now, boot_id, btime, suspended: suspended_secs() }
    }

    #[cfg(unix)]
    pub fn read(now: i64) -> Self {
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default().trim().to_owned();
        let btime = std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse().ok())))
            .unwrap_or(0);
        Self { now, boot_id, btime, suspended: suspended_secs() }
    }
}

#[cfg(unix)]
fn clock(id: libc::clockid_t) -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid, writable timespec.
    if unsafe { libc::clock_gettime(id, &mut ts) } != 0 {
        return 0;
    }
    #[allow(clippy::unnecessary_cast)] // `time_t` is 32-bit on some targets.
    let secs = ts.tv_sec as i64;
    secs
}

/// Seconds the host spent suspended since boot.
#[cfg(unix)]
pub fn suspended_secs() -> i64 {
    (clock(libc::CLOCK_BOOTTIME) - clock(libc::CLOCK_MONOTONIC)).max(0)
}

/// Seconds the host spent asleep since boot.
#[cfg(windows)]
pub fn suspended_secs() -> i64 {
    crate::win::sys::suspended_secs()
}

/// Adds `secs` of `col` to the hours that `[from, from + secs)` covers.
fn add_span(store: &mut Store, col: &str, from: i64, to: i64) -> Result<()> {
    let tx = store.conn_mut().transaction()?;
    let mut t = from;
    while t < to {
        let hour = t.div_euclid(HOUR) * HOUR;
        let end = (hour + HOUR).min(to);
        tx.execute(
            &format!(
                "INSERT INTO uptime_hours (hour, {col}) VALUES (?1, ?2)
                 ON CONFLICT (hour) DO UPDATE SET {col} = {col} + excluded.{col}"
            ),
            params![hour, end - t],
        )?;
        t = end;
    }
    tx.commit()
}

/// The heartbeat. Up time is kept in memory and written once a minute
/// ([`Heartbeat::flush`]), so an SD card isn't written every sample.
#[derive(Debug, Default)]
pub struct Heartbeat {
    last: Option<i64>,
    /// Up spans not written yet.
    pending: Vec<(i64, i64)>,
}

impl Heartbeat {
    /// Reconciles the time since the last run, then starts beating.
    pub fn start(store: &mut Store, c: &Clocks) -> Result<Self> {
        let last_alive: Option<i64> = store.meta("last_alive")?.and_then(|v| v.parse().ok());
        let last_boot = store.meta("boot_id")?;
        let last_suspended: i64 = store.meta("suspended")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let clean = store.meta("clean_stop")?.as_deref() == Some("1");

        if let Some(last) = last_alive.filter(|l| *l < c.now) {
            let rebooted = last_boot.as_deref().is_some_and(|b| b != c.boot_id);
            if rebooted && c.btime > last {
                let state = if clean { OutageState::Asleep } else { OutageState::Down };
                record_outage(store, last, c.btime, state, clean)?;
                store.event_add(&NewEvent::new(c.btime, EventKind::Boot, Level::Info, "Booted", ""))?;
                // Boot → node start: the host was up.
                if c.now > c.btime {
                    add_span(store, "up", c.btime, c.now.min(c.btime + MAX_GAP * 5))?;
                }
            } else if !rebooted {
                let slept = (c.suspended - last_suspended).min(c.now - last);
                if slept >= MIN_SLEEP {
                    record_outage(store, last, last + slept, OutageState::Asleep, false)?;
                }
            }
        }
        store.set_meta("boot_id", &c.boot_id)?;
        store.set_meta("clean_stop", "0")?;
        let mut hb = Self::default();
        hb.beat(c.now);
        hb.flush(store, c)?;
        Ok(hb)
    }

    /// One sample's worth of being up.
    pub fn beat(&mut self, now: i64) {
        if let Some(last) = self.last.filter(|l| now > *l) {
            let from = now - (now - last).min(MAX_GAP);
            match self.pending.last_mut() {
                Some(span) if span.1 == from => span.1 = now,
                _ => self.pending.push((from, now)),
            }
        }
        self.last = Some(now);
    }

    /// Writes pending up time and the heartbeat marks.
    pub fn flush(&mut self, store: &mut Store, c: &Clocks) -> Result<()> {
        for (from, to) in std::mem::take(&mut self.pending) {
            add_span(store, "up", from, to)?;
        }
        store.set_meta("last_alive", &c.now.to_string())?;
        store.set_meta("suspended", &c.suspended.to_string())?;
        Ok(())
    }

    /// A clean stop: flush and remember it was clean.
    pub fn stop(&mut self, store: &mut Store, c: &Clocks) -> Result<()> {
        self.beat(c.now);
        self.flush(store, c)?;
        store.set_meta("clean_stop", "1")
    }
}

fn record_outage(store: &mut Store, start: i64, end: i64, state: OutageState, clean: bool) -> Result<()> {
    if end <= start {
        return Ok(());
    }
    let col = match state {
        OutageState::Down => "down",
        OutageState::Asleep => "asleep",
    };
    add_span(store, col, start, end)?;
    let span = span_words(end - start);
    let (level, title, detail) = match (state, clean) {
        (OutageState::Down, _) => {
            (Level::Critical, "Down", format!("down for about {span}: no clean shutdown (crash or power loss)"))
        }
        (OutageState::Asleep, true) => {
            (Level::Info, "Restarted", format!("off for about {span} after a clean shutdown"))
        }
        (OutageState::Asleep, false) => (Level::Info, "Suspended", format!("asleep for about {span}")),
    };
    let mut e = NewEvent::new(end, nightgrid_node_proto::EventKind::Outage, level, title, detail);
    e.outage = Some(OutageSpan { start, end, state });
    store.event_add(&e)?;
    Ok(())
}

/// `1h 5m`, `3d 2h`, `40s` (the app's `timeline::span`).
pub fn span_words(secs: i64) -> String {
    let s = secs.max(0);
    let (d, h, m) = (s / 86_400, (s % 86_400) / 3600, (s % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

impl Store {
    /// Closed hours with `hour > after`, oldest first.
    pub fn uptime_after(&self, after: i64, now: i64, limit: usize) -> Result<Vec<UptimeHour>> {
        let mut st = self.conn().prepare_cached(
            "SELECT hour, up, down, asleep FROM uptime_hours WHERE hour > ?1 AND hour + 3600 <= ?2 ORDER BY hour LIMIT ?3",
        )?;
        let rows = st.query_map(params![after, now, limit as i64], |r| {
            Ok(UptimeHour { hour: r.get(0)?, up: r.get(1)?, down: r.get(2)?, asleep: r.get(3)? })
        })?;
        rows.collect()
    }

    pub fn prune_uptime(&self, now: i64) -> Result<usize> {
        self.conn().execute("DELETE FROM uptime_hours WHERE hour < ?1", [now - KEEP_DAYS * 86_400])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clocks(now: i64, boot: &str, btime: i64, suspended: i64) -> Clocks {
        Clocks { now, boot_id: boot.into(), btime, suspended }
    }

    fn hours(s: &Store) -> Vec<UptimeHour> {
        s.uptime_after(i64::MIN, i64::MAX, 1000).unwrap()
    }

    const H: i64 = 1_000 * 3600;

    #[test]
    fn beats_add_up_time_across_hours_and_cap_gaps() {
        let mut s = Store::open_in_memory().unwrap();
        let c = clocks(H - 30, "a", H - 1000, 0);
        let mut hb = Heartbeat::start(&mut s, &c).unwrap();
        hb.beat(H - 20);
        hb.beat(H + 10); // crosses the hour
        hb.beat(H + 500); // gap of 490 s: only 60 count
        hb.flush(&mut s, &clocks(H + 500, "a", H - 1000, 0)).unwrap();
        let h = hours(&s);
        assert_eq!(h.iter().map(|x| (x.hour, x.up)).collect::<Vec<_>>(), [(H - 3600, 30), (H, 70)]);
        // The current hour isn't served until it's over.
        assert_eq!(s.uptime_after(0, H + 600, 10).unwrap().len(), 1);
    }

    #[test]
    fn crash_reboot_is_down_clean_reboot_is_asleep() {
        let mut s = Store::open_in_memory().unwrap();
        let mut hb = Heartbeat::start(&mut s, &clocks(H, "a", H - 5000, 0)).unwrap();
        hb.beat(H + 10);
        hb.flush(&mut s, &clocks(H + 10, "a", H - 5000, 0)).unwrap();
        // Power loss: no stop(); boots again 600 s later, node up 20 s after boot.
        Heartbeat::start(&mut s, &clocks(H + 630, "b", H + 610, 0)).unwrap();
        let ev = s.events_after(0, 10).unwrap();
        assert_eq!(ev.iter().map(|e| e.kind).collect::<Vec<_>>(), [EventKind::Outage, EventKind::Boot]);
        let o = ev[0].outage.unwrap();
        assert_eq!((o.start, o.end, o.state), (H + 10, H + 610, OutageState::Down));
        assert_eq!(ev[0].level, Level::Critical);
        assert_eq!(hours(&s)[0].down, 600);
        assert_eq!(hours(&s)[0].up, 10 + 20);

        // Clean shutdown, then a reboot: asleep.
        let mut hb = Heartbeat::start(&mut s, &clocks(H + 700, "b", H + 610, 0)).unwrap();
        hb.stop(&mut s, &clocks(H + 700, "b", H + 610, 0)).unwrap();
        Heartbeat::start(&mut s, &clocks(H + 1000, "c", H + 990, 0)).unwrap();
        let ev = s.events_after(2, 10).unwrap();
        assert_eq!(ev[0].outage.unwrap().state, OutageState::Asleep);
        assert_eq!(ev[0].title, "Restarted");
        assert_eq!(hours(&s)[0].asleep, 290);
    }

    #[test]
    fn suspend_is_asleep_and_a_stopped_node_is_unknown() {
        let mut s = Store::open_in_memory().unwrap();
        let mut hb = Heartbeat::start(&mut s, &clocks(H, "a", H - 5000, 100)).unwrap();
        hb.stop(&mut s, &clocks(H, "a", H - 5000, 100)).unwrap();
        // Service stopped for 10 minutes, no suspend: nothing recorded.
        Heartbeat::start(&mut s, &clocks(H + 600, "a", H - 5000, 100)).unwrap();
        assert!(s.events_after(0, 10).unwrap().is_empty());
        assert!(hours(&s).iter().all(|h| h.down + h.asleep == 0));
        // Suspended for 1000 s while running.
        let c = clocks(H + 600, "a", H - 5000, 100);
        let mut hb = Heartbeat::start(&mut s, &c).unwrap();
        hb.flush(&mut s, &c).unwrap();
        Heartbeat::start(&mut s, &clocks(H + 1700, "a", H - 5000, 1100)).unwrap();
        let ev = s.events_after(0, 10).unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(
            (ev[0].title.as_str(), ev[0].outage.unwrap().end - ev[0].outage.unwrap().start),
            ("Suspended", 1000)
        );
        assert_eq!(hours(&s)[0].asleep, 1000);
    }

    #[test]
    fn reads_this_machine() {
        let c = Clocks::read(1);
        assert!(!c.boot_id.is_empty() && c.btime > 0 && c.suspended >= 0);
    }
}
