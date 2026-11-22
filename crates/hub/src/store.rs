//! The hub's SQLite store: each member's history under its node id, with
//! the member's own `seq` numbers and bucket times, so a cursor means the
//! same here as on the node (N-054).

use std::path::Path;

use nightgrid_node_proto::hub::{Member, ProfileInfo, ProfilePulled, ProfilePushResult, ProfileRecord};
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
    Migrations::new(vec![
        M::up(
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
        ),
        // 0.45.0: the app's sealed profile (`profile` 1). The hub never
        // reads `blob`.
        M::up(
            "CREATE TABLE profiles (
                 id TEXT PRIMARY KEY, seq INTEGER NOT NULL DEFAULT 0, bytes INTEGER NOT NULL DEFAULT 0,
                 last_push INTEGER, last_device TEXT, pruned_through INTEGER NOT NULL DEFAULT 0,
                 created INTEGER NOT NULL);
             CREATE TABLE profile_items (
                 profile TEXT NOT NULL, tbl TEXT NOT NULL, uid TEXT NOT NULL, seq INTEGER NOT NULL,
                 modified INTEGER NOT NULL, deleted INTEGER NOT NULL, blob TEXT,
                 PRIMARY KEY (profile, tbl, uid)) WITHOUT ROWID;
             CREATE INDEX profile_items_seq ON profile_items (profile, seq);",
        ),
    ])
}

/// What one profile record counts against the cap.
pub fn record_cost(tbl: &str, uid: &str, blob: Option<&str>) -> i64 {
    (tbl.len() + uid.len() + blob.map_or(0, str::len) + 32) as i64
}

/// Why a push stored nothing.
#[derive(Debug)]
pub enum PushError {
    /// The profile would pass its cap (bytes it would have).
    TooLarge(i64),
    /// A new profile, and the hub holds its most already.
    TooMany,
    Sql(rusqlite::Error),
}

impl From<rusqlite::Error> for PushError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
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

    // ---- the app's profile (0.45.0) ---------------------------------------------------

    pub fn profile_ids(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT id FROM profiles ORDER BY created, id")?;
        let r = st.query_map([], |r| r.get(0))?;
        r.collect()
    }

    pub fn profile_info(&self, id: &str, max_bytes: u64) -> Result<Option<ProfileInfo>> {
        let row = self
            .conn
            .query_row(
                "SELECT seq, bytes, last_push, last_device, pruned_through FROM profiles WHERE id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((seq, bytes, last_push, last_device, pruned_through)) = row else { return Ok(None) };
        let (items, tombstones): (i64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(deleted = 0), 0), COALESCE(SUM(deleted != 0), 0) FROM profile_items WHERE profile = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(Some(ProfileInfo {
            profile_id: id.to_owned(),
            items: items as u64,
            tombstones: tombstones as u64,
            bytes: bytes as u64,
            max_bytes,
            seq,
            last_push,
            last_device,
            pruned_through,
        }))
    }

    /// Stores each record that is newer than the hub's copy (last write
    /// wins: `modified`, then a tombstone over an item, then the larger
    /// blob, so every copy settles on the same winner). All or nothing.
    pub fn profile_push(
        &mut self,
        id: &str,
        device: Option<&str>,
        records: &[ProfileRecord],
        now: i64,
        max_bytes: u64,
        max_profiles: usize,
    ) -> std::result::Result<ProfilePushResult, PushError> {
        let tx = self.conn.transaction()?;
        let row: Option<(i64, i64)> = tx
            .query_row("SELECT seq, bytes FROM profiles WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let (mut seq, mut bytes) = match row {
            Some(r) => r,
            None => {
                let n: i64 = tx.query_row("SELECT COUNT(*) FROM profiles", [], |r| r.get(0))?;
                if n as usize >= max_profiles {
                    return Err(PushError::TooMany);
                }
                (0, 0)
            }
        };
        let (mut accepted, mut ignored) = (0u32, 0u32);
        {
            let mut get = tx.prepare_cached(
                "SELECT modified, deleted, blob FROM profile_items WHERE profile = ?1 AND tbl = ?2 AND uid = ?3",
            )?;
            let mut put = tx.prepare_cached(
                "INSERT INTO profile_items (profile, tbl, uid, seq, modified, deleted, blob)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(profile, tbl, uid) DO UPDATE SET seq = excluded.seq, modified = excluded.modified,
                     deleted = excluded.deleted, blob = excluded.blob",
            )?;
            for r in records {
                let old: Option<(i64, bool, Option<String>)> = get
                    .query_row(params![id, r.tbl, r.uid], |x| Ok((x.get(0)?, x.get::<_, i64>(1)? != 0, x.get(2)?)))
                    .optional()?;
                if let Some((m, d, b)) = &old {
                    if (r.modified, r.deleted, r.blob.as_deref()) <= (*m, *d, b.as_deref()) {
                        ignored += 1;
                        continue;
                    }
                    bytes -= record_cost(&r.tbl, &r.uid, b.as_deref());
                }
                bytes += record_cost(&r.tbl, &r.uid, r.blob.as_deref());
                seq += 1;
                put.execute(params![id, r.tbl, r.uid, seq, r.modified, i64::from(r.deleted), r.blob])?;
                accepted += 1;
            }
        }
        if bytes > max_bytes as i64 {
            return Err(PushError::TooLarge(bytes));
        }
        if accepted == 0 {
            // Nothing new: don't create an empty profile or touch the times.
            return Ok(ProfilePushResult { accepted, ignored, seq });
        }
        tx.execute(
            "INSERT INTO profiles (id, seq, bytes, last_push, last_device, created) VALUES (?1, ?2, ?3, ?4, ?5, ?4)
             ON CONFLICT(id) DO UPDATE SET seq = excluded.seq, bytes = excluded.bytes,
                 last_push = excluded.last_push, last_device = excluded.last_device",
            params![id, seq, bytes, now, device],
        )?;
        tx.commit()?;
        Ok(ProfilePushResult { accepted, ignored, seq })
    }

    /// Records with `seq > after`, oldest first, at most `limit` and about
    /// `max_bytes` of blobs (always at least one). Also whether there are
    /// more, and the profile's `pruned_through`.
    pub fn profile_pull(
        &self,
        id: &str,
        after: i64,
        limit: usize,
        max_bytes: usize,
    ) -> Result<(Vec<ProfilePulled>, bool, i64)> {
        let pruned: i64 = self
            .conn
            .query_row("SELECT pruned_through FROM profiles WHERE id = ?1", [id], |r| r.get(0))
            .optional()?
            .unwrap_or(0);
        let mut st = self.conn.prepare_cached(
            "SELECT seq, tbl, uid, modified, deleted, blob FROM profile_items
             WHERE profile = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = st.query_map(params![id, after, limit as i64 + 1], |r| {
            Ok(ProfilePulled {
                seq: r.get(0)?,
                record: ProfileRecord {
                    tbl: r.get(1)?,
                    uid: r.get(2)?,
                    modified: r.get(3)?,
                    deleted: r.get::<_, i64>(4)? != 0,
                    blob: r.get(5)?,
                },
            })
        })?;
        let mut out = Vec::new();
        let mut size = 0usize;
        let mut more = false;
        for r in rows {
            let r = r?;
            let cost = record_cost(&r.record.tbl, &r.record.uid, r.record.blob.as_deref()) as usize;
            if out.len() >= limit || (!out.is_empty() && size + cost > max_bytes) {
                more = true;
                break;
            }
            size += cost;
            out.push(r);
        }
        Ok((out, more, pruned))
    }

    pub fn profile_drop(&mut self, id: &str) -> Result<bool> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM profile_items WHERE profile = ?1", [id])?;
        let n = tx.execute("DELETE FROM profiles WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Drops tombstones last modified before `cutoff_ms`; returns how many.
    pub fn profile_prune(&mut self, cutoff_ms: i64) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let ids: Vec<String> = {
            let mut st = tx.prepare("SELECT id FROM profiles")?;
            let r = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>>>()?;
            r
        };
        let mut n = 0;
        for id in ids {
            let (top, cost): (Option<i64>, i64) = tx.query_row(
                "SELECT MAX(seq), COALESCE(SUM(LENGTH(tbl) + LENGTH(uid) + COALESCE(LENGTH(blob), 0) + 32), 0)
                 FROM profile_items WHERE profile = ?1 AND deleted != 0 AND modified < ?2",
                params![id, cutoff_ms],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let Some(top) = top else { continue };
            n += tx.execute(
                "DELETE FROM profile_items WHERE profile = ?1 AND deleted != 0 AND modified < ?2",
                params![id, cutoff_ms],
            )?;
            tx.execute(
                "UPDATE profiles SET bytes = MAX(0, bytes - ?2), pruned_through = MAX(pruned_through, ?3) WHERE id = ?1",
                params![id, cost, top],
            )?;
        }
        tx.commit()?;
        Ok(n)
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
