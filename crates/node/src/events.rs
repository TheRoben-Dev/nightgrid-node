//! The events stream: what happened on the host, numbered so the app can
//! pull everything after the last `seq` it saw.

use nightgrid_node_proto::{Alert, Event, EventKind, Level, OutageSpan};
use rusqlite::params;

use crate::store::{Result, Store};

/// Events older than this are dropped (the app's timeline keeps 90 days).
pub const KEEP_SECS: i64 = 90 * 86_400;
/// At most this many events are kept.
pub const KEEP_ROWS: i64 = 20_000;

fn kind_str(k: EventKind) -> &'static str {
    match k {
        EventKind::Alert => "alert",
        EventKind::Recovered => "recovered",
        EventKind::Boot => "boot",
        EventKind::Outage => "outage",
        EventKind::Container => "container",
        EventKind::Login => "login",
        EventKind::File => "file",
        EventKind::Network => "network",
        EventKind::Action => "action",
        EventKind::Port => "port",
        EventKind::Job => "job",
        EventKind::Other => "other",
    }
}

fn level_str(l: Level) -> &'static str {
    match l {
        Level::Info => "info",
        Level::Ok => "ok",
        Level::Warn => "warn",
        Level::Critical => "critical",
    }
}

/// Trims, caps and drops secret-looking text (same rule as the app's timeline).
pub fn tidy(s: &str, max: usize) -> String {
    let s = s.trim();
    if crate::redact::line_has_secret(s) {
        return "(hidden: looked like it contained a secret)".into();
    }
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

/// An event to record (`seq` is assigned by the store).
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub at: i64,
    pub kind: EventKind,
    pub level: Level,
    pub title: String,
    pub detail: String,
    pub alert: Option<Alert>,
    pub outage: Option<OutageSpan>,
}

impl NewEvent {
    pub fn new(at: i64, kind: EventKind, level: Level, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { at, kind, level, title: title.into(), detail: detail.into(), alert: None, outage: None }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Data {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alert: Option<Alert>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outage: Option<OutageSpan>,
}

impl Store {
    /// Records an event and trims old ones. Returns its `seq`.
    pub fn event_add(&self, e: &NewEvent) -> Result<i64> {
        let data = (e.alert.is_some() || e.outage.is_some()).then(|| {
            let mut alert = e.alert.clone();
            if let Some(a) = alert.as_mut() {
                a.title = tidy(&a.title, 200);
                a.detail = tidy(&a.detail, 600);
            }
            serde_json::to_string(&Data { alert, outage: e.outage }).unwrap_or_default()
        });
        let c = self.conn();
        c.execute(
            "INSERT INTO events (at, kind, level, title, detail, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![e.at, kind_str(e.kind), level_str(e.level), tidy(&e.title, 200), tidy(&e.detail, 600), data],
        )?;
        let seq = c.last_insert_rowid();
        c.execute(
            "DELETE FROM events WHERE at < ?1 OR seq <= (SELECT max(seq) FROM events) - ?2",
            params![e.at - KEEP_SECS, KEEP_ROWS],
        )?;
        Ok(seq)
    }

    /// Events with `seq > after`, oldest first.
    pub fn events_after(&self, after: i64, limit: usize) -> Result<Vec<Event>> {
        self.events_where("seq > ?1", &[&after], limit)
    }

    /// Events matching `cond` (with its `?N` arguments), oldest first.
    fn events_where(&self, cond: &str, args: &[&dyn rusqlite::ToSql], limit: usize) -> Result<Vec<Event>> {
        let mut st = self.conn().prepare_cached(&format!(
            "SELECT seq, at, kind, level, title, detail, data FROM events WHERE {cond} ORDER BY seq LIMIT {limit}"
        ))?;
        let rows = st.query_map(args, |r| {
            let kind: String = r.get(2)?;
            let level: String = r.get(3)?;
            let data: Option<String> = r.get(6)?;
            let data: Data = data.and_then(|d| serde_json::from_str(&d).ok()).unwrap_or_default();
            Ok(Event {
                seq: r.get(0)?,
                at: r.get(1)?,
                kind: serde_json::from_value(serde_json::Value::String(kind)).unwrap_or(EventKind::Other),
                level: serde_json::from_value(serde_json::Value::String(level)).unwrap_or(Level::Info),
                title: r.get(4)?,
                detail: r.get(5)?,
                alert: data.alert,
                outage: data.outage,
            })
        })?;
        rows.collect()
    }

    /// Events with `from <= at <= to`, oldest first (at most `limit`, the newest kept).
    pub fn events_between(&self, from: i64, to: i64, limit: usize) -> Result<Vec<Event>> {
        let first: Option<i64> = self.conn().query_row(
            "SELECT min(seq) FROM (SELECT seq FROM events WHERE at >= ?1 AND at <= ?2 ORDER BY seq DESC LIMIT ?3)",
            params![from, to, limit as i64],
            |r| r.get(0),
        )?;
        let Some(first) = first else { return Ok(Vec::new()) };
        Ok(self.events_after(first - 1, limit)?.into_iter().filter(|e| e.at >= from && e.at <= to).collect())
    }

    /// When the oldest event still kept happened.
    pub fn events_oldest_at(&self) -> Result<Option<i64>> {
        self.conn().query_row("SELECT min(at) FROM events", [], |r| r.get(0))
    }

    pub fn events_oldest(&self) -> Result<Option<i64>> {
        self.conn().query_row("SELECT min(seq) FROM events", [], |r| r.get(0))
    }

    pub fn events_newest(&self) -> Result<Option<i64>> {
        self.conn().query_row("SELECT max(seq) FROM events", [], |r| r.get(0))
    }

    /// `events.sync` with an optional kind filter: (events, next, more). With
    /// `kinds`, `next` passes the events it skipped (up to the newest when
    /// the page isn't full), so the client's cursor moves even when nothing
    /// matched. Without it, `next` is the last event's seq, or `after`.
    pub fn events_page(&self, after: i64, limit: usize, kinds: Option<&[String]>) -> Result<(Vec<Event>, i64, bool)> {
        let Some(kinds) = kinds.filter(|k| !k.is_empty()) else {
            let mut events = self.events_after(after, limit + 1)?;
            let more = events.len() > limit;
            events.truncate(limit);
            let next = events.last().map_or(after, |e| e.seq);
            return Ok((events, next, more));
        };
        let kinds: Vec<&String> = kinds.iter().take(64).collect();
        let marks: Vec<String> = (0..kinds.len()).map(|i| format!("?{}", i + 2)).collect();
        let mut args: Vec<&dyn rusqlite::ToSql> = vec![&after];
        args.extend(kinds.iter().map(|k| *k as &dyn rusqlite::ToSql));
        let cond = format!("seq > ?1 AND kind IN ({})", marks.join(","));
        let mut events = self.events_where(&cond, &args, limit + 1)?;
        let more = events.len() > limit;
        events.truncate(limit);
        let next = if more {
            events.last().map_or(after, |e| e.seq)
        } else {
            self.events_newest()?.unwrap_or(after).max(after)
        };
        Ok((events, next, more))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightgrid_node_proto::{AlertKind, OutageState, Severity};

    #[test]
    fn events_sync_by_kind_moves_past_what_it_skips() {
        let s = Store::open_in_memory().unwrap();
        for (at, k) in [(1, EventKind::Login), (2, EventKind::Boot), (3, EventKind::File), (4, EventKind::Login)] {
            s.event_add(&NewEvent::new(at, k, Level::Info, "x", "")).unwrap();
        }
        let only = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        let (ev, next, more) = s.events_page(0, 10, Some(&only(&["boot"]))).unwrap();
        assert_eq!((ev.iter().map(|e| e.seq).collect::<Vec<_>>(), next, more), (vec![2], 4, false));
        // Nothing matched after 2: the cursor still goes to the newest.
        let (ev, next, _) = s.events_page(2, 10, Some(&only(&["boot"]))).unwrap();
        assert!(ev.is_empty() && next == 4);
        // A full page stops at its last event.
        let (ev, next, more) = s.events_page(0, 1, Some(&only(&["login", "file"]))).unwrap();
        assert_eq!((ev[0].seq, next, more), (1, 1, true));
        // Without kinds: next == after on an empty page, as PROTOCOL.md promises.
        assert_eq!(s.events_page(4, 10, None).unwrap(), (vec![], 4, false));
        assert_eq!(s.events_page(0, 10, Some(&[])).unwrap().0.len(), 4);
    }

    #[test]
    fn events_in_a_window() {
        let s = Store::open_in_memory().unwrap();
        for at in [100, 200, 300, 400] {
            s.event_add(&NewEvent::new(at, EventKind::Boot, Level::Info, "booted", "")).unwrap();
        }
        let w: Vec<i64> = s.events_between(150, 350, 10).unwrap().iter().map(|e| e.at).collect();
        assert_eq!(w, [200, 300]);
        let newest: Vec<i64> = s.events_between(0, 1000, 2).unwrap().iter().map(|e| e.at).collect();
        assert_eq!(newest, [300, 400], "the newest are kept");
        assert!(s.events_between(500, 600, 10).unwrap().is_empty());
        assert_eq!(s.events_oldest_at().unwrap(), Some(100));
    }

    #[test]
    fn add_read_and_trim() {
        let s = Store::open_in_memory().unwrap();
        let a = Alert {
            id: 1,
            key: "disk:/srv".into(),
            kind: AlertKind::DiskFull,
            severity: Severity::Warn,
            title: "server01".into(),
            detail: "/srv is 91% full".into(),
            since: 100,
            resolved: None,
        };
        let mut e = NewEvent::new(100, EventKind::Alert, Level::Warn, "DISK FULL · server01", "/srv is 91% full");
        e.alert = Some(a.clone());
        assert_eq!(s.event_add(&e).unwrap(), 1);
        let mut o = NewEvent::new(200, EventKind::Outage, Level::Critical, "Down", "no clean shutdown");
        o.outage = Some(OutageSpan { start: 150, end: 190, state: OutageState::Down });
        assert_eq!(s.event_add(&o).unwrap(), 2);
        s.event_add(&NewEvent::new(300, EventKind::Boot, Level::Info, "Booted", "api_token=abc123secret")).unwrap();

        let all = s.events_after(0, 10).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!((all[0].kind, all[0].alert.as_ref()), (EventKind::Alert, Some(&a)));
        assert_eq!(all[1].outage.unwrap().state, OutageState::Down);
        assert_eq!(all[2].detail, "(hidden: looked like it contained a secret)");
        assert_eq!(s.events_after(2, 10).unwrap().len(), 1);

        // Old events go; seq keeps counting.
        let seq = s.event_add(&NewEvent::new(300 + KEEP_SECS + 1, EventKind::Boot, Level::Info, "Booted", "")).unwrap();
        assert_eq!(seq, 4);
        assert_eq!(s.events_oldest().unwrap(), Some(4));
    }
}
