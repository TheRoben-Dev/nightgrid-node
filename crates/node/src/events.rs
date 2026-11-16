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
        let mut st = self.conn().prepare_cached(
            "SELECT seq, at, kind, level, title, detail, data FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = st.query_map(params![after, limit as i64], |r| {
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

    pub fn events_oldest(&self) -> Result<Option<i64>> {
        self.conn().query_row("SELECT min(seq) FROM events", [], |r| r.get(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightgrid_node_proto::{AlertKind, OutageState, Severity};

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
