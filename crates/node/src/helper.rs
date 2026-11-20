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
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};

use crate::compat::LocalStream as UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use nightgrid_node_proto::{action, ActionRequest};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::fixes::LocalFixes;

/// Where the helper's socket unit listens.
#[cfg(unix)]
pub const SOCKET: &str = "/run/nightgrid-node-helper.sock";
/// The Windows helper service's pipe (`crate::win::helper`).
#[cfg(windows)]
pub const SOCKET: &str = crate::win::helper::PIPE;
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
    pub fn no(detail: impl Into<String>) -> Self {
        Self { ok: false, detail: detail.into() }
    }
}

/// Runs a fixed command; `Err` carries what it printed.
pub trait Runner {
    fn run(&mut self, prog: &str, args: &[&str]) -> Result<(), String>;
    /// Like `run`, but also returns what it printed (stdout, then stderr).
    fn output(&mut self, prog: &str, args: &[&str]) -> Result<String, String> {
        self.run(prog, args).map(|()| String::new())
    }
    /// `restart`, `start` or `stop` a unit: systemd on Linux, the Service
    /// Control Manager on Windows.
    fn unit(&mut self, verb: &str, unit: &str) -> Result<(), String> {
        self.run("systemctl", &[verb, "--", unit])
    }
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

    fn output(&mut self, prog: &str, args: &[&str]) -> Result<String, String> {
        let out =
            Command::new(prog).args(args).env("SYSTEMD_PAGER", "").output().map_err(|e| format!("{prog}: {e}"))?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        if out.status.success() {
            Ok(text)
        } else {
            let tail: Vec<&str> = text.lines().rev().filter(|l| !l.trim().is_empty()).take(3).collect();
            let msg: Vec<&str> = tail.into_iter().rev().collect();
            let msg = crate::events::tidy(msg.join(" / ").trim(), 300);
            Err(if msg.is_empty() { format!("{prog} failed ({})", out.status) } else { msg })
        }
    }

    #[cfg(windows)]
    fn unit(&mut self, verb: &str, unit: &str) -> Result<(), String> {
        crate::win::scm::control(verb, unit)
    }
}

/// Whether this platform's helper can carry the action out. Windows has
/// unit (service) and power actions so far; blocks, rollbacks and the
/// local fixes come later (docs/WINDOWS.md, W2b).
pub fn supported(a: &str) -> bool {
    !cfg!(windows)
        || !crate::fixes::needs_helper(a)
        || action::takes_unit(a)
        || matches!(a, action::POWER_REBOOT | action::POWER_POWEROFF)
}

/// `shutdown.exe` by its full path (the helper never searches `PATH`).
#[cfg(windows)]
fn shutdown_exe() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    format!(r"{root}\System32\shutdown.exe")
}

/// Checks a request against root's config and carries it out.
pub fn handle(config: &Config, local: &LocalFixes, req: &HelperRequest, store: &Path, run: &mut dyn Runner) -> Reply {
    let a = req.action.as_str();
    if !supported(a) || (cfg!(windows) && a.starts_with("files.")) {
        return Reply::no(format!("{a} is not available on Windows yet"));
    }
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
    if !crate::fixes::needs_helper(a) || a == action::FILES_ROLLBACK {
        return Reply::no(format!("the helper doesn't do {a:?}"));
    }
    if !crate::fixes::allowed(config, local, a) {
        return Reply::no(format!("{a} is not in [actions] allow"));
    }
    if crate::fixes::is_local(a) {
        return crate::fixes::helper_run(config, local, a, req.target.as_deref(), run);
    }
    if action::takes_address(a) {
        return crate::firewall::handle(&config.block, a, req.target.as_deref().unwrap_or(""), run);
    }
    if action::takes_unit(a) {
        let unit = req.target.as_deref().unwrap_or("");
        if !crate::fixes::unit_listed(config, local, unit) {
            return Reply::no(format!("{unit:?} is not in [actions] units"));
        }
        let (verb, done) = match a {
            action::UNIT_RESTART => ("restart", "restarted"),
            action::UNIT_START => ("start", "started"),
            _ => ("stop", "stopped"),
        };
        return match run.unit(verb, unit) {
            Ok(()) => Reply { ok: true, detail: format!("{done} {unit}") },
            Err(e) => Reply::no(e),
        };
    }
    power(a, run)
}

#[cfg(unix)]
fn power(a: &str, run: &mut dyn Runner) -> Reply {
    let (flag, what) = if a == action::POWER_REBOOT { ("-r", "reboot") } else { ("-P", "power-off") };
    match run.run("shutdown", &[flag, "+1", "requested from Nightgrid Node"]) {
        Ok(()) => Reply { ok: true, detail: format!("{what} in 1 minute (cancel on the host with: shutdown -c)") },
        Err(e) => Reply::no(e),
    }
}

/// `shutdown.exe /r` or `/s` a minute ahead, with a planned reason code,
/// so it shows in the System log; an administrator cancels with `/a`.
#[cfg(windows)]
fn power(a: &str, run: &mut dyn Runner) -> Reply {
    let (flag, what) = if a == action::POWER_REBOOT { ("/r", "reboot") } else { ("/s", "power-off") };
    let exe = shutdown_exe();
    match run.run(&exe, &[flag, "/t", "60", "/d", "p:0:0", "/c", "Requested from Nightgrid Node"]) {
        Ok(()) => Reply { ok: true, detail: format!("{what} in 1 minute (cancel on the host with: shutdown /a)") },
        Err(e) => Reply::no(e),
    }
}

/// uid of the service account (none on Windows, where callers are SIDs).
#[cfg(windows)]
pub fn service_uid() -> Option<u32> {
    None
}

/// uid of the service account, or `None` if it doesn't exist.
#[cfg(unix)]
pub fn service_uid() -> Option<u32> {
    let name = std::ffi::CString::new(crate::install::USER).ok()?;
    // SAFETY: getpwnam takes a valid C string; the result is read at once.
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    // SAFETY: non-null pointers from getpwnam point to a valid passwd.
    (!pw.is_null()).then(|| unsafe { (*pw).pw_uid })
}

#[cfg(unix)]
pub fn peer_uid(fd: i32) -> Option<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for getsockopt to write.
    let r = unsafe {
        libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len)
    };
    (r == 0).then_some(cred.uid)
}

/// `nightgrid-node helper` on Windows: the helper service's command line.
#[cfg(windows)]
pub fn serve(config_path: &Path) -> Result<(), String> {
    crate::win::helper::serve(&crate::win::helper::ADMIN, config_path)
}

/// `nightgrid-node helper`: serves the one connection on stdin/stdout.
#[cfg(unix)]
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
                    Ok(config) => {
                        // A broken fixes.toml adds nothing (config.toml still counts).
                        let local = LocalFixes::load(&crate::fixes::path_for(config_path)).unwrap_or_else(|e| {
                            crate::warn!("helper: {e}");
                            LocalFixes::default()
                        });
                        handle(&config, &local, &req, Path::new(ROLLBACK_DIR), &mut RealRunner)
                    }
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

/// Whether the helper is installed (`allow helper`). A pipe exists only
/// while its server runs, so the helper's service is asked for instead.
#[cfg(windows)]
pub fn installed(socket: &Path) -> bool {
    crate::win::helper::by_pipe(socket).is_some_and(crate::win::helper::installed)
}

/// Whether the helper's socket is there (`allow helper`).
#[cfg(unix)]
pub fn installed(socket: &Path) -> bool {
    use crate::compat::fs::FileTypeExt;
    std::fs::metadata(socket).is_ok_and(|m| m.file_type().is_socket())
}

/// Asks the helper (from the service). `Err` is a transport problem.
pub fn call(socket: &Path, req: &HelperRequest) -> Result<Reply, String> {
    #[cfg(windows)]
    let s = if let Some(k) = crate::win::helper::by_pipe(socket) {
        crate::win::helper::connect(k)?
    } else {
        UnixStream::connect(socket).map_err(|e| format!("admin helper ({}): {e}", socket.display()))?
    };
    #[cfg(unix)]
    let s = UnixStream::connect(socket).map_err(|e| format!("root helper ({}): {e}", socket.display()))?;
    // Fixes run up to fixes::MAX_SECS in their own unit; the rest are quick.
    let wait = if crate::fixes::is_local(&req.action) { u64::from(crate::fixes::MAX_SECS) + 60 } else { 150 };
    s.set_read_timeout(Some(Duration::from_secs(wait))).map_err(|e| e.to_string())?;
    exchange(s, req)
}

/// Sends one request on an open connection and reads the reply.
pub fn exchange(mut s: UnixStream, req: &HelperRequest) -> Result<Reply, String> {
    let mut line = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    line.push(b'\n');
    s.write_all(&line).map_err(|e| format!("root helper: {e}"))?;
    let mut answer = String::new();
    BufReader::new(s.take(MAX_REQUEST)).read_line(&mut answer).map_err(|e| format!("root helper: {e}"))?;
    let hint = if cfg!(windows) {
        r"see C:\ProgramData\Nightgrid Node\data\log\helper.log"
    } else {
        "see journalctl -u 'nightgrid-node-helper@*'"
    };
    serde_json::from_str(&answer).map_err(|_| format!("root helper: no answer ({hint})"))
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
        assert!(
            !handle(
                &c,
                &LocalFixes::default(),
                &req("unit.restart", Some("nginx.service")),
                Path::new("/nonexistent"),
                &mut f
            )
            .ok
        );
        c.actions.allow = vec!["unit.restart".into(), "power.reboot".into()];
        let r = handle(
            &c,
            &LocalFixes::default(),
            &req("unit.restart", Some("nginx.service")),
            Path::new("/nonexistent"),
            &mut f,
        );
        assert_eq!(r, Reply::no("\"nginx.service\" is not in [actions] units"));
        c.actions.units = vec!["nginx.service".into()];
        let r = handle(
            &c,
            &LocalFixes::default(),
            &req("unit.restart", Some("nginx.service")),
            Path::new("/nonexistent"),
            &mut f,
        );
        assert_eq!(r, Reply { ok: true, detail: "restarted nginx.service".into() });
        assert!(
            !handle(
                &c,
                &LocalFixes::default(),
                &req("unit.stop", Some("nginx.service")),
                Path::new("/nonexistent"),
                &mut f
            )
            .ok,
            "stop not allowed"
        );
        assert!(
            !handle(
                &c,
                &LocalFixes::default(),
                &req("container.restart", Some("web")),
                Path::new("/nonexistent"),
                &mut f
            )
            .ok
        );
        assert!(
            !handle(&c, &LocalFixes::default(), &req("power.poweroff", None), Path::new("/nonexistent"), &mut f).ok
        );
        let r = handle(&c, &LocalFixes::default(), &req("power.reboot", None), Path::new("/nonexistent"), &mut f);
        let cancel = if cfg!(windows) { "shutdown /a" } else { "shutdown -c" };
        assert!(r.ok && r.detail.contains(cancel), "{r:?}");
        #[cfg(unix)]
        assert_eq!(f.log, ["systemctl restart -- nginx.service", "shutdown -r +1 requested from Nightgrid Node"]);
        #[cfg(windows)]
        assert_eq!(
            f.log,
            [
                "systemctl restart -- nginx.service".to_owned(),
                format!("{} /r /t 60 /d p:0:0 /c Requested from Nightgrid Node", shutdown_exe())
            ]
        );
        f.fail = true;
        let r = handle(
            &c,
            &LocalFixes::default(),
            &req("unit.restart", Some("nginx.service")),
            Path::new("/nonexistent"),
            &mut f,
        );
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
        for ok in ["W32Time", "sshd", "MSSQL$SQLEXPRESS", "wuauserv", "Tomcat9.0"] {
            assert!(action::valid_service(ok), "{ok}");
        }
        for bad in ["", "-x", "a b", "Nightgrid-Node", "nightgrid-node-helper", "..\\x", "a/b", "x;y"] {
            assert!(!action::valid_service(bad), "{bad}");
        }
        // Windows refuses what it can't do yet before asking anyone.
        assert!(supported("unit.restart") && supported("power.reboot") && supported("container.restart"));
        assert_eq!(supported("firewall.block"), !cfg!(windows));
        assert_eq!(supported("files.rollback"), !cfg!(windows));
    }

    #[cfg(unix)]
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
