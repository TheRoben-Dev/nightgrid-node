//! Typed actions and their audit log (N4).
//!
//! Nothing runs unless root listed the action type in `[actions] allow`.
//! An action is a named operation with a checked target, carried out
//! through the container engine's API, never a shell string. Every
//! request, dry or real, refused or done, is appended to `action_log`,
//! where each row's hash covers its fields and the previous row's hash:
//! editing or removing a row breaks the chain, which `actions.log` reports.
//! Each entry is also an `action` event, so the app sees it on reconnect.

use nightgrid_node_proto::{
    action, ActionOutcome, ActionRecord, ActionRequest, ActionsLogResult, ActionsPolicy, ContainerAccess, EventKind,
    Level,
};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::daemon::{now_secs, Shared};
use crate::events::NewEvent;
use crate::store::{Result, Store};

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// Rows kept; older ones are dropped (the chain is checked from the oldest kept).
const KEEP_ROWS: i64 = 100_000;
pub const MAX_LOG: usize = 500;

pub fn policy(config: &Config) -> ActionsPolicy {
    ActionsPolicy {
        allowed: config.actions.allow.clone(),
        known: action::ALL.iter().map(|s| (*s).to_owned()).collect(),
        max_per_hour: config.actions.max_per_hour,
        units: config.actions.units.clone(),
        helper: crate::helper::installed(std::path::Path::new(crate::helper::SOCKET)),
    }
}

/// A name the engine would accept, and safe to put in a URL path.
pub fn valid_container(name: &str) -> bool {
    let mut c = name.chars();
    c.next().is_some_and(|f| f.is_ascii_alphanumeric())
        && name.len() <= 128
        && c.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
}

fn verb(a: &str) -> (&'static str, &'static str) {
    match a {
        action::CONTAINER_RESTART => ("restart", "restarted"),
        action::CONTAINER_START => ("start", "started"),
        action::CONTAINER_STOP => ("stop", "stopped"),
        action::IMAGES_PRUNE => ("prune images", "pruned images"),
        action::UNIT_RESTART => ("restart", "restarted"),
        action::UNIT_START => ("start", "started"),
        action::UNIT_STOP => ("stop", "stopped"),
        action::POWER_REBOOT => ("reboot", "rebooting"),
        action::POWER_POWEROFF => ("power off", "powering off"),
        _ => ("run", "ran"),
    }
}

fn outcome_str(o: ActionOutcome) -> &'static str {
    match o {
        ActionOutcome::Ok => "ok",
        ActionOutcome::DryRun => "dryRun",
        ActionOutcome::Failed => "failed",
        ActionOutcome::Refused => "refused",
    }
}

fn outcome_from(s: &str) -> ActionOutcome {
    match s {
        "ok" => ActionOutcome::Ok,
        "dryRun" => ActionOutcome::DryRun,
        "failed" => ActionOutcome::Failed,
        _ => ActionOutcome::Refused,
    }
}

#[allow(clippy::too_many_arguments)]
fn digest(
    prev: &str,
    at: i64,
    source: &str,
    act: &str,
    target: &str,
    dry: bool,
    outcome: &str,
    detail: &str,
) -> String {
    let mut h = Sha256::new();
    for part in [prev, &at.to_string(), source, act, target, if dry { "1" } else { "0" }, outcome, detail] {
        h.update(part.as_bytes());
        h.update(b"\n");
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Appends one entry and returns it with its `seq` and hash.
pub fn append(
    store: &Store,
    at: i64,
    source: &str,
    req: &ActionRequest,
    outcome: ActionOutcome,
    detail: &str,
) -> Result<ActionRecord> {
    let target = req.target.clone().unwrap_or_default();
    let prev: String = store
        .conn()
        .query_row("SELECT hash FROM action_log ORDER BY seq DESC LIMIT 1", [], |r| r.get(0))
        .optional()?
        .unwrap_or_else(|| GENESIS.to_owned());
    let o = outcome_str(outcome);
    let hash = digest(&prev, at, source, &req.action, &target, req.dry_run, o, detail);
    store.conn().execute(
        "INSERT INTO action_log (at, source, action, target, dry_run, outcome, detail, prev, hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![at, source, req.action, target, req.dry_run, o, detail, prev, hash],
    )?;
    let seq = store.conn().last_insert_rowid();
    store
        .conn()
        .execute("DELETE FROM action_log WHERE seq <= (SELECT max(seq) FROM action_log) - ?1", params![KEEP_ROWS])?;
    Ok(ActionRecord {
        seq,
        at,
        source: source.to_owned(),
        action: req.action.clone(),
        target: req.target.clone(),
        dry_run: req.dry_run,
        outcome,
        detail: detail.to_owned(),
        hash,
    })
}

/// Entries after `after`, and whether the chain holds for them.
pub fn log(store: &Store, after: i64, limit: usize) -> Result<ActionsLogResult> {
    let mut st = store.conn().prepare_cached(
        "SELECT seq, at, source, action, target, dry_run, outcome, detail, prev, hash
         FROM action_log WHERE seq > ?1 ORDER BY seq LIMIT ?2",
    )?;
    let rows: Vec<(ActionRecord, String)> = st
        .query_map(params![after, (limit + 1) as i64], |r| {
            let target: String = r.get(4)?;
            Ok((
                ActionRecord {
                    seq: r.get(0)?,
                    at: r.get(1)?,
                    source: r.get(2)?,
                    action: r.get(3)?,
                    target: (!target.is_empty()).then_some(target),
                    dry_run: r.get(5)?,
                    outcome: outcome_from(&r.get::<_, String>(6)?),
                    detail: r.get(7)?,
                    hash: r.get(9)?,
                },
                r.get::<_, String>(8)?,
            ))
        })?
        .collect::<Result<_>>()?;
    let more = rows.len() > limit;
    let rows = &rows[..rows.len().min(limit)];
    // The entry before the first one returned, if it is still kept.
    let mut expect: Option<String> = match rows.first() {
        Some((first, _)) => store
            .conn()
            .query_row("SELECT hash FROM action_log WHERE seq < ?1 ORDER BY seq DESC LIMIT 1", [first.seq], |r| {
                r.get(0)
            })
            .optional()?,
        None => None,
    };
    let mut verified = true;
    for (rec, prev) in rows {
        if expect.as_deref().is_some_and(|e| e != prev) {
            verified = false;
        }
        let o = outcome_str(rec.outcome);
        let t = rec.target.as_deref().unwrap_or("");
        if digest(prev, rec.at, &rec.source, &rec.action, t, rec.dry_run, o, &rec.detail) != rec.hash {
            verified = false;
        }
        expect = Some(rec.hash.clone());
    }
    Ok(ActionsLogResult {
        next: rows.last().map_or(after, |(r, _)| r.seq),
        records: rows.iter().map(|(r, _)| r.clone()).collect(),
        more,
        verified,
    })
}

/// Real runs (not dry, not refused) in the last hour.
fn runs_last_hour(store: &Store, now: i64) -> Result<u32> {
    store.conn().query_row(
        "SELECT count(*) FROM action_log WHERE at > ?1 AND dry_run = 0 AND outcome IN ('ok', 'failed')",
        [now - 3600],
        |r| r.get(0),
    )
}

/// Decides whether `req` may run; `Err` is the refusal reason.
fn check(shared: &Shared, req: &ActionRequest, now: i64) -> std::result::Result<(), String> {
    if !action::ALL.contains(&req.action.as_str()) {
        return Err(format!("unknown action {:?}", req.action));
    }
    if req.action == action::FILES_ROLLBACK {
        return Err("files.rollback runs through changes.begin".into());
    }
    if !shared.config.actions.allow.iter().any(|a| a == &req.action) {
        return Err(format!(
            "{} is not allowed on this host (root can add it to [actions] allow in /etc/nightgrid-node/config.toml)",
            req.action
        ));
    }
    if action::needs_helper(&req.action) {
        if action::takes_unit(&req.action) {
            let unit = req.target.as_deref().unwrap_or("");
            if !shared.config.actions.units.iter().any(|u| u == unit) {
                return Err(format!(
                    "{unit:?} is not a unit root listed (root can add it to [actions] units in \
                     /etc/nightgrid-node/config.toml)"
                ));
            }
        }
        if !crate::helper::installed(&shared.helper_socket) {
            return Err("the root helper isn't installed (root can run: nightgrid-node allow helper)".into());
        }
        return rate_limit(shared, req, now);
    }
    let containers = shared.containers.lock().unwrap_or_else(|e| e.into_inner()).result.clone();
    if containers.access != ContainerAccess::Ok {
        return Err("no access to the container engine (root can run: nightgrid-node allow docker)".into());
    }
    if action::takes_container(&req.action) {
        let name = req.target.as_deref().unwrap_or("");
        if !valid_container(name) {
            return Err(format!("not a container name: {name:?}"));
        }
        if !containers.containers.iter().any(|c| c.name == name) {
            return Err(format!("no container named {name}"));
        }
    }
    rate_limit(shared, req, now)
}

fn rate_limit(shared: &Shared, req: &ActionRequest, now: i64) -> std::result::Result<(), String> {
    if !req.dry_run {
        let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        let n = runs_last_hour(&st, now).map_err(|e| e.to_string())?;
        if n >= shared.config.actions.max_per_hour {
            return Err(format!("rate limit: {n} actions ran in the last hour (max_per_hour)"));
        }
    }
    Ok(())
}

fn human_bytes(b: u64) -> String {
    crate::alerts::human_bytes(b)
}

/// Carries out an allowed, checked action through the engine.
fn execute(shared: &Shared, req: &ActionRequest) -> std::result::Result<String, String> {
    if action::needs_helper(&req.action) {
        let r = crate::helper::call(&shared.helper_socket, &req.into())?;
        return if r.ok { Ok(r.detail) } else { Err(r.detail) };
    }
    let sock = &shared.config.docker.socket;
    let name = req.target.as_deref().unwrap_or("");
    let (_, done) = verb(&req.action);
    match req.action.as_str() {
        action::CONTAINER_RESTART => crate::docker::post(sock, &format!("/containers/{name}/restart?t=10")),
        action::CONTAINER_START => crate::docker::post(sock, &format!("/containers/{name}/start")),
        action::CONTAINER_STOP => crate::docker::post(sock, &format!("/containers/{name}/stop?t=10")),
        action::IMAGES_PRUNE => {
            let body = crate::docker::post(sock, "/images/prune?filters=%7B%22dangling%22%3A%5B%22true%22%5D%7D")
                .map_err(|e| e.message)?;
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let n = v["ImagesDeleted"].as_array().map_or(0, Vec::len);
            let freed = v["SpaceReclaimed"].as_u64().unwrap_or(0);
            return Ok(format!("removed {n} image layers, freed {}", human_bytes(freed)));
        }
        other => return Err(format!("unknown action {other}")),
    }
    .map(|_| format!("{done} {name}"))
    .map_err(|e| e.message)
}

/// Checks, runs (or dry-runs) and logs one action. `source` says who asked.
pub fn run(shared: &Shared, req: &ActionRequest, source: &str) -> ActionRecord {
    let now = now_secs();
    let (word, _) = verb(&req.action);
    let what = match req.target.as_deref() {
        Some(t) if action::takes_container(&req.action) || action::takes_unit(&req.action) => format!("{word} {t}"),
        _ => word.to_owned(),
    };
    let (outcome, detail) = match check(shared, req, now) {
        Err(why) => (ActionOutcome::Refused, why),
        Ok(()) if req.dry_run => (ActionOutcome::DryRun, format!("would {what}")),
        Ok(()) => match execute(shared, req) {
            Ok(d) => (ActionOutcome::Ok, d),
            Err(e) => (ActionOutcome::Failed, format!("could not {what}: {e}")),
        },
    };
    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    let rec = match append(&st, now, source, req, outcome, &detail) {
        Ok(r) => r,
        Err(e) => {
            crate::error!("could not write the action log: {e}");
            ActionRecord {
                seq: 0,
                at: now,
                source: source.to_owned(),
                action: req.action.clone(),
                target: req.target.clone(),
                dry_run: req.dry_run,
                outcome,
                detail: detail.clone(),
                hash: String::new(),
            }
        }
    };
    let (title, level) = match outcome {
        ActionOutcome::Ok => (detail.clone(), Level::Info),
        ActionOutcome::DryRun => (format!("dry run: would {what}"), Level::Info),
        ActionOutcome::Failed => (format!("could not {what}"), Level::Warn),
        ActionOutcome::Refused => (format!("refused to {what}"), Level::Warn),
    };
    let ev = NewEvent::new(now, EventKind::Action, level, title, format!("{source}: {detail}"));
    if let Err(e) = shared.bus.record(&st, &ev) {
        crate::warn!("could not record an action event: {e}");
    }
    crate::info!("action {} {} by {source}: {detail}", req.action, req.target.as_deref().unwrap_or(""));
    rec
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(a: &str, t: Option<&str>, dry: bool) -> ActionRequest {
        ActionRequest { action: a.into(), target: t.map(str::to_owned), dry_run: dry }
    }

    #[test]
    fn chain_verifies_and_detects_edits() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..5 {
            append(
                &store,
                100 + i,
                "app (uid 1000)",
                &req("container.restart", Some("web"), i % 2 == 0),
                ActionOutcome::DryRun,
                "x",
            )
            .unwrap();
        }
        let all = log(&store, 0, 100).unwrap();
        assert_eq!(all.records.len(), 5);
        assert!(all.verified);
        let page = log(&store, 2, 2).unwrap();
        assert_eq!((page.records[0].seq, page.next, page.more, page.verified), (3, 4, true, true));

        // Edit a row's detail: its hash no longer matches.
        store.conn().execute("UPDATE action_log SET detail = 'y' WHERE seq = 3", []).unwrap();
        assert!(!log(&store, 0, 100).unwrap().verified);
        assert!(log(&store, 3, 100).unwrap().verified, "rows after the edit still chain to each other");
        store.conn().execute("UPDATE action_log SET detail = 'x' WHERE seq = 3", []).unwrap();
        // Remove a row: the next one no longer links.
        store.conn().execute("DELETE FROM action_log WHERE seq = 3", []).unwrap();
        assert!(!log(&store, 0, 100).unwrap().verified);
    }

    #[test]
    fn container_names() {
        assert!(valid_container("web") && valid_container("my_app-1.db"));
        for bad in ["", "-x", "../x", "a/b", "a b", "a?x=1", &"x".repeat(200)] {
            assert!(!valid_container(bad), "{bad}");
        }
    }

    #[test]
    fn refuses_what_root_did_not_allow_and_logs_it() {
        let shared = crate::daemon::Shared::new(Config::default(), Store::open_in_memory().unwrap());
        let r = run(&shared, &req("container.restart", Some("web"), false), "app (uid 1000)");
        assert_eq!(r.outcome, ActionOutcome::Refused);
        assert!(r.detail.contains("not allowed"), "{}", r.detail);
        let r = run(&shared, &req("rm -rf /", None, false), "app (uid 1000)");
        assert_eq!((r.outcome, r.detail.as_str()), (ActionOutcome::Refused, "unknown action \"rm -rf /\""));
        let st = shared.store.lock().unwrap();
        let l = log(&st, 0, 10).unwrap();
        assert_eq!((l.records.len(), l.verified), (2, true));
        let ev = st.events_after(0, 10).unwrap();
        assert_eq!(ev[0].title, "refused to restart web");
        assert_eq!(ev[0].kind, EventKind::Action);
    }

    #[test]
    fn allowed_but_no_engine_access_or_unknown_container() {
        let mut cfg = Config::default();
        cfg.actions.allow = vec!["container.restart".into()];
        let shared = crate::daemon::Shared::new(cfg, Store::open_in_memory().unwrap());
        let r = run(&shared, &req("container.restart", Some("web"), true), "rule r1");
        assert!(r.detail.contains("no access to the container engine"), "{}", r.detail);
        {
            let mut c = shared.containers.lock().unwrap();
            c.result.access = ContainerAccess::Ok;
            c.result.containers = vec![nightgrid_node_proto::ContainerInfo {
                id: "abc".into(),
                name: "web".into(),
                image: "nginx".into(),
                state: "running".into(),
                status: "Up".into(),
                health: None,
                project: None,
            }];
        }
        let r = run(&shared, &req("container.restart", Some("db"), true), "rule r1");
        assert_eq!(r.detail, "no container named db");
        let r = run(&shared, &req("container.restart", Some("web"), true), "rule r1");
        assert_eq!((r.outcome, r.detail.as_str()), (ActionOutcome::DryRun, "would restart web"));
    }

    #[test]
    fn unit_actions_go_through_the_helper() {
        use std::io::{BufRead, BufReader, Write};
        let d = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        c.actions.allow = vec!["unit.restart".into()];
        c.actions.units = vec!["nginx.service".into()];
        let mut shared = crate::daemon::Shared::new(c, Store::open_in_memory().unwrap());
        std::sync::Arc::get_mut(&mut shared).unwrap().helper_socket = d.path().join("h.sock");
        let r = run(&shared, &req("unit.restart", Some("sshd.service"), false), "app (uid 1000)");
        assert!(r.outcome == ActionOutcome::Refused && r.detail.contains("[actions] units"), "{}", r.detail);
        let r = run(&shared, &req("unit.restart", Some("nginx.service"), false), "app (uid 1000)");
        assert!(r.detail.contains("allow helper"), "{}", r.detail);

        let l = std::os::unix::net::UnixListener::bind(d.path().join("h.sock")).unwrap();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&s).read_line(&mut line).unwrap();
            assert!(line.contains("nginx.service"));
            (&s).write_all(b"{\"ok\":true,\"detail\":\"restarted nginx.service\"}\n").unwrap();
        });
        let r = run(&shared, &req("unit.restart", Some("nginx.service"), true), "app (uid 1000)");
        assert_eq!((r.outcome, r.detail.as_str()), (ActionOutcome::DryRun, "would restart nginx.service"));
        let r = run(&shared, &req("unit.restart", Some("nginx.service"), false), "rule r");
        assert_eq!((r.outcome, r.detail.as_str()), (ActionOutcome::Ok, "restarted nginx.service"));
        t.join().unwrap();
        assert!(policy(&shared.config).units == ["nginx.service"]);
    }
}
