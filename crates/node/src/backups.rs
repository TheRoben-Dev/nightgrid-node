// Partly vendored from Nightgrid (https://github.com/TheRoben-Dev/nightgrid),
// core/src/backup_jobs.rs at commit 8eedcf2, MIT License, (c) 2026 TheRoben.
// `normalize`, the status rules (`unit_status`, `path_status`, `stale`) and
// `parse_totals` follow the app, so a job reads the same from both. Local
// changes are marked `NODE:`.

//! Backup job status (N7b): restic, borg, rsync and rclone jobs the app
//! defines, read with the app closed. Like the app, the node doesn't open
//! repositories or run anything: a job points at a systemd unit (its last
//! run, exit status and the run's journal output, where the tools print
//! their totals) or a path (the newest file's time and the size on disk).

use std::path::Path;

use nightgrid_node_proto::{AlertKind, BackupJob, BackupResult, BackupSource, BackupStatus, Severity};

use crate::alerts::Condition;
use crate::redact::line_has_secret;

pub const MAX_NAME: usize = 80;
/// At most 90 days.
pub const MAX_STALE_HOURS: u32 = 24 * 90;
/// NODE: most jobs one host watches.
pub const MAX_JOBS: usize = 100;

/// "26 hours", "3 days".
pub fn stale_words(hours: u32) -> String {
    if hours % 24 == 0 && hours >= 48 {
        format!("{} days", hours / 24)
    } else if hours == 1 {
        "hour".into()
    } else {
        format!("{hours} hours")
    }
}

pub fn valid_unit(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.starts_with('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | ':' | '-' | '\\'))
}

/// Trims and checks a job. NODE: returns the error text.
pub fn normalize(j: &BackupJob) -> Result<BackupJob, String> {
    let name = j.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Err(format!("name must be 1-{MAX_NAME} characters"));
    }
    let target = j.target.trim().to_owned();
    match j.source {
        BackupSource::Unit if !valid_unit(&target) => return Err(format!("{target:?} isn't a systemd unit name")),
        BackupSource::Path
            if !target.starts_with('/') || target.len() > 4096 || target.chars().any(char::is_control) =>
        {
            return Err("target must be an absolute path".into())
        }
        _ => {}
    }
    if j.stale_hours == 0 || j.stale_hours > MAX_STALE_HOURS {
        return Err(format!("staleHours must be 1-{MAX_STALE_HOURS}"));
    }
    Ok(BackupJob { name, target, ..j.clone() })
}

/// NODE: the whole list, as `jobs.set` takes it.
pub fn validate(list: &[BackupJob]) -> Result<Vec<BackupJob>, String> {
    if list.len() > MAX_JOBS {
        return Err(format!("at most {MAX_JOBS} backup jobs"));
    }
    let mut out: Vec<BackupJob> = Vec::with_capacity(list.len());
    for j in list {
        if out.iter().any(|o| o.id == j.id) {
            return Err(format!("backup job id {} is listed twice", j.id));
        }
        out.push(normalize(j).map_err(|e| format!("backup job {}: {e}", j.id))?);
    }
    Ok(out)
}

pub fn empty(job_id: i32) -> BackupStatus {
    BackupStatus {
        job_id,
        result: BackupResult::Unknown,
        last_run: None,
        last_ok: None,
        exit_code: None,
        size: None,
        added: None,
        summary: None,
        stale: false,
        error: None,
    }
}

/// `systemctl show` properties of one unit.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Props(pub Vec<(String, String)>);

impl Props {
    pub fn get(&self, k: &str) -> &str {
        self.0.iter().find(|(key, _)| key == k).map_or("", |(_, v)| v.as_str())
    }

    /// Blocks of `systemctl show a b c` output, in order (blank-line separated).
    pub fn parse_many(out: &str) -> Vec<Props> {
        let mut v = Vec::new();
        let mut cur = Props::default();
        for line in out.lines() {
            if line.trim().is_empty() {
                if !cur.0.is_empty() {
                    v.push(std::mem::take(&mut cur));
                }
                continue;
            }
            if let Some((k, val)) = line.split_once('=') {
                cur.0.push((k.to_owned(), val.to_owned()));
            }
        }
        if !cur.0.is_empty() {
            v.push(cur);
        }
        v
    }
}

/// The unit's last run. `boot` is when the host booted (unix seconds).
/// NODE: takes parsed properties and the log instead of the script's output.
pub fn unit_status(id: i32, props: &Props, log: &[&str], boot: f64) -> BackupStatus {
    let mut st = empty(id);
    if props.get("LoadState") == "not-found" || props.0.is_empty() {
        st.error = Some("The unit wasn't found on this host.".into());
        return st;
    }
    let mono = |k: &str| props.get(k).parse::<f64>().ok().filter(|v| *v > 0.0);
    let started = mono("ExecMainStartTimestampMonotonic").map(|m| boot + m / 1e6);
    let exited = mono("ExecMainExitTimestampMonotonic").map(|m| boot + m / 1e6);
    let active = props.get("ActiveState");
    let code = props.get("ExecMainStatus").parse::<i32>().ok();
    let result = props.get("Result");
    let totals = parse_totals(log);
    st.size = totals.size;
    st.added = totals.added;
    st.summary = totals.summary;
    if active == "activating" || (active == "active" && props.get("SubState") == "running") {
        st.result = BackupResult::Running;
        st.last_run = started;
        st.summary = Some("Running now".into());
        return st;
    }
    let Some(done) = exited.or(started) else {
        st.summary = Some("No run since the host booted".into());
        return st;
    };
    st.last_run = Some(done);
    st.exit_code = code;
    if result == "success" && code.unwrap_or(0) == 0 {
        st.result = BackupResult::Ok;
        st.last_ok = Some(done);
    } else {
        st.result = BackupResult::Failed;
        st.summary = totals.error.or_else(|| Some(format!("Result: {result}, exit {}", code.unwrap_or(-1))));
    }
    st
}

/// What the node read of a path. NODE: read in Rust, not with `stat`/`find`/`du`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PathRead {
    pub missing: bool,
    pub denied: bool,
    pub mtime: Option<f64>,
    pub is_dir: bool,
    /// Newest file within three levels (directories).
    pub newest: Option<f64>,
    pub size: Option<u64>,
}

pub fn path_status(id: i32, p: &PathRead) -> BackupStatus {
    let mut st = empty(id);
    if p.missing {
        st.error = Some("The path doesn't exist on this host.".into());
        return st;
    }
    if p.denied {
        st.error = Some("The path can't be read by the node's service account.".into());
        return st;
    }
    let mtime = if p.is_dir { p.newest.map(f64::floor).or(p.mtime) } else { p.mtime };
    st.size = p.size;
    st.last_run = mtime;
    st.last_ok = mtime;
    st.result = if mtime.is_some() { BackupResult::Ok } else { BackupResult::Unknown };
    if p.is_dir && st.size.is_none() {
        st.summary = Some("Size not read (took too long)".into());
    }
    st
}

/// The app's stale rule. `uptime` in seconds.
pub fn stale(st: &BackupStatus, job: &BackupJob, now: f64, uptime: f64) -> bool {
    let limit = f64::from(job.stale_hours) * 3600.0;
    st.error.is_none()
        && st.result != BackupResult::Running
        && match (st.last_ok, st.last_run) {
            (Some(t), _) => now - t > limit,
            (None, Some(_)) => true,
            (None, None) => job.source == BackupSource::Path || uptime > limit,
        }
}

/// The app's `alerts::backup` condition, keyed `backup:<id>`.
pub fn condition(job: &BackupJob, host: &str, st: &BackupStatus) -> Option<Condition> {
    let base = |kind, severity, detail: String| Condition {
        key: format!("backup:{}", job.id),
        kind,
        severity,
        title: format!("{} on {host}", job.name),
        detail,
    };
    if st.result == BackupResult::Failed {
        let why = st.summary.clone().unwrap_or_else(|| "the last run failed".into());
        return Some(base(AlertKind::BackupFailed, Severity::Critical, why));
    }
    st.stale.then(|| {
        base(
            AlertKind::BackupStale,
            Severity::Warn,
            format!("no good run in the last {}", stale_words(job.stale_hours)),
        )
    })
}

#[derive(Debug, Default, PartialEq)]
pub struct Totals {
    pub size: Option<u64>,
    pub added: Option<u64>,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub ok_seen: bool,
}

/// Totals that restic, borg (`--stats`), rsync (`--stats` or `-v`) and
/// rclone print at the end of a run. Later lines win.
pub fn parse_totals(lines: &[&str]) -> Totals {
    let mut t = Totals::default();
    for raw in lines {
        let l = raw.trim();
        let lower = l.to_ascii_lowercase();
        if let Some(rest) = l.strip_prefix("processed ") {
            if let Some((_, after)) = rest.split_once(" files, ") {
                if let Some(sz) = size_at(after) {
                    t.size = Some(sz);
                }
            }
        }
        if let Some(rest) = l.strip_prefix("Added to the repository:") {
            t.added = size_at(rest.trim());
        }
        if l.starts_with("snapshot ") && l.ends_with(" saved") {
            t.summary = Some(l.to_owned());
            t.ok_seen = true;
        }
        if let Some(rest) = l.strip_prefix("This archive:") {
            let sizes = sizes_in(rest);
            if let Some(first) = sizes.first() {
                t.size = Some(*first);
            }
            if sizes.len() >= 3 {
                t.added = Some(sizes[2]);
            }
            t.ok_seen = true;
        }
        if let Some(rest) = l.strip_prefix("Archive name:") {
            t.summary = Some(format!("archive {}", rest.trim()));
        }
        if let Some(rest) = l.strip_prefix("Total file size:") {
            t.size = size_at(rest.trim());
        }
        if let Some(rest) = l.strip_prefix("Total transferred file size:") {
            t.added = size_at(rest.trim());
        }
        if let Some(rest) = l.strip_prefix("sent ") {
            if rest.contains(" received ") {
                t.added = t.added.or(size_at(rest));
                t.ok_seen = true;
            }
        }
        if let Some(rest) = l.strip_prefix("total size is ") {
            t.size = size_at(rest);
        }
        if let Some(rest) = l.strip_prefix("Transferred:") {
            if rest.contains('/') {
                t.added = size_at(rest.trim());
            }
        }
        if (lower.starts_with("fatal") || lower.contains("error:") || lower.starts_with("error")) && !line_has_secret(l)
        {
            t.error = Some(l.chars().take(200).collect());
        }
    }
    t
}

fn size_at(s: &str) -> Option<u64> {
    let mut it = s.split_whitespace();
    let num = it.next()?;
    let unit = it.next().unwrap_or("").trim_end_matches([',', '.', ')']);
    to_bytes(num, unit)
}

fn sizes_in(s: &str) -> Vec<u64> {
    let w: Vec<&str> = s.split_whitespace().collect();
    w.windows(2).filter_map(|p| to_bytes(p[0], p[1]).filter(|_| unit_factor(p[1]).is_some())).collect()
}

fn unit_factor(unit: &str) -> Option<f64> {
    Some(match unit {
        "B" | "bytes" | "byte" => 1.0,
        "kB" | "KB" | "K" => 1e3,
        "MB" | "M" => 1e6,
        "GB" | "G" => 1e9,
        "TB" | "T" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1024f64.powi(2),
        "GiB" => 1024f64.powi(3),
        "TiB" => 1024f64.powi(4),
        _ => return None,
    })
}

fn to_bytes(num: &str, unit: &str) -> Option<u64> {
    let n: f64 = num.replace(',', "").parse().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    Some((n * unit_factor(unit).unwrap_or(1.0)).round() as u64)
}

// ---- NODE: reading a path -------------------------------------------------

/// Entries a size or newest-file walk reads at most.
const WALK_LIMIT: usize = 200_000;

fn secs(t: std::io::Result<std::time::SystemTime>) -> Option<f64> {
    t.ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs_f64())
}

/// Stats a path; for a directory, finds the newest file within three levels.
/// The size (which can take long on a big repository) only when `with_size`.
pub fn read_path(p: &Path, with_size: bool) -> PathRead {
    let meta = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent_readable = p.parent().is_some_and(|d| std::fs::read_dir(d).is_ok());
            return PathRead { missing: parent_readable, denied: !parent_readable, ..Default::default() };
        }
        Err(_) => return PathRead { denied: true, ..Default::default() },
    };
    let mut r = PathRead { mtime: secs(meta.modified()), is_dir: meta.is_dir(), ..Default::default() };
    if !r.is_dir {
        r.size = Some(meta.len());
        return r;
    }
    if std::fs::read_dir(p).is_err() {
        return PathRead { denied: true, ..Default::default() };
    }
    let mut seen = 0usize;
    r.newest = newest(p, 3, &mut seen);
    if with_size {
        let mut seen = 0usize;
        let mut total = 0u64;
        if size_of(p, &mut seen, &mut total) {
            r.size = Some(total);
        }
    }
    r
}

fn newest(dir: &Path, depth: u32, seen: &mut usize) -> Option<f64> {
    let mut best: Option<f64> = None;
    let Ok(rd) = std::fs::read_dir(dir) else { return None };
    for e in rd.flatten() {
        *seen += 1;
        if *seen > WALK_LIMIT {
            break;
        }
        let Ok(ft) = e.file_type() else { continue };
        let t = if ft.is_file() {
            e.metadata().ok().and_then(|m| secs(m.modified()))
        } else if ft.is_dir() && depth > 1 {
            newest(&e.path(), depth - 1, seen)
        } else {
            None
        };
        if let Some(t) = t {
            best = Some(best.map_or(t, |b: f64| b.max(t)));
        }
    }
    best
}

/// Bytes on disk (like `du`), `false` when the walk hit its limit.
fn size_of(dir: &Path, seen: &mut usize, total: &mut u64) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(rd) = std::fs::read_dir(dir) else { return true };
    for e in rd.flatten() {
        *seen += 1;
        if *seen > WALK_LIMIT {
            return false;
        }
        let Ok(m) = e.metadata() else { continue };
        *total += m.blocks() * 512;
        if m.is_dir() && !size_of(&e.path(), seen, total) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightgrid_node_proto::BackupTool;

    fn job(id: i32, source: BackupSource, target: &str) -> BackupJob {
        BackupJob {
            id,
            name: "nightly".into(),
            tool: BackupTool::Restic,
            source,
            target: target.into(),
            stale_hours: 26,
        }
    }

    fn props(s: &str) -> Props {
        Props::parse_many(s).pop().unwrap_or_default()
    }

    #[test]
    fn validates_like_the_app() {
        let mut j = job(1, BackupSource::Unit, " restic-backup.timer ");
        j.name = " nightly ".into();
        let n = normalize(&j).unwrap();
        assert_eq!((n.name.as_str(), n.target.as_str()), ("nightly", "restic-backup.timer"));
        j.target = "x; rm -rf /".into();
        assert!(normalize(&j).is_err());
        j.source = BackupSource::Path;
        j.target = "relative".into();
        assert!(normalize(&j).is_err());
        j.target = "/srv/restic".into();
        j.stale_hours = 0;
        assert!(normalize(&j).is_err());
        let a = job(1, BackupSource::Path, "/a");
        assert!(validate(&[a.clone(), a]).unwrap_err().contains("twice"));
    }

    #[test]
    fn many_blocks() {
        let v = Props::parse_many("Id=a.service\nLoadState=loaded\n\nId=b.service\nLoadState=not-found\n");
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].get("LoadState"), "not-found");
        assert_eq!(v[0].get("Nope"), "");
    }

    #[test]
    fn unit_ok_failed_running() {
        let ok = props(
            "Id=a.service\nLoadState=loaded\nActiveState=inactive\nSubState=dead\nResult=success\nExecMainStatus=0\n\
             ExecMainStartTimestampMonotonic=90000000000\nExecMainExitTimestampMonotonic=90060000000\n",
        );
        let st = unit_status(1, &ok, &["snapshot deadbeef saved", "processed 3 files, 1.000 KiB in 0:01"], 1000.0);
        assert_eq!(st.result, BackupResult::Ok);
        assert_eq!(st.last_ok, Some(1000.0 + 90060.0));
        assert_eq!(st.size, Some(1024));
        assert_eq!(st.summary.as_deref(), Some("snapshot deadbeef saved"));
        let failed = props(
            "Id=b.service\nLoadState=loaded\nActiveState=failed\nResult=exit-code\nExecMainStatus=1\n\
             ExecMainStartTimestampMonotonic=98000000000\nExecMainExitTimestampMonotonic=98001000000\n",
        );
        let st = unit_status(2, &failed, &["Fatal: repository is locked"], 0.0);
        assert_eq!((st.result, st.exit_code), (BackupResult::Failed, Some(1)));
        assert_eq!(st.summary.as_deref(), Some("Fatal: repository is locked"));
        let running = props(
            "Id=c.service\nLoadState=loaded\nActiveState=activating\nSubState=start\n\
             ExecMainStartTimestampMonotonic=5000000\n",
        );
        assert_eq!(unit_status(3, &running, &[], 0.0).result, BackupResult::Running);
        assert!(unit_status(4, &props("Id=d.service\nLoadState=not-found\n"), &[], 0.0).error.is_some());
    }

    #[test]
    fn stale_rules() {
        let j = job(1, BackupSource::Unit, "a.timer");
        let mut st = empty(1);
        st.result = BackupResult::Ok;
        st.last_ok = Some(1000.0);
        st.last_run = Some(1000.0);
        assert!(!stale(&st, &j, 1000.0 + 25.0 * 3600.0, 1e9));
        assert!(stale(&st, &j, 1000.0 + 27.0 * 3600.0, 1e9));
        // No run since boot: stale only once the host has been up past the limit.
        let none = empty(1);
        assert!(!stale(&none, &j, 0.0, 3600.0));
        assert!(stale(&none, &j, 0.0, 27.0 * 3600.0));
        let mut failed = empty(1);
        failed.result = BackupResult::Failed;
        failed.last_run = Some(5.0);
        let c = condition(&j, "nas", &BackupStatus { stale: true, ..failed }).unwrap();
        assert_eq!((c.kind, c.key.as_str(), c.title.as_str()), (AlertKind::BackupFailed, "backup:1", "nightly on nas"));
        let c = condition(&j, "nas", &BackupStatus { stale: true, ..st }).unwrap();
        assert_eq!(c.kind, AlertKind::BackupStale);
        assert_eq!(c.detail, "no good run in the last 26 hours");
    }

    #[test]
    fn totals() {
        let borg = [
            "Archive name: host-2026-11-15",
            "This archive:                1.23 GB            800.00 MB             12.30 MB",
        ];
        let t = parse_totals(&borg);
        assert_eq!((t.size, t.added), (Some(1_230_000_000), Some(12_300_000)));
        let t = parse_totals(&["sent 1,234 bytes  received 56 bytes  2,580.00 bytes/sec", "total size is 9,876,543"]);
        assert_eq!((t.size, t.added), (Some(9_876_543), Some(1234)));
        assert!(parse_totals(&["error: password=hunter2222 rejected"]).error.is_none());
    }

    #[test]
    fn reads_paths() {
        let dir = std::env::temp_dir().join(format!("ngn-bk-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/data"), vec![0u8; 10_000]).unwrap();
        let r = read_path(&dir, true);
        assert!(r.is_dir && r.newest.is_some(), "{r:?}");
        assert!(r.size.unwrap() >= 10_000, "{r:?}");
        let st = path_status(1, &r);
        assert_eq!(st.result, BackupResult::Ok);
        assert!(read_path(&dir.join("nope"), false).missing);
        let f = read_path(&dir.join("a/b/data"), false);
        assert_eq!(f.size, Some(10_000));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
