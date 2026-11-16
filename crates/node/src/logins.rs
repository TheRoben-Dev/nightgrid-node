//! SSH logins and failed logins, read from the journal.
//!
//! The service can only read the system journal after root runs
//! `nightgrid-node allow journal` (a drop-in adding the journal's group).
//! It follows `journalctl -o json` for sshd's messages, one child process
//! for the life of the service, resuming from a saved cursor so logins while
//! the node was stopped are still recorded. Successful logins are one event
//! each; failed ones are summed per minute so a scanned host doesn't flood
//! the timeline.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nightgrid_node_proto::{EventKind, Level};

use crate::daemon::Shared;
use crate::events::NewEvent;

const META_CURSOR: &str = "logins_cursor";
/// Failed logins are summed over this many seconds.
const WINDOW: i64 = 60;

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
    NewEvent::new(at, EventKind::Login, Level::Info, format!("{user} logged in from {from}"), format!("ssh, {method}"))
}

/// The journal's group and whether this process is in it.
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

enum Msg {
    Line { at: i64, cursor: String, line: Line },
    Cursor(String),
    Ended,
}

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
                    dirty = true;
                    match line {
                        Line::Accepted { user, from, method } => {
                            flush(shared, &mut fails, None);
                            record(shared, &accepted_event(at, &user, &from, &method), cursor.as_deref());
                            dirty = false;
                        }
                        Line::Failed { user, from, .. } => {
                            if fails.count > 0 && at - fails.start >= WINDOW {
                                flush(shared, &mut fails, None);
                            }
                            fails.add(at, &user, &from);
                        }
                    }
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
