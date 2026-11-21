//! The helper services on Windows (decisions N-080, N-084): what the root
//! helper's and the health helper's socket units are on Linux.
//!
//! - `allow helper` installs `nightgrid-node-helper` ([`ADMIN`]): actions
//!   (restart a listed service, reboot), checked against the config by
//!   [`crate::helper::handle`].
//! - `allow health` installs `nightgrid-node-health` ([`HEALTH`]): fixed
//!   reads (disks, Storage Spaces, Windows Update), by
//!   [`crate::health::watch::handle`]. Its token is write-restricted, so it
//!   can read what an administrator can but change almost nothing.
//!
//! Both run as LocalSystem with demand start and only the privileges they
//! need, have no network (a Service Hardening rule), and have a service DACL
//! that lets the node's own service SID start them and nothing more. The
//! node starts one when it has something to ask and it stops by itself after
//! a few idle minutes, like a socket-activated unit.
//!
//! Each serves its own pipe: remote clients rejected, a DACL for SYSTEM,
//! Administrators and the node's service SID, and every caller checked
//! again from its token. Each request is one JSON line, the same as on
//! Linux, checked against the Administrators-only config; nothing the node
//! says is trusted beyond that.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use windows_sys::Win32::Foundation::{ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, NO_ERROR};
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, SERVICE_ACCEPT_SHUTDOWN,
    SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};

use super::{acl, pipe, scm};
use crate::config::Config;
use crate::fixes::LocalFixes;
use crate::helper::{HelperRequest, Reply};
use crate::install::Report;

/// One of the two helper services.
pub struct Kind {
    pub service: &'static str,
    pub display: &'static str,
    pub pipe: &'static str,
    /// What it is called in messages.
    pub label: &'static str,
    /// The `allow` word.
    pub grant: &'static str,
    pub description: &'static str,
    pub privileges: &'static [&'static str],
    /// A write-restricted service SID (the read-only one).
    pub restricted: bool,
    /// Its log file in the data folder.
    pub log: &'static str,
    /// Answers one request line; `None` when the caller was refused.
    pub answer: fn(&Path, Option<&str>) -> (Vec<u8>, String),
}

/// The admin helper: actions.
pub static ADMIN: Kind = Kind {
    service: "nightgrid-node-helper",
    display: "Nightgrid Node helper",
    pipe: r"\\.\pipe\nightgrid-node-helper",
    label: "admin helper",
    grant: "helper",
    description: "Carries out the Nightgrid Node actions an administrator allowed in its config \
                  (restart a listed service, reboot). Started by the node when needed; no network access.",
    privileges: &["SeShutdownPrivilege", "SeChangeNotifyPrivilege"],
    restricted: false,
    log: "helper.log",
    answer: admin_answer,
};

/// The health helper: read-only health reads.
pub static HEALTH: Kind = Kind {
    service: "nightgrid-node-health",
    display: "Nightgrid Node health",
    pipe: r"\\.\pipe\nightgrid-node-health",
    label: "health helper",
    grant: "health",
    description: "Reads disk health, Storage Spaces and pending Windows updates for Nightgrid Node. \
                  Read-only; started by the node when needed; no network access.",
    privileges: &["SeChangeNotifyPrivilege"],
    restricted: true,
    log: "health.log",
    answer: health_answer,
};

/// The admin helper's service name and pipe, for the rest of the crate.
pub const SERVICE: &str = "nightgrid-node-helper";
pub const PIPE: &str = r"\\.\pipe\nightgrid-node-helper";
pub const HEALTH_PIPE: &str = r"\\.\pipe\nightgrid-node-health";

/// A helper stops after this long without a request.
pub const IDLE: Duration = Duration::from_secs(180);
const MAX_REQUEST: usize = 64 * 1024;

/// The helper for a pipe path, if it is one of theirs.
pub fn by_pipe(p: &Path) -> Option<&'static Kind> {
    [&ADMIN, &HEALTH].into_iter().find(|k| p == Path::new(k.pipe))
}

/// A helper pipe's DACL: SYSTEM and Administrators, and the node's service
/// SID may read and write. Nobody else.
pub fn pipe_sddl(node_sid: Option<&str>) -> String {
    let mut s = String::from("D:P(A;;GA;;;SY)(A;;GA;;;BA)");
    if let Some(sid) = node_sid.filter(|s| s.starts_with("S-1-")) {
        s.push_str(&format!("(A;;GRGW;;;{sid})"));
    }
    s
}

/// A helper service's DACL: Windows' default for a service, plus "query
/// status" and "start" (`LCRP`) for the node's service SID. The node can't
/// stop, change or delete it.
pub fn service_sddl(node_sid: &str) -> String {
    format!(
        "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWLOCRRC;;;IU)\
         (A;;CCLCSWLOCRRC;;;SU)(A;;LCRP;;;{node_sid})"
    )
}

fn node_sid() -> Option<String> {
    pipe::sid_of(&format!("NT SERVICE\\{}", super::SERVICE))
}

/// Whether the helper's service is installed (`allow helper`, `allow health`).
pub fn installed(k: &Kind) -> bool {
    scm::status(k.service).ok().flatten().is_some()
}

// ---------------------------------------------------------------- client

/// Opens a helper's pipe for the node, starting its service when it isn't
/// running. The pipe must belong to SYSTEM or Administrators.
pub fn connect(k: &Kind) -> Result<crate::compat::LocalStream, String> {
    let mut woke = false;
    for _ in 0..50 {
        match pipe::open_system(k.pipe) {
            Ok(f) => return Ok(crate::compat::LocalStream::from_file(f)),
            // No instance: the helper isn't running (or is just stopping).
            Err(e) if e.raw_os_error() == Some(2) => {
                if !woke {
                    wake(k)?;
                    woke = true;
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            Err(e) => return Err(format!("{} ({}): {e}", k.label, k.pipe)),
        }
    }
    Err(format!("{} ({}): it started but its pipe did not appear", k.label, k.pipe))
}

/// Starts a helper, first waiting out an idle stop in progress.
fn wake(k: &Kind) -> Result<(), String> {
    let until = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        match scm::status(k.service)? {
            None => {
                return Err(format!(
                    "the {} isn't installed (an administrator can run: nightgrid-node allow {})",
                    k.label, k.grant
                ))
            }
            Some(s) if s.state == SERVICE_STOP_PENDING && std::time::Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Some(_) => break,
        }
    }
    scm::start(k.service, Duration::from_secs(30)).map_err(|e| format!("{}: {e}", k.label))
}

// ---------------------------------------------------------------- server

static STATUS: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static STOP: AtomicBool = AtomicBool::new(false);
static STOP_NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
static CONFIG: Mutex<Option<PathBuf>> = Mutex::new(None);
static KIND: OnceLock<&'static Kind> = OnceLock::new();
static RESULT: Mutex<Option<Result<(), String>>> = Mutex::new(None);

/// True once the SCM asked this helper to stop: long reads give up.
pub fn stopping() -> bool {
    STOP.load(Ordering::SeqCst)
}

fn stop_notify() -> &'static tokio::sync::Notify {
    STOP_NOTIFY.get_or_init(tokio::sync::Notify::new)
}

fn set(state: u32, exit: u32, wait_ms: u32) {
    let h = STATUS.load(Ordering::SeqCst);
    if h.is_null() {
        return;
    }
    let accept = if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 };
    let st = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accept,
        dwWin32ExitCode: exit,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: wait_ms,
    };
    // SAFETY: a handle from RegisterServiceCtrlHandlerExW and a full struct.
    unsafe { SetServiceStatus(h as SERVICE_STATUS_HANDLE, &st) };
}

unsafe extern "system" fn handler(
    control: u32,
    _t: u32,
    _d: *mut core::ffi::c_void,
    _c: *mut core::ffi::c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            set(SERVICE_STOP_PENDING, NO_ERROR, 20_000);
            if !STOP.swap(true, Ordering::SeqCst) {
                // A read in progress (a Windows Update search can take
                // minutes) kills its child when it sees STOP; anything else
                // still running after 15 s is cut short, so a stop or an
                // uninstall never waits on a request.
                let _ = std::thread::Builder::new().name("stop".into()).spawn(|| {
                    std::thread::sleep(Duration::from_secs(15));
                    crate::warn!("helper: a request was still running at stop; exiting");
                    set(SERVICE_STOPPED, NO_ERROR, 0);
                    std::process::exit(0);
                });
            }
            stop_notify().notify_waiters();
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => 120, // ERROR_CALL_NOT_IMPLEMENTED
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let Some(k) = KIND.get().copied() else { return };
    let name = super::wide(k.service);
    // SAFETY: a static handler; the name lives for the call.
    let h = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(handler), std::ptr::null()) };
    if h.is_null() {
        return;
    }
    STATUS.store(h as *mut _, Ordering::SeqCst);
    set(SERVICE_START_PENDING, NO_ERROR, 10_000);
    let _ = crate::log::to_file(&super::data_dir().join("log").join(k.log));
    let config = CONFIG.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default();
    let r = run(k, &config, k.pipe, node_sid(), IDLE, || set(SERVICE_RUNNING, NO_ERROR, 0));
    if let Err(e) = &r {
        crate::error!("{}: {e}", k.label);
    }
    let exit = if r.is_ok() { NO_ERROR } else { 1066 }; // ERROR_SERVICE_SPECIFIC_ERROR
    *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
    set(SERVICE_STOPPED, exit, 0);
}

/// `nightgrid-node helper` / `health-helper`: a helper service's command
/// line. Started from a console (by an administrator, for a test) it serves
/// the same way until it has been idle.
pub fn serve(k: &'static Kind, config_path: &Path) -> Result<(), String> {
    *CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = Some(config_path.to_owned());
    let _ = KIND.set(k);
    let mut name = super::wide(k.service);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: std::ptr::null_mut(), lpServiceProc: None },
    ];
    // SAFETY: a NULL-terminated table whose strings outlive the call.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } != 0 {
        return RESULT.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or(Ok(()));
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT as i32) {
        return Err(format!("service dispatcher: {err}"));
    }
    if !crate::compat::am_root() {
        return Err(format!("the {} runs as its service (or from an elevated prompt)", k.label));
    }
    run(k, config_path, k.pipe, node_sid(), IDLE, || {})
}

/// Serves the pipe one request at a time until stopped or idle.
pub fn run(
    k: &Kind,
    config_path: &Path,
    name: &str,
    node_sid: Option<String>,
    idle: Duration,
    ready: impl FnOnce(),
) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let sddl = || acl::Sddl::new(&pipe_sddl(node_sid.as_deref())).map_err(|e| e.to_string());
        let mut server = pipe::create_with(name, sddl()?, true).map_err(|e| {
            if e.raw_os_error() == Some(5) {
                format!("{name}: another process already holds this pipe name ({e})")
            } else {
                format!("{name}: {e}")
            }
        })?;
        ready();
        crate::info!("{}: serving {name}", k.label);
        loop {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            let stopping = stop_notify().notified();
            tokio::select! {
                _ = stopping => break,
                _ = tokio::time::sleep(idle) => {
                    crate::info!("{}: idle, stopping", k.label);
                    break;
                }
                r = server.connect() => {
                    let next = pipe::create_with(name, sddl()?, false).map_err(|e| format!("{name}: {e}"))?;
                    let client = std::mem::replace(&mut server, next);
                    if let Err(e) = r {
                        crate::debug!("{} connect: {e}", k.label);
                        continue;
                    }
                    if let Err(e) = one(k, client, config_path, node_sid.as_deref()).await {
                        crate::debug!("{} client: {e}", k.label);
                    }
                }
            }
        }
        Ok(())
    })
}

/// Reads one request, checks the caller and answers.
async fn one(
    k: &Kind,
    client: tokio::net::windows::named_pipe::NamedPipeServer,
    config_path: &Path,
    node_sid: Option<&str>,
) -> std::io::Result<()> {
    let who = tokio::time::timeout(Duration::from_secs(10), pipe::identify(client)).await;
    let (mut stream, caller) = who.unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into()))?;
    let answer = k.answer;
    let mut out = if caller.root || node_sid.is_some_and(|s| s == caller.sid) {
        let mut line = String::new();
        let read = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::io::BufReader::new((&mut stream).take(MAX_REQUEST as u64)).read_line(&mut line),
        )
        .await;
        let line = match read {
            Ok(Ok(_)) => line,
            _ => String::new(),
        };
        let path = config_path.to_owned();
        let (out, log) = tokio::task::spawn_blocking(move || answer(&path, Some(&line)))
            .await
            .unwrap_or_else(|e| answer(Path::new(""), Some(&format!("panic: {e}"))));
        crate::info!("{}: {log} for {}", k.label, caller.name);
        out
    } else {
        crate::warn!("{}: refused a connection from {} ({})", k.label, caller.name, caller.sid);
        answer(config_path, None).0
    };
    out.push(b'\n');
    stream.write_all(&out).await?;
    stream.flush().await
}

/// The admin helper: loads the Administrators-only config afresh and
/// carries the request out.
fn admin_answer(config_path: &Path, line: Option<&str>) -> (Vec<u8>, String) {
    let Some(line) = line else {
        return (json(&Reply::no("only the node's service may use the helper")), String::new());
    };
    let Ok(req) = serde_json::from_str::<HelperRequest>(line) else {
        return (json(&Reply::no("bad request")), "bad request".into());
    };
    let reply = match Config::load(config_path) {
        Ok(config) => {
            let local = LocalFixes::load(&crate::fixes::path_for(config_path)).unwrap_or_else(|e| {
                crate::warn!("helper: {e}");
                LocalFixes::default()
            });
            crate::helper::handle(
                &config,
                &local,
                &req,
                Path::new(crate::helper::ROLLBACK_DIR),
                &mut crate::helper::RealRunner,
            )
        }
        Err(e) => Reply::no(format!("config: {e}")),
    };
    let log = format!(
        "{} {}: {}{}",
        req.action,
        req.target.as_deref().or(req.id.as_deref()).unwrap_or(""),
        if reply.ok { "" } else { "refused or failed: " },
        reply.detail
    );
    (json(&reply), log)
}

/// The health helper: one read, checked against the config.
fn health_answer(config_path: &Path, line: Option<&str>) -> (Vec<u8>, String) {
    use crate::health::watch::{ReadReply, ReadRequest};
    let no = |e: &str| ReadReply { error: e.into(), ..Default::default() };
    let Some(line) = line else {
        return (json(&no("only the node's service may use the health helper")), String::new());
    };
    let Ok(req) = serde_json::from_str::<ReadRequest>(line) else {
        return (json(&no("bad request")), "bad request".into());
    };
    let reply = match Config::load(config_path) {
        Ok(config) => crate::health::watch::handle(&config, &req),
        Err(e) => no(&format!("config: {e}")),
    };
    let log = format!(
        "read {}: {}",
        req.read,
        if reply.ok { format!("{} bytes", reply.output.len()) } else { format!("failed: {}", reply.error) }
    );
    (json(&reply), log)
}

fn json<T: serde::Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

// ---------------------------------------------------------------- install

/// `"C:\Program Files\Nightgrid Node\nightgrid-node.exe" helper --config "…"`
pub fn service_command(k: &Kind, bin: &Path, config: &Path) -> String {
    let cmd = if k.grant == "health" { "health-helper" } else { "helper" };
    format!("\"{}\" {cmd} --config \"{}\"", bin.display(), config.display())
}

/// `allow helper|health` / `deny helper|health`.
pub fn allow(k: &Kind, on: bool) -> Result<Report, String> {
    super::install::preflight()?;
    let mut rep = Report { host: None, ..super::install::report(if on { "allow" } else { "deny" }) };
    let bin = super::installed_binary();
    if !on {
        let had = installed(k);
        scm::stop(k.service, Duration::from_secs(30))?;
        if had {
            if let Err(e) = firewall(k, false, &bin) {
                rep.warnings.push(format!("firewall rule: {e}"));
            }
        }
        if scm::delete(k.service)? {
            step(&mut rep, format!("removed the {} service", k.label));
        } else {
            step(&mut rep, format!("{} was not allowed", k.grant));
        }
        return Ok(rep);
    }
    let node = node_sid().ok_or("the node's service isn't installed (run: nightgrid-node install)")?;
    if !bin.is_file() {
        return Err(format!("{} is missing (run: nightgrid-node install)", bin.display()));
    }
    let m = scm::manager(true)?;
    let created = scm::ensure(
        &m,
        &scm::Spec {
            name: k.service,
            display: k.display,
            description: k.description,
            command: service_command(k, &bin, &super::etc_dir().join("config.toml")),
            account: None,
            auto_start: false,
            privileges: k.privileges,
            restricted: k.restricted,
            restart: false,
        },
    )?;
    drop(m);
    step(&mut rep, format!("{} the {} service", if created { "created" } else { "updated" }, k.label));
    scm::set_dacl(k.service, &service_sddl(&node))?;
    step(&mut rep, format!("the node may start the {}, not stop or change it", k.label));
    match firewall(k, true, &bin) {
        Ok(()) => {
            step(&mut rep, format!("blocked all network traffic for the {} (Windows Service Hardening)", k.label))
        }
        Err(e) => rep.warnings.push(format!("could not add the {}'s firewall restriction: {e}", k.label)),
    }
    rep.warnings.push(if k.restricted {
        "the health helper runs as LocalSystem with a write-restricted token; it only runs fixed reads of disk \
         health, Storage Spaces and Windows Update"
            .into()
    } else {
        "the helper runs as LocalSystem; it only carries out what [actions] allow and [actions] units in \
         config.toml list"
            .into()
    });
    Ok(rep)
}

fn step(r: &mut Report, s: impl Into<String>) {
    r.steps.push(s.into());
}

/// A helper's Service Hardening rule: no traffic in or out.
fn firewall(k: &Kind, on: bool, bin: &Path) -> Result<(), String> {
    let exe = bin.to_string_lossy().replace('\'', "''");
    let svc = k.service;
    let script = if on {
        format!(
            "$ErrorActionPreference='Stop'\n\
             $sr = (New-Object -ComObject HNetCfg.FwPolicy2).ServiceRestriction\n\
             $sr.RestrictService('{svc}', '{exe}', $true, $false)\n\
             if (-not $sr.ServiceRestricted('{svc}', '{exe}')) {{\n\
               $sr.RestrictService('{svc}', '{exe}', $false, $false)\n\
               $sr.RestrictService('{svc}', '{exe}', $true, $false)\n\
             }}\n\
             if (-not $sr.ServiceRestricted('{svc}', '{exe}')) {{ throw 'not restricted' }}\n"
        )
    } else {
        format!(
            "try {{ (New-Object -ComObject HNetCfg.FwPolicy2).ServiceRestriction.RestrictService('{svc}', '{exe}', $false, $false) }} catch {{}}\n\
             exit 0\n"
        )
    };
    super::install::powershell(&script).map(|_| ())
}

/// Removes both helpers on uninstall.
pub fn remove(rep: &mut Report) {
    for k in [&ADMIN, &HEALTH] {
        if !installed(k) {
            continue;
        }
        if let Err(e) = scm::stop(k.service, Duration::from_secs(30)) {
            rep.warnings.push(e);
        }
        if let Err(e) = firewall(k, false, &super::installed_binary()) {
            rep.warnings.push(format!("{} firewall rule: {e}", k.label));
        }
        match scm::delete(k.service) {
            Ok(true) => step(rep, format!("removed the {} service", k.label)),
            Ok(false) => {}
            Err(e) => rep.warnings.push(e),
        }
    }
}

/// Stops both helpers (before their exe is replaced).
pub fn stop_all() -> Result<(), String> {
    for k in [&ADMIN, &HEALTH] {
        scm::stop(k.service, Duration::from_secs(30))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_lists() {
        assert_eq!(pipe_sddl(None), "D:P(A;;GA;;;SY)(A;;GA;;;BA)");
        assert_eq!(pipe_sddl(Some("S-1-5-80-9")), "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;S-1-5-80-9)");
        assert_eq!(pipe_sddl(Some("(A;;GA;;;WD)")), "D:P(A;;GA;;;SY)(A;;GA;;;BA)");
        assert!(acl::Sddl::new(&service_sddl("S-1-5-80-9")).is_ok());
        assert!(service_sddl("S-1-5-80-9").ends_with("(A;;LCRP;;;S-1-5-80-9)"));
        assert_eq!(by_pipe(Path::new(PIPE)).map(|k| k.service), Some(SERVICE));
        assert_eq!(by_pipe(Path::new(HEALTH_PIPE)).map(|k| k.service), Some("nightgrid-node-health"));
        assert!(by_pipe(Path::new(r"\\.\pipe\nightgrid-node")).is_none());
        assert!(service_command(&HEALTH, Path::new(r"C:\n.exe"), Path::new(r"C:\c.toml")).contains(" health-helper "));
    }

    /// A console-run helper answers the node's request over its pipe and
    /// checks it against the config: here nothing is allowed.
    #[test]
    fn answers_over_its_pipe() {
        let d = tempfile::tempdir().unwrap();
        let config = d.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let name = format!(r"\\.\pipe\nightgrid-node-helper-test-{}", std::process::id());
        let me = super::super::sys::my_sid();
        let (n2, c2) = (name.clone(), config.clone());
        let server = std::thread::spawn(move || run(&ADMIN, &c2, &n2, Some(me), Duration::from_secs(5), || {}));
        let mut f = None;
        for _ in 0..100 {
            if let Ok(x) = pipe::open_sync(&name) {
                f = Some(x);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let s = crate::compat::LocalStream::from_file(f.expect("helper pipe"));
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let r = crate::helper::exchange(
            s,
            &HelperRequest { action: "unit.restart".into(), target: Some("W32Time".into()), ..Default::default() },
        )
        .unwrap();
        assert_eq!(r, Reply::no("unit.restart is not in [actions] allow"));
        server.join().unwrap().unwrap();
    }
}
