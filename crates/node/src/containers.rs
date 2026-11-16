//! Container watching: the list for `containers.list`, container-down
//! conditions for the alert engine, and container events for the timeline.
//!
// `ContainerWatch`, `exit_code` and `timeline_events` are vendored from
// Nightgrid (https://github.com/TheRoben-Dev/nightgrid), core/src/alerts.rs
// and core/src/timeline.rs at commit 8eedcf2, MIT License, (c) 2026 TheRoben.
// NODE: keys have no host segment (`docker:<name>`); the watch's memory of
// running containers is saved, so a restart of the node doesn't forget it.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use nightgrid_node_proto::{AlertKind, ContainerAccess, ContainerInfo, ContainersResult, EventKind, Level, Severity};

use crate::alerts::Condition;
use crate::daemon::{now_secs, Shared};
use crate::docker::{self, RawEvent};
use crate::events::NewEvent;

/// Events are written this long after they happen, so a `die` and the
/// `start` that follows it become one "restarted" entry.
pub const HOLD: i64 = 120;
const META_SEEN: &str = "containers_seen";

/// What the watcher last saw.
#[derive(Debug, Clone)]
pub struct ContainerState {
    pub result: ContainersResult,
    /// Container-down conditions from the last good read; `None` until one.
    /// Kept while the engine is unreachable, so alerts don't resolve just
    /// because the node couldn't look.
    pub conditions: Option<Vec<Condition>>,
}

impl ContainerState {
    pub fn new(access: ContainerAccess) -> Self {
        Self { result: ContainersResult { access, message: None, at: 0, containers: vec![] }, conditions: None }
    }
}

/// Exit code from the engine's status line, e.g. `Exited (137) 2 minutes ago`.
pub fn exit_code(status: &str) -> Option<i32> {
    let rest = status.strip_prefix("Exited (")?;
    rest.split(')').next()?.trim().parse().ok()
}

/// Remembers which containers were seen running, so only containers that
/// *stop* raise an alert (not ones that were already off, or that a compose
/// file keeps stopped).
#[derive(Debug, Default)]
pub struct ContainerWatch {
    pub seen_running: HashSet<String>,
}

impl ContainerWatch {
    pub fn conditions(&mut self, host: &str, containers: &[ContainerInfo]) -> Vec<Condition> {
        let names: HashSet<&str> = containers.iter().map(|c| c.name.as_str()).collect();
        self.seen_running.retain(|n| names.contains(n.as_str()));
        let mut out = Vec::new();
        for c in containers {
            let cond = |severity, detail: String| Condition {
                key: format!("docker:{}", c.name),
                kind: AlertKind::ContainerDown,
                severity,
                title: format!("{} on {host}", c.name),
                detail,
            };
            match c.state.as_str() {
                "running" => {
                    self.seen_running.insert(c.name.clone());
                    if c.health.as_deref() == Some("unhealthy") {
                        out.push(cond(Severity::Warn, "running but unhealthy".into()));
                    }
                }
                "restarting" => {
                    self.seen_running.insert(c.name.clone());
                    out.push(cond(Severity::Critical, "restarting (crash loop?)".into()));
                }
                "exited" | "dead" if self.seen_running.contains(&c.name) => {
                    // 0 = clean exit, 143 = SIGTERM from `docker stop`.
                    match exit_code(&c.status) {
                        Some(0 | 143) => {
                            self.seen_running.remove(&c.name);
                        }
                        code => out.push(cond(
                            Severity::Critical,
                            match code {
                                Some(137) => "exited with code 137 (killed, maybe out of memory)".into(),
                                Some(n) => format!("exited with code {n}"),
                                None => c.status.clone(),
                            },
                        )),
                    }
                }
                _ => {}
            }
        }
        out
    }
}

fn exit_text(code: i32) -> String {
    match code {
        137 => "exit 137 (killed, maybe out of memory)".into(),
        143 => "exit 143 (stopped)".into(),
        n => format!("exit {n}"),
    }
}

fn ev(at: i64, level: Level, title: String, detail: String) -> NewEvent {
    NewEvent::new(at, EventKind::Container, level, title, detail)
}

/// Turns raw container events into timeline entries: a `die` followed by a
/// `start` of the same container within two minutes is one "restarted"
/// entry; clean stops are info, crashes warn, OOM is critical.
pub fn timeline_events(mut raw: Vec<RawEvent>) -> Vec<NewEvent> {
    raw.sort_by_key(|e| e.at);
    let mut out: Vec<NewEvent> = Vec::new();
    let mut open_die: HashMap<String, usize> = HashMap::new();
    let mut unhealthy: HashMap<String, bool> = HashMap::new();
    for e in raw {
        let action = e.action.as_str();
        match action {
            "die" => {
                let code = e.exit_code.unwrap_or(0);
                let (level, word) = match code {
                    0 | 143 => (Level::Info, "stopped"),
                    _ => (Level::Warn, "exited"),
                };
                let detail = if code == 0 { String::new() } else { exit_text(code) };
                open_die.insert(e.name.clone(), out.len());
                out.push(ev(e.at, level, format!("{} {word}", e.name), detail));
            }
            "oom" => out.push(ev(e.at, Level::Critical, format!("{} ran out of memory", e.name), String::new())),
            "start" => {
                if let Some(i) = open_die.remove(&e.name) {
                    if e.at - out[i].at <= HOLD {
                        let was = out[i].detail.clone();
                        let crashed = out[i].level != Level::Info;
                        out[i] = ev(
                            out[i].at,
                            if crashed { Level::Warn } else { Level::Info },
                            format!("{} restarted", e.name),
                            if was.is_empty() { String::new() } else { format!("after {was}") },
                        );
                        continue;
                    }
                }
                out.push(ev(e.at, Level::Info, format!("{} started", e.name), String::new()));
            }
            "restart" => {
                // `docker restart` sends die + start + restart; the merged
                // entry already says it.
                if out.last().is_some_and(|p| p.title == format!("{} restarted", e.name) && e.at - p.at <= HOLD) {
                    continue;
                }
                out.push(ev(e.at, Level::Info, format!("{} restarted", e.name), String::new()));
            }
            a if a.starts_with("health_status") => {
                let bad = a.ends_with("unhealthy");
                let was_bad = unhealthy.insert(e.name.clone(), bad).unwrap_or(false);
                if bad && !was_bad {
                    out.push(ev(e.at, Level::Warn, format!("{} unhealthy", e.name), String::new()));
                } else if !bad && was_bad {
                    out.push(ev(e.at, Level::Ok, format!("{} healthy again", e.name), String::new()));
                }
            }
            _ => {}
        }
    }
    out
}

/// Raw events of the last few minutes; entries are written once they are
/// older than [`HOLD`], so pairs that straddle two reads still merge.
#[derive(Debug)]
pub struct EventBuffer {
    raw: Vec<RawEvent>,
    /// Entries at or before this were written.
    written: i64,
    /// Events up to this second were fetched.
    fetched: i64,
}

impl EventBuffer {
    pub fn new(now: i64) -> Self {
        Self { raw: Vec::new(), written: now - HOLD, fetched: now - 2 * HOLD }
    }

    /// The `[since, until]` window to fetch next, or `None` if nothing new.
    pub fn window(&self, now: i64) -> Option<(i64, i64)> {
        let until = now - 1;
        (until > self.fetched).then_some((self.fetched + 1, until))
    }

    /// Adds what was fetched for `window` and returns the entries to write.
    pub fn push(&mut self, window: (i64, i64), mut fetched: Vec<RawEvent>, now: i64) -> Vec<NewEvent> {
        fetched.retain(|e| e.at >= window.0 && e.at <= window.1);
        self.raw.extend(fetched);
        self.fetched = window.1;
        let upto = now - HOLD;
        let out: Vec<NewEvent> =
            timeline_events(self.raw.clone()).into_iter().filter(|e| e.at > self.written && e.at <= upto).collect();
        self.written = self.written.max(upto);
        let keep = self.written - HOLD;
        self.raw.retain(|e| e.at > keep);
        out
    }
}

/// Handle to the watcher thread.
pub struct Watcher {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    pub fn start(shared: Arc<Shared>) -> std::io::Result<Self> {
        let (stop, rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new().name("containers".into()).spawn(move || run(&shared, &rx))?;
        Ok(Self { stop, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(shared: &Shared, stop: &mpsc::Receiver<()>) {
    let cfg = &shared.config.docker;
    let interval = Duration::from_secs(cfg.interval_secs.into());
    let mut watch = ContainerWatch { seen_running: load_seen(shared) };
    let mut buffer = EventBuffer::new(now_secs());
    let mut last_access = None;
    loop {
        let now = now_secs();
        round(shared, &mut watch, &mut buffer, &mut last_access, now);
        match stop.recv_timeout(interval) {
            Err(RecvTimeoutError::Timeout) => continue,
            _ => break,
        }
    }
}

/// One read: the list, conditions, and events that are due.
pub fn round(
    shared: &Shared,
    watch: &mut ContainerWatch,
    buffer: &mut EventBuffer,
    last_access: &mut Option<ContainerAccess>,
    now: i64,
) {
    let socket = &shared.config.docker.socket;
    let list = docker::containers(socket);
    let access = list.as_ref().map_or_else(|e| e.access, |_| ContainerAccess::Ok);
    if *last_access != Some(access) {
        match &list {
            Ok(l) => crate::info!("reading {} containers through {}", l.len(), socket.display()),
            Err(e) if e.access == ContainerAccess::Absent => crate::debug!("no container engine: {e}"),
            Err(e) if e.access == ContainerAccess::Denied => {
                crate::info!("no access to {} (root can run: nightgrid-node allow docker)", socket.display())
            }
            Err(e) => crate::warn!("container engine: {e}"),
        }
        *last_access = Some(access);
    }
    let mut state = shared.containers.lock().unwrap_or_else(|e| e.into_inner());
    match list {
        Ok(list) => {
            let before = watch.seen_running.clone();
            let conds = watch.conditions(&shared.identity.hostname, &list);
            if watch.seen_running != before {
                save_seen(shared, &watch.seen_running);
            }
            state.conditions = Some(conds);
            state.result = ContainersResult { access, message: None, at: now, containers: list };
        }
        Err(e) => {
            state.result.access = e.access;
            state.result.message = Some(e.message);
            return;
        }
    }
    drop(state);
    if let Some(w) = buffer.window(now) {
        match docker::events(socket, w.0, w.1) {
            Ok(raw) => {
                let due = buffer.push(w, raw, now);
                if !due.is_empty() {
                    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
                    for e in &due {
                        if let Err(err) = st.event_add(e) {
                            crate::warn!("could not record a container event: {err}");
                        }
                    }
                }
            }
            Err(e) => crate::debug!("container events: {e}"),
        }
    }
}

fn load_seen(shared: &Shared) -> HashSet<String> {
    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    st.meta(META_SEEN).ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

fn save_seen(shared: &Shared, seen: &HashSet<String>) {
    let mut v: Vec<&String> = seen.iter().collect();
    v.sort();
    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = st.set_meta(META_SEEN, &serde_json::to_string(&v).unwrap_or_default()) {
        crate::warn!("could not save container state: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, state: &str, status: &str) -> ContainerInfo {
        ContainerInfo {
            id: String::new(),
            name: name.into(),
            image: String::new(),
            state: state.into(),
            status: status.into(),
            health: None,
            project: None,
        }
    }

    fn raw(at: i64, action: &str, name: &str, code: Option<i32>) -> RawEvent {
        RawEvent { at, action: action.into(), name: name.into(), exit_code: code }
    }

    #[test]
    fn only_containers_that_stop_alert() {
        let mut w = ContainerWatch::default();
        // Already off when first seen: nothing.
        assert!(w.conditions("server01", &[c("old", "exited", "Exited (1) 3 days ago")]).is_empty());
        w.conditions("server01", &[c("web", "running", "Up"), c("db", "running", "Up")]);
        let conds = w.conditions(
            "server01",
            &[c("web", "exited", "Exited (137) 1 minute ago"), c("db", "exited", "Exited (0) 1 minute ago")],
        );
        assert_eq!(conds.len(), 1);
        assert_eq!((conds[0].key.as_str(), conds[0].severity), ("docker:web", Severity::Critical));
        assert_eq!(conds[0].title, "web on server01");
        assert_eq!(conds[0].detail, "exited with code 137 (killed, maybe out of memory)");
        // The clean exit was forgotten; a removed container is forgotten too.
        assert_eq!(w.seen_running, HashSet::from(["web".to_owned()]));
        assert!(w.conditions("server01", &[]).is_empty());
        assert!(w.seen_running.is_empty());
    }

    #[test]
    fn restarts_merge_and_crashes_warn() {
        let e = timeline_events(vec![
            raw(100, "die", "web", Some(1)),
            raw(105, "start", "web", None),
            raw(200, "die", "db", Some(0)),
            raw(300, "oom", "big", None),
            raw(301, "health_status: unhealthy", "api", None),
            raw(400, "health_status: healthy", "api", None),
        ]);
        let t: Vec<(&str, Level)> = e.iter().map(|e| (e.title.as_str(), e.level)).collect();
        assert_eq!(
            t,
            [
                ("web restarted", Level::Warn),
                ("db stopped", Level::Info),
                ("big ran out of memory", Level::Critical),
                ("api unhealthy", Level::Warn),
                ("api healthy again", Level::Ok)
            ]
        );
        assert_eq!(e[0].detail, "after exit 1");
    }

    #[test]
    fn buffer_merges_across_reads_and_writes_each_entry_once() {
        let t0 = 10_000;
        let mut b = EventBuffer::new(t0);
        // A die near the end of one read, its start in the next.
        let w = b.window(t0 + 15).unwrap();
        assert!(b.push(w, vec![raw(t0 + 10, "die", "web", Some(1))], t0 + 15).is_empty());
        let w = b.window(t0 + 30).unwrap();
        assert_eq!(w.0, t0 + 15);
        assert!(b.push(w, vec![raw(t0 + 20, "start", "web", None)], t0 + 30).is_empty());
        // Due once it is HOLD old.
        let w = b.window(t0 + 10 + HOLD).unwrap();
        let out = b.push(w, vec![], t0 + 10 + HOLD);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "web restarted");
        let w = b.window(t0 + 400).unwrap();
        assert!(b.push(w, vec![], t0 + 400).is_empty());
        assert!(b.raw.is_empty());
        // Events outside the window (already fetched) are ignored.
        let w = b.window(t0 + 410).unwrap();
        assert!(b.push(w, vec![raw(t0 + 20, "oom", "x", None)], t0 + 410).is_empty());
        assert!(b.raw.is_empty());
    }
}
