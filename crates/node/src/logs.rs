//! Log watching (N5b): error bursts and new error patterns in the journal and
//! in running containers' logs, with the app's detector (`logwatch`), and
//! the app's saved searches that have alerts on.
//!
//! A round runs every `[logs] interval_secs` (10 minutes by default, like the
//! app) and reads what was logged since the last one: journal lines of
//! priority err and worse, and up to 500 lines per running container. The
//! first round after a start only learns which errors are usual. Nothing is
//! stored on disk; recent error lines are kept in memory for `logs.samples`,
//! leaving out lines that look like they hold a secret.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use nightgrid_node_proto::{
    AlertKind, ContainerAccess, LogSearch, LogSearchHits, LogSearches, LogSource, LogsStatus, Severity,
};

use crate::alerts::Condition;
use crate::daemon::{now_secs, Shared};
use crate::logwatch::{self, Detector, Finding};
use crate::redact::line_has_secret;

/// Where the saved searches are kept.
pub const META_SEARCHES: &str = "log_searches";
/// Journal lines read per round: error lines, and lines for saved searches.
const JOURNAL_ERRORS: u32 = 1000;
const JOURNAL_SEARCH: u32 = 5000;
/// Lines read per container per round, and containers read.
const CONTAINER_TAIL: u32 = 500;
const MAX_CONTAINERS: usize = 60;
/// Sources the detector follows; more are ignored until the node restarts.
const MAX_SOURCES: usize = 64;

/// A compiled saved search (the app's `logsearch::Matcher`).
#[derive(Debug, Clone)]
pub enum Matcher {
    All,
    Plain { needle: String, case_sensitive: bool },
    Regex(regex_lite::Regex),
}

impl Matcher {
    pub fn new(q: &LogSearch) -> Result<Self, String> {
        let t = q.text.trim();
        if t.is_empty() {
            return Ok(Self::All);
        }
        if q.regex {
            // regex-lite runs in time linear in the line, so a pattern can't
            // stall the round; its size is bounded by the 500-character text.
            let re = regex_lite::RegexBuilder::new(t)
                .case_insensitive(!q.case_sensitive)
                .build()
                .map_err(|e| format!("search {}: not a valid regular expression: {e}", q.id))?;
            return Ok(Self::Regex(re));
        }
        Ok(Self::Plain {
            needle: if q.case_sensitive { t.to_owned() } else { t.to_lowercase() },
            case_sensitive: q.case_sensitive,
        })
    }

    pub fn is_match(&self, line: &str) -> bool {
        match self {
            Self::All => true,
            Self::Plain { needle, case_sensitive: true } => line.contains(needle.as_str()),
            Self::Plain { needle, case_sensitive: false } => line.to_lowercase().contains(needle.as_str()),
            Self::Regex(re) => re.is_match(line),
        }
    }
}

/// Checks and compiles every search.
pub fn compile(s: &LogSearches) -> Result<Vec<(LogSearch, Matcher)>, String> {
    s.validate()?;
    s.searches.iter().map(|q| Ok((q.clone(), Matcher::new(q)?))).collect()
}

/// What the server and the alert engine read.
#[derive(Debug, Default)]
pub struct LogsState {
    pub status: LogsStatus,
    pub searches: LogSearches,
    pub detector: Detector,
    /// Alert conditions from recent rounds, with when each may clear.
    pub conditions: Vec<Condition>,
    held_until: BTreeMap<String, i64>,
}

/// A finding stays an active alert at least this long, so the alert engine
/// (which looks once a minute) sees it even with short rounds.
const HOLD_SECS: i64 = 150;

impl LogsState {
    pub fn load(st: &crate::store::Store) -> Self {
        let searches = st
            .meta(META_SEARCHES)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<LogSearches>(&s).ok())
            .filter(|s| compile(s).is_ok())
            .unwrap_or_default();
        Self { searches, ..Default::default() }
    }
}

/// One journal line: identifier, unit, priority, message.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalLine {
    pub ident: String,
    pub unit: String,
    pub priority: u8,
    pub message: String,
}

/// `journalctl -o json` lines with the fields we use.
pub fn parse_journal_json(out: &str) -> Vec<JournalLine> {
    out.lines()
        .filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            let s = |k: &str| v[k].as_str().unwrap_or_default().to_owned();
            // MESSAGE is an array of bytes when it isn't valid UTF-8.
            let message = match &v["MESSAGE"] {
                serde_json::Value::String(m) => m.clone(),
                serde_json::Value::Array(b) => {
                    String::from_utf8_lossy(&b.iter().filter_map(|x| x.as_u64().map(|n| n as u8)).collect::<Vec<_>>())
                        .into_owned()
                }
                _ => return None,
            };
            let ident = Some(s("SYSLOG_IDENTIFIER")).filter(|i| !i.is_empty()).unwrap_or_else(|| s("_COMM"));
            Some(JournalLine {
                ident: if ident.is_empty() { "unknown".into() } else { ident },
                unit: s("_SYSTEMD_UNIT"),
                priority: s("PRIORITY").parse().unwrap_or(6),
                message: message.chars().take(2000).collect(),
            })
        })
        .collect()
}

fn journal(since: i64, max_priority: u8, lines: u32) -> Result<Vec<JournalLine>, String> {
    let out = crate::health::watch::run_capped(
        "journalctl",
        &[
            &format!("--since=@{since}"),
            &format!("--priority=0..{max_priority}"),
            &format!("--lines={lines}"),
            "--output=json",
            "--output-fields=MESSAGE,SYSLOG_IDENTIFIER,_COMM,_SYSTEMD_UNIT,PRIORITY",
            "--no-pager",
            "--quiet",
        ],
        Duration::from_secs(60),
    )?;
    Ok(parse_journal_json(&out))
}

/// The app's `alerts::log_anomaly`, with the node's keys (no host id: the
/// app adds it when it merges the node's alerts).
pub fn anomaly_conditions(host: &str, source: &str, finding: &Finding) -> Vec<Condition> {
    let mut out = Vec::new();
    let label = source.split_once('/').map_or(source, |(_, n)| n);
    if let Some((n, usual)) = finding.burst {
        out.push(Condition {
            key: format!("logs:{source}:burst"),
            kind: AlertKind::LogBurst,
            severity: Severity::Warn,
            title: format!("{label} on {host}"),
            detail: format!("{n} error lines since the last check (usually about {:.0})", usual.max(0.0)),
        });
    }
    if let Some(first) = finding.new_patterns.first() {
        let more = finding.new_patterns.len() - 1;
        let sample = first.sample.clone().unwrap_or_else(|| first.pattern.clone());
        let sample: String = sample.chars().take(160).collect();
        out.push(Condition {
            key: format!("logs:{source}:new"),
            kind: AlertKind::LogNewError,
            severity: Severity::Warn,
            title: format!("{label} on {host}"),
            detail: if more > 0 { format!("{sample} (+{more} more new)") } else { sample },
        });
    }
    out
}

/// A saved search's alert: how many lines matched, and the newest.
pub fn search_condition(host: &str, q: &LogSearch, hits: usize, newest: &str) -> Condition {
    let newest = if line_has_secret(newest) {
        "(line hidden: it looks like it holds a secret)".to_owned()
    } else {
        newest.chars().take(160).collect()
    };
    Condition {
        key: format!("logsearch:{}", q.id),
        kind: AlertKind::LogMatch,
        severity: Severity::Warn,
        title: format!("{} on {host}", q.name),
        detail: format!("{hits} new matching line{}: {newest}", if hits == 1 { "" } else { "s" }),
    }
}

/// Everything one round read: lines per source.
#[derive(Debug, Default)]
pub struct Read {
    pub journal_ok: bool,
    pub containers_ok: bool,
    /// Error lines (priority err and worse) by identifier.
    pub journal_errors: Vec<(String, String)>,
    /// All lines saved searches may look at (empty without journal searches).
    pub journal_all: Vec<JournalLine>,
    /// Lines per running container, by name.
    pub containers: BTreeMap<String, Vec<String>>,
}

/// Works out alerts and status from what a round read. Pure, for tests.
pub fn evaluate(
    state: &mut LogsState,
    host: &str,
    anomaly: bool,
    searches: &[(LogSearch, Matcher)],
    read: &Read,
    now: i64,
) {
    let mut conds = Vec::new();
    let mut sources = Vec::new();
    if anomaly {
        let mut by_source: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if read.journal_ok {
            for (ident, msg) in &read.journal_errors {
                by_source.entry(format!("journal/{ident}")).or_default().push(msg.clone());
            }
            for k in state.detector.sources_with_prefix("journal/") {
                by_source.entry(k).or_default();
            }
        }
        if read.containers_ok {
            for (name, lines) in &read.containers {
                let errors: Vec<String> = lines.iter().filter(|l| logwatch::is_error_line(l)).cloned().collect();
                by_source.insert(format!("container/{name}"), errors);
            }
            for k in state.detector.sources_with_prefix("container/") {
                by_source.entry(k).or_default();
            }
        }
        let known = state.detector.sources_with_prefix("");
        let mut followed = known.len();
        for (source, lines) in by_source {
            if !known.contains(&source) {
                if followed >= MAX_SOURCES {
                    continue;
                }
                followed += 1;
            }
            let f = state.detector.observe(&source, &lines);
            conds.extend(anomaly_conditions(host, &source, &f));
            sources.push(LogSource {
                source,
                errors: lines.len() as u32,
                burst_usual: f.burst.map(|b| b.1),
                new_patterns: f.new_patterns.iter().map(|p| p.pattern.clone()).collect(),
            });
        }
    }
    let mut hits = Vec::new();
    for (q, m) in searches {
        let mut n = 0usize;
        let mut newest = String::new();
        let mut see = |line: &str| {
            if m.is_match(line) {
                n += 1;
                newest = line.to_owned();
            }
        };
        if q.journal && read.journal_ok {
            let unit = q.unit.as_deref().map(str::trim).filter(|u| !u.is_empty());
            for l in &read.journal_all {
                if q.priority.is_some_and(|p| l.priority > p) {
                    continue;
                }
                if unit.is_some_and(|u| l.unit != u && l.unit != format!("{u}.service")) {
                    continue;
                }
                see(&l.message);
            }
        }
        if q.containers && read.containers_ok {
            let filter = q.container.as_deref().map(str::trim).filter(|c| !c.is_empty());
            for (name, lines) in &read.containers {
                if filter.is_some_and(|f| !name.contains(f)) {
                    continue;
                }
                lines.iter().for_each(|l| see(l));
            }
        }
        if n > 0 {
            conds.push(search_condition(host, q, n, &newest));
        }
        hits.push(LogSearchHits { id: q.id.clone(), hits: n as u32, error: None });
    }
    // Keep what earlier rounds found until it has been held long enough.
    let fresh: std::collections::BTreeSet<String> = conds.iter().map(|c| c.key.clone()).collect();
    let mut kept: Vec<Condition> = std::mem::take(&mut state.conditions)
        .into_iter()
        .filter(|c| !fresh.contains(&c.key) && state.held_until.get(&c.key).is_some_and(|t| *t > now))
        .collect();
    for c in &conds {
        state.held_until.insert(c.key.clone(), now + HOLD_SECS);
    }
    kept.extend(conds);
    state.held_until.retain(|k, _| kept.iter().any(|c| &c.key == k));
    state.conditions = kept;
    state.status.at = now;
    state.status.anomaly = anomaly;
    state.status.sources = sources;
    state.status.searches = hits;
}

fn container_access(shared: &Shared) -> ContainerAccess {
    if !shared.config.docker.enabled {
        return ContainerAccess::Off;
    }
    shared.containers.lock().unwrap_or_else(|e| e.into_inner()).result.access
}

fn access_word(a: ContainerAccess) -> String {
    serde_json::to_value(a).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

/// One round: read, then evaluate.
fn round(shared: &Shared, since: i64, now: i64) {
    let rules = shared.rules.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let anomaly = rules.enabled && rules.log_anomaly;
    let searches = {
        let l = shared.logs.lock().unwrap_or_else(|e| e.into_inner());
        compile(&l.searches).unwrap_or_default()
    };
    let journal_word = crate::logins::journal_access();
    let containers = container_access(shared);
    let mut read = Read {
        journal_ok: journal_word == "ok",
        containers_ok: containers == ContainerAccess::Ok,
        ..Default::default()
    };
    if anomaly || !searches.is_empty() {
        if read.journal_ok && anomaly {
            match journal(since, 3, JOURNAL_ERRORS) {
                Ok(lines) => read.journal_errors = lines.into_iter().map(|l| (l.ident, l.message)).collect(),
                Err(e) => crate::warn!("logs: could not read the journal: {e}"),
            }
        }
        let journal_searches: Vec<&LogSearch> = searches.iter().map(|s| &s.0).filter(|q| q.journal).collect();
        if read.journal_ok && !journal_searches.is_empty() {
            let max_p = journal_searches.iter().map(|q| q.priority.unwrap_or(7)).max().unwrap_or(7);
            match journal(since, max_p, JOURNAL_SEARCH) {
                Ok(lines) => read.journal_all = lines,
                Err(e) => crate::warn!("logs: could not read the journal: {e}"),
            }
        }
        let want_containers = anomaly || searches.iter().any(|s| s.0.containers);
        if read.containers_ok && want_containers {
            read.containers = read_containers(&shared.config.docker.socket, since);
        }
    }
    let host = shared.identity.hostname.clone();
    let mut l = shared.logs.lock().unwrap_or_else(|e| e.into_inner());
    evaluate(&mut l, &host, anomaly, &searches, &read, now);
    l.status.journal = journal_word.to_owned();
    l.status.containers = access_word(containers);
}

fn read_containers(socket: &Path, since: i64) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let Ok(list) = crate::docker::containers(socket) else { return out };
    for c in list.iter().filter(|c| c.state == "running").take(MAX_CONTAINERS) {
        match crate::docker::logs(socket, &c.id, since, CONTAINER_TAIL) {
            Ok(lines) => {
                out.insert(c.name.clone(), lines);
            }
            Err(e) => crate::debug!("logs of {}: {e}", c.name),
        }
    }
    out
}

/// Handle to the log thread.
pub struct LogWatcher {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LogWatcher {
    pub fn start(shared: Arc<Shared>) -> std::io::Result<Self> {
        let (stop, rx) = mpsc::channel::<()>();
        let every = i64::from(shared.config.logs.interval_secs);
        let thread = std::thread::Builder::new().name("logs".into()).spawn(move || {
            let started = now_secs();
            shared.logs.lock().unwrap_or_else(|e| e.into_inner()).status.every_secs = every as u32;
            // The first round a minute after start, over the minute before it.
            let mut last = started - every;
            let mut next = started + 60.min(every);
            loop {
                let wait = (next - now_secs()).max(0) as u64;
                match rx.recv_timeout(Duration::from_secs(wait.min(5))) {
                    Err(RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
                let now = now_secs();
                if now < next {
                    continue;
                }
                round(&shared, last.max(now - 2 * every), now);
                last = now;
                next = now + every;
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

    fn q(id: &str, text: &str) -> LogSearch {
        LogSearch { id: id.into(), name: format!("search {id}"), text: text.into(), ..Default::default() }
    }

    #[test]
    fn matchers() {
        assert!(Matcher::new(&q("a", "")).unwrap().is_match("anything"));
        assert!(Matcher::new(&q("a", "Disk Full")).unwrap().is_match("ERROR: disk full on /srv"));
        let re = LogSearch { regex: true, ..q("r", r"timeout after \d+ ms") };
        assert!(Matcher::new(&re).unwrap().is_match("Timeout after 300 ms"));
        let bad = LogSearch { regex: true, ..q("b", "(") };
        assert!(Matcher::new(&bad).unwrap_err().contains("not a valid regular expression"));
        let set = LogSearches { searches: vec![q("a", "x"), q("a", "y")] };
        assert!(compile(&set).unwrap_err().contains("twice"));
    }

    #[test]
    fn journal_json() {
        let out = concat!(
            r#"{"MESSAGE":"Failed to start x","SYSLOG_IDENTIFIER":"systemd","_SYSTEMD_UNIT":"init.scope","PRIORITY":"3"}"#,
            "\n",
            r#"{"MESSAGE":[104,105],"_COMM":"app","PRIORITY":"6"}"#,
            "\nnot json\n"
        );
        let l = parse_journal_json(out);
        assert_eq!(l.len(), 2);
        assert_eq!((l[0].ident.as_str(), l[0].priority, l[0].unit.as_str()), ("systemd", 3, "init.scope"));
        assert_eq!((l[1].ident.as_str(), l[1].message.as_str()), ("app", "hi"));
    }

    #[test]
    fn rounds_learn_then_report_and_searches_match() {
        let mut st = LogsState::default();
        let searches = compile(&LogSearches {
            searches: vec![
                LogSearch { containers: false, unit: Some("nginx".into()), ..q("s1", "upstream timed out") },
                LogSearch { journal: false, container: Some("db".into()), ..q("s2", "password=") },
            ],
        })
        .unwrap();
        let mut read = Read { journal_ok: true, containers_ok: true, ..Default::default() };
        read.containers.insert("web".into(), vec!["GET / 200".into(), "ERROR connection refused to db:5432".into()]);
        evaluate(&mut st, "srv", true, &searches, &read, 100);
        // The first round only learns.
        assert!(st.conditions.is_empty(), "{:?}", st.conditions);
        assert_eq!(st.status.sources[0].source, "container/web");

        read.containers.insert("web".into(), vec!["ERROR disk quota exceeded for user 7".into()]);
        read.containers.insert("db".into(), vec!["login ok password=hunter2".into()]);
        read.journal_errors = vec![("sshd".into(), "error: kex failed".into())];
        read.journal_all = vec![JournalLine {
            ident: "nginx".into(),
            unit: "nginx.service".into(),
            priority: 3,
            message: "upstream timed out (110) while reading".into(),
        }];
        evaluate(&mut st, "srv", true, &searches, &read, 700);
        let keys: Vec<&str> = st.conditions.iter().map(|c| c.key.as_str()).collect();
        // sshd is new to the detector, so its first round only learns.
        assert_eq!(keys, ["logs:container/web:new", "logsearch:s1", "logsearch:s2"], "{:?}", st.conditions);
        assert_eq!(st.conditions[0].title, "web on srv");
        assert!(st.conditions[0].detail.contains("disk quota exceeded"));
        assert!(st.conditions[2].detail.contains("line hidden"), "{}", st.conditions[2].detail);
        assert_eq!(st.status.searches[0].hits, 1);

        // Nothing new: alerts are held a little, then clear; with anomaly
        // off, sources aren't followed.
        evaluate(&mut st, "srv", false, &[], &Read::default(), 760);
        assert_eq!(st.conditions.len(), 3);
        assert!(st.status.sources.is_empty());
        evaluate(&mut st, "srv", false, &[], &Read::default(), 700 + HOLD_SECS);
        assert!(st.conditions.is_empty());
    }

    #[test]
    fn container_logs_demux() {
        let mut b = vec![1u8, 0, 0, 0, 0, 0, 0, 6];
        b.extend_from_slice(b"hello\n");
        b.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0, 12]);
        b.extend_from_slice(b"ERROR boom\r\n");
        assert_eq!(crate::docker::demux_logs(&b), ["hello", "ERROR boom"]);
        assert_eq!(crate::docker::demux_logs(b"tty line\nsecond\n"), ["tty line", "second"]);
    }
}
