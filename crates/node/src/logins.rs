//! SSH logins and failed logins, read from the journal.
//!
//! The service can only read the system journal after root runs
//! `nightgrid-node allow journal` (a drop-in adding the journal's group).
//! It follows `journalctl -o json` for sshd's messages, one child process
//! for the life of the service, resuming from a saved cursor so logins while
//! the node was stopped are still recorded. Successful logins are one event
//! each; failed ones are summed per minute so a scanned host doesn't flood
//! the timeline.

#[cfg(unix)]
use crate::compat::fs::MetadataExt;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
#[cfg(unix)]
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nightgrid_node_proto::{AlertKind, EventKind, Level, Severity};

use crate::alerts::Condition;
use crate::daemon::Shared;
use crate::events::NewEvent;

const META_CURSOR: &str = "logins_cursor";
/// Failed logins are summed over this many seconds.
const WINDOW: i64 = 60;

/// Failed logins counted for the burst alert, in seconds.
pub const BURST_WINDOW: i64 = 300;
/// Failures remembered for the burst alert, at most.
const BURST_KEEP: usize = 20_000;

/// Recent failed logins (time, address), for the login-burst alert and
/// (N6c) auto-block.
#[derive(Debug, Default)]
pub struct Recent {
    fails: VecDeque<(i64, String)>,
    /// Addresses with a successful login, and when (never auto-blocked for a day).
    good: BTreeMap<String, i64>,
}

impl Recent {
    /// A successful login from `from`.
    pub fn accepted(&mut self, at: i64, from: &str) {
        if self.good.len() >= 1000 {
            let cutoff = at - 86_400;
            self.good.retain(|_, t| *t > cutoff);
            if self.good.len() >= 1000 {
                return;
            }
        }
        self.good.insert(from.to_owned(), at);
    }

    /// Whether `addr` logged in successfully in the last day.
    pub fn trusted(&self, addr: &str, now: i64) -> bool {
        self.good.get(addr).is_some_and(|t| *t > now - 86_400)
    }

    /// Failures from `addr` in the window.
    pub fn count_from(&mut self, addr: &str, now: i64) -> u32 {
        self.prune(now);
        self.fails.iter().filter(|(_, a)| a == addr).count() as u32
    }

    pub fn add(&mut self, at: i64, from: &str) {
        if self.fails.len() >= BURST_KEEP {
            self.fails.pop_front();
        }
        self.fails.push_back((at, from.chars().take(64).collect()));
    }

    fn prune(&mut self, now: i64) {
        while self.fails.front().is_some_and(|(t, _)| *t <= now - BURST_WINDOW) {
            self.fails.pop_front();
        }
    }

    /// Failures per address in the window, most first.
    pub fn by_address(&mut self, now: i64) -> Vec<(String, u32)> {
        self.prune(now);
        let mut m: BTreeMap<&str, u32> = BTreeMap::new();
        for (_, a) in &self.fails {
            *m.entry(a.as_str()).or_default() += 1;
        }
        let mut v: Vec<(String, u32)> = m.into_iter().map(|(a, n)| (a.to_owned(), n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }

    /// The login-burst alert while at least `burst` failures fall in the
    /// last five minutes; critical at ten times that.
    pub fn condition(&mut self, host: &str, burst: u32, now: i64) -> Option<Condition> {
        let by = self.by_address(now);
        let total: u32 = by.iter().map(|(_, n)| n).sum();
        if total < burst {
            return None;
        }
        let top: Vec<String> = by.iter().take(3).map(|(a, n)| format!("{a} ×{n}")).collect();
        let more = if by.len() > 3 { format!(" and {} more", by.len() - 3) } else { String::new() };
        Some(Condition {
            key: "logins:burst".into(),
            kind: AlertKind::LoginBurst,
            severity: if total >= burst.saturating_mul(10) { Severity::Critical } else { Severity::Warn },
            title: format!("failed SSH logins on {host}"),
            detail: format!("{total} in the last 5 minutes from {}{more}", top.join(", ")),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Accepted { user: String, from: String, method: String },
    Failed { user: String, from: String, invalid: bool },
}

/// Parses one sshd message.
pub fn parse(msg: &str) -> Option<Line> {
    // "Accepted publickey for alice from 10.0.0.5 port 50522 ssh2: ED25519 SHA256:…"
    if let Some(rest) = msg.strip_prefix("Accepted ") {
        let (method, rest) = rest.split_once(" for ")?;
        let (user, rest) = rest.split_once(" from ")?;
        let from = rest.split_whitespace().next()?;
        return Some(Line::Accepted { user: user.into(), from: from.into(), method: method.into() });
    }
    // "Failed password for invalid user admin from 1.2.3.4 port 4242 ssh2"
    // "Failed publickey for root from 1.2.3.4 port 4242 ssh2"
    if let Some(rest) = msg.strip_prefix("Failed ") {
        let (_, rest) = rest.split_once(" for ")?;
        let (invalid, rest) = match rest.strip_prefix("invalid user ") {
            Some(r) => (true, r),
            None => (false, rest),
        };
        let (user, rest) = rest.rsplit_once(" from ")?;
        let from = rest.split_whitespace().next()?;
        return Some(Line::Failed { user: user.into(), from: from.into(), invalid });
    }
    None
}

/// Failed logins of one window, summed.
#[derive(Debug, Default)]
pub struct Failures {
    pub start: i64,
    pub count: u32,
    pub users: BTreeSet<String>,
    pub from: BTreeMap<String, u32>,
}

impl Failures {
    pub fn add(&mut self, at: i64, user: &str, from: &str) {
        if self.count == 0 {
            self.start = at;
        }
        self.count += 1;
        if self.users.len() < 50 {
            self.users.insert(user.to_owned());
        }
        if self.from.len() < 200 || self.from.contains_key(from) {
            *self.from.entry(from.to_owned()).or_default() += 1;
        }
    }

    /// The summary event, emptying the window.
    pub fn take(&mut self) -> Option<NewEvent> {
        if self.count == 0 {
            return None;
        }
        let f = std::mem::take(self);
        let mut top: Vec<(&String, &u32)> = f.from.iter().collect();
        top.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let addrs: Vec<String> = top.iter().take(3).map(|(a, n)| format!("{a} ×{n}")).collect();
        let users: Vec<&str> = f.users.iter().take(5).map(String::as_str).collect();
        let title = if f.count == 1 {
            format!("failed login for {} from {}", users[0], top[0].0)
        } else {
            format!(
                "{} failed logins from {} address{}",
                f.count,
                f.from.len(),
                if f.from.len() == 1 { "" } else { "es" }
            )
        };
        let more = if f.users.len() > 5 { ", …" } else { "" };
        let detail = format!("users: {}{more}; from: {}", users.join(", "), addrs.join(", "));
        let level = if f.count >= 10 { Level::Warn } else { Level::Info };
        Some(NewEvent::new(f.start, EventKind::Login, level, title, detail))
    }
}

pub fn accepted_event(at: i64, user: &str, from: &str, method: &str) -> NewEvent {
    // Windows: Remote Desktop logins come from the Security log.
    let via = if method == RDP { RDP.to_owned() } else { format!("ssh, {method}") };
    NewEvent::new(at, EventKind::Login, Level::Info, format!("{user} logged in from {from}"), via)
}

/// The `method` of a Remote Desktop login (Windows).
pub const RDP: &str = "rdp";

/// Windows: whether the service may read the Event Log ("Event Log
/// Readers", given by `allow eventlog`), the journal's counterpart.
#[cfg(windows)]
pub fn journal_access() -> &'static str {
    if crate::compat::am_root() || crate::win::sys::in_group(crate::win::EVENT_LOG_READERS) {
        "ok"
    } else {
        "denied"
    }
}

/// A Security log logon event (4624 success, 4625 failure) as a login
/// line: Remote Desktop logons, and failed network logons from another
/// address (password guessing over RDP or SMB). Local, service and SSH
/// logons are left out (sshd's own lines already cover SSH).
pub fn security_line(r: &crate::evtx::Record) -> Option<Line> {
    let logon_type = r.get("LogonType").unwrap_or("");
    let from = r.get("IpAddress").unwrap_or("").trim();
    if from.is_empty() || from == "-" || from == "::1" || from.starts_with("127.") {
        return None;
    }
    if r.get("ProcessName").is_some_and(|p| p.to_ascii_lowercase().contains("sshd")) {
        return None;
    }
    let user = r.get("TargetUserName").filter(|u| !u.is_empty() && *u != "-").unwrap_or("?").to_owned();
    match (r.event_id, logon_type) {
        (4624, "10") => Some(Line::Accepted { user, from: from.into(), method: RDP.into() }),
        (4625, "3" | "10") => Some(Line::Failed { user, from: from.into(), invalid: false }),
        _ => None,
    }
}

/// The journal's group and whether this process is in it.
#[cfg(unix)]
pub fn journal_access() -> &'static str {
    let dir = ["/var/log/journal", "/run/log/journal"].into_iter().map(Path::new).find(|p| p.is_dir());
    let Some(dir) = dir else { return "absent" };
    let Ok(md) = std::fs::metadata(dir) else { return "absent" };
    // SAFETY: getegid/getgroups only write into the buffer we pass.
    let groups = unsafe {
        let mut g = vec![0 as libc::gid_t; 256];
        let n = libc::getgroups(g.len() as i32, g.as_mut_ptr());
        g.truncate(n.max(0) as usize);
        g.push(libc::getegid());
        g
    };
    if groups.contains(&md.gid()) || unsafe { libc::geteuid() } == 0 {
        "ok"
    } else {
        "denied"
    }
}

/// Handle to the login watcher.
pub struct LoginWatcher {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LoginWatcher {
    pub fn start(shared: Arc<Shared>) -> std::io::Result<Self> {
        let (stop, rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new().name("logins".into()).spawn(move || run(&shared, &rx))?;
        Ok(Self { stop, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(unix)]
enum Msg {
    Line { at: i64, cursor: String, line: Line },
    Cursor(String),
    Ended,
}

#[cfg(unix)]
fn spawn(cursor: Option<&str>) -> std::io::Result<Child> {
    let mut c = Command::new("journalctl");
    c.args(["--follow", "--output=json", "--no-pager", "--identifier=sshd", "--identifier=sshd-session"]);
    match cursor {
        Some(cur) => c.arg(format!("--after-cursor={cur}")),
        // Not `--lines=0`: following from the tail that way can miss the
        // first entry written after journalctl starts. Starting from a time
        // has no such gap (at worst a login from this very second is read).
        None => c.arg(format!("--since=@{}", crate::daemon::now_secs())),
    };
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
}

#[cfg(unix)]
fn reader(stdout: std::process::ChildStdout, tx: mpsc::Sender<Msg>) {
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let cursor = v["__CURSOR"].as_str().unwrap_or_default().to_owned();
        let at = v["__REALTIME_TIMESTAMP"].as_str().and_then(|t| t.parse::<i64>().ok()).unwrap_or(0) / 1_000_000;
        let msg = v["MESSAGE"].as_str().unwrap_or_default();
        let m = match parse(msg) {
            Some(line) => Msg::Line { at, cursor, line },
            None => Msg::Cursor(cursor),
        };
        if tx.send(m).is_err() {
            return;
        }
    }
    let _ = tx.send(Msg::Ended);
}

#[cfg(unix)]
fn run(shared: &Shared, stop: &mpsc::Receiver<()>) {
    let access = journal_access();
    if access != "ok" {
        if access == "denied" {
            crate::info!(
                "no access to the journal, logins are not recorded (root can run: nightgrid-node allow journal)"
            );
        }
        // Wait to be stopped; access only changes with a restart.
        let _ = stop.recv();
        return;
    }
    let mut cursor: Option<String> = {
        let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        st.meta(META_CURSOR).ok().flatten()
    };
    let mut backoff = Duration::from_secs(1);
    'outer: loop {
        let mut child = match spawn(cursor.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                crate::warn!("could not run journalctl: {e}; logins are not recorded");
                let _ = stop.recv();
                return;
            }
        };
        let (tx, rx) = mpsc::channel();
        let out = child.stdout.take().expect("piped");
        let t = std::thread::Builder::new().name("logins-read".into()).spawn(move || reader(out, tx));
        let started = Instant::now();
        let mut fails = Failures::default();
        let mut last_saved = Instant::now();
        let mut dirty = false;
        loop {
            if let Ok(()) | Err(mpsc::TryRecvError::Disconnected) = stop.try_recv() {
                let _ = child.kill();
                let _ = child.wait();
                flush(shared, &mut fails, cursor.as_deref());
                break 'outer;
            }
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Msg::Line { at, cursor: c, line }) => {
                    cursor = Some(c);
                    dirty = !on_line(shared, at, line, &mut fails, cursor.as_deref());
                }
                Ok(Msg::Cursor(c)) => {
                    cursor = Some(c);
                    dirty = true;
                }
                Ok(Msg::Ended) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            if fails.count > 0 && crate::daemon::now_secs() - fails.start >= WINDOW {
                flush(shared, &mut fails, cursor.as_deref());
                dirty = false;
            }
            if dirty && last_saved.elapsed() >= Duration::from_secs(60) {
                save_cursor(shared, cursor.as_deref());
                last_saved = Instant::now();
                dirty = false;
            }
        }
        // journalctl ended (journald restarted, rotated away…): start again.
        let _ = child.kill();
        let _ = child.wait();
        if let Ok(t) = t {
            let _ = t.join();
        }
        flush(shared, &mut fails, cursor.as_deref());
        backoff = if started.elapsed() > Duration::from_secs(60) {
            Duration::from_secs(1)
        } else {
            (backoff * 2).min(Duration::from_secs(60))
        };
        crate::warn!("journalctl stopped; restarting in {} s", backoff.as_secs());
        match stop.recv_timeout(backoff) {
            Err(RecvTimeoutError::Timeout) => continue,
            _ => break,
        }
    }
}

/// One login line: an accepted login is recorded at once (with the
/// cursor, so true comes back: saved); failures are summed.
fn on_line(shared: &Shared, at: i64, line: Line, fails: &mut Failures, cursor: Option<&str>) -> bool {
    match line {
        Line::Accepted { user, from, method } => {
            shared.logins.lock().unwrap_or_else(|e| e.into_inner()).accepted(at, &from);
            flush(shared, fails, None);
            record(shared, &accepted_event(at, &user, &from, &method), cursor);
            true
        }
        Line::Failed { user, from, .. } => {
            if fails.count > 0 && at - fails.start >= WINDOW {
                flush(shared, fails, None);
            }
            fails.add(at, &user, &from);
            let n = {
                let mut r = shared.logins.lock().unwrap_or_else(|e| e.into_inner());
                r.add(at, &from);
                r.count_from(&from, crate::daemon::now_secs())
            };
            // Exactly at the threshold, so one burst asks once.
            if n == shared.config.block.threshold {
                if let Some(tx) = shared.autoblock.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    let _ = tx.send(from.clone());
                }
            }
            false
        }
    }
}

/// Windows: the channels read, with their filter. sshd writes its usual
/// lines to OpenSSH/Operational; Remote Desktop and network logons are in
/// the Security log.
#[cfg(windows)]
const CHANNELS: [(&str, &str); 2] = [("OpenSSH/Operational", ""), ("Security", "EventID=4624 or EventID=4625")];

/// `OpenSSH/Operational=12;Security=34`: the last record read per channel.
pub fn parse_cursor(c: &str) -> BTreeMap<String, u64> {
    c.split(';')
        .filter_map(|kv| kv.split_once('='))
        .filter_map(|(k, v)| Some((k.to_owned(), v.parse().ok()?)))
        .collect()
}

pub fn format_cursor(m: &BTreeMap<String, u64>) -> String {
    m.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(";")
}

/// Windows: polls the channels every few seconds from the saved record ids.
#[cfg(windows)]
fn run(shared: &Shared, stop: &mpsc::Receiver<()>) {
    use crate::win::eventlog;
    if journal_access() != "ok" {
        crate::info!(
            "no access to the Event Log, logins are not recorded (an administrator can run: nightgrid-node allow eventlog)"
        );
        let _ = stop.recv();
        return;
    }
    let saved = {
        let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        st.meta(META_CURSOR).ok().flatten()
    };
    // A cursor from the journal (a moved database) means nothing here.
    let mut at: BTreeMap<String, u64> = saved.as_deref().map(parse_cursor).unwrap_or_default();
    let mut fails = Failures::default();
    let mut missing: BTreeMap<&str, Instant> = BTreeMap::new();
    let mut warned = false;
    let mut last_saved = saved.unwrap_or_default();
    loop {
        let mut lines: Vec<(i64, u64, &str, Line)> = Vec::new();
        for (channel, filter) in CHANNELS {
            if missing.get(channel).is_some_and(|t| t.elapsed() < Duration::from_secs(300)) {
                continue;
            }
            let after = match at.get(channel) {
                Some(n) => *n,
                None => match eventlog::last_id(channel) {
                    Ok(n) => {
                        at.insert(channel.to_owned(), n);
                        continue;
                    }
                    Err(e) => {
                        if e.raw_os_error() != Some(eventlog::ERROR_EVT_CHANNEL_NOT_FOUND) && !warned {
                            crate::warn!("logins: could not read {channel}: {e}");
                            warned = true;
                        }
                        missing.insert(channel, Instant::now());
                        continue;
                    }
                },
            };
            match eventlog::read(channel, filter, after, 2000, false) {
                Ok(events) => {
                    missing.remove(channel);
                    for e in events {
                        at.insert(channel.to_owned(), e.record.id);
                        let line = if channel == "Security" {
                            security_line(&e.record)
                        } else {
                            e.record.get("payload").and_then(parse)
                        };
                        if let Some(l) = line {
                            lines.push((e.record.at, e.record.id, channel, l));
                        }
                    }
                }
                Err(e) => {
                    if !warned {
                        crate::warn!("logins: could not read {channel}: {e}");
                        warned = true;
                    }
                    missing.insert(channel, Instant::now());
                }
            }
        }
        lines.sort_by_key(|(t, id, ..)| (*t, *id));
        let cursor = format_cursor(&at);
        for (t, _, _, line) in lines {
            if on_line(shared, t, line, &mut fails, Some(&cursor)) {
                last_saved = cursor.clone();
            }
        }
        if fails.count > 0 && crate::daemon::now_secs() - fails.start >= WINDOW {
            flush(shared, &mut fails, Some(&cursor));
            last_saved = cursor.clone();
        } else if cursor != last_saved && fails.count == 0 {
            // Not while failures are pending: a restart then reads them again.
            save_cursor(shared, Some(&cursor));
            last_saved = cursor.clone();
        }
        match stop.recv_timeout(Duration::from_secs(5)) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => {
                flush(shared, &mut fails, Some(&cursor));
                return;
            }
        }
    }
}

fn flush(shared: &Shared, fails: &mut Failures, cursor: Option<&str>) {
    if let Some(e) = fails.take() {
        record(shared, &e, cursor);
    } else if cursor.is_some() {
        save_cursor(shared, cursor);
    }
}

fn record(shared: &Shared, e: &NewEvent, cursor: Option<&str>) {
    {
        let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(err) = shared.bus.record(&st, e) {
            crate::warn!("could not record a login: {err}");
        }
    }
    save_cursor(shared, cursor);
}

fn save_cursor(shared: &Shared, cursor: Option<&str>) {
    let Some(c) = cursor else { return };
    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = st.set_meta(META_CURSOR, c) {
        crate::warn!("could not save the journal cursor: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logon(id: u32, ty: &str, ip: &str, process: &str) -> crate::evtx::Record {
        crate::evtx::Record {
            event_id: id,
            data: vec![
                ("TargetUserName".into(), "admin".into()),
                ("LogonType".into(), ty.into()),
                ("IpAddress".into(), ip.into()),
                ("ProcessName".into(), process.into()),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn security_log_logons() {
        assert_eq!(
            security_line(&logon(4624, "10", "10.0.0.7", "C:\\Windows\\System32\\svchost.exe")),
            Some(Line::Accepted { user: "admin".into(), from: "10.0.0.7".into(), method: RDP.into() })
        );
        assert_eq!(
            security_line(&logon(4625, "3", "203.0.113.9", "-")),
            Some(Line::Failed { user: "admin".into(), from: "203.0.113.9".into(), invalid: false })
        );
        // Network logons that worked (file shares), local ones and sshd's own are left out.
        assert_eq!(security_line(&logon(4624, "3", "10.0.0.7", "-")), None);
        assert_eq!(security_line(&logon(4625, "10", "-", "-")), None);
        assert_eq!(security_line(&logon(4625, "10", "127.0.0.1", "-")), None);
        assert_eq!(security_line(&logon(4625, "8", "10.0.0.7", "C:\\Windows\\System32\\OpenSSH\\sshd.exe")), None);
        assert_eq!(accepted_event(1, "admin", "10.0.0.7", RDP).detail, "rdp");
    }

    #[test]
    fn event_log_cursor_round_trips() {
        let m = parse_cursor("OpenSSH/Operational=12;Security=3400;bad;x=y");
        assert_eq!(m.len(), 2);
        assert_eq!(format_cursor(&m), "OpenSSH/Operational=12;Security=3400");
        assert!(parse_cursor("s=1234:abc").is_empty());
    }

    #[test]
    fn burst_alert_counts_the_last_five_minutes() {
        let mut r = Recent::default();
        for i in 0..15 {
            r.add(1000 + i, "203.0.113.7");
        }
        for i in 0..5 {
            r.add(1100 + i, if i % 2 == 0 { "198.51.100.2" } else { "198.51.100.3" });
        }
        assert!(r.condition("nas", 21, 1200).is_none());
        let c = r.condition("nas", 20, 1200).unwrap();
        assert_eq!((c.key.as_str(), c.kind, c.severity), ("logins:burst", AlertKind::LoginBurst, Severity::Warn));
        assert_eq!(c.detail, "20 in the last 5 minutes from 203.0.113.7 ×15, 198.51.100.2 ×3, 198.51.100.3 ×2");
        assert_eq!(r.condition("nas", 2, 1200).unwrap().severity, Severity::Critical);
        // Five minutes after the first ones, only the later five are left.
        assert_eq!(r.by_address(1315).iter().map(|(_, n)| n).sum::<u32>(), 5);
        assert!(r.condition("nas", 20, 1315).is_none());
    }

    #[test]
    fn parses_sshd_messages() {
        assert_eq!(
            parse("Accepted publickey for alice from 10.0.0.5 port 50522 ssh2: ED25519 SHA256:abc"),
            Some(Line::Accepted { user: "alice".into(), from: "10.0.0.5".into(), method: "publickey".into() })
        );
        assert_eq!(
            parse("Failed password for invalid user admin from 192.0.2.7 port 4242 ssh2"),
            Some(Line::Failed { user: "admin".into(), from: "192.0.2.7".into(), invalid: true })
        );
        assert_eq!(
            parse("Failed publickey for root from 2001:db8::1 port 22 ssh2"),
            Some(Line::Failed { user: "root".into(), from: "2001:db8::1".into(), invalid: false })
        );
        assert_eq!(parse("Connection closed by 192.0.2.7 port 4242 [preauth]"), None);
        assert_eq!(parse("Server listening on 0.0.0.0 port 22."), None);
    }

    #[test]
    fn failures_are_summed() {
        let mut f = Failures::default();
        assert!(f.take().is_none());
        f.add(100, "root", "192.0.2.7");
        let e = f.take().unwrap();
        assert_eq!((e.title.as_str(), e.level, e.at), ("failed login for root from 192.0.2.7", Level::Info, 100));
        for i in 0..12 {
            f.add(
                200 + i,
                if i % 2 == 0 { "root" } else { "admin" },
                if i < 10 { "192.0.2.7" } else { "198.51.100.1" },
            );
        }
        let e = f.take().unwrap();
        assert_eq!((e.title.as_str(), e.level, e.at), ("12 failed logins from 2 addresses", Level::Warn, 200));
        assert_eq!(e.detail, "users: admin, root; from: 192.0.2.7 ×10, 198.51.100.1 ×2");
        assert_eq!(f.count, 0);
    }
}
