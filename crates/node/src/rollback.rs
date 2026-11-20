//! Rollbacks (N4c): snapshot files before a change, put them back unless
//! the app confirms the change in time.
//!
//! The file work happens in the root helper ([`snapshot`], [`restore`],
//! [`discard`]), which checks every path against root's `[rollback] paths`
//! itself. The service keeps the list of changes and the dead-man timer
//! ([`begin`], [`confirm`], [`revert`], [`tick`]) in its database, so a
//! change made just before a crash or a reboot is still reverted when the
//! node starts again.

use crate::compat::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use nightgrid_node_proto::{
    action, ActionOutcome, ActionRequest, Change, ChangeBegin, ChangeState, ChangesResult, EventKind, Level,
    MAX_CHANGE_FILES, MAX_CHANGE_FILE_BYTES,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::daemon::{now_secs, Shared};
use crate::events::NewEvent;
use crate::helper::HelperRequest;
use crate::store::Store;

/// Changes waiting for confirmation at once.
const MAX_PENDING: usize = 8;
const KEEP_DONE: i64 = 200;

// ---------------------------------------------------------------- helper side

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    path: String,
    existed: bool,
    mode: u32,
    uid: u32,
    gid: u32,
}

fn valid_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Whether root's config lets the helper touch `p`, after resolving the
/// directories it is in (so a symlinked directory can't lead elsewhere).
fn allowed(config: &Config, p: &str) -> Result<PathBuf, String> {
    let path = Path::new(p);
    let plain = path.is_absolute()
        && p.len() <= 1024
        && path.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    let own = path.components().any(|c| c.as_os_str().to_string_lossy().starts_with("nightgrid-node"));
    if !plain || own || path.starts_with("/etc/nightgrid-node") {
        return Err(format!("{p}: not a path a rollback may touch"));
    }
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(format!("{p}: not a file path"));
    };
    let real = dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?.join(name);
    let under = config.rollback.paths.iter().any(|r| {
        let r = Path::new(r);
        let r = r.canonicalize().unwrap_or_else(|_| r.to_path_buf());
        real.starts_with(&r)
    });
    if !under {
        return Err(format!("{p}: not under [rollback] paths"));
    }
    if real.starts_with("/etc/nightgrid-node") {
        return Err(format!("{p}: not a path a rollback may touch"));
    }
    Ok(real)
}

/// Copies the files into `store/<id>/`.
pub fn snapshot(config: &Config, store: &Path, id: &str, paths: &[String]) -> Result<String, String> {
    if !valid_id(id) {
        return Err("bad change id".into());
    }
    if paths.is_empty() || paths.len() > MAX_CHANGE_FILES {
        return Err(format!("a change covers 1-{MAX_CHANGE_FILES} files"));
    }
    let dir = store.join(id);
    if dir.exists() {
        return Err("change id in use".into());
    }
    let mut entries = Vec::new();
    let mut blobs = Vec::new();
    for p in paths {
        let real = allowed(config, p)?;
        match std::fs::symlink_metadata(&real) {
            Ok(md) if md.file_type().is_file() => {
                if md.len() > MAX_CHANGE_FILE_BYTES {
                    return Err(format!("{p}: larger than {} KiB", MAX_CHANGE_FILE_BYTES / 1024));
                }
                let data = std::fs::read(&real).map_err(|e| format!("{p}: {e}"))?;
                entries.push(Entry {
                    path: real.to_string_lossy().into_owned(),
                    existed: true,
                    mode: md.mode() & 0o7777,
                    uid: md.uid(),
                    gid: md.gid(),
                });
                blobs.push(Some(data));
            }
            Ok(_) => return Err(format!("{p}: not a regular file")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                entries.push(Entry {
                    path: real.to_string_lossy().into_owned(),
                    existed: false,
                    mode: 0,
                    uid: 0,
                    gid: 0,
                });
                blobs.push(None);
            }
            Err(e) => return Err(format!("{p}: {e}")),
        }
    }
    std::fs::create_dir_all(store).map_err(|e| format!("{}: {e}", store.display()))?;
    std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let write = |name: &str, data: &[u8]| -> Result<(), String> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(name))
            .map_err(|e| e.to_string())?;
        f.write_all(data).and_then(|_| f.sync_all()).map_err(|e| e.to_string())
    };
    let result = (|| {
        for (i, b) in blobs.iter().enumerate() {
            if let Some(b) = b {
                write(&i.to_string(), b)?;
            }
        }
        // The manifest last: its presence means the snapshot is complete.
        write("manifest.json", &serde_json::to_vec(&entries).map_err(|e| e.to_string())?)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(format!("snapshot: {e}"));
    }
    let new = entries.iter().filter(|e| !e.existed).count();
    Ok(format!(
        "saved {} file{}{}",
        entries.len(),
        if entries.len() == 1 { "" } else { "s" },
        if new > 0 { format!(" ({new} new, removed on revert)") } else { String::new() }
    ))
}

fn put_back(e: &Entry, data: &[u8]) -> std::io::Result<()> {
    let path = Path::new(&e.path);
    let dir = path.parent().unwrap_or(Path::new("/"));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.ngn-rollback"));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    let r = (|| {
        f.write_all(data)?;
        f.sync_all()?;
        crate::compat::fs::fchown(&f, Some(e.uid), Some(e.gid))?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(e.mode))?;
        std::fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Puts the files from `store/<id>/` back and drops the snapshot.
pub fn restore(store: &Path, id: &str) -> Result<String, String> {
    if !valid_id(id) {
        return Err("bad change id".into());
    }
    let dir = store.join(id);
    let manifest = std::fs::read(dir.join("manifest.json")).map_err(|_| format!("no snapshot for change {id}"))?;
    let entries: Vec<Entry> = serde_json::from_slice(&manifest).map_err(|e| format!("snapshot manifest: {e}"))?;
    let mut errors = Vec::new();
    let (mut back, mut removed) = (0, 0);
    for (i, e) in entries.iter().enumerate() {
        let r = if e.existed {
            std::fs::read(dir.join(i.to_string())).and_then(|d| put_back(e, &d)).map(|_| back += 1)
        } else {
            match std::fs::symlink_metadata(&e.path) {
                Ok(md) if md.file_type().is_file() => std::fs::remove_file(&e.path).map(|_| removed += 1),
                Ok(_) => Err(std::io::Error::other("no longer a regular file; left alone")),
                Err(_) => Ok(()),
            }
        };
        if let Err(err) = r {
            errors.push(format!("{}: {err}", e.path));
        }
    }
    if !errors.is_empty() {
        // Keep the snapshot so root can put the rest back by hand.
        return Err(format!(
            "put back {back}, removed {removed}, failed: {} (snapshot kept in {})",
            errors.join("; "),
            dir.display()
        ));
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(format!(
        "put back {back} file{}{}",
        if back == 1 { "" } else { "s" },
        if removed > 0 { format!(", removed {removed} new") } else { String::new() }
    ))
}

/// Drops a snapshot after the change was confirmed.
pub fn discard(store: &Path, id: &str) -> Result<String, String> {
    if !valid_id(id) {
        return Err("bad change id".into());
    }
    match std::fs::remove_dir_all(store.join(id)) {
        Ok(()) => Ok("snapshot removed".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("no snapshot left".into()),
        Err(e) => Err(e.to_string()),
    }
}

// --------------------------------------------------------------- service side

fn new_id() -> String {
    let mut b = [0u8; 8];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b)).is_err() {
        b = (crate::daemon::now_millis() as u64).to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn state_str(s: ChangeState) -> &'static str {
    match s {
        ChangeState::Pending => "pending",
        ChangeState::Confirmed => "confirmed",
        ChangeState::Reverted => "reverted",
        ChangeState::Failed => "failed",
    }
}

fn state_from(s: &str) -> ChangeState {
    match s {
        "pending" => ChangeState::Pending,
        "confirmed" => ChangeState::Confirmed,
        "reverted" => ChangeState::Reverted,
        _ => ChangeState::Failed,
    }
}

const COLS: &str = "id, created, deadline, paths, on_revert, note, state, detail, finished";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Change> {
    let paths: String = r.get(3)?;
    let on_revert: String = r.get(4)?;
    Ok(Change {
        id: r.get(0)?,
        created: r.get(1)?,
        deadline: r.get(2)?,
        paths: serde_json::from_str(&paths).unwrap_or_default(),
        on_revert: serde_json::from_str(&on_revert).ok(),
        note: r.get(5)?,
        state: state_from(&r.get::<_, String>(6)?),
        detail: r.get(7)?,
        finished: r.get(8)?,
    })
}

fn get(store: &Store, id: &str) -> crate::store::Result<Option<Change>> {
    store.conn().query_row(&format!("SELECT {COLS} FROM changes WHERE id = ?1"), [id], row).optional()
}

fn finish(store: &Store, id: &str, state: ChangeState, detail: &str, now: i64) -> crate::store::Result<()> {
    store.conn().execute(
        "UPDATE changes SET state = ?2, detail = ?3, finished = ?4 WHERE id = ?1",
        params![id, state_str(state), detail, now],
    )?;
    Ok(())
}

pub fn list(store: &Store) -> crate::store::Result<ChangesResult> {
    let mut st = store.conn().prepare_cached(&format!(
        "SELECT {COLS} FROM changes ORDER BY state = 'pending' DESC, created DESC LIMIT ?1"
    ))?;
    let changes = st.query_map([50 + MAX_PENDING as i64], row)?.collect::<Result<_, _>>()?;
    Ok(ChangesResult { changes })
}

/// Logs to the action log and the timeline.
fn announce(shared: &Shared, now: i64, source: &str, id: &str, outcome: ActionOutcome, detail: &str, title: String) {
    let req = ActionRequest { action: action::FILES_ROLLBACK.into(), target: Some(id.to_owned()), dry_run: false };
    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = crate::actions::append(&st, now, source, &req, outcome, detail) {
        crate::error!("could not write the action log: {e}");
    }
    let level = if outcome == ActionOutcome::Ok { Level::Info } else { Level::Warn };
    let ev = NewEvent::new(now, EventKind::Action, level, title, format!("{source}: {detail}"));
    if let Err(e) = shared.bus.record(&st, &ev) {
        crate::warn!("could not record a change event: {e}");
    }
    crate::info!("change {id} by {source}: {detail}");
}

fn note_of(c: &Change) -> String {
    if c.note.is_empty() {
        format!("change {}", c.id)
    } else {
        format!("\"{}\"", c.note)
    }
}

/// `changes.begin`: snapshot through the helper, then arm the timer.
pub fn begin(shared: &Shared, req: &ChangeBegin, source: &str) -> Result<Change, String> {
    let now = now_secs();
    let note = crate::events::tidy(&req.note, 200);
    let refuse = |why: String| -> Result<Change, String> {
        announce(shared, now, source, "-", ActionOutcome::Refused, &why, format!("refused a change: {why}"));
        Err(why)
    };
    if !shared.config.actions.allow.iter().any(|a| a == action::FILES_ROLLBACK) {
        return refuse(format!(
            "{} is not allowed on this host (root can add it to [actions] allow in /etc/nightgrid-node/config.toml)",
            action::FILES_ROLLBACK
        ));
    }
    if !(30..=3600).contains(&req.confirm_within_secs) {
        return Err("confirmWithinSecs must be 30-3600".into());
    }
    if req.paths.is_empty() || req.paths.len() > MAX_CHANGE_FILES {
        return Err(format!("paths: 1-{MAX_CHANGE_FILES} files"));
    }
    if let Some(a) = &req.on_revert {
        let ok = action::takes_container(&a.action) || action::takes_unit(&a.action);
        if !ok || !shared.config.actions.allow.iter().any(|x| x == &a.action) || a.dry_run {
            return refuse(format!("onRevert: {} isn't an allowed container or unit action", a.action));
        }
    }
    if !crate::helper::installed(&shared.helper_socket) {
        return refuse("the root helper isn't installed (root can run: nightgrid-node allow helper)".into());
    }
    {
        let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        let pending: i64 = st
            .conn()
            .query_row("SELECT count(*) FROM changes WHERE state = 'pending'", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if pending as usize >= MAX_PENDING {
            return Err(format!("{pending} changes are already waiting for confirmation"));
        }
    }
    let id = new_id();
    let hreq = HelperRequest {
        action: "files.snapshot".into(),
        id: Some(id.clone()),
        paths: req.paths.clone(),
        ..Default::default()
    };
    let reply = crate::helper::call(&shared.helper_socket, &hreq)?;
    if !reply.ok {
        return refuse(reply.detail);
    }
    let change = Change {
        id: id.clone(),
        created: now,
        deadline: now + i64::from(req.confirm_within_secs),
        paths: req.paths.clone(),
        on_revert: req.on_revert.clone(),
        note,
        state: ChangeState::Pending,
        detail: reply.detail.clone(),
        finished: None,
    };
    {
        let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        st.conn()
            .execute(
                &format!("INSERT INTO changes ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, NULL)"),
                params![
                    id,
                    now,
                    change.deadline,
                    serde_json::to_string(&change.paths).unwrap_or_default(),
                    change.on_revert.as_ref().map(|a| serde_json::to_string(a).unwrap_or_default()).unwrap_or_default(),
                    change.note,
                    change.detail,
                ],
            )
            .map_err(|e| e.to_string())?;
    }
    announce(
        shared,
        now,
        source,
        &id,
        ActionOutcome::Ok,
        &format!(
            "{}; reverts in {} unless confirmed",
            reply.detail,
            crate::uptime::span_words(i64::from(req.confirm_within_secs))
        ),
        format!("change started: {}", note_of(&change)),
    );
    Ok(change)
}

fn pending(shared: &Shared, id: &str) -> Result<Change, String> {
    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    match get(&st, id).map_err(|e| e.to_string())? {
        Some(c) if c.state == ChangeState::Pending => Ok(c),
        Some(c) => Err(format!("change {id} is already {}", state_str(c.state))),
        None => Err(format!("no change {id}")),
    }
}

/// `changes.confirm`: keep the change, drop the snapshot.
pub fn confirm(shared: &Shared, id: &str, source: &str) -> Result<Change, String> {
    let c = pending(shared, id)?;
    let now = now_secs();
    let hreq = HelperRequest { action: "files.discard".into(), id: Some(id.to_owned()), ..Default::default() };
    // The change is confirmed even if the helper is gone; the snapshot is
    // then left for root, which the detail says.
    let detail = match crate::helper::call(&shared.helper_socket, &hreq) {
        Ok(r) if r.ok => "confirmed".to_owned(),
        Ok(r) => format!("confirmed; snapshot not removed: {}", r.detail),
        Err(e) => format!("confirmed; snapshot not removed: {e}"),
    };
    {
        let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        finish(&st, id, ChangeState::Confirmed, &detail, now).map_err(|e| e.to_string())?;
    }
    announce(shared, now, source, id, ActionOutcome::Ok, &detail, format!("change confirmed: {}", note_of(&c)));
    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    get(&st, id).map_err(|e| e.to_string())?.ok_or_else(|| "gone".into())
}

/// `changes.revert` and the timer: put the files back, then run `onRevert`.
pub fn revert(shared: &Shared, id: &str, source: &str) -> Result<Change, String> {
    let c = pending(shared, id)?;
    let now = now_secs();
    let hreq = HelperRequest { action: "files.restore".into(), id: Some(id.to_owned()), ..Default::default() };
    let (state, detail) = match crate::helper::call(&shared.helper_socket, &hreq) {
        Ok(r) if r.ok => (ChangeState::Reverted, r.detail),
        Ok(r) => (ChangeState::Failed, r.detail),
        Err(e) => (ChangeState::Failed, e),
    };
    {
        let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        finish(&st, id, state, &detail, now).map_err(|e| e.to_string())?;
    }
    let (outcome, title) = if state == ChangeState::Reverted {
        (ActionOutcome::Ok, format!("change reverted: {}", note_of(&c)))
    } else {
        (ActionOutcome::Failed, format!("could not revert {}", note_of(&c)))
    };
    announce(shared, now, source, id, outcome, &detail, title);
    if state == ChangeState::Reverted {
        if let Some(a) = &c.on_revert {
            crate::actions::run(shared, a, &format!("rollback {id}"));
        }
    }
    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    get(&st, id).map_err(|e| e.to_string())?.ok_or_else(|| "gone".into())
}

/// Reverts every pending change past its deadline (the dead-man timer).
pub fn tick(shared: &Shared) {
    let now = now_secs();
    let due: Vec<String> = {
        let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        let r = st
            .conn()
            .prepare_cached("SELECT id FROM changes WHERE state = 'pending' AND deadline <= ?1")
            .and_then(|mut q| q.query_map([now], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>());
        let _ = st.conn().execute(
            "DELETE FROM changes WHERE state != 'pending' AND rowid NOT IN
             (SELECT rowid FROM changes WHERE state != 'pending' ORDER BY created DESC LIMIT ?1)",
            [KEEP_DONE],
        );
        r.unwrap_or_default()
    };
    for id in due {
        crate::warn!("change {id} was not confirmed in time; reverting");
        if let Err(e) = revert(shared, &id, "dead-man timer") {
            crate::warn!("change {id}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Config, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let data = d.path().join("srv");
        std::fs::create_dir_all(&data).unwrap();
        let mut c = Config::default();
        c.rollback.paths = vec![data.to_string_lossy().into_owned()];
        let store = d.path().join("store");
        (d, c, store)
    }

    #[test]
    fn snapshot_restore_and_new_files() {
        let (d, c, store) = setup();
        let f = d.path().join("srv/compose.yml");
        std::fs::write(&f, "v1").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
        let new = d.path().join("srv/new.env");
        let p = |x: &PathBuf| x.to_string_lossy().into_owned();
        let id = "0123456789abcdef";
        let r = snapshot(&c, &store, id, &[p(&f), p(&new)]).unwrap();
        assert_eq!(r, "saved 2 files (1 new, removed on revert)");
        assert!(snapshot(&c, &store, id, &[p(&f)]).is_err(), "id reuse");
        std::fs::write(&f, "v2 broken").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o666)).unwrap();
        std::fs::write(&new, "x").unwrap();
        assert_eq!(restore(&store, id).unwrap(), "put back 1 file, removed 1 new");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        assert_eq!(std::fs::metadata(&f).unwrap().mode() & 0o777, 0o640);
        assert!(!new.exists());
        assert!(!store.join(id).exists());
        assert!(restore(&store, id).is_err());
    }

    #[test]
    fn refuses_paths_root_did_not_list() {
        let (d, c, store) = setup();
        let id = "00000000000000aa";
        let outside = d.path().join("other.txt");
        std::fs::write(&outside, "x").unwrap();
        for bad in [
            outside.to_string_lossy().into_owned(),
            format!("{}/srv/../other.txt", d.path().display()),
            "relative.txt".into(),
            "/etc/nightgrid-node/config.toml".into(),
        ] {
            assert!(snapshot(&c, &store, id, std::slice::from_ref(&bad)).is_err(), "{bad}");
        }
        // A symlinked directory inside a listed path that points outside it.
        crate::compat::fs::symlink(d.path(), d.path().join("srv/escape")).unwrap();
        let via = format!("{}/srv/escape/other.txt", d.path().display());
        assert!(snapshot(&c, &store, id, &[via]).unwrap_err().contains("not under"));
        // A symlink as the file itself.
        crate::compat::fs::symlink(&outside, d.path().join("srv/link")).unwrap();
        let link = format!("{}/srv/link", d.path().display());
        assert!(snapshot(&c, &store, id, &[link]).unwrap_err().contains("not a regular file"));
        assert!(snapshot(&c, &store, "../x", &[]).is_err());
        assert!(!store.join(id).exists());
        // Too big.
        let big = d.path().join("srv/big");
        std::fs::write(&big, vec![0u8; (MAX_CHANGE_FILE_BYTES + 1) as usize]).unwrap();
        assert!(snapshot(&c, &store, id, &[big.to_string_lossy().into_owned()]).unwrap_err().contains("larger"));
    }

    #[test]
    fn discard_is_idempotent() {
        let (d, c, store) = setup();
        let f = d.path().join("srv/a");
        std::fs::write(&f, "a").unwrap();
        let id = "abcdefabcdefabcd";
        snapshot(&c, &store, id, &[f.to_string_lossy().into_owned()]).unwrap();
        assert_eq!(discard(&store, id).unwrap(), "snapshot removed");
        assert_eq!(discard(&store, id).unwrap(), "no snapshot left");
    }

    #[cfg(unix)]
    #[test]
    fn config_paths() {
        use crate::config::rollback_path_ok as ok;
        assert!(ok("/srv/compose") && ok("/etc/ssh/sshd_config"));
        for bad in ["/", "srv", "/srv/", "/srv/../etc", "/proc/x", "/etc/nightgrid-node", "/a b", "/a\"b"] {
            assert!(!ok(bad), "{bad}");
        }
    }

    /// A helper on a socket that does the real file work in a temp store.
    #[cfg(unix)]
    fn fake_helper(sock: PathBuf, config: Config, store: PathBuf) -> std::sync::mpsc::Receiver<String> {
        use std::io::{BufRead, BufReader};
        struct Log(std::sync::mpsc::Sender<String>);
        impl crate::helper::Runner for Log {
            fn run(&mut self, prog: &str, args: &[&str]) -> Result<(), String> {
                let _ = self.0.send(format!("{prog} {}", args.join(" ")));
                Ok(())
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let l = std::os::unix::net::UnixListener::bind(sock).unwrap();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(s) = s else { break };
                let mut line = String::new();
                BufReader::new(&s).read_line(&mut line).unwrap();
                let req: HelperRequest = serde_json::from_str(&line).unwrap();
                let r = crate::helper::handle(
                    &config,
                    &crate::fixes::LocalFixes::default(),
                    &req,
                    &store,
                    &mut Log(tx.clone()),
                );
                let mut out = serde_json::to_vec(&r).unwrap();
                out.push(b'\n');
                (&s).write_all(&out).unwrap();
            }
        });
        rx
    }

    #[cfg(unix)]
    #[test]
    fn dead_man_timer_reverts_and_confirm_keeps() {
        let (d, mut c, store) = setup();
        c.actions.allow = vec!["files.rollback".into(), "unit.restart".into()];
        c.actions.units = vec!["sshd.service".into()];
        let f = d.path().join("srv/sshd_config");
        std::fs::write(&f, "Port 22").unwrap();
        let mut shared = crate::daemon::Shared::new(c.clone(), Store::open_in_memory().unwrap());
        std::sync::Arc::get_mut(&mut shared).unwrap().helper_socket = d.path().join("h.sock");
        let ran = fake_helper(d.path().join("h.sock"), c, store.clone());

        let begin_req = ChangeBegin {
            paths: vec![f.to_string_lossy().into_owned()],
            confirm_within_secs: 60,
            on_revert: Some(ActionRequest {
                action: "unit.restart".into(),
                target: Some("sshd.service".into()),
                dry_run: false,
            }),
            note: "move ssh to 2222".into(),
        };
        let ch = begin(&shared, &begin_req, "app (uid 1000)").unwrap();
        assert_eq!(ch.state, ChangeState::Pending);
        std::fs::write(&f, "Port 2222").unwrap();
        tick(&shared);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "Port 2222", "not due yet");
        // The app never came back: move the deadline into the past.
        shared.store.lock().unwrap().conn().execute("UPDATE changes SET deadline = 0", []).unwrap();
        tick(&shared);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "Port 22");
        let l = list(&shared.store.lock().unwrap()).unwrap();
        assert_eq!(l.changes[0].state, ChangeState::Reverted);
        assert_eq!(ran.recv_timeout(std::time::Duration::from_secs(2)).unwrap(), "systemctl restart -- sshd.service");
        assert!(confirm(&shared, &ch.id, "app").unwrap_err().contains("already reverted"));

        // A confirmed change stays, and its snapshot goes.
        let ch = begin(&shared, &ChangeBegin { on_revert: None, ..begin_req.clone() }, "app").unwrap();
        std::fs::write(&f, "Port 2222").unwrap();
        assert_eq!(confirm(&shared, &ch.id, "app").unwrap().state, ChangeState::Confirmed);
        assert!(!store.join(&ch.id).exists());
        shared.store.lock().unwrap().conn().execute("UPDATE changes SET deadline = 0", []).unwrap();
        tick(&shared);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "Port 2222");

        // Everything is in the audit log and the timeline.
        let st = shared.store.lock().unwrap();
        let log = crate::actions::log(&st, 0, 100).unwrap();
        assert!(log.verified);
        let titles: Vec<String> = st.events_after(0, 100).unwrap().into_iter().map(|e| e.title).collect();
        assert!(titles.contains(&"change reverted: \"move ssh to 2222\"".to_string()), "{titles:?}");
        assert!(titles.contains(&"change confirmed: \"move ssh to 2222\"".to_string()), "{titles:?}");
        assert!(titles.contains(&"restarted sshd.service".to_string()), "{titles:?}");
    }

    #[test]
    fn begin_refuses_without_roots_allow() {
        let (d, c, _) = setup();
        let shared = crate::daemon::Shared::new(c, Store::open_in_memory().unwrap());
        let r = ChangeBegin {
            paths: vec![format!("{}/srv/x", d.path().display())],
            confirm_within_secs: 60,
            ..Default::default()
        };
        assert!(begin(&shared, &r, "app").unwrap_err().contains("not allowed"));
    }
}
