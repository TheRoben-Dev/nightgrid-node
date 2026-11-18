//! The hub's SQLite store: each member's history under its node id, with
//! the member's own `seq` numbers and bucket times, so a cursor means the
//! same here as on the node (N-054).

use std::path::Path;

use nightgrid_node_proto::hub::Member;
use rusqlite::{params, Connection, OptionalExtension};
use rusqlite_migration::{Migrations, M};
use serde_json::Value;

pub type Result<T> = std::result::Result<T, rusqlite::Error>;

/// Days each metric tier is kept, after the minute tier's `minute_days`.
pub const ROLLUP_DAYS: i64 = 90;
pub const HOURLY_DAYS: i64 = 400;
/// Newest events kept per member, and for the hub's own timeline.
pub const MAX_EVENTS: i64 = 5000;
pub const EVENT_DAYS: i64 = 90;

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE members (
             id TEXT PRIMARY KEY, pos INTEGER NOT NULL, name TEXT NOT NULL, address TEXT NOT NULL,
             fingerprint TEXT NOT NULL, added INTEGER NOT NULL);
         CREATE TABLE state (
             id TEXT PRIMARY KEY, last_contact INTEGER, last_try INTEGER, version TEXT, hostname TEXT,
             error TEXT, refused INTEGER NOT NULL DEFAULT 0, offline INTEGER NOT NULL DEFAULT 0,
             alerts TEXT, info TEXT);
         CREATE TABLE cursors (id TEXT NOT NULL, stream TEXT NOT NULL, cursor INTEGER NOT NULL,
             PRIMARY KEY (id, stream)) WITHOUT ROWID;
         CREATE TABLE metrics (id TEXT NOT NULL, res INTEGER NOT NULL, ts INTEGER NOT NULL, point TEXT NOT NULL,
             PRIMARY KEY (id, res, ts)) WITHOUT ROWID;
         CREATE TABLE events (id TEXT NOT NULL, seq INTEGER NOT NULL, at INTEGER NOT NULL, event TEXT NOT NULL,
             PRIMARY KEY (id, seq)) WITHOUT ROWID;
         CREATE TABLE uptime (id TEXT NOT NULL, hour INTEGER NOT NULL, row TEXT NOT NULL,
             PRIMARY KEY (id, hour)) WITHOUT ROWID;
         CREATE TABLE disk (id TEXT NOT NULL, ts INTEGER NOT NULL, mount TEXT NOT NULL, row TEXT NOT NULL,
             PRIMARY KEY (id, ts, mount)) WITHOUT ROWID;
         CREATE TABLE hub_events (seq INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, event TEXT NOT NULL);",
    )])
}

pub struct Store {
    conn: Connection,
}

/// What the hub keeps about one member between pulls.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemberRow {
    pub last_contact: Option<i64>,
    pub last_try: Option<i64>,
    pub version: Option<String>,
    pub hostname: Option<String>,
    pub error: Option<String>,
    /// The member answered but refused to serve this hub.
    pub refused: bool,
    /// Counted as offline (an event was recorded).
    pub offline: bool,
    pub alerts: Option<Value>,
    pub info: Option<Value>,
}

impl Store {
    pub fn open(path: &Path) -> std::result::Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::setup(conn)
    }

    pub fn open_in_memory() -> std::result::Result<Self, String> {
        Self::setup(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn setup(mut conn: Connection) -> std::result::Result<Self, String> {
        conn.pragma_update(None, "journal_mode", "WAL").map_err(|e| e.to_string())?;
        conn.pragma_update(None, "synchronous", "NORMAL").map_err(|e| e.to_string())?;
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL").map_err(|e| e.to_string())?;
        conn.pragma_update(None, "foreign_keys", "ON").map_err(|e| e.to_string())?;
        migrations().to_latest(&mut conn).map_err(|e| format!("migrate: {e}"))?;
        let s = Self { conn };
        if s.meta("hub_id").map_err(|e| e.to_string())?.is_none() {
            s.set_meta("hub_id", &nightgrid_node::store::random_id()).map_err(|e| e.to_string())?;
        }
        Ok(s)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn hub_id(&self) -> String {
        self.meta("hub_id").ok().flatten().unwrap_or_default()
    }

    /// Bytes on disk (pages in use), for the cap and the overview.
    pub fn size(&self) -> u64 {
        let q = |p: &str| self.conn.query_row(&format!("PRAGMA {p}"), [], |r| r.get::<_, i64>(0)).unwrap_or(0);
        u64::try_from((q("page_count") - q("freelist_count")) * q("page_size")).unwrap_or(0)
    }

    // ---- members ----------------------------------------------------------------

    pub fn members(&self) -> Result<Vec<Member>> {
        let mut st = self.conn.prepare("SELECT id, name, address, fingerprint FROM members ORDER BY pos")?;
        let rows = st.query_map([], |r| {
            Ok(Member { id: r.get(0)?, name: r.get(1)?, address: r.get(2)?, fingerprint: r.get(3)? })
        })?;
        rows.collect()
    }

    pub fn added(&self, id: &str) -> Result<Option<i64>> {
        self.conn.query_row("SELECT added FROM members WHERE id = ?1", [id], |r| r.get(0)).optional()
    }

    /// Replaces the member list. A removed member's copy goes with it; a
    /// kept member keeps its history and cursors.
    pub fn set_members(&mut self, list: &[Member], now: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        let old: Vec<String> = {
            let mut st = tx.prepare("SELECT id FROM members")?;
            let r = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>>>()?;
            r
        };
        for id in old.iter().filter(|id| !list.iter().any(|m| &m.id == *id)) {
            for t in ["members", "state", "cursors", "metrics", "events", "uptime", "disk"] {
                tx.execute(&format!("DELETE FROM {t} WHERE id = ?1"), [id])?;
            }
        }
        for (i, m) in list.iter().enumerate() {
            tx.execute(
                "INSERT INTO members (id, pos, name, address, fingerprint, added) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET pos = excluded.pos, name = excluded.name,
                     address = excluded.address, fingerprint = excluded.fingerprint",
                params![m.id, i as i64, m.name, m.address, m.fingerprint, now],
            )?;
        }
        tx.commit()
    }

    pub fn member_row(&self, id: &str) -> Result<MemberRow> {
        let r = self
            .conn
            .query_row(
                "SELECT last_contact, last_try, version, hostname, error, refused, offline, alerts, info
                 FROM state WHERE id = ?1",
                [id],
                |r| {
                    let js = |s: Option<String>| s.and_then(|s| serde_json::from_str(&s).ok());
                    Ok(MemberRow {
                        last_contact: r.get(0)?,
                        last_try: r.get(1)?,
                        version: r.get(2)?,
                        hostname: r.get(3)?,
                        error: r.get(4)?,
                        refused: r.get::<_, i64>(5)? != 0,
                        offline: r.get::<_, i64>(6)? != 0,
                        alerts: js(r.get(7)?),
                        info: js(r.get(8)?),
                    })
                },
            )
            .optional()?;
        Ok(r.unwrap_or_default())
    }

    pub fn set_member_row(&self, id: &str, m: &MemberRow) -> Result<()> {
        let js = |v: &Option<Value>| v.as_ref().map(|v| v.to_string());
        self.conn.execute(
            "INSERT INTO state (id, last_contact, last_try, version, hostname, error, refused, offline, alerts, info)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET last_contact = excluded.last_contact, last_try = excluded.last_try,
                 version = excluded.version, hostname = excluded.hostname, error = excluded.error,
                 refused = excluded.refused, offline = excluded.offline, alerts = excluded.alerts,
                 info = excluded.info",
            params![
                id,
                m.last_contact,
                m.last_try,
                m.version,
                m.hostname,
                m.error,
                i64::from(m.refused),
                i64::from(m.offline),
                js(&m.alerts),
                js(&m.info)
            ],
        )?;
        Ok(())
    }

    pub fn cursors(&self, id: &str) -> Result<std::collections::BTreeMap<String, i64>> {
        let mut st = self.conn.prepare("SELECT stream, cursor FROM cursors WHERE id = ?1")?;
        let r = st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        r.collect()
    }

    fn set_cursor(tx: &rusqlite::Transaction<'_>, id: &str, stream: &str, cursor: i64) -> Result<()> {
        // Only forward: an old page arriving late never moves a cursor back.
        tx.execute(
            "INSERT INTO cursors (id, stream, cursor) VALUES (?1, ?2, ?3)
             ON CONFLICT(id, stream) DO UPDATE SET cursor = MAX(cursor, excluded.cursor)",
            params![id, stream, cursor],
        )?;
        Ok(())
    }

    // ---- ingest -----------------------------------------------------------------

    /// One `metrics.sync` page: points as JSON with their bucket time.
    pub fn put_metrics(&mut self, id: &str, res: i64, points: &[(i64, String)], next: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut st =
                tx.prepare_cached("INSERT OR IGNORE INTO metrics (id, res, ts, point) VALUES (?1, ?2, ?3, ?4)")?;
            for (ts, p) in points {
                st.execute(params![id, res, ts, p])?;
            }
        }
        Self::set_cursor(&tx, id, &format!("metrics.{res}"), next)?;
        tx.commit()
    }

    pub fn put_events(&mut self, id: &str, events: &[(i64, i64, String)], next: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut st =
                tx.prepare_cached("INSERT OR IGNORE INTO events (id, seq, at, event) VALUES (?1, ?2, ?3, ?4)")?;
            for (seq, at, e) in events {
                st.execute(params![id, seq, at, e])?;
            }
        }
        Self::set_cursor(&tx, id, "events", next)?;
        tx.commit()
    }

    pub fn put_uptime(&mut self, id: &str, hours: &[(i64, String)], next: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare_cached("INSERT OR REPLACE INTO uptime (id, hour, row) VALUES (?1, ?2, ?3)")?;
            for (h, r) in hours {
                st.execute(params![id, h, r])?;
            }
        }
        Self::set_cursor(&tx, id, "uptime", next)?;
        tx.commit()
    }

    pub fn put_disk(&mut self, id: &str, rows: &[(i64, String, String)], next: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut st =
                tx.prepare_cached("INSERT OR REPLACE INTO disk (id, ts, mount, row) VALUES (?1, ?2, ?3, ?4)")?;
            for (ts, m, r) in rows {
                st.execute(params![id, ts, m, r])?;
            }
        }
        Self::set_cursor(&tx, id, "disk", next)?;
        tx.commit()
    }

    // ---- serving ------------------------------------------------------------------

    /// Points with `ts > after`, oldest first.
    pub fn metrics_after(&self, id: &str, res: i64, after: i64, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT ts, point FROM metrics WHERE id = ?1 AND res = ?2 AND ts > ?3 ORDER BY ts LIMIT ?4",
        )?;
        let r = st.query_map(params![id, res, after, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        r.collect()
    }

    pub fn metrics_oldest(&self, id: &str, res: i64) -> Result<Option<i64>> {
        self.conn.query_row("SELECT MIN(ts) FROM metrics WHERE id = ?1 AND res = ?2", params![id, res], |r| r.get(0))
    }

    pub fn events_after(&self, id: &str, after: i64, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT seq, event FROM events WHERE id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3")?;
        let r = st.query_map(params![id, after, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        r.collect()
    }

    /// Like `events_after`, only the given kinds (all when `None`).
    pub fn events_after_kinds(
        &self,
        id: &str,
        after: i64,
        limit: usize,
        kinds: Option<&[String]>,
    ) -> Result<Vec<(i64, String)>> {
        let Some(kinds) = kinds else { return self.events_after(id, after, limit) };
        let kinds: Vec<&String> = kinds.iter().take(64).collect();
        let marks: Vec<String> = (0..kinds.len()).map(|i| format!("?{}", i + 3)).collect();
        let marks = marks.join(",");
        let mut st = self.conn.prepare(&format!(
            "SELECT seq, event FROM events WHERE id = ?1 AND seq > ?2 AND json_extract(event, '$.kind') IN ({marks}) \
             ORDER BY seq LIMIT {limit}"
        ))?;
        let mut args: Vec<&dyn rusqlite::ToSql> = vec![&id, &after];
        args.extend(kinds.iter().map(|k| *k as &dyn rusqlite::ToSql));
        let r = st.query_map(args.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))?;
        r.collect()
    }

    pub fn events_newest(&self, id: &str) -> Result<Option<i64>> {
        self.conn.query_row("SELECT MAX(seq) FROM events WHERE id = ?1", [id], |r| r.get(0))
    }

    pub fn events_oldest(&self, id: &str) -> Result<Option<i64>> {
        self.conn.query_row("SELECT MIN(seq) FROM events WHERE id = ?1", [id], |r| r.get(0))
    }

    pub fn uptime_after(&self, id: &str, after: i64, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT hour, row FROM uptime WHERE id = ?1 AND hour > ?2 ORDER BY hour LIMIT ?3")?;
        let r = st.query_map(params![id, after, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        r.collect()
    }

    pub fn disk_after(&self, id: &str, after: i64, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT ts, row FROM disk WHERE id = ?1 AND ts > ?2 ORDER BY ts, mount LIMIT ?3")?;
        let r = st.query_map(params![id, after, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        r.collect()
    }

    // ---- the hub's own events -------------------------------------------------------

    /// Records an event; returns its `seq`.
    pub fn hub_event(&self, at: i64, mut e: Value) -> Result<i64> {
        self.conn.execute("INSERT INTO hub_events (at, event) VALUES (?1, '{}')", [at])?;
        let seq = self.conn.last_insert_rowid();
        e["seq"] = seq.into();
        self.conn.execute("UPDATE hub_events SET event = ?2 WHERE seq = ?1", params![seq, e.to_string()])?;
        Ok(seq)
    }

    pub fn hub_events_after(&self, after: i64, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut st =
            self.conn.prepare_cached("SELECT seq, event FROM hub_events WHERE seq > ?1 ORDER BY seq LIMIT ?2")?;
        let r = st.query_map(params![after, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        r.collect()
    }

    pub fn hub_events_oldest(&self) -> Result<Option<i64>> {
        self.conn.query_row("SELECT MIN(seq) FROM hub_events", [], |r| r.get(0))
    }

    // ---- pruning --------------------------------------------------------------------

    /// Drops what is past its age, then, while the store is over 90% of
    /// `max_bytes`, the oldest minute points. Returns rows deleted and
    /// whether the cap forced minutes out.
    pub fn prune(&mut self, now: i64, minute_days: i64, max_bytes: u64) -> Result<(usize, bool)> {
        let mut n = 0;
        let tx = self.conn.transaction()?;
        for (res, days) in [(60, minute_days), (900, ROLLUP_DAYS.max(minute_days)), (3600, HOURLY_DAYS)] {
            n += tx.execute("DELETE FROM metrics WHERE res = ?1 AND ts < ?2", params![res, now - days * 86400])?;
        }
        n += tx.execute("DELETE FROM events WHERE at < ?1", [now - EVENT_DAYS * 86400])?;
        n += tx.execute(
            "DELETE FROM events WHERE (id, seq) IN (
                 SELECT id, seq FROM (SELECT id, seq, ROW_NUMBER() OVER (PARTITION BY id ORDER BY seq DESC) AS k
                                      FROM events) WHERE k > ?1)",
            [MAX_EVENTS],
        )?;
        n += tx.execute("DELETE FROM uptime WHERE hour < ?1", [now - HOURLY_DAYS * 86400])?;
        n += tx.execute("DELETE FROM disk WHERE ts < ?1", [now - HOURLY_DAYS * 86400])?;
        n += tx.execute(
            "DELETE FROM hub_events WHERE seq <= (SELECT MAX(seq) FROM hub_events) - ?1 OR at < ?2",
            params![MAX_EVENTS, now - EVENT_DAYS * 86400],
        )?;
        tx.commit()?;
        let mut capped = false;
        // A day of the oldest minutes at a time, until under the cap.
        for _ in 0..400 {
            if self.size() <= max_bytes / 10 * 9 {
                break;
            }
            let oldest: Option<i64> =
                self.conn.query_row("SELECT MIN(ts) FROM metrics WHERE res = 60", [], |r| r.get(0))?;
            let Some(oldest) = oldest else { break };
            n += self.conn.execute("DELETE FROM metrics WHERE res = 60 AND ts < ?1", [oldest + 86400])?;
            self.conn.execute_batch("PRAGMA incremental_vacuum")?;
            capped = true;
        }
        if n > 0 {
            self.conn.execute_batch("PRAGMA incremental_vacuum")?;
        }
        Ok((n, capped))
    }

    /// Oldest minute point across members, for the overview after a cap.
    pub fn minutes_since(&self) -> Result<Option<i64>> {
        self.conn.query_row("SELECT MIN(ts) FROM metrics WHERE res = 60", [], |r| r.get(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(c: char) -> Member {
        Member {
            id: c.to_string().repeat(32),
            name: format!("h{c}"),
            address: "127.0.0.1:1".into(),
            fingerprint: "f".repeat(64),
        }
    }

    #[test]
    fn cursors_only_move_forward_and_removed_members_go() {
        let mut s = Store::open_in_memory().unwrap();
        assert_eq!(s.hub_id().len(), 32);
        s.set_members(&[m('a'), m('b')], 1).unwrap();
        let a = "a".repeat(32);
        s.put_events(&a, &[(1, 10, "{\"seq\":1}".into()), (2, 11, "{\"seq\":2}".into())], 2).unwrap();
        s.put_events(&a, &[(1, 10, "{\"seq\":1}".into())], 1).unwrap();
        assert_eq!(s.cursors(&a).unwrap()["events"], 2);
        assert_eq!(s.events_after(&a, 1, 10).unwrap().len(), 1);
        s.put_metrics(&a, 60, &[(60, "{}".into()), (120, "{}".into())], 120).unwrap();
        assert_eq!(s.metrics_oldest(&a, 60).unwrap(), Some(60));
        s.set_members(&[m('b')], 2).unwrap();
        assert!(s.events_after(&a, 0, 10).unwrap().is_empty());
        assert!(s.cursors(&a).unwrap().is_empty());
        assert_eq!(s.members().unwrap().len(), 1);
    }

    #[test]
    fn prunes_by_age_and_keeps_the_newest_events() {
        let mut s = Store::open_in_memory().unwrap();
        let a = "a".repeat(32);
        let day = 86400;
        let now = 1000 * day;
        s.put_metrics(&a, 60, &[(now - 20 * day, "{}".into()), (now - day, "{}".into())], now).unwrap();
        let ev: Vec<_> = (1..=MAX_EVENTS + 10).map(|i| (i, now - 10, "{}".to_owned())).collect();
        s.put_events(&a, &ev, MAX_EVENTS + 10).unwrap();
        s.prune(now, 14, u64::MAX).unwrap();
        assert_eq!(s.metrics_after(&a, 60, 0, 10).unwrap().len(), 1);
        assert_eq!(s.events_oldest(&a).unwrap(), Some(11));
    }

    #[test]
    fn hub_events_get_their_seq() {
        let s = Store::open_in_memory().unwrap();
        let seq = s.hub_event(5, serde_json::json!({"at": 5, "title": "x"})).unwrap();
        let got = s.hub_events_after(0, 10).unwrap();
        assert_eq!(got[0].0, seq);
        assert!(got[0].1.contains(&format!("\"seq\":{seq}")));
    }
}
