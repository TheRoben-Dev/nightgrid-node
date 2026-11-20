//! Running under the Service Control Manager: `nightgrid-node run` is the
//! service's command line. Under the SCM it reports start, running and stop
//! like systemd's `Type=notify`; started from a console it runs until
//! Ctrl+C, which is how tests and `--data-dir` runs use it.
//!
//! At start the process also puts itself in a job object with a memory cap
//! (the counterpart of the unit's `MemoryMax=64M`).

use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, NO_ERROR};
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, SERVICE_ACCEPT_SHUTDOWN,
    SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};

use crate::config::Config;
use crate::daemon::Paths;

/// The memory cap for the service process, like `MemoryMax=64M`.
pub const MEMORY_MAX: usize = 64 << 20;

static STATUS: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static STOP: AtomicBool = AtomicBool::new(false);
static STOP_NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
static JOB: OnceLock<super::Handle> = OnceLock::new();
type Job = (Config, Paths);
static PENDING: Mutex<Option<Job>> = Mutex::new(None);
static RESULT: Mutex<Option<Result<(), String>>> = Mutex::new(None);

fn notify_handle() -> &'static tokio::sync::Notify {
    STOP_NOTIFY.get_or_init(tokio::sync::Notify::new)
}

/// Resolves once the SCM (or Ctrl+C on a console) asks the node to stop.
pub async fn stopping() {
    let console = STATUS.load(Ordering::SeqCst).is_null();
    loop {
        if STOP.load(Ordering::SeqCst) {
            return;
        }
        let n = notify_handle().notified();
        if console {
            tokio::select! {
                _ = n => {}
                r = tokio::signal::ctrl_c() => {
                    if r.is_err() {
                        std::future::pending::<()>().await;
                    }
                    STOP.store(true, Ordering::SeqCst);
                }
            }
        } else {
            n.await;
        }
    }
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

/// `sd_notify` for the SCM: READY=1 → running, STOPPING=1 → stop pending.
pub fn notify(state: &str) {
    match state {
        "READY=1" => set(SERVICE_RUNNING, NO_ERROR, 0),
        "STOPPING=1" => set(SERVICE_STOP_PENDING, NO_ERROR, 10_000),
        _ => {}
    }
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
            notify_handle().notify_waiters();
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => 120, // ERROR_CALL_NOT_IMPLEMENTED
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let name = super::wide(super::SERVICE);
    // SAFETY: a static handler; the name lives for the call.
    let h = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(handler), std::ptr::null()) };
    if h.is_null() {
        return;
    }
    STATUS.store(h as *mut _, Ordering::SeqCst);
    set(SERVICE_START_PENDING, NO_ERROR, 30_000);
    let job = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some((_, paths)) = &job {
        let _ = crate::log::to_file(&paths.data_dir.join("log").join("node.log"));
    }
    let r = match job {
        Some((config, paths)) => crate::daemon::run(config, paths),
        None => Err("no configuration".into()),
    };
    let exit = if r.is_ok() { NO_ERROR } else { 1066 }; // ERROR_SERVICE_SPECIFIC_ERROR
    if let Err(e) = &r {
        crate::error!("{e}");
    }
    *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
    set(SERVICE_STOPPED, exit, 0);
}

/// Puts this process in a job with a memory cap. Failure is only logged:
/// the cap is a guard rail, not a requirement for running.
pub fn cap_memory(bytes: usize) {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain Win32 calls with owned handles and a sized struct.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            crate::warn!("memory cap: {}", std::io::Error::last_os_error());
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.ProcessMemoryLimit = bytes;
        let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size,
        ) == 0
            || AssignProcessToJobObject(job, GetCurrentProcess()) == 0
        {
            crate::warn!("memory cap: {}", std::io::Error::last_os_error());
            super::Handle(job);
            return;
        }
        let _ = JOB.set(super::Handle(job));
    }
}

/// `nightgrid-node run`: as a service when the SCM started us, else in the
/// foreground until Ctrl+C.
pub fn run(config: Config, paths: Paths) -> Result<(), String> {
    cap_memory(MEMORY_MAX);
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((config, paths));
    let mut name = super::wide(super::SERVICE);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: std::ptr::null_mut(), lpServiceProc: None },
    ];
    // SAFETY: a NULL-terminated table whose strings outlive the call, which
    // returns when the service has stopped.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } != 0 {
        return RESULT.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or(Ok(()));
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT as i32) {
        return Err(format!("service dispatcher: {err}"));
    }
    // Not under the SCM: a console run.
    let (config, paths) = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take().ok_or("no configuration")?;
    crate::daemon::run(config, paths)
}
