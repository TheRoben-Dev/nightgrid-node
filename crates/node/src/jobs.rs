//! Jobs (N7b): the app's backup jobs and scheduled jobs, watched with the
//! app closed.
//!
//! The node schedules nothing itself: the app already installs systemd
//! timers (`nightgrid-job-*` for commands, `nightgrid-power-*` for reboots,
//! shutdowns and suspends), and systemd runs them whether or not anything
//! else is up. The node watches them read-only: each finished run becomes a
//! `job` event, a failed or stale backup becomes an alert, and a scheduled
//! reboot or shutdown is recorded after the host is back (from the previous
//! boot's journal). It needs no privileges beyond `allow journal` for the
//! runs' output; without it, results and exit codes still show.

use std::collections::BTreeMap;
use std::path::Path;
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use nightgrid_node_proto::{BackupJob, BackupResult, BackupSource, BackupStatus, EventKind, JobRun, JobsLatest, Level};

use crate::alerts::Condition;
use crate::backups::{self, Props};
use crate::daemon::{now_secs, Shared};
use crate::events::NewEvent;
use crate::redact::line_has_secret;

const META_JOBS: &str = "jobs";
const META_SEEN: &str = "jobs_seen";
#[cfg(unix)]
const META_POWER_BOOT: &str = "jobs_power_boot";
/// Paths are read at most this often (a newest-file walk isn't free).
const PATH_EVERY: i64 = 600;
/// A directory's size is read again after this long even if nothing changed.
const SIZE_EVERY: i64 = 6 * 3600;
pub const JOB_PREFIX: &str = "nightgrid-job-";
pub const POWER_PREFIX: &str = "nightgrid-power-";
#[cfg(unix)]
const PROPS: &str = "Id,Description,LoadState,ActiveState,SubState,Result,ExecMainStatus,\
ExecMainStartTimestampMonotonic,ExecMainExitTimestampMonotonic,InvocationID";

/// What the app asked for and what was read last.
#[derive(Debug, Default)]
pub struct JobsState {
    pub list: Vec<BackupJob>,
    pub latest: JobsLatest,
    /// Bumped by `jobs.set`, so the watcher reads a new list right away.
    pub generation: u64,
}

impl JobsState {
    /// Takes a new list from the app.
    pub fn set(&mut self, list: Vec<BackupJob>) {
        self.latest.backups.retain(|st| list.iter().any(|j| j.id == st.job_id));
        self.list = list;
        self.generation += 1;
    }

    /// Failed and stale backups, for listed jobs only.
    pub fn conditions(&self, host: &str) -> Vec<Condition> {
        self.latest
            .backups
            .iter()
            .filter_map(|st| self.list.iter().find(|j| j.id == st.job_id).and_then(|j| backups::condition(j, host, st)))
            .collect()
    }
}

pub fn load(store: &crate::store::Store) -> Vec<BackupJob> {
    store.meta(META_JOBS).ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

pub fn save(store: &crate::store::Store, list: &[BackupJob]) -> crate::store::Result<()> {
    store.set_meta(META_JOBS, &serde_json::to_string(list).unwrap_or_default())
}

/// The service a backup job's unit stands for: a timer's `Unit=`, a
/// `.service` itself, or `<name>.service`.
pub fn service_of(target: &str, timers: &BTreeMap<String, String>) -> String {
    if target.ends_with(".timer") {
        timers.get(target).cloned().unwrap_or_else(|| format!("{}.service", target.trim_end_matches(".timer")))
    } else if target.ends_with(".service") {
        target.to_owned()
    } else {
        format!("{target}.service")
    }
}

/// A run that finished, identified so it is reported once.
pub fn run_key(p: &Props) -> Option<String> {
    let active = p.get("ActiveState");
    if active == "activating" || (active == "active" && p.get("SubState") == "running") {
        return None;
    }
    let exit = p.get("ExecMainExitTimestampMonotonic");
    if exit.is_empty() || exit == "0" {
        return None;
    }
    Some(format!("{}:{exit}", p.get("InvocationID")))
}

/// The last line worth showing from a run's output, masked if it looks secret.
pub fn last_line(log: &[&str]) -> Option<String> {
    let l = log.iter().rev().map(|l| l.trim()).find(|l| !l.is_empty())?;
    Some(if line_has_secret(l) {
        "(output hidden: it looks like it holds a secret)".into()
    } else {
        l.chars().take(200).collect()
    })
}

/// "Nightgrid job: nightly sync" → "nightly sync".
fn job_label(unit: &str, description: &str) -> String {
    let d = description.trim();
    let d = d.strip_prefix("Nightgrid job:").map(str::trim).unwrap_or(d);
    if d.is_empty() || d == unit || d == format!("{unit}.service") {
        unit.strip_prefix(JOB_PREFIX).or_else(|| unit.strip_prefix(POWER_PREFIX)).unwrap_or(unit).to_owned()
    } else {
        d.chars().take(120).collect()
    }
}

/// One scheduled job's last run.
pub fn job_run(unit: &str, p: &Props, log: &[&str], boot: f64) -> JobRun {
    let st = backups::unit_status(0, p, &[], boot);
    let totals = backups::parse_totals(log);
    let summary = match st.result {
        BackupResult::Failed => totals.error.or_else(|| last_line(log)).or(st.summary),
        BackupResult::Running => st.summary,
        _ => last_line(log).or(st.summary),
    };
    JobRun {
        unit: unit.to_owned(),
        description: job_label(unit, p.get("Description")),
        result: st.result,
        started: p
            .get("ExecMainStartTimestampMonotonic")
            .parse::<f64>()
            .ok()
            .filter(|v| *v > 0.0)
            .map(|m| boot + m / 1e6),
        finished: if st.result == BackupResult::Running { None } else { st.last_run },
        exit_code: st.exit_code,
        summary,
    }
}

fn size_words(b: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// The event for a backup unit's finished run.
pub fn backup_event(job: &BackupJob, st: &BackupStatus, at: i64) -> NewEvent {
    let ok = st.result == BackupResult::Ok;
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = &st.summary {
        parts.push(s.clone());
    }
    if let Some(s) = st.size {
        parts.push(size_words(s));
    }
    if let Some(a) = st.added {
        parts.push(format!("added {}", size_words(a)));
    }
    if !ok {
        if let Some(c) = st.exit_code {
            parts.push(format!("exit {c}"));
        }
    }
    NewEvent::new(
        at,
        EventKind::Job,
        if ok { Level::Info } else { Level::Warn },
        format!("backup {} {}", job.name, if ok { "finished" } else { "failed" }),
        parts.join(" · "),
    )
}

/// The event for a scheduled job's finished run.
pub fn run_event(r: &JobRun, at: i64) -> NewEvent {
    let ok = r.result == BackupResult::Ok;
    let title = if ok {
        format!("job {} finished", r.description)
    } else {
        format!("job {} failed{}", r.description, r.exit_code.map(|c| format!(" (exit {c})")).unwrap_or_default())
    };
    NewEvent::new(
        at,
        EventKind::Job,
        if ok { Level::Info } else { Level::Warn },
        title,
        r.summary.clone().unwrap_or_default(),
    )
}

/// Reads a scheduled power job from the previous boot's journal (JSON lines):
/// the latest `nightgrid-power-*` unit that started, and when.
pub fn power_from_journal(out: &str) -> Option<(String, i64)> {
    let mut best: Option<(String, i64)> = None;
    for line in out.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let unit = ["UNIT", "_SYSTEMD_UNIT"].iter().find_map(|k| v[*k].as_str()).unwrap_or_default();
        let Some(name) = unit.strip_suffix(".service").filter(|u| u.starts_with(POWER_PREFIX)) else { continue };
        let at = v["__REALTIME_TIMESTAMP"].as_str().and_then(|t| t.parse::<i64>().ok()).unwrap_or(0) / 1_000_000;
        if best.as_ref().is_none_or(|b| at >= b.1) {
            best = Some((name.to_owned(), at));
        }
    }
    best
}

pub fn power_event(unit: &str, at: i64) -> NewEvent {
    let what = unit.strip_prefix(POWER_PREFIX).unwrap_or(unit);
    let what = what.split('-').next().unwrap_or(what);
    NewEvent::new(
        at,
        EventKind::Job,
        Level::Info,
        format!("scheduled {what} ran"),
        format!("{unit} started the {what} (from the app's power schedule)"),
    )
}

// ---- reading the host ------------------------------------------------------

#[cfg(unix)]
fn run(prog: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(prog)
        .args(args)
        .env("SYSTEMD_PAGER", "")
        .env("SYSTEMD_COLORS", "0")
        .env_remove("NOTIFY_SOCKET")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(unix)]
fn show(units: &[String], props: &str) -> BTreeMap<String, Props> {
    let mut map = BTreeMap::new();
    if units.is_empty() {
        return map;
    }
    let mut args: Vec<&str> = vec!["show", "-p", props, "--"];
    args.extend(units.iter().map(String::as_str));
    if let Some(out) = run("systemctl", &args) {
        for p in Props::parse_many(&out) {
            let id = p.get("Id").to_owned();
            if !id.is_empty() {
                map.insert(id, p);
            }
        }
    }
    map
}

#[cfg(unix)]
fn scheduled_units() -> Vec<String> {
    let Some(out) = run(
        "systemctl",
        &[
            "list-unit-files",
            "--no-legend",
            "--plain",
            "--type=service",
            "--",
            "nightgrid-job-*.service",
            "nightgrid-power-*.service",
        ],
    ) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|u| u.starts_with(JOB_PREFIX) || u.starts_with(POWER_PREFIX))
        .filter(|u| backups::valid_unit(u))
        .map(str::to_owned)
        .collect()
}

#[cfg(unix)]
fn journal(inv: &str, service: &str) -> Vec<String> {
    let mut out = String::new();
    if !inv.is_empty() && inv.chars().all(|c| c.is_ascii_hexdigit()) {
        out =
            run("journalctl", &[&format!("_SYSTEMD_INVOCATION_ID={inv}"), "-o", "cat", "--no-pager", "-q", "-n", "80"])
                .unwrap_or_default();
    }
    if out.trim().is_empty() {
        out = run("journalctl", &["-u", service, "-n", "60", "-o", "cat", "--no-pager", "-q"]).unwrap_or_default();
    }
    out.lines().map(str::to_owned).collect()
}

#[cfg(windows)]
fn uptime() -> f64 {
    crate::win::sys::uptime_secs()
}

#[cfg(unix)]
fn uptime() -> f64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(0.0)
}

#[cfg(unix)]
fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|s| s.trim().to_owned()).unwrap_or_default()
}

/// Kept between rounds.
#[derive(Default)]
struct Cache {
    /// Service → its last finished run, as reported.
    seen: BTreeMap<String, String>,
    /// Service → the output of its last run.
    #[cfg(unix)]
    logs: BTreeMap<String, Vec<String>>,
    /// Backup job id → (read at, what was read, size read at).
    paths: BTreeMap<i32, (i64, backups::PathRead, i64)>,
    /// The last failed read, logged once (Windows).
    #[cfg(windows)]
    error: Option<String>,
}

/// A path job's status, read at most every [`PATH_EVERY`].
fn path_job(paths: &mut BTreeMap<i32, (i64, backups::PathRead, i64)>, j: &BackupJob, now: i64) -> BackupStatus {
    let due = paths.get(&j.id).is_none_or(|(at, _, _)| now - at >= PATH_EVERY);
    if due {
        let prev = paths.get(&j.id).cloned();
        let quick = backups::read_path(Path::new(&j.target), false);
        let size_due = match &prev {
            Some((_, old, size_at)) => old.newest != quick.newest || now - size_at >= SIZE_EVERY,
            None => true,
        };
        let (read, size_at) = if size_due && quick.is_dir {
            (backups::read_path(Path::new(&j.target), true), now)
        } else {
            let mut q = quick;
            if q.is_dir {
                q.size = prev.as_ref().and_then(|p| p.1.size);
            }
            (q, prev.map_or(now, |p| p.2))
        };
        paths.insert(j.id, (now, read, size_at));
    }
    backups::path_status(j.id, &paths[&j.id].1)
}

/// One round on Windows: the listed tasks and `\Nightgrid\` in one
/// PowerShell read, paths in Rust.
#[cfg(windows)]
fn round(shared: &Shared, cache: &mut Cache, first: bool) -> (JobsLatest, Vec<NewEvent>) {
    use crate::wintasks;
    let list = shared.jobs.lock().unwrap_or_else(|e| e.into_inner()).list.clone();
    let now = now_secs();
    let up = uptime();
    let mut events = Vec::new();
    let tasks: Vec<String> = list.iter().filter(|j| j.source == BackupSource::Unit).map(|j| j.target.clone()).collect();
    // Tasks an administrator registered can be read only by an
    // administrator: through the health helper (LocalSystem, write-
    // restricted) when it is allowed, else as the node, which sees only
    // the tasks it may read.
    let req = crate::health::watch::ReadRequest { read: "tasks".into(), target: Some(tasks.join("\n")) };
    let read = match crate::health::watch::call(&shared.health_socket, &req) {
        Ok(o) => Ok(o),
        Err(e) => {
            crate::debug!("jobs: health helper: {e}; reading as the node");
            crate::health::watch::powershell(&wintasks::script(&tasks), 60)
        }
    }
    .and_then(|o| wintasks::parse(&o));
    let doc = match read {
        Ok(d) => {
            cache.error = None;
            d
        }
        Err(e) => {
            if cache.error.as_deref() != Some(e.as_str()) {
                crate::warn!("jobs: Task Scheduler read failed: {e}");
                cache.error = Some(e);
            }
            // Keep what was read last rather than flap to "not found".
            let mut l = shared.jobs.lock().unwrap_or_else(|e| e.into_inner()).latest.clone();
            l.access = "ok".into();
            return (l, events);
        }
    };

    // New finished runs, by task path.
    let mut finished: Vec<String> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    for t in doc.jobs.iter().chain(&doc.runs) {
        paths.push(t.path.clone());
        let Some(key) = wintasks::run_key(t) else { continue };
        if cache.seen.get(&t.path) != Some(&key) {
            if cache.seen.contains_key(&t.path) || !first {
                finished.push(t.path.clone());
            }
            cache.seen.insert(t.path.clone(), key);
        }
    }
    cache.seen.retain(|k, _| paths.contains(k));

    let mut statuses = Vec::new();
    let mut reported: Vec<String> = Vec::new();
    for j in &list {
        let mut st = match j.source {
            BackupSource::Unit => {
                let t = doc.jobs.iter().find(|t| t.path.eq_ignore_ascii_case(&j.target));
                let st = wintasks::task_status(j.id, t);
                if let Some(t) = t.filter(|t| finished.contains(&t.path)) {
                    if st.result != BackupResult::Running {
                        events.push(backup_event(j, &st, st.last_run.map_or(now, |v| v as i64)));
                        reported.push(t.path.clone());
                    }
                }
                st
            }
            BackupSource::Path => path_job(&mut cache.paths, j, now),
        };
        st.stale = backups::stale(&st, j, now as f64, up);
        statuses.push(st);
    }
    cache.paths.retain(|id, _| list.iter().any(|j| j.id == *id));

    let mut runs = Vec::new();
    for t in &doc.runs {
        let r = wintasks::task_run(t);
        if finished.contains(&t.path) && !reported.contains(&t.path) && r.result != BackupResult::Running {
            events.push(run_event(&r, r.finished.map_or(now, |v| v as i64)));
        }
        runs.push(r);
    }
    (JobsLatest { access: "ok".into(), at: now, backups: statuses, runs }, events)
}

/// One round: reads every job, returns the result and the events to record.
#[cfg(unix)]
fn round(shared: &Shared, cache: &mut Cache, first: bool) -> (JobsLatest, Vec<NewEvent>) {
    let list = shared.jobs.lock().unwrap_or_else(|e| e.into_inner()).list.clone();
    let now = now_secs();
    let up = uptime();
    let boot = now as f64 - up;
    let mut events = Vec::new();

    let timers: Vec<String> = list
        .iter()
        .filter(|j| j.source == BackupSource::Unit && j.target.ends_with(".timer"))
        .map(|j| j.target.clone())
        .collect();
    let timer_units: BTreeMap<String, String> = show(&timers, "Id,Unit")
        .into_iter()
        .filter_map(|(id, p)| {
            let u = p.get("Unit");
            (!u.is_empty() && backups::valid_unit(u)).then(|| (id, u.to_owned()))
        })
        .collect();
    let scheduled = scheduled_units();
    let mut services: Vec<String> = list
        .iter()
        .filter(|j| j.source == BackupSource::Unit)
        .map(|j| service_of(&j.target, &timer_units))
        .chain(scheduled.iter().cloned())
        .collect();
    services.sort();
    services.dedup();
    let props = show(&services, PROPS);

    // New finished runs: read their output once.
    let mut finished: Vec<String> = Vec::new();
    for s in &services {
        let Some(p) = props.get(s) else { continue };
        let Some(key) = run_key(p) else { continue };
        if cache.seen.get(s) != Some(&key) || !cache.logs.contains_key(s) {
            cache.logs.insert(s.clone(), journal(p.get("InvocationID"), s));
            if cache.seen.get(s).is_some_and(|k| *k != key) || (!cache.seen.contains_key(s) && !first) {
                finished.push(s.clone());
            }
            cache.seen.insert(s.clone(), key);
        }
    }
    cache.logs.retain(|k, _| services.contains(k));
    cache.seen.retain(|k, _| services.contains(k));

    let log_of = |s: &str| -> Vec<&str> {
        cache.logs.get(s).map(|v| v.iter().map(String::as_str).collect()).unwrap_or_default()
    };
    let mut statuses = Vec::new();
    let mut reported: Vec<String> = Vec::new();
    for j in &list {
        let mut st = match j.source {
            BackupSource::Unit => {
                let svc = service_of(&j.target, &timer_units);
                let empty = Props::default();
                let st = backups::unit_status(j.id, props.get(&svc).unwrap_or(&empty), &log_of(&svc), boot);
                if finished.contains(&svc) && st.result != BackupResult::Running {
                    events.push(backup_event(j, &st, st.last_run.map_or(now, |t| t as i64)));
                    reported.push(svc);
                }
                st
            }
            BackupSource::Path => path_job(&mut cache.paths, j, now),
        };
        st.stale = backups::stale(&st, j, now as f64, up);
        statuses.push(st);
    }
    cache.paths.retain(|id, _| list.iter().any(|j| j.id == *id));

    let mut runs = Vec::new();
    for s in &scheduled {
        let Some(p) = props.get(s) else { continue };
        let unit = s.trim_end_matches(".service");
        let r = job_run(unit, p, &log_of(s), boot);
        if finished.contains(s) && !reported.contains(s) && r.result != BackupResult::Running {
            events.push(run_event(&r, r.finished.map_or(now, |t| t as i64)));
        }
        runs.push(r);
    }
    (JobsLatest { access: "ok".into(), at: now, backups: statuses, runs }, events)
}

/// After a reboot: whether the previous boot ended with a scheduled power job.
#[cfg(unix)]
fn power_check(shared: &Shared) {
    let id = boot_id();
    if id.is_empty() {
        return;
    }
    let last = shared.store.lock().unwrap_or_else(|e| e.into_inner()).meta(META_POWER_BOOT).ok().flatten();
    if last.as_deref() == Some(id.as_str()) {
        return;
    }
    let out = run("journalctl", &["-b", "-1", "-u", "nightgrid-power-*", "-o", "json", "-q", "--no-pager", "-n", "50"])
        .unwrap_or_default();
    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    // Only the first start of a boot looks; a node installed mid-boot
    // records nothing for earlier boots.
    if last.is_some() {
        if let Some((unit, at)) = power_from_journal(&out) {
            if let Err(e) = shared.bus.record(&st, &power_event(&unit, at)) {
                crate::warn!("could not record a scheduled power job: {e}");
            }
        }
    }
    if let Err(e) = st.set_meta(META_POWER_BOOT, &id) {
        crate::warn!("could not save the boot id: {e}");
    }
}

fn watch(shared: &Shared, stop: &mpsc::Receiver<()>) {
    let interval = Duration::from_secs(shared.config.jobs.interval_secs.into());
    // Let the other watchers start first.
    if let Ok(()) | Err(RecvTimeoutError::Disconnected) = stop.recv_timeout(Duration::from_secs(5)) {
        return;
    }
    #[cfg(unix)]
    power_check(shared);
    let mut cache = Cache {
        seen: {
            let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
            st.meta(META_SEEN).ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
        },
        ..Default::default()
    };
    // With nothing saved, the first round only learns what has run already.
    let mut first = cache.seen.is_empty();
    loop {
        let before = cache.seen.clone();
        let (latest, events) = round(shared, &mut cache, first);
        first = false;
        {
            let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
            for e in &events {
                if let Err(err) = shared.bus.record(&st, e) {
                    crate::warn!("could not record a job run: {err}");
                }
            }
            if cache.seen != before {
                if let Err(e) = st.set_meta(META_SEEN, &serde_json::to_string(&cache.seen).unwrap_or_default()) {
                    crate::warn!("could not save the job runs seen: {e}");
                }
            }
        }
        let generation = {
            let mut j = shared.jobs.lock().unwrap_or_else(|e| e.into_inner());
            j.latest = latest;
            j.generation
        };
        // Sleep the interval, or less when the app sends a new list.
        let start = std::time::Instant::now();
        while start.elapsed() < interval {
            match stop.recv_timeout(Duration::from_secs(2).min(interval.saturating_sub(start.elapsed()))) {
                Err(RecvTimeoutError::Timeout) => {}
                _ => return,
            }
            if shared.jobs.lock().unwrap_or_else(|e| e.into_inner()).generation != generation {
                break;
            }
        }
    }
}

/// Handle to the jobs watcher.
pub struct JobsWatcher {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl JobsWatcher {
    pub fn start(shared: Arc<Shared>) -> std::io::Result<Self> {
        let (stop, rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new().name("jobs".into()).spawn(move || watch(&shared, &rx))?;
        Ok(Self { stop, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(s: &str) -> Props {
        Props::parse_many(s).pop().unwrap()
    }

    #[test]
    fn services_of_targets() {
        let mut t = BTreeMap::new();
        t.insert("restic.timer".to_owned(), "restic-backup.service".to_owned());
        assert_eq!(service_of("restic.timer", &t), "restic-backup.service");
        assert_eq!(service_of("borg.timer", &t), "borg.service");
        assert_eq!(service_of("x.service", &t), "x.service");
        assert_eq!(service_of("x", &t), "x.service");
    }

    #[test]
    fn finished_runs_have_keys() {
        let done = props("ActiveState=inactive\nInvocationID=abc\nExecMainExitTimestampMonotonic=5\n");
        assert_eq!(run_key(&done).as_deref(), Some("abc:5"));
        assert!(run_key(&props("ActiveState=activating\nExecMainExitTimestampMonotonic=5\n")).is_none());
        assert!(run_key(&props("ActiveState=inactive\nExecMainExitTimestampMonotonic=0\n")).is_none());
    }

    #[test]
    fn scheduled_job_runs() {
        let p = props(
            "Id=nightgrid-job-sync.service\nDescription=Nightgrid job: nightly sync\nLoadState=loaded\n\
             ActiveState=failed\nResult=exit-code\nExecMainStatus=2\nExecMainStartTimestampMonotonic=1000000\n\
             ExecMainExitTimestampMonotonic=3000000\n",
        );
        let r = job_run("nightgrid-job-sync", &p, &["copying", "rsync error: some files vanished", ""], 100.0);
        assert_eq!(r.description, "nightly sync");
        assert_eq!((r.result, r.exit_code), (BackupResult::Failed, Some(2)));
        assert_eq!((r.started, r.finished), (Some(101.0), Some(103.0)));
        assert_eq!(r.summary.as_deref(), Some("rsync error: some files vanished"));
        let e = run_event(&r, 103);
        assert_eq!(e.title, "job nightly sync failed (exit 2)");
        assert_eq!(e.level, Level::Warn);
        let ok = props(
            "Id=nightgrid-power-reboot.service\nDescription=nightgrid-power-reboot.service\nLoadState=loaded\n\
             ActiveState=inactive\nResult=success\nExecMainStatus=0\nExecMainExitTimestampMonotonic=3000000\n",
        );
        let r = job_run("nightgrid-power-reboot", &ok, &["PASSWORD=hunter2hunter2 done"], 0.0);
        assert_eq!(r.description, "reboot");
        assert_eq!(r.result, BackupResult::Ok);
        assert!(r.summary.unwrap().contains("hidden"));
    }

    #[test]
    fn backup_events() {
        let j = BackupJob {
            id: 3,
            name: "nightly".into(),
            tool: nightgrid_node_proto::BackupTool::Restic,
            source: BackupSource::Unit,
            target: "restic.timer".into(),
            stale_hours: 26,
        };
        let mut st = backups::empty(3);
        st.result = BackupResult::Ok;
        st.summary = Some("snapshot 1a2b saved".into());
        st.size = Some(5 * 1024 * 1024 * 1024);
        st.added = Some(12 * 1024 * 1024);
        let e = backup_event(&j, &st, 10);
        assert_eq!(e.title, "backup nightly finished");
        assert_eq!(e.detail, "snapshot 1a2b saved · 5.0 GiB · added 12.0 MiB");
        st.result = BackupResult::Failed;
        st.exit_code = Some(1);
        assert_eq!(backup_event(&j, &st, 10).level, Level::Warn);
    }

    #[test]
    fn power_jobs_from_the_journal() {
        let out = r#"{"UNIT":"nightgrid-power-reboot-sat.service","__REALTIME_TIMESTAMP":"1700000000000000","MESSAGE":"Starting"}
{"_SYSTEMD_UNIT":"other.service","__REALTIME_TIMESTAMP":"1700000500000000"}
not json"#;
        let (unit, at) = power_from_journal(out).unwrap();
        assert_eq!((unit.as_str(), at), ("nightgrid-power-reboot-sat", 1_700_000_000));
        assert_eq!(power_event(&unit, at).title, "scheduled reboot ran");
        assert!(power_from_journal("").is_none());
    }
}
