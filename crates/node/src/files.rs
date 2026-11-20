//! Watched files: root lists files and directories in the config; the node
//! records when one is added, removed or changed. It never stores contents:
//! only size, owner, mode, modification time and a SHA-256 of files it can
//! read (so `/etc/shadow` still reports *that* it changed, not how).
//!
//! It polls metadata (a `stat` per entry, cheap for a few dozen paths)
//! instead of inotify, so editors that replace files by renaming, and
//! directories created later, need no special handling.

use crate::compat::fs::MetadataExt;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use nightgrid_node_proto::{EventKind, Level};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::daemon::{now_secs, Shared};
use crate::events::NewEvent;

const META_BASELINE: &str = "files_baseline";
/// Who took the saved baseline: `direct` or `helper`.
const META_SOURCE: &str = "files_source";
/// Through the health helper, checks are at least this far apart.
const HELPER_MIN_SECS: u64 = 30;
/// Login users whose `authorized_keys` are watched, at most.
const MAX_USERS: usize = 64;
/// Files larger than this are compared by metadata only.
const MAX_HASH: u64 = 4 << 20;
/// Entries read from one watched directory.
const MAX_ENTRIES: usize = 500;

/// What is known about one path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snap {
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    /// First 16 hex digits of the SHA-256, when readable and small enough.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dir: bool,
}

pub type Snapshot = BTreeMap<PathBuf, Snap>;

fn hash(path: &Path, size: u64) -> Option<String> {
    if size > MAX_HASH {
        return None;
    }
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect())
}

fn snap(path: &Path, md: &std::fs::Metadata) -> Snap {
    let dir = md.is_dir();
    Snap {
        size: if dir { 0 } else { md.len() },
        mode: md.mode() & 0o7777,
        uid: md.uid(),
        gid: md.gid(),
        mtime: md.mtime(),
        sha: if md.is_file() { hash(path, md.len()) } else { None },
        dir,
    }
}

/// Reads every watched path (and the direct entries of watched directories).
/// `previous` lets unchanged files skip hashing.
pub fn take(watch: &[PathBuf], previous: &Snapshot) -> Snapshot {
    let mut out = Snapshot::new();
    let mut add = |p: PathBuf, md: std::fs::Metadata| {
        let quick = Snap {
            size: if md.is_dir() { 0 } else { md.len() },
            mode: md.mode() & 0o7777,
            uid: md.uid(),
            gid: md.gid(),
            mtime: md.mtime(),
            sha: None,
            dir: md.is_dir(),
        };
        // Same metadata as last time: keep the old hash instead of re-reading.
        let s = match previous.get(&p) {
            Some(old) if Snap { sha: None, ..old.clone() } == quick => old.clone(),
            _ => snap(&p, &md),
        };
        out.insert(p, s);
    };
    for w in watch {
        let Ok(md) = std::fs::symlink_metadata(w) else { continue };
        let is_dir = md.is_dir();
        add(w.clone(), md);
        if is_dir {
            let Ok(rd) = std::fs::read_dir(w) else { continue };
            let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            entries.sort();
            for p in entries.into_iter().take(MAX_ENTRIES) {
                if let Ok(md) = std::fs::symlink_metadata(&p) {
                    // Direct entries only; a subdirectory is one entry.
                    add(p, md);
                }
            }
        }
    }
    out
}

fn sensitive(p: &Path) -> bool {
    let s = p.to_string_lossy();
    ["ssh", "sudoers", "shadow", "passwd", "/group", "ld.so.preload", "pam.d", "/cron", "sudo.conf"]
        .iter()
        .any(|k| s.contains(k))
}

/// `authorized_keys` of every login user in `/etc/passwd` text (uid 1000 and
/// up with a real shell; root's are in the baseline list).
pub fn authorized_keys(passwd: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in passwd.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 7 {
            continue;
        }
        let uid: u32 = f[2].parse().unwrap_or(0);
        let (home, shell) = (f[5], f[6]);
        if uid < 1000 || uid == 65534 || shell.ends_with("nologin") || shell.ends_with("false") || shell.is_empty() {
            continue;
        }
        if !home.starts_with('/') || home == "/" || home.contains("/../") {
            continue;
        }
        let ssh = Path::new(home).join(".ssh");
        out.push(ssh.join("authorized_keys"));
        out.push(ssh.join("authorized_keys2"));
        if out.len() >= MAX_USERS * 2 {
            break;
        }
    }
    out
}

/// The paths root's config makes the node watch: `watch`, plus the baseline
/// and users' `authorized_keys` when `baseline` is on. Computed the same way
/// on the helper's side, so the service can't ask it for other files.
pub fn root_paths(cfg: &crate::config::FilesConfig, passwd: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = cfg.watch.clone();
    if cfg.baseline {
        out.extend(crate::config::BASELINE_WATCH.iter().map(PathBuf::from));
        out.extend(authorized_keys(passwd));
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

fn passwd() -> String {
    std::fs::read_to_string("/etc/passwd").unwrap_or_default()
}

/// The helper's side of `read: "files"`: a snapshot of root's paths as JSON.
pub fn helper_snapshot(cfg: &crate::config::FilesConfig) -> Result<String, String> {
    if !cfg.enabled {
        return Err("[files] is off".into());
    }
    let snap = take(&root_paths(cfg, &passwd()), &Snapshot::new());
    serde_json::to_string(&snap).map_err(|e| e.to_string())
}

fn human(b: u64) -> String {
    crate::alerts::human_bytes(b)
}

/// What changed about one path, in words.
fn describe(old: &Snap, new: &Snap) -> String {
    let mut parts = Vec::new();
    if old.size != new.size {
        parts.push(format!("size {} → {}", human(old.size), human(new.size)));
    }
    match (&old.sha, &new.sha) {
        (Some(a), Some(b)) if a != b => parts.push(format!("sha256 {a}… → {b}…")),
        _ => {}
    }
    if old.mode != new.mode {
        parts.push(format!("mode {:o} → {:o}", old.mode, new.mode));
    }
    if (old.uid, old.gid) != (new.uid, new.gid) {
        parts.push(format!("owner {}:{} → {}:{}", old.uid, old.gid, new.uid, new.gid));
    }
    if parts.is_empty() && old.mtime != new.mtime {
        parts.push("modified (contents not readable by the node)".into());
    }
    parts.join(", ")
}

/// Events for the differences between two snapshots.
pub fn diff(old: &Snapshot, new: &Snapshot, at: i64, note: &str) -> Vec<NewEvent> {
    let mut out = Vec::new();
    let level = |p: &Path| if sensitive(p) { Level::Warn } else { Level::Info };
    let with_note = |d: String| {
        if note.is_empty() {
            d
        } else if d.is_empty() {
            note.to_owned()
        } else {
            format!("{d} {note}")
        }
    };
    for (p, n) in new {
        match old.get(p) {
            None => {
                let what = if n.dir { "directory" } else { "file" };
                out.push(NewEvent::new(
                    at,
                    EventKind::File,
                    level(p),
                    format!("added {}", p.display()),
                    with_note(
                        format!("new {what}, {}", if n.dir { String::new() } else { human(n.size) })
                            .trim_end_matches(", ")
                            .to_owned(),
                    ),
                ));
            }
            Some(o) if o != n => {
                // A directory's own mtime changes whenever an entry does;
                // the entry itself is reported, so skip that noise.
                if o.dir && n.dir && (Snap { mtime: 0, ..o.clone() }) == (Snap { mtime: 0, ..n.clone() }) {
                    continue;
                }
                let d = describe(o, n);
                if d.is_empty() {
                    continue;
                }
                out.push(NewEvent::new(
                    at,
                    EventKind::File,
                    level(p),
                    format!("changed {}", p.display()),
                    with_note(d),
                ));
            }
            _ => {}
        }
    }
    for p in old.keys().filter(|p| !new.contains_key(*p)) {
        out.push(NewEvent::new(
            at,
            EventKind::File,
            level(p),
            format!("removed {}", p.display()),
            with_note(String::new()),
        ));
    }
    out
}

/// Handle to the file watcher thread.
pub struct FileWatcher {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FileWatcher {
    pub fn start(shared: Arc<Shared>) -> std::io::Result<Self> {
        let (stop, rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new().name("files".into()).spawn(move || run(&shared, &rx))?;
        Ok(Self { stop, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Compose files of the containers the node knows, not already in `root`.
fn compose_paths(shared: &Shared, root: &[PathBuf]) -> Vec<PathBuf> {
    if !shared.config.files.baseline {
        return Vec::new();
    }
    let c = shared.containers.lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<PathBuf> = Vec::new();
    for f in c.result.containers.iter().flat_map(|c| c.compose_files.iter()) {
        let p = PathBuf::from(f);
        if !root.contains(&p) && !out.contains(&p) {
            out.push(p);
        }
        if out.len() >= 64 {
            break;
        }
    }
    out
}

/// One full snapshot: through the health helper when it's installed (it can
/// read homes and root-only files), else directly. `None` when the helper is
/// installed but didn't answer: better to skip a round than to report every
/// file it alone can see as removed.
fn snapshot(shared: &Shared, previous: &Snapshot, helper: bool) -> Option<Snapshot> {
    let root = root_paths(&shared.config.files, &passwd());
    let mut snap = if helper {
        let req = crate::health::watch::ReadRequest { read: "files".into(), target: None };
        match crate::health::watch::call(&shared.health_socket, &req)
            .and_then(|o| serde_json::from_str::<Snapshot>(&o).map_err(|e| e.to_string()))
        {
            Ok(s) => s,
            Err(e) => {
                crate::debug!("files through the health helper: {e}");
                return None;
            }
        }
    } else {
        take(&root, previous)
    };
    snap.extend(take(&compose_paths(shared, &root), previous));
    Some(snap)
}

fn run(shared: &Shared, stop: &mpsc::Receiver<()>) {
    let cfg = &shared.config.files;
    let helper = crate::helper::installed(&shared.health_socket) && shared.config.health.enabled;
    let secs = if helper { u64::from(cfg.interval_secs).max(HELPER_MIN_SECS) } else { cfg.interval_secs.into() };
    let interval = Duration::from_secs(secs);
    let source = if helper { "helper" } else { "direct" };
    let (saved, saved_source): (Option<Snapshot>, Option<String>) = {
        let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        (
            st.meta(META_BASELINE).ok().flatten().and_then(|v| serde_json::from_str(&v).ok()),
            st.meta(META_SOURCE).ok().flatten(),
        )
    };
    // Give the container watcher a moment to list compose files.
    if let Ok(()) | Err(RecvTimeoutError::Disconnected) = stop.recv_timeout(Duration::from_secs(3)) {
        return;
    }
    let mut current = loop {
        if let Some(s) = snapshot(shared, saved.as_ref().unwrap_or(&Snapshot::new()), helper) {
            break s;
        }
        if let Ok(()) | Err(RecvTimeoutError::Disconnected) = stop.recv_timeout(interval) {
            return;
        }
    };
    // A baseline from the other source sees a different set of files (and
    // hashes): start over instead of reporting the difference as changes.
    let same_source = saved_source.as_deref().unwrap_or("direct") == source;
    match &saved {
        Some(old) if same_source => {
            record(shared, diff(old, &current, now_secs(), "(while the node was not running)"), &current)
        }
        Some(_) => {
            crate::info!("file baseline taken again ({source} reads now)");
            save(shared, &current);
        }
        // First start: this is the baseline, nothing to report.
        None => save(shared, &current),
    }
    {
        let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        let _ = st.set_meta(META_SOURCE, source);
    }
    while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(interval) {
        let Some(next) = snapshot(shared, &current, helper) else { continue };
        if next != current {
            record(shared, diff(&current, &next, now_secs(), ""), &next);
            current = next;
        }
    }
}

fn record(shared: &Shared, events: Vec<NewEvent>, snapshot: &Snapshot) {
    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    for e in &events {
        if let Err(err) = shared.bus.record(&st, e) {
            crate::warn!("could not record a file change: {err}");
        }
    }
    drop(st);
    save(shared, snapshot);
}

fn save(shared: &Shared, snapshot: &Snapshot) {
    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = st.set_meta(META_BASELINE, &serde_json::to_string(snapshot).unwrap_or_default()) {
        crate::warn!("could not save the file baseline: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::fs::PermissionsExt;

    #[test]
    fn reports_added_changed_removed_and_never_contents() {
        let d = tempfile::tempdir().unwrap();
        let conf = d.path().join("sshd_config");
        let dir = d.path().join("cron.d");
        std::fs::write(&conf, "PermitRootLogin no\n").unwrap();
        std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::create_dir(&dir).unwrap();
        let watch = vec![conf.clone(), dir.clone(), d.path().join("missing")];
        let a = take(&watch, &Snapshot::new());
        assert_eq!(a.len(), 2);
        assert!(a[&conf].sha.as_ref().unwrap().len() == 16);

        std::fs::write(&conf, "PermitRootLogin yes\n").unwrap();
        std::fs::write(dir.join("backup"), "0 3 * * * root /x\n").unwrap();
        let b = take(&watch, &a);
        let ev = diff(&a, &b, 100, "");
        let t: Vec<(&str, Level)> = ev.iter().map(|e| (e.title.as_str(), e.level)).collect();
        // cron.d is a way to keep access, so a new entry is a warning.
        assert!(t.contains(&(format!("added {}", dir.join("backup").display()).as_str(), Level::Warn)), "{t:?}");
        let changed = ev.iter().find(|e| e.title == format!("changed {}", conf.display())).unwrap();
        assert_eq!(changed.level, Level::Warn);
        assert!(changed.detail.starts_with("size 19 B → 20 B, sha256 "), "{}", changed.detail);
        assert!(!changed.detail.contains("PermitRootLogin"));
        // The directory's own mtime change is not reported separately.
        assert!(!t.iter().any(|(s, _)| *s == format!("changed {}", dir.display())));

        std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_file(dir.join("backup")).unwrap();
        let c = take(&watch, &b);
        let ev = diff(&b, &c, 200, "(while the node was not running)");
        let titles: Vec<&str> = ev.iter().map(|e| e.title.as_str()).collect();
        assert!(titles.contains(&format!("removed {}", dir.join("backup").display()).as_str()));
        let m = ev.iter().find(|e| e.title.starts_with("changed")).unwrap();
        assert!(
            m.detail.contains("mode 644 → 600") && m.detail.ends_with("(while the node was not running)"),
            "{}",
            m.detail
        );
        // Nothing changed: nothing to report, and the hash is reused.
        assert!(diff(&c, &take(&watch, &c), 300, "").is_empty());
    }

    #[test]
    fn baseline_paths_come_from_roots_config_and_passwd() {
        let passwd = "root:x:0:0:root:/root:/bin/bash
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
alice:x:1000:1000:Alice:/home/alice:/bin/bash
svc:x:1001:1001::/srv/svc:/bin/false
bob:x:1002:1002::/home/bob:/usr/bin/zsh
nobody:x:65534:65534:nobody:/nonexistent:/bin/sh
bad:x:1003:1003::relative:/bin/sh
";
        let k = authorized_keys(passwd);
        assert_eq!(
            k,
            [
                "/home/alice/.ssh/authorized_keys",
                "/home/alice/.ssh/authorized_keys2",
                "/home/bob/.ssh/authorized_keys",
                "/home/bob/.ssh/authorized_keys2"
            ]
            .map(PathBuf::from)
        );
        let mut cfg = crate::config::FilesConfig::default();
        let all = root_paths(&cfg, passwd);
        assert!(all.contains(&PathBuf::from("/etc/shadow")) && all.contains(&PathBuf::from("/etc/pam.d")));
        assert!(all.contains(&PathBuf::from("/home/bob/.ssh/authorized_keys")));
        let n = all.len();
        cfg.watch.push("/etc/pam.d".into());
        assert_eq!(root_paths(&cfg, passwd).len(), n, "no duplicates");
        cfg.baseline = false;
        assert_eq!(root_paths(&cfg, passwd), cfg.watch);
        assert!(sensitive(Path::new("/home/bob/.ssh/authorized_keys")) && sensitive(Path::new("/etc/pam.d/sshd")));
    }
}
