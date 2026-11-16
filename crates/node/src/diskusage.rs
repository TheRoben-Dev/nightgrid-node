//! Hourly usage per mount and fill-up forecasts.
//!
// `trend`, `days_left`, `forecasts`, `days_words` and the condition rule are
// vendored from Nightgrid (https://github.com/TheRoben-Dev/nightgrid),
// core/src/diskforecast.rs at commit 8eedcf2, MIT License, (c) 2026 TheRoben.

use nightgrid_node_proto::{AlertKind, DiskUsageRow, MountUsage, Severity};
use rusqlite::params;

use crate::alerts::Condition;
use crate::store::{Result, Store};

const HOUR: i64 = 3600;
const DAY: f64 = 86_400.0;
/// History kept per mount (the app keeps 90 days).
pub const KEEP_DAYS: i64 = 90;
/// The trend is fitted over this many recent days.
pub const WINDOW_DAYS: i64 = 14;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsagePoint {
    pub ts: i64,
    pub used: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MountForecast {
    pub mount: String,
    pub size: u64,
    pub used: u64,
    pub at: i64,
    pub per_day: Option<f64>,
    pub days_left: Option<f64>,
}

impl Store {
    /// Records the latest usage of each mount in the current hour.
    pub fn disk_usage_record(&self, mounts: &[MountUsage], now: i64) -> Result<()> {
        let ts = now - now.rem_euclid(HOUR);
        for m in mounts {
            let size = m.used.saturating_add(m.avail);
            if size == 0 {
                continue;
            }
            self.conn().execute(
                "INSERT OR REPLACE INTO disk_usage (mount, ts, used, size) VALUES (?1, ?2, ?3, ?4)",
                params![m.mount, ts, m.used as i64, size as i64],
            )?;
        }
        Ok(())
    }

    pub fn prune_disk_usage(&self, now: i64) -> Result<usize> {
        self.conn().execute("DELETE FROM disk_usage WHERE ts < ?1", [now - KEEP_DAYS * 86_400])
    }

    /// Rows of closed hours with `ts > after`, by time then mount.
    pub fn disk_usage_after(&self, after: i64, now: i64, limit: usize) -> Result<Vec<DiskUsageRow>> {
        let mut st = self.conn().prepare_cached(
            "SELECT mount, ts, used, size FROM disk_usage WHERE ts > ?1 AND ts + 3600 <= ?2 ORDER BY ts, mount LIMIT ?3",
        )?;
        let rows = st.query_map(params![after, now, limit as i64], |r| {
            Ok(DiskUsageRow {
                mount: r.get(0)?,
                ts: r.get(1)?,
                used: r.get::<_, i64>(2)? as u64,
                size: r.get::<_, i64>(3)? as u64,
            })
        })?;
        rows.collect()
    }

    /// Forecasts for every mount with recent history.
    pub fn disk_forecasts(&self, now: i64) -> Result<Vec<MountForecast>> {
        let mut st = self
            .conn()
            .prepare_cached("SELECT mount, ts, used, size FROM disk_usage WHERE ts >= ?1 ORDER BY mount, ts")?;
        let rows = st
            .query_map([now - WINDOW_DAYS * 86_400], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, i64>(3)? as u64,
                ))
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(forecasts(&rows, now))
    }
}

/// Least-squares slope (bytes per day) within the window. Needs a day of
/// history and six hourly points.
pub fn trend(points: &[UsagePoint], now: i64) -> Option<f64> {
    let recent: Vec<&UsagePoint> = points.iter().filter(|p| p.ts >= now - WINDOW_DAYS * 86_400).collect();
    let span = match (recent.first(), recent.last()) {
        (Some(a), Some(b)) => (b.ts - a.ts) as f64 / DAY,
        _ => 0.0,
    };
    if recent.len() < 6 || span < 1.0 {
        return None;
    }
    let n = recent.len() as f64;
    let t0 = recent[0].ts;
    let xs: Vec<f64> = recent.iter().map(|p| (p.ts - t0) as f64 / DAY).collect();
    let ys: Vec<f64> = recent.iter().map(|p| p.used as f64).collect();
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let sxx: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
    if sxx == 0.0 {
        return None;
    }
    let sxy: f64 = xs.iter().zip(&ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    Some(sxy / sxx)
}

/// Days until `used` reaches `size`; growth under 0.01 % of the size per day is flat.
pub fn days_left(used: u64, size: u64, per_day: Option<f64>) -> Option<f64> {
    let rate = per_day?;
    if size == 0 || rate <= size as f64 * 0.0001 {
        return None;
    }
    Some((size.saturating_sub(used)) as f64 / rate)
}

/// Groups `(mount, ts, used, size)` rows sorted by mount, then time.
pub fn forecasts(rows: &[(String, i64, u64, u64)], now: i64) -> Vec<MountForecast> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let mount = &rows[i].0;
        let j = rows[i..].iter().position(|r| &r.0 != mount).map_or(rows.len(), |k| i + k);
        let group = &rows[i..j];
        i = j;
        let points: Vec<UsagePoint> = group.iter().map(|r| UsagePoint { ts: r.1, used: r.2 }).collect();
        let last = &group[group.len() - 1];
        let per_day = trend(&points, now);
        out.push(MountForecast {
            mount: mount.clone(),
            size: last.3,
            used: last.2,
            at: last.1,
            per_day,
            days_left: days_left(last.2, last.3, per_day),
        });
    }
    out
}

/// "4 days", "18 hours", "an hour".
pub fn days_words(d: f64) -> String {
    if d >= 2.0 {
        format!("{} days", d.floor() as i64)
    } else if d * 24.0 >= 1.5 {
        format!("{} hours", (d * 24.0).round() as i64)
    } else {
        "an hour".into()
    }
}

/// "FILLING UP" when a mount is forecast full within `within_days` (critical inside three).
pub fn conditions(host: &str, list: &[MountForecast], within_days: u16, now: i64) -> Vec<Condition> {
    list.iter()
        .filter(|f| now - f.at <= 86_400)
        .filter_map(|f| {
            let d = f.days_left?;
            (d < f64::from(within_days)).then(|| Condition {
                key: format!("fill:{}", f.mount),
                kind: AlertKind::DiskFilling,
                severity: if d < 3.0 { Severity::Critical } else { Severity::Warn },
                title: host.to_owned(),
                detail: format!("{} full in about {} at the current rate", f.mount, days_words(d)),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1_000_000_000;

    fn mount(m: &str, used: u64, avail: u64) -> MountUsage {
        MountUsage {
            device: "/dev/sda1".into(),
            mount: m.into(),
            fstype: "ext4".into(),
            total: used + avail,
            used,
            avail,
        }
    }

    #[test]
    fn records_forecasts_alerts_and_syncs() {
        let s = Store::open_in_memory().unwrap();
        let start = 1_700_000_000 - 1_700_000_000 % 3600;
        for k in 0..72 {
            let t = start + k * 3600;
            s.disk_usage_record(
                &[mount("/data", 900 * GB + k as u64 * GB, 100 * GB - k as u64 * GB), mount("/", 20 * GB, 80 * GB)],
                t,
            )
            .unwrap();
        }
        let now = start + 71 * 3600 + 700;
        let f = s.disk_forecasts(now).unwrap();
        assert_eq!(f.iter().map(|m| m.mount.as_str()).collect::<Vec<_>>(), ["/", "/data"]);
        assert_eq!(f[0].days_left, None);
        assert!((f[1].per_day.unwrap() / GB as f64 - 24.0).abs() < 0.01);
        let c = conditions("server01", &f, 14, now);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].key.as_str(), c[0].severity), ("fill:/data", Severity::Critical));
        // Sync: closed hours only (the newest hour is still open).
        let rows = s.disk_usage_after(0, now, 10_000).unwrap();
        assert_eq!(rows.len(), 71 * 2);
        assert_eq!(s.disk_usage_after(start + 70 * 3600, now, 10).unwrap().len(), 0);
        assert_eq!(days_words(4.7), "4 days");
    }
}
