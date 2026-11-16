//! The root helper (N4b, decision N-010).
//!
//! `nightgrid-node allow helper` installs a socket unit owned by the
//! service account (mode 0600). systemd starts one short-lived root process
//! per connection (`Accept=yes`) running `nightgrid-node helper` with the
//! connection on stdin and stdout. It reads one typed request, checks it
//! against the root-owned config itself (it doesn't trust the service),
//! runs it with fixed arguments and no shell, answers, and exits.
//!
//! Only `unit.restart`/`start`/`stop` on units listed in `[actions] units`,
//! and `power.reboot`/`poweroff`, each also listed in `[actions] allow`.
//! Power is scheduled a minute ahead with `shutdown`, so the service can log
//! it and the app can show it, and root can still cancel with `shutdown -c`.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use nightgrid_node_proto::{action, ActionRequest};
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Where the helper's socket unit listens.
pub const SOCKET: &str = "/run/nightgrid-node-helper.sock";
const MAX_REQUEST: u64 = 64 * 1024;
/// Where file snapshots for rollbacks live (the helper unit's StateDirectory).
pub const ROLLBACK_DIR: &str = "/var/lib/nightgrid-node-rollback";

/// What the service asks the helper. Action requests fit as they are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HelperRequest {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// For `files.*`: the change id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// For `files.snapshot`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

impl From<&ActionRequest> for HelperRequest {
    fn from(r: &ActionRequest) -> Self {
        Self { action: r.action.clone(), target: r.target.clone(), ..Default::default() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    pub detail: String,
}

impl Reply {
    fn no(detail: impl Into<String>) -> Self {
        Self { ok: false, detail: detail.into() }
    }
}

/// Runs a fixed command; `Err` carries what it printed.
pub trait Runner {
    fn run(&mut self, prog: &str, args: &[&str]) -> Result<(), String>;
}

pub struct RealRunner;

impl Runner for RealRunner {
    fn run(&mut self, prog: &str, args: &[&str]) -> Result<(), String> {
        let out =
            Command::new(prog).args(args).env("SYSTEMD_PAGER", "").output().map_err(|e| format!("{prog}: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            let msg = String::from_utf8_lossy(&out.stderr);
            let msg = crate::events::tidy(msg.trim(), 300);
            Err(if msg.is_empty() { format!("{prog} failed ({})", out.status) } else { msg })
        }
    }
}

/// Checks a request against root's config and carries it out.
pub fn handle(config: &Config, req: &HelperRequest, store: &Path, run: &mut dyn Runner) -> Reply {
    let a = req.action.as_str();
    if let Some(op) = a.strip_prefix("files.").filter(|op| matches!(*op, "snapshot" | "restore" | "discard")) {
        if !config.actions.allow.iter().any(|x| x == action::FILES_ROLLBACK) {
            return Reply::no(format!("{} is not in [actions] allow", action::FILES_ROLLBACK));
        }
        let id = req.id.as_deref().unwrap_or("");
        let r = match op {
            "snapshot" => crate::rollback::snapshot(config, store, id, &req.paths),
            "restore" => crate::rollback::restore(store, id),
            _ => crate::rollback::discard(store, id),
        };
        return match r {
            Ok(detail) => Reply { ok: true, detail },
            Err(e) => Reply::no(e),
        };
    }
    if !action::needs_helper(a) || a == action::FILES_ROLLBACK {
        return Reply::no(format!("the helper doesn't do {a:?}"));
    }
    if !config.actions.allow.iter().any(|x| x == a) {
        return Reply::no(format!("{a} is not in [actions] allow"));
    }
    if action::takes_unit(a) {
        let unit = req.target.as_deref().unwrap_or("");
        if !action::valid_unit(unit) || !config.actions.units.iter().any(|u| u == unit) {
            return Reply::no(format!("{unit:?} is not in [actions] units"));
        }
        let (verb, done) = match a {
            action::UNIT_RESTART => ("restart", "restarted"),
            action::UNIT_START => ("start", "started"),
            _ => ("stop", "stopped"),
        };
        return match run.run("systemctl", &[verb, "--", unit]) {
            Ok(()) => Reply { ok: true, detail: format!("{done} {unit}") },
            Err(e) => Reply::no(e),
        };
    }
    let (flag, what) = if a == action::POWER_REBOOT { ("-r", "reboot") } else { ("-P", "power-off") };
    match run.run("shutdown", &[flag, "+1", "requested from Nightgrid Node"]) {
        Ok(()) => Reply { ok: true, detail: format!("{what} in 1 minute (cancel on the host with: shutdown -c)") },
        Err(e) => Reply::no(e),
    }
}

/// uid of the service account, or `None` if it doesn't exist.
fn service_uid() -> Option<u32> {
    let name = std::ffi::CString::new(crate::install::USER).ok()?;
    // SAFETY: getpwnam takes a valid C string; the result is read at once.
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    // SAFETY: non-null pointers from getpwnam point to a valid passwd.
    (!pw.is_null()).then(|| unsafe { (*pw).pw_uid })
}

fn peer_uid(fd: i32) -> Option<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for getsockopt to write.
    let r = unsafe {
        libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len)
    };
    (r == 0).then_some(cred.uid)
}

/// `nightgrid-node helper`: serves the one connection on stdin/stdout.
pub fn serve(config_path: &Path) -> Result<(), String> {
    // SAFETY: systemd hands the accepted connection over as fd 0; the
    // stream takes ownership of it for the life of this process.
    let stream = unsafe { UnixStream::from_raw_fd(0) };
    let uid = peer_uid(stream.as_raw_fd()).ok_or("stdin is not a unix socket (run by the helper's socket unit)")?;
    let reply = if uid == 0 || Some(uid) == service_uid() {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let mut line = String::new();
        let read = BufReader::new((&stream).take(MAX_REQUEST)).read_line(&mut line);
        match (read, serde_json::from_str::<HelperRequest>(&line)) {
            (Ok(_), Ok(req)) => {
                let reply = match Config::load(config_path) {
                    Ok(config) => handle(&config, &req, Path::new(ROLLBACK_DIR), &mut RealRunner),
                    Err(e) => Reply::no(format!("config: {e}")),
                };
                crate::info!(
                    "helper: {} {} for uid {uid}: {}{}",
                    req.action,
                    req.target.as_deref().or(req.id.as_deref()).unwrap_or(""),
                    if reply.ok { "" } else { "refused or failed: " },
                    reply.detail
                );
                reply
            }
            _ => Reply::no("bad request"),
        }
    } else {
        crate::warn!("helper: refused a connection from uid {uid}");
        Reply::no("only the node's service may use the helper")
    };
    let mut out = serde_json::to_vec(&reply).unwrap_or_default();
    out.push(b'\n');
    (&stream).write_all(&out).map_err(|e| e.to_string())
}

/// Whether the helper's socket is there (`allow helper`).
pub fn installed(socket: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(socket).is_ok_and(|m| m.file_type().is_socket())
}

/// Asks the helper (from the service). `Err` is a transport problem.
pub fn call(socket: &Path, req: &HelperRequest) -> Result<Reply, String> {
    let mut s = UnixStream::connect(socket).map_err(|e| format!("root helper ({}): {e}", socket.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(90))).map_err(|e| e.to_string())?;
    let mut line = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    line.push(b'\n');
    s.write_all(&line).map_err(|e| format!("root helper: {e}"))?;
    let mut answer = String::new();
    BufReader::new(s.take(MAX_REQUEST)).read_line(&mut answer).map_err(|e| format!("root helper: {e}"))?;
    serde_json::from_str(&answer)
        .map_err(|_| "root helper: no answer (see journalctl -u 'nightgrid-node-helper@*')".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        log: Vec<String>,
        fail: bool,
    }

    impl Runner for Fake {
        fn run(&mut self, prog: &str, args: &[&str]) -> Result<(), String> {
            self.log.push(format!("{prog} {}", args.join(" ")));
            if self.fail {
                Err("Unit x.service not found.".into())
            } else {
                Ok(())
            }
        }
    }

    fn req(a: &str, t: Option<&str>) -> HelperRequest {
        HelperRequest { action: a.into(), target: t.map(Into::into), ..Default::default() }
    }

    #[test]
    fn checks_roots_config_itself() {
        let mut c = Config::default();
        let mut f = Fake::default();
        assert!(!handle(&c, &req("unit.restart", Some("nginx.service")), Path::new("/nonexistent"), &mut f).ok);
        c.actions.allow = vec!["unit.restart".into(), "power.reboot".into()];
        let r = handle(&c, &req("unit.restart", Some("nginx.service")), Path::new("/nonexistent"), &mut f);
        assert_eq!(r, Reply::no("\"nginx.service\" is not in [actions] units"));
        c.actions.units = vec!["nginx.service".into()];
        let r = handle(&c, &req("unit.restart", Some("nginx.service")), Path::new("/nonexistent"), &mut f);
        assert_eq!(r, Reply { ok: true, detail: "restarted nginx.service".into() });
        assert!(
            !handle(&c, &req("unit.stop", Some("nginx.service")), Path::new("/nonexistent"), &mut f).ok,
            "stop not allowed"
        );
        assert!(!handle(&c, &req("container.restart", Some("web")), Path::new("/nonexistent"), &mut f).ok);
        assert!(!handle(&c, &req("power.poweroff", None), Path::new("/nonexistent"), &mut f).ok);
        let r = handle(&c, &req("power.reboot", None), Path::new("/nonexistent"), &mut f);
        assert!(r.ok && r.detail.contains("shutdown -c"), "{r:?}");
        assert_eq!(f.log, ["systemctl restart -- nginx.service", "shutdown -r +1 requested from Nightgrid Node"]);
        f.fail = true;
        let r = handle(&c, &req("unit.restart", Some("nginx.service")), Path::new("/nonexistent"), &mut f);
        assert_eq!(r, Reply::no("Unit x.service not found."));
    }

    #[test]
    fn unit_names() {
        for ok in ["nginx.service", "getty@tty1.service", "backup.timer", "docker.socket", "a\\x2db.service"] {
            assert!(action::valid_unit(ok), "{ok}");
        }
        for bad in
            ["nginx", "-x.service", ".service", "a b.service", "nightgrid-node.service", "x.mount", "../x.service"]
        {
            assert!(!action::valid_unit(bad), "{bad}");
        }
    }

    #[test]
    fn talks_over_a_socket() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("h.sock");
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(installed(&path) && !installed(d.path()));
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&s).read_line(&mut line).unwrap();
            let r: ActionRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(r.action, "power.reboot");
            (&s).write_all(b"{\"ok\":true,\"detail\":\"reboot in 1 minute\"}\n").unwrap();
        });
        let r = call(&path, &req("power.reboot", None)).unwrap();
        assert!(r.ok);
        t.join().unwrap();
    }
}
