//! The admin helper on Windows (W2, decision N-080): what the root
//! helper's socket unit is on Linux.
//!
//! `nightgrid-node allow helper` installs the service
//! `nightgrid-node-helper`: LocalSystem, demand start, only the
//! privileges it needs (shutting down), no network (a Service Hardening
//! rule), and a service DACL that lets the node's own service SID start it
//! and nothing more. The node starts it when it has something to ask and
//! it stops by itself after a few idle minutes, like a socket-activated
//! unit.
//!
//! It serves `\\.\pipe\nightgrid-node-helper`: remote clients rejected, a
//! DACL for SYSTEM, Administrators and the node's service SID, and every
//! caller checked again from its token. Each request is one JSON line, the
//! same [`HelperRequest`] as on Linux, checked against the
//! Administrators-only config by [`crate::helper::handle`]; nothing the
//! node says is trusted beyond that.

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

/// The helper's service name.
pub const SERVICE: &str = "nightgrid-node-helper";
pub const DISPLAY: &str = "Nightgrid Node helper";
/// The helper's pipe.
pub const PIPE: &str = r"\\.\pipe\nightgrid-node-helper";
/// The helper stops after this long without a request.
pub const IDLE: Duration = Duration::from_secs(180);
const MAX_REQUEST: usize = 64 * 1024;

/// The helper pipe's DACL: SYSTEM and Administrators, and the node's
/// service SID may read and write. Nobody else.
pub fn pipe_sddl(node_sid: Option<&str>) -> String {
    let mut s = String::from("D:P(A;;GA;;;SY)(A;;GA;;;BA)");
    if let Some(sid) = node_sid.filter(|s| s.starts_with("S-1-")) {
        s.push_str(&format!("(A;;GRGW;;;{sid})"));
    }
    s
}

/// The helper service's DACL: Windows' default for a service, plus "query
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

/// Whether the helper service is installed (`allow helper`).
pub fn installed() -> bool {
    scm::status(SERVICE).ok().flatten().is_some()
}

// ---------------------------------------------------------------- client

/// Opens the helper's pipe for the node, starting the service when it
/// isn't running. The pipe must belong to SYSTEM or Administrators.
pub fn connect() -> Result<crate::compat::LocalStream, String> {
    let mut woke = false;
    for _ in 0..50 {
        match pipe::open_system(PIPE) {
            Ok(f) => return Ok(crate::compat::LocalStream::from_file(f)),
            // No instance: the helper isn't running (or is just stopping).
            Err(e) if e.raw_os_error() == Some(2) => {
                if !woke {
                    wake()?;
                    woke = true;
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            Err(e) => return Err(format!("admin helper ({PIPE}): {e}")),
        }
    }
    Err(format!("admin helper ({PIPE}): it started but its pipe did not appear"))
}

/// Starts the helper, first waiting out an idle stop in progress.
fn wake() -> Result<(), String> {
    let until = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        match scm::status(SERVICE)? {
            None => return Err("the admin helper isn't installed (an administrator can run: allow helper)".into()),
            Some(s) if s.state == SERVICE_STOP_PENDING && std::time::Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Some(_) => break,
        }
    }
    scm::start(SERVICE, Duration::from_secs(30)).map_err(|e| format!("admin helper: {e}"))
}

// ---------------------------------------------------------------- server

static STATUS: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static STOP: AtomicBool = AtomicBool::new(false);
static STOP_NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
static CONFIG: Mutex<Option<PathBuf>> = Mutex::new(None);
static RESULT: Mutex<Option<Result<(), String>>> = Mutex::new(None);

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
            set(SERVICE_STOP_PENDING, NO_ERROR, 10_000);
            STOP.store(true, Ordering::SeqCst);
            stop_notify().notify_waiters();
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => 120, // ERROR_CALL_NOT_IMPLEMENTED
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let name = super::wide(SERVICE);
    // SAFETY: a static handler; the name lives for the call.
    let h = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(handler), std::ptr::null()) };
    if h.is_null() {
        return;
    }
    STATUS.store(h as *mut _, Ordering::SeqCst);
    set(SERVICE_START_PENDING, NO_ERROR, 10_000);
    let _ = crate::log::to_file(&super::data_dir().join("log").join("helper.log"));
    let config = CONFIG.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default();
    let r = run(&config, PIPE, node_sid(), IDLE, || set(SERVICE_RUNNING, NO_ERROR, 0));
    if let Err(e) = &r {
        crate::error!("helper: {e}");
    }
    let exit = if r.is_ok() { NO_ERROR } else { 1066 }; // ERROR_SERVICE_SPECIFIC_ERROR
    *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
    set(SERVICE_STOPPED, exit, 0);
}

/// `nightgrid-node helper`: the helper service's command line. Started
/// from a console (by an administrator, for a test) it serves the same way
/// until it has been idle.
pub fn serve(config_path: &Path) -> Result<(), String> {
    *CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = Some(config_path.to_owned());
    let mut name = super::wide(SERVICE);
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
        return Err("the helper runs as its service (or from an elevated prompt)".into());
    }
    run(config_path, PIPE, node_sid(), IDLE, || {})
}

/// Serves the pipe one request at a time until stopped or idle.
pub fn run(
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
        crate::info!("helper: serving {name}");
        loop {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            let stopping = stop_notify().notified();
            tokio::select! {
                _ = stopping => break,
                _ = tokio::time::sleep(idle) => {
                    crate::info!("helper: idle, stopping");
                    break;
                }
                r = server.connect() => {
                    let next = pipe::create_with(name, sddl()?, false).map_err(|e| format!("{name}: {e}"))?;
                    let client = std::mem::replace(&mut server, next);
                    if let Err(e) = r {
                        crate::debug!("helper connect: {e}");
                        continue;
                    }
                    if let Err(e) = one(client, config_path, node_sid.as_deref()).await {
                        crate::debug!("helper client: {e}");
                    }
                }
            }
        }
        Ok(())
    })
}

/// Reads one request, checks the caller and answers.
async fn one(
    client: tokio::net::windows::named_pipe::NamedPipeServer,
    config_path: &Path,
    node_sid: Option<&str>,
) -> std::io::Result<()> {
    let who = tokio::time::timeout(Duration::from_secs(10), pipe::identify(client)).await;
    let (mut stream, caller) = who.unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into()))?;
    let reply = if caller.root || node_sid.is_some_and(|s| s == caller.sid) {
        let mut line = String::new();
        let read = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::io::BufReader::new((&mut stream).take(MAX_REQUEST as u64)).read_line(&mut line),
        )
        .await;
        match (read, serde_json::from_str::<HelperRequest>(&line)) {
            (Ok(Ok(_)), Ok(req)) => {
                let path = config_path.to_owned();
                let r2 = req.clone();
                let reply = tokio::task::spawn_blocking(move || answer(&path, &r2))
                    .await
                    .unwrap_or_else(|e| Reply::no(format!("helper: {e}")));
                crate::info!(
                    "helper: {} {} for {}: {}{}",
                    req.action,
                    req.target.as_deref().or(req.id.as_deref()).unwrap_or(""),
                    caller.name,
                    if reply.ok { "" } else { "refused or failed: " },
                    reply.detail
                );
                reply
            }
            _ => Reply::no("bad request"),
        }
    } else {
        crate::warn!("helper: refused a connection from {} ({})", caller.name, caller.sid);
        Reply::no("only the node's service may use the helper")
    };
    let mut out = serde_json::to_vec(&reply).unwrap_or_default();
    out.push(b'\n');
    stream.write_all(&out).await?;
    stream.flush().await
}

/// Loads the Administrators-only config afresh and carries the request out.
fn answer(config_path: &Path, req: &HelperRequest) -> Reply {
    match Config::load(config_path) {
        Ok(config) => {
            let local = LocalFixes::load(&crate::fixes::path_for(config_path)).unwrap_or_else(|e| {
                crate::warn!("helper: {e}");
                LocalFixes::default()
            });
            crate::helper::handle(
                &config,
                &local,
                req,
                Path::new(crate::helper::ROLLBACK_DIR),
                &mut crate::helper::RealRunner,
            )
        }
        Err(e) => Reply::no(format!("config: {e}")),
    }
}

// ---------------------------------------------------------------- install

/// `"C:\Program Files\Nightgrid Node\nightgrid-node.exe" helper --config "…"`
pub fn service_command(bin: &Path, config: &Path) -> String {
    format!("\"{}\" helper --config \"{}\"", bin.display(), config.display())
}

/// `allow helper` / `deny helper`.
pub fn allow(on: bool) -> Result<Report, String> {
    super::install::preflight()?;
    let mut rep = Report { host: None, ..super::install::report(if on { "allow" } else { "deny" }) };
    let bin = super::installed_binary();
    if !on {
        let had = installed();
        scm::stop(SERVICE, Duration::from_secs(30))?;
        if had {
            if let Err(e) = firewall(false, &bin) {
                rep.warnings.push(format!("firewall rule: {e}"));
            }
        }
        if scm::delete(SERVICE)? {
            step(&mut rep, "removed the admin helper service");
        } else {
            step(&mut rep, "helper was not allowed");
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
            name: SERVICE,
            display: DISPLAY,
            description: "Carries out the Nightgrid Node actions an administrator allowed in its config \
                          (restart a listed service, reboot). Started by the node when needed; no network access.",
            command: service_command(&bin, &super::etc_dir().join("config.toml")),
            account: None,
            auto_start: false,
            privileges: &["SeShutdownPrivilege", "SeChangeNotifyPrivilege"],
            restricted: false,
            restart: false,
        },
    )?;
    drop(m);
    step(&mut rep, if created { "created the admin helper service" } else { "updated the admin helper service" });
    scm::set_dacl(SERVICE, &service_sddl(&node))?;
    step(&mut rep, "the node may start the helper, not stop or change it");
    match firewall(true, &bin) {
        Ok(()) => step(&mut rep, "blocked all network traffic for the helper (Windows Service Hardening)"),
        Err(e) => rep.warnings.push(format!("could not add the helper's firewall restriction: {e}")),
    }
    rep.warnings.push(
        "the helper runs as LocalSystem; it only carries out what [actions] allow and [actions] units in \
         config.toml list"
            .into(),
    );
    Ok(rep)
}

fn step(r: &mut Report, s: impl Into<String>) {
    r.steps.push(s.into());
}

/// The helper's Service Hardening rule: no traffic in or out.
fn firewall(on: bool, bin: &Path) -> Result<(), String> {
    let exe = bin.to_string_lossy().replace('\'', "''");
    let script = if on {
        format!(
            "$ErrorActionPreference='Stop'\n\
             $sr = (New-Object -ComObject HNetCfg.FwPolicy2).ServiceRestriction\n\
             $sr.RestrictService('{SERVICE}', '{exe}', $true, $false)\n\
             if (-not $sr.ServiceRestricted('{SERVICE}', '{exe}')) {{\n\
               $sr.RestrictService('{SERVICE}', '{exe}', $false, $false)\n\
               $sr.RestrictService('{SERVICE}', '{exe}', $true, $false)\n\
             }}\n\
             if (-not $sr.ServiceRestricted('{SERVICE}', '{exe}')) {{ throw 'not restricted' }}\n"
        )
    } else {
        format!(
            "try {{ (New-Object -ComObject HNetCfg.FwPolicy2).ServiceRestriction.RestrictService('{SERVICE}', '{exe}', $false, $false) }} catch {{}}\n\
             exit 0\n"
        )
    };
    super::install::powershell(&script).map(|_| ())
}

/// Removes the helper on uninstall.
pub fn remove(rep: &mut Report) {
    if !installed() {
        return;
    }
    if let Err(e) = scm::stop(SERVICE, Duration::from_secs(30)) {
        rep.warnings.push(e);
    }
    if let Err(e) = firewall(false, &super::installed_binary()) {
        rep.warnings.push(format!("helper firewall rule: {e}"));
    }
    match scm::delete(SERVICE) {
        Ok(true) => step(rep, "removed the admin helper service"),
        Ok(false) => {}
        Err(e) => rep.warnings.push(e),
    }
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
        let server = std::thread::spawn(move || run(&c2, &n2, Some(me), Duration::from_secs(5), || {}));
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
