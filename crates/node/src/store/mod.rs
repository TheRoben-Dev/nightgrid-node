//! The node's SQLite database: identity plus metric history in three tiers.
//!
//! Minute points are written once (a repeated minute is ignored) and folded
//! into 15-minute and hourly rollups as running averages, exactly like the
//! app's `Store::insert_metric_point`, so a point synced from the node is the
//! point the app would have stored itself.

use std::path::Path;

use nightgrid_node_proto::{Coverage, Point, HOURLY, MINUTE, ROLLUP};
use rusqlite::{params, Connection, OptionalExtension};
use rusqlite_migration::{Migrations, M};

use crate::metrics::{point, values, COLS, N};

/// 15-minute rollups are kept at least this long.
pub const ROLLUP_DAYS: u32 = 90;
/// Hourly rollups are kept at least this long (the app's 400-day tier).
pub const HOURLY_DAYS: u32 = 400;

pub type Result<T> = std::result::Result<T, rusqlite::Error>;

pub struct Store {
    conn: Connection,
}

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(include_str!("migrations/0001_init.sql")),
        M::up(include_str!("migrations/0002_events_uptime_disk.sql")),
        M::up(include_str!("migrations/0003_net_devices.sql")),
    ])
}

impl Store {
    pub fn open(path: &Path) -> std::result::Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> std::result::Result<Self, String> {
        Self::init(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn init(mut conn: Connection) -> std::result::Result<Self, String> {
        // auto_vacuum only takes effect before the first table exists.
        conn.execute_batch(
            "PRAGMA auto_vacuum = INCREMENTAL;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -1024;
             PRAGMA foreign_keys = ON;",
        )
        .map_err(|e| e.to_string())?;
        migrations().to_latest(&mut conn).map_err(|e| format!("migrate: {e}"))?;
        let s = Self { conn };
        s.ensure_node_id().map_err(|e| e.to_string())?;
        Ok(s)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    fn ensure_node_id(&self) -> Result<()> {
        if self.meta("node_id")?.is_none() {
            self.conn.execute("INSERT INTO meta (key, value) VALUES ('node_id', ?1)", [random_id()])?;
        }
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    pub fn node_id(&self) -> String {
        self.meta("node_id").ok().flatten().unwrap_or_default()
    }

    /// Stores a minute point and folds it into its rollups. Returns false if
    /// that minute was already stored.
    pub fn insert_minute(&mut self, p: &Point) -> Result<bool> {
        let ts = (p.ts as i64).div_euclid(MINUTE) * MINUTE;
        let v = values(p);
        let cols = COLS.join(", ");
        let ph: String = (4..4 + N).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ");
        let args = |res: i64, ts: i64, n: u32| {
            let mut a: Vec<rusqlite::types::Value> = vec![res.into(), ts.into(), i64::from(n).into()];
            a.extend(v.iter().map(|x| x.map_or(rusqlite::types::Value::Null, rusqlite::types::Value::Real)));
            rusqlite::params_from_iter(a)
        };
        let tx = self.conn.transaction()?;
        let inserted = tx.execute(
            &format!(
                "INSERT INTO metric_points (res, ts, n, {cols}) VALUES (?1, ?2, ?3, {ph})
                 ON CONFLICT (res, ts) DO NOTHING"
            ),
            args(MINUTE, ts, p.n.max(1)),
        )?;
        if inserted == 0 {
            return Ok(false);
        }
        // Running average per column; a NULL leaves the column as it was.
        let upd: Vec<String> = COLS
            .iter()
            .map(|c| {
                format!(
                    "{c} = CASE WHEN excluded.{c} IS NULL THEN {c} WHEN {c} IS NULL THEN excluded.{c} \
                     ELSE ({c} * n + excluded.{c}) / (n + 1) END"
                )
            })
            .collect();
        let sql = format!(
            "INSERT INTO metric_points (res, ts, n, {cols}) VALUES (?1, ?2, ?3, {ph})
             ON CONFLICT (res, ts) DO UPDATE SET {}, n = n + 1",
            upd.join(", ")
        );
        for res in [ROLLUP, HOURLY] {
            tx.execute(&sql, args(res, ts.div_euclid(res) * res, 1))?;
        }
        tx.commit()?;
        Ok(true)
    }

    fn select(&self, sql_where: &str, args: impl rusqlite::Params, limit: usize) -> Result<Vec<Point>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT ts, n, {} FROM metric_points WHERE {sql_where} ORDER BY ts LIMIT {limit}",
            COLS.join(", ")
        ))?;
        let rows = stmt.query_map(args, |r| {
            let v: Vec<Option<f64>> = (2..2 + N).map(|i| r.get(i)).collect::<rusqlite::Result<_>>()?;
            let n: i64 = r.get(1)?;
            Ok(point(r.get(0)?, u32::try_from(n).unwrap_or(u32::MAX), &v))
        })?;
        rows.collect()
    }

    /// Points of tier `res` in `[from, to]`, oldest first, at most `limit`.
    pub fn history(&self, res: i64, from: i64, to: i64, limit: usize) -> Result<Vec<Point>> {
        self.select("res = ?1 AND ts >= ?2 AND ts <= ?3", params![res, from.div_euclid(res) * res, to], limit)
    }

    /// Closed buckets of tier `res` with `ts > after`, oldest first.
    pub fn sync(&self, res: i64, after: i64, now: i64, limit: usize) -> Result<Vec<Point>> {
        self.select("res = ?1 AND ts > ?2 AND ts + ?1 <= ?3", params![res, after, now], limit)
    }

    pub fn oldest(&self, res: i64) -> Result<Option<i64>> {
        self.conn.query_row("SELECT min(ts) FROM metric_points WHERE res = ?1", [res], |r| r.get(0))
    }

    pub fn coverage(&self) -> Result<Vec<Coverage>> {
        [MINUTE, ROLLUP, HOURLY]
            .into_iter()
            .map(|res| {
                self.conn.query_row("SELECT count(*), min(ts), max(ts) FROM metric_points WHERE res = ?1", [res], |r| {
                    Ok(Coverage { res, points: r.get::<_, i64>(0)? as u64, oldest: r.get(1)?, newest: r.get(2)? })
                })
            })
            .collect()
    }

    /// Drops minute points older than `minute_days`, 15-minute rollups older
    /// than [`ROLLUP_DAYS`] (or `minute_days`, if longer) and hourly ones
    /// older than [`HOURLY_DAYS`]. Returns rows removed.
    pub fn prune(&mut self, now: i64, minute_days: u32) -> Result<usize> {
        let days = i64::from(minute_days.clamp(1, 365));
        let mut n = 0;
        for (res, keep) in
            [(MINUTE, days), (ROLLUP, days.max(ROLLUP_DAYS.into())), (HOURLY, days.max(HOURLY_DAYS.into()))]
        {
            n += self
                .conn
                .execute("DELETE FROM metric_points WHERE res = ?1 AND ts < ?2", params![res, now - keep * 86400])?;
        }
        if n > 0 {
            self.conn.execute_batch("PRAGMA incremental_vacuum;")?;
        }
        Ok(n)
    }

    /// Database size in bytes (main file, without the WAL).
    pub fn size(&self) -> u64 {
        self.conn
            .query_row("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|v| v as u64)
            .unwrap_or(0)
    }
}

/// 128 random bits as hex, from the kernel.
pub fn random_id() -> String {
    use std::io::Read;
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        // Practically unreachable on Linux; fall back to time and pid.
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        b[..16].copy_from_slice(&(t ^ (u128::from(std::process::id()) << 64)).to_le_bytes());
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(ts: i64, cpu: f32) -> Point {
        Point { ts: ts as f64, cpu: Some(cpu), net_rx: Some(f64::from(cpu) * 10.0), n: 6, ..Default::default() }
    }

    #[test]
    fn migrations_are_valid() {
        migrations().validate().unwrap();
    }

    #[test]
    fn identity_survives_reopen() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("node.db");
        let id = Store::open(&path).unwrap().node_id();
        assert_eq!(id.len(), 32);
        assert_eq!(Store::open(&path).unwrap().node_id(), id);
        assert_ne!(Store::open_in_memory().unwrap().node_id(), id);
    }

    /// Same numbers as the app's `stores_rolls_up_queries_and_prunes`.
    #[test]
    fn stores_rolls_up_syncs_and_prunes() {
        let mut s = Store::open_in_memory().unwrap();
        let day = 86400;
        let base = 100 * day;
        for i in 0..30 {
            assert!(s.insert_minute(&p(base + i * 60, i as f32)).unwrap());
        }
        assert!(!s.insert_minute(&p(base, 99.0)).unwrap(), "same minute again is ignored");
        let recent = s.history(MINUTE, base, base + 3600, 5000).unwrap();
        assert_eq!(recent.len(), 30);
        assert_eq!((recent[0].cpu, recent[0].n), (Some(0.0), 6));
        let long = s.history(ROLLUP, base - 7 * day, base + 3600, 5000).unwrap();
        assert_eq!(long.iter().map(|p| (p.cpu, p.n)).collect::<Vec<_>>(), [(Some(7.0), 15), (Some(22.0), 15)]);
        let hourly = s.history(HOURLY, base - 300 * day, base + 3600, 5000).unwrap();
        assert_eq!((hourly.len(), hourly[0].cpu, hourly[0].n), (1, Some(14.5), 30));

        // Sync returns only closed buckets after the cursor, oldest first.
        let now = base + 30 * 60;
        let page = s.sync(MINUTE, base + 60 * 9, now, 5).unwrap();
        assert_eq!(
            page.iter().map(|p| p.ts as i64).collect::<Vec<_>>(),
            (10..15).map(|i| base + i * 60).collect::<Vec<_>>()
        );
        assert_eq!(s.sync(ROLLUP, 0, now, 100).unwrap().len(), 2);
        // The hour isn't over yet: nothing to sync in the hourly tier.
        assert!(s.sync(HOURLY, 0, now, 100).unwrap().is_empty());
        assert_eq!(s.sync(HOURLY, 0, base + 3600, 100).unwrap().len(), 1);

        let cov = s.coverage().unwrap();
        assert_eq!(cov[0], Coverage { res: MINUTE, points: 30, oldest: Some(base), newest: Some(base + 29 * 60) });

        // Prune: minute points after their retention, rollups after 90 days, hourly after 400.
        assert_eq!(s.prune(base + 8 * day, 7).unwrap(), 30);
        assert_eq!(s.oldest(MINUTE).unwrap(), None);
        assert_eq!(s.history(ROLLUP, base - 7 * day, base + 3600, 5000).unwrap().len(), 2);
        s.prune(base + 91 * day, 7).unwrap();
        assert!(s.history(ROLLUP, base - 7 * day, base + 3600, 5000).unwrap().is_empty());
        assert_eq!(s.history(HOURLY, base - 300 * day, base + 3600, 5000).unwrap().len(), 1);
        s.prune(base + 401 * day, 7).unwrap();
        assert!(s.history(HOURLY, base - 300 * day, base + 3600, 5000).unwrap().is_empty());
    }

    #[test]
    fn null_columns_do_not_drag_averages() {
        let mut s = Store::open_in_memory().unwrap();
        s.insert_minute(&Point { ts: 0.0, gpu: Some(50.0), temp: Some(40.0), ..Default::default() }).unwrap();
        s.insert_minute(&Point { ts: 60.0, gpu: None, temp: Some(60.0), ..Default::default() }).unwrap();
        let r = s.history(ROLLUP, 0, 900, 10).unwrap();
        assert_eq!((r[0].gpu, r[0].temp, r[0].n), (Some(50.0), Some(50.0), 2));
    }

    #[test]
    fn history_limit_and_size() {
        let mut s = Store::open_in_memory().unwrap();
        for i in 0..10 {
            s.insert_minute(&p(i * 60, 1.0)).unwrap();
        }
        assert_eq!(s.history(MINUTE, 0, 10_000, 3).unwrap().len(), 3);
        assert!(s.size() > 0);
    }
}
