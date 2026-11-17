//! The health reads: the root side ([`serve`], one request per process,
//! started by `nightgrid-node-health.socket`) and the service side
//! ([`HealthWatcher`], which asks on a schedule and keeps the results).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nightgrid_node_proto::{AlertKind, EventKind, Level, Severity};
use serde::{Deserialize, Serialize};

use super::packages::UpdatesReport;
use super::pools::{kind_word, PoolsReport};
use super::smart::{DisksReport, Health};
use super::ups::UpsReading;
use crate::alerts::Condition;
use crate::config::Config;
use crate::daemon::{now_secs, Shared};
use crate::events::NewEvent;

/// Where the health helper's socket unit listens.
pub const SOCKET: &str = "/run/nightgrid-node-health.sock";
const MAX_OUTPUT: u64 = 4 << 20;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReadRequest {
    /// `smart`, `pools`, `updates`, `ups` or `files`.
    pub read: String,
    /// The UPS, for `ups`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReadReply {
    pub ok: bool,
    pub output: String,
    pub error: String,
}

/// Runs `prog args` with a time limit; stdout (capped), or why not.
pub(crate) fn run_capped(prog: &str, args: &[&str], limit: Duration) -> Result<String, String> {
    let mut child = Command::new(prog)
        .args(args)
        .env("LANG", "C")
        .env("SYSTEMD_PAGER", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{prog}: {e}"))?;
    let mut out = child.stdout.take().ok_or("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut out).take(MAX_OUTPUT).read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{prog} took longer than {} s", limit.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    }
    let buf = reader.join().unwrap_or_default();
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Carries out one read (root side). Only fixed scripts and `upsc` on a UPS
/// root listed.
pub fn handle(config: &Config, req: &ReadRequest) -> ReadReply {
    if !config.health.enabled {
        return ReadReply { error: "[health] is off".into(), ..Default::default() };
    }
    let script = |s: String, secs: u64| run_capped("/bin/sh", &["-c", &s], Duration::from_secs(secs));
    let r = match req.read.as_str() {
        "smart" => script(super::smart::scan_command(), 240),
        "pools" => script(super::pools::scan_command(), 60),
        "updates" => script(super::packages::check_command(), 120),
        "ups" => {
            let t = req.target.as_deref().unwrap_or("");
            if !super::ups::valid_target(t) || !config.health.ups.iter().any(|u| u == t) {
                Err(format!("{t:?} is not in [health] ups"))
            } else {
                match run_capped("upsc", &[t], Duration::from_secs(15)) {
                    Err(e) if e.contains("No such file") => Ok("NIGHTGRID: no upsc\n".into()),
                    r => r,
                }
            }
        }
        // Snapshots of the files root's [files] config lists (metadata and a
        // short hash only, never contents), for the ones the service can't read.
        "files" => crate::files::helper_snapshot(&config.files),
        other => Err(format!("unknown read {other:?}")),
    };
    match r {
        Ok(output) => ReadReply { ok: true, output, error: String::new() },
        Err(error) => ReadReply { ok: false, output: String::new(), error },
    }
}

/// `nightgrid-node health-helper`: serves the one connection on stdin/stdout.
pub fn serve(config_path: &Path) -> Result<(), String> {
    // SAFETY: systemd hands the accepted connection over as fd 0.
    let stream = unsafe { UnixStream::from_raw_fd(0) };
    let uid = crate::helper::peer_uid(stream.as_raw_fd()).ok_or("stdin is not a unix socket")?;
    let reply = if uid == 0 || Some(uid) == crate::helper::service_uid() {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let mut line = String::new();
        let read = BufReader::new((&stream).take(4096)).read_line(&mut line);
        match (read, serde_json::from_str::<ReadRequest>(&line)) {
            (Ok(_), Ok(req)) => match Config::load(config_path) {
                Ok(config) => handle(&config, &req),
                Err(e) => ReadReply { error: format!("config: {e}"), ..Default::default() },
            },
            _ => ReadReply { error: "bad request".into(), ..Default::default() },
        }
    } else {
        crate::warn!("health helper: refused a connection from uid {uid}");
        ReadReply { error: "only the node's service may use the health helper".into(), ..Default::default() }
    };
    let mut out = serde_json::to_vec(&reply).unwrap_or_default();
    out.push(b'\n');
    (&stream).write_all(&out).map_err(|e| e.to_string())
}

/// Asks the health helper (service side).
pub fn call(socket: &Path, req: &ReadRequest) -> Result<String, String> {
    let mut s = UnixStream::connect(socket).map_err(|e| format!("health helper: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(300))).map_err(|e| e.to_string())?;
    let mut line = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    line.push(b'\n');
    s.write_all(&line).map_err(|e| format!("health helper: {e}"))?;
    let mut answer = String::new();
    BufReader::new(s.take(MAX_OUTPUT * 2)).read_line(&mut answer).map_err(|e| format!("health helper: {e}"))?;
    let r: ReadReply = serde_json::from_str(&answer).map_err(|_| "health helper: no answer".to_string())?;
    if r.ok {
        Ok(r.output)
    } else {
        Err(r.error)
    }
}

/// One kind of reading: when, and the report or why there is none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reading<T> {
    pub at: i64,
    // Missing Options read as None without `default` (which would make serde
    // ask for `T: Default`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpsStatus {
    pub target: String,
    pub at: i64,
    pub reading: UpsReading,
}

/// `health.latest`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResult {
    /// `ok` when the health helper is installed, else `denied` (or `off`).
    pub access: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smart: Option<Reading<DisksReport>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pools: Option<Reading<PoolsReport>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updates: Option<Reading<UpdatesReport>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ups: Vec<UpsStatus>,
}

/// What the watcher keeps for the server and the alert engine.
#[derive(Debug, Default)]
pub struct HealthState {
    pub result: HealthResult,
    /// Alert conditions from the last good reads, by source.
    pub smart: Vec<Condition>,
    pub pools: Vec<Condition>,
    pub ups: Vec<Condition>,
}

pub fn smart_conditions(host: &str, r: &DisksReport) -> Vec<Condition> {
    r.disks
        .iter()
        .filter(|d| matches!(d.health, Health::Warn | Health::Bad))
        .map(|d| Condition {
            key: format!("smart:{}", d.name),
            kind: AlertKind::SmartWarning,
            severity: if d.health == Health::Bad { Severity::Critical } else { Severity::Warn },
            title: host.to_owned(),
            detail: format!(
                "{} {}: {}",
                d.name,
                d.model.as_deref().unwrap_or(""),
                d.issues.first().map_or("health check failed", String::as_str)
            )
            .replace("  ", " "),
        })
        .collect()
}

pub fn pool_conditions(host: &str, r: &PoolsReport) -> Vec<Condition> {
    r.pools
        .iter()
        .filter(|p| matches!(p.health, Health::Warn | Health::Bad))
        .map(|p| Condition {
            key: format!("pool:{:?}:{}", p.kind, p.name),
            kind: AlertKind::PoolDegraded,
            severity: if p.health == Health::Bad { Severity::Critical } else { Severity::Warn },
            title: host.to_owned(),
            detail: format!(
                "{} {}: {}",
                kind_word(p.kind),
                p.name,
                p.issues.first().map_or("needs attention", String::as_str)
            ),
        })
        .collect()
}

/// Runtime below this, on battery, is critical.
const UPS_LOW_SECS: u32 = 300;

pub fn ups_conditions(host: &str, target: &str, r: &UpsReading) -> Vec<Condition> {
    let mut out = Vec::new();
    if r.error.is_some() {
        return out;
    }
    let words = super::ups::words;
    let base = |suffix: &str, severity, detail: String| Condition {
        key: format!("ups:{target}:{suffix}"),
        kind: AlertKind::UpsOnBattery,
        severity,
        title: host.to_owned(),
        detail,
    };
    let left = match (r.charge, r.runtime) {
        (Some(c), Some(t)) => format!(" ({c:.0}%, about {} left)", words(t)),
        (Some(c), None) => format!(" ({c:.0}%)"),
        (None, Some(t)) => format!(" (about {} left)", words(t)),
        _ => String::new(),
    };
    let low = r.low_battery || r.runtime.is_some_and(|t| t < UPS_LOW_SECS);
    if r.on_battery && low {
        out.push(base("power", Severity::Critical, format!("{target}: battery low{left}")));
    } else if r.on_battery {
        out.push(base("power", Severity::Warn, format!("{target}: running on battery{left}")));
    }
    if r.replace_battery {
        out.push(base("rb", Severity::Warn, format!("{target}: the UPS says its battery needs replacing")));
    }
    out
}

/// Events for news in an updates report compared with the previous one.
pub fn update_events(old: Option<&UpdatesReport>, new: &UpdatesReport, now: i64) -> Vec<NewEvent> {
    let mut out = Vec::new();
    let had = old.map_or(0, |o| o.updates.len());
    if !new.updates.is_empty() && had == 0 {
        let names: Vec<&str> = new.updates.iter().take(5).map(|u| u.name.as_str()).collect();
        let more = new.updates.len().saturating_sub(names.len());
        out.push(NewEvent::new(
            now,
            EventKind::Other,
            Level::Info,
            format!("{} package update{} available", new.updates.len(), if new.updates.len() == 1 { "" } else { "s" }),
            format!("{}{}", names.join(", "), if more > 0 { format!(" and {more} more") } else { String::new() }),
        ));
    }
    if new.reboot_required && !old.is_some_and(|o| o.reboot_required) {
        out.push(NewEvent::new(now, EventKind::Other, Level::Info, "reboot required", new.reboot_reasons.join("; ")));
    }
    out
}

struct Due {
    every: i64,
    next: i64,
}

impl Due {
    fn new(every: i64, first: i64) -> Self {
        Self { every, next: first }
    }
    fn take(&mut self, now: i64) -> bool {
        if now >= self.next {
            self.next = now + self.every;
            true
        } else {
            false
        }
    }
}

fn read<T>(shared: &Shared, what: &str, parse: impl Fn(&str) -> Result<T, String>, now: i64) -> Reading<T> {
    let r = call(&shared.health_socket, &ReadRequest { read: what.into(), target: None }).and_then(|o| parse(&o));
    match r {
        Ok(report) => Reading { at: now, report: Some(report), error: None },
        Err(e) => {
            crate::debug!("health read {what}: {e}");
            Reading { at: now, report: None, error: Some(e) }
        }
    }
}

fn tick(shared: &Shared, due: &mut [Due; 4]) {
    let now = now_secs();
    let installed = crate::helper::installed(&shared.health_socket);
    let host = shared.identity.hostname.clone();
    shared.health.lock().unwrap_or_else(|e| e.into_inner()).result.access =
        if installed { "ok" } else { "denied" }.into();
    if !installed {
        let mut h = shared.health.lock().unwrap_or_else(|e| e.into_inner());
        h.smart.clear();
        h.pools.clear();
        h.ups.clear();
        return;
    }
    if due[0].take(now) {
        let r = read(shared, "pools", |o| Ok(super::pools::parse_scan(o)), now);
        let mut h = shared.health.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rep) = &r.report {
            h.pools = pool_conditions(&host, rep);
        }
        h.result.pools = Some(r);
    }
    if due[1].take(now) {
        let r = read(shared, "smart", super::smart::parse_scan, now);
        let mut h = shared.health.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rep) = &r.report {
            h.smart = smart_conditions(&host, rep);
        }
        h.result.smart = Some(r);
    }
    if due[2].take(now) {
        let r = read(shared, "updates", |o| Ok(super::packages::parse_check(o)), now);
        let events = {
            let mut h = shared.health.lock().unwrap_or_else(|e| e.into_inner());
            let old = h.result.updates.as_ref().and_then(|x| x.report.clone());
            let ev = match &r.report {
                Some(new) => update_events(old.as_ref(), new, now),
                None => Vec::new(),
            };
            if r.report.is_some() || old.is_none() {
                h.result.updates = Some(r);
            }
            ev
        };
        if !events.is_empty() {
            let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
            for e in events {
                let _ = shared.bus.record(&st, &e);
            }
        }
    }
    if !shared.config.health.ups.is_empty() && due[3].take(now) {
        let mut statuses = Vec::new();
        let mut conds = Vec::new();
        for t in &shared.config.health.ups {
            let req = ReadRequest { read: "ups".into(), target: Some(t.clone()) };
            let reading = match call(&shared.health_socket, &req) {
                Ok(o) => super::ups::parse(&o),
                Err(e) => UpsReading { error: Some(e), ..Default::default() },
            };
            conds.extend(ups_conditions(&host, t, &reading));
            statuses.push(UpsStatus { target: t.clone(), at: now, reading });
        }
        let target = shared.config.ups.target(&shared.config.health);
        let followed = statuses.iter().find(|s| Some(s.target.as_str()) == target).map(|s| s.reading.clone());
        {
            let mut h = shared.health.lock().unwrap_or_else(|e| e.into_inner());
            h.ups = conds;
            h.result.ups = statuses;
        }
        guard(shared, followed.as_ref(), now, &mut due[3]);
    } else if shared.ups_guard.lock().unwrap_or_else(|e| e.into_inner()).active() {
        // Between reads: the countdown still ends on time.
        guard(shared, None, now, &mut due[3]);
    }
}

/// Steps the UPS shutdown guard; reads the UPS every tick while it is on battery.
fn guard(shared: &Shared, r: Option<&UpsReading>, now: i64, due: &mut Due) {
    let steps = {
        let mut g = shared.ups_guard.lock().unwrap_or_else(|e| e.into_inner());
        let steps = g.step(&shared.config, r, now);
        if g.active() {
            due.next = due.next.min(now + 10);
        }
        steps
    };
    crate::upsguard::apply(shared, steps);
}

/// Handle to the health thread.
pub struct HealthWatcher {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HealthWatcher {
    pub fn start(shared: Arc<Shared>) -> std::io::Result<Self> {
        let (stop, rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new().name("health".into()).spawn(move || {
            let now = now_secs();
            // Pools at once, SMART and updates a little later so a start
            // doesn't spin every disk up together with everything else.
            let mut due =
                [Due::new(300, now), Due::new(3600, now + 60), Due::new(6 * 3600, now + 30), Due::new(30, now)];
            loop {
                tick(&shared, &mut due);
                match rx.recv_timeout(Duration::from_secs(10)) {
                    Err(RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
        })?;
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

    #[test]
    fn ups_alerts() {
        let r = super::super::ups::parse("ups.status: OB DISCHRG\nbattery.charge: 80\nbattery.runtime: 1200\n");
        let c = ups_conditions("srv", "ups@localhost", &r);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].severity, c[0].key.as_str()), (Severity::Warn, "ups:ups@localhost:power"));
        assert!(c[0].detail.contains("running on battery (80%"), "{}", c[0].detail);
        let r = super::super::ups::parse("ups.status: OB LB\nbattery.runtime: 120\n");
        assert_eq!(ups_conditions("srv", "u", &r)[0].severity, Severity::Critical);
        let r = super::super::ups::parse("ups.status: OL CHRG RB\n");
        let c = ups_conditions("srv", "u", &r);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].key, "ups:u:rb");
        assert!(ups_conditions("srv", "u", &super::super::ups::parse("Error: Unknown UPS\n")).is_empty());
    }

    #[test]
    fn smart_and_pool_alerts() {
        let worn = include_str!("fixtures/smart_hdd_worn.json");
        let r = super::super::smart::parse_scan(&format!("{}\n{worn}\n", super::super::SECTION)).unwrap();
        let c = smart_conditions("srv", &r);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].severity, Severity::Warn);
        assert!(c[0].key.starts_with("smart:/dev/"));
        assert!(c[0].detail.contains("reallocated"), "{}", c[0].detail);
        let md = format!(
            "{s}\nPersonalities : [raid1]\nmd0 : active raid1 sdb1[1] sda1[0](F)\n      1046528 blocks super 1.2 [2/1] [_U]\n\nunused devices: <none>\n{s}\n{s}\n",
            s = super::super::SECTION
        );
        let p = super::super::pools::parse_scan(&md);
        let c = pool_conditions("srv", &p);
        assert_eq!(c.len(), 1, "{p:?}");
        assert_eq!(c[0].key, "pool:Mdraid:md0");
    }

    #[test]
    fn update_news() {
        let parse = |s: &str| super::super::packages::parse_check(s);
        let none = parse(&format!("@mgr apt\n@age 60\n{}\n", super::super::SECTION));
        let some = parse(&format!(
            "@mgr apt\n@age 60\nbash/jammy-updates 5.2 amd64 [upgradable from: 5.1]\n{}\n@flag\nlinux-base\n",
            super::super::SECTION
        ));
        let ev = update_events(Some(&none), &some, 5);
        let t: Vec<&str> = ev.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(t, ["1 package update available", "reboot required"]);
        assert!(update_events(Some(&some), &some, 6).is_empty());
    }

    #[test]
    fn reads_are_checked_on_the_root_side() {
        let mut c = Config::default();
        let r = handle(&c, &ReadRequest { read: "ups".into(), target: Some("ups@localhost".into()) });
        assert!(!r.ok && r.error.contains("[health] ups"));
        let r = handle(&c, &ReadRequest { read: "rm".into(), target: None });
        assert!(!r.ok);
        c.health.ups = vec!["-x".into()];
        assert!(!handle(&c, &ReadRequest { read: "ups".into(), target: Some("-x".into()) }).ok);
        let r = run_capped("/bin/sh", &["-c", "sleep 5"], Duration::from_millis(200));
        assert!(r.unwrap_err().contains("longer"));
        assert_eq!(run_capped("/bin/sh", &["-c", "echo hi"], Duration::from_secs(5)).unwrap(), "hi\n");
    }

    #[test]
    fn tick_reads_through_the_socket_and_raises_alerts() {
        use std::io::{BufRead, BufReader, Write};
        let d = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        c.health.ups = vec!["ups@localhost".into()];
        let mut shared = crate::daemon::Shared::new(c, crate::store::Store::open_in_memory().unwrap());
        let sock = d.path().join("health.sock");
        std::sync::Arc::get_mut(&mut shared).unwrap().health_socket = sock.clone();
        let mut due = [Due::new(300, 0), Due::new(3600, 0), Due::new(6 * 3600, 0), Due::new(30, 0)];
        tick(&shared, &mut due);
        assert_eq!(shared.health.lock().unwrap().result.access, "denied");
        assert!(shared.health.lock().unwrap().result.pools.is_none());

        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let s = super::super::SECTION;
        let t = std::thread::spawn(move || {
            for _ in 0..4 {
                let (c, _) = l.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&c).read_line(&mut line).unwrap();
                let req: ReadRequest = serde_json::from_str(&line).unwrap();
                let output = match req.read.as_str() {
                    "pools" => format!(
                        "{s}\nPersonalities : [raid1]\nmd0 : active raid1 sdb1[1] sda1[0](F)\n      1046528 blocks super 1.2 [2/1] [_U]\n\n{s}\n{s}\n"
                    ),
                    "smart" => "@nosmartctl\n".into(),
                    "updates" => format!("@mgr apt\n@age 100\nbash/stable 5.2-2 amd64 [upgradable from: 5.2-1]\n{s}\n"),
                    "ups" => "ups.status: OB DISCHRG\nbattery.charge: 90\nbattery.runtime: 1800\n".into(),
                    _ => unreachable!(),
                };
                let reply = ReadReply { ok: true, output, error: String::new() };
                (&c).write_all(format!("{}\n", serde_json::to_string(&reply).unwrap()).as_bytes()).unwrap();
            }
        });
        tick(&shared, &mut due);
        t.join().unwrap();
        let h = shared.health.lock().unwrap();
        assert_eq!(h.result.access, "ok");
        assert_eq!(h.pools.len(), 1, "{:?}", h.result.pools);
        assert_eq!(h.ups.len(), 1);
        assert!(h.smart.is_empty());
        let updates = h.result.updates.as_ref().and_then(|u| u.report.as_ref()).unwrap();
        assert_eq!(updates.updates.len(), 1);
        drop(h);
        let r = serde_json::to_value(&shared.health.lock().unwrap().result).unwrap();
        assert_eq!(r["ups"][0]["target"], "ups@localhost");
        assert!(r["pools"]["at"].as_i64().unwrap() > 0);
        let events = shared.store.lock().unwrap().events_after(0, 10).unwrap();
        assert!(events.iter().any(|e| e.title.contains("1 package update")), "{events:?}");
    }
}
