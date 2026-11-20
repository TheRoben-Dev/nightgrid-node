//! The Service Control Manager: create, configure, start, stop, query and
//! delete the node's services (what `systemctl` does on Linux).

use std::time::{Duration, Instant};

use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QueryServiceConfigW, QueryServiceStatusEx, StartServiceW, QUERY_SERVICE_CONFIGW,
    SC_ACTION, SC_ACTION_NONE, SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_ALL_ACCESS, SC_MANAGER_CONNECT,
    SC_STATUS_PROCESS_INFO, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO, SERVICE_CONFIG_SERVICE_SID_INFO,
    SERVICE_CONTROL_STOP, SERVICE_DEMAND_START, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW,
    SERVICE_NO_CHANGE, SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS, SERVICE_REQUIRED_PRIVILEGES_INFOW, SERVICE_RUNNING,
    SERVICE_SID_INFO, SERVICE_STATUS, SERVICE_STATUS_PROCESS, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
};

use super::wide;

const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;
const ERROR_SERVICE_ALREADY_RUNNING: i32 = 1056;
const SERVICE_SID_TYPE_RESTRICTED: u32 = 3;

pub struct Sc(pub(super) SC_HANDLE);

impl Drop for Sc {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we own the handle.
            unsafe { CloseServiceHandle(self.0) };
        }
    }
}

fn err(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

/// The SCM, for reading (`write` false) or creating and changing services.
pub fn manager(write: bool) -> Result<Sc, String> {
    let access = if write { SC_MANAGER_ALL_ACCESS } else { SC_MANAGER_CONNECT };
    // SAFETY: local machine, default database.
    let h = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), access) };
    if h.is_null() {
        return Err(err("open the Service Control Manager"));
    }
    Ok(Sc(h))
}

/// The SCM, for listing services.
pub fn manager_enumerate() -> Result<Sc, String> {
    use windows_sys::Win32::System::Services::SC_MANAGER_ENUMERATE_SERVICE;
    // SAFETY: local machine, default database.
    let h = unsafe {
        OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)
    };
    if h.is_null() {
        return Err(err("open the Service Control Manager"));
    }
    Ok(Sc(h))
}

/// A service by name, `None` when it doesn't exist.
pub fn open(m: &Sc, name: &str, access: u32) -> Result<Option<Sc>, String> {
    let w = wide(name);
    // SAFETY: valid manager handle and name.
    let h = unsafe { OpenServiceW(m.0, w.as_ptr(), access) };
    if h.is_null() {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) {
            return Ok(None);
        }
        return Err(format!("open service {name}: {e}"));
    }
    Ok(Some(Sc(h)))
}

/// What a service should look like.
pub struct Spec<'a> {
    pub name: &'a str,
    pub display: &'a str,
    pub description: &'a str,
    /// Full command line, binary quoted.
    pub command: String,
    /// `NT SERVICE\name` for a virtual account, or `None` for LocalSystem.
    pub account: Option<String>,
    pub auto_start: bool,
    /// Privileges the service keeps; every other one is removed.
    pub privileges: &'a [&'a str],
    /// Make the token write-restricted to the service's own SID.
    pub restricted: bool,
    /// Restart after a crash, like `Restart=on-failure`.
    pub restart: bool,
}

/// Creates the service or brings an existing one in line with `spec`.
/// Returns true when it was created.
pub fn ensure(m: &Sc, spec: &Spec) -> Result<bool, String> {
    let name = wide(spec.name);
    let display = wide(spec.display);
    let cmd = wide(&spec.command);
    let account = wide(spec.account.as_deref().unwrap_or("LocalSystem"));
    let acc_ptr = account.as_ptr();
    let start = if spec.auto_start { SERVICE_AUTO_START } else { SERVICE_DEMAND_START };
    let (svc, created) = match open(m, spec.name, SERVICE_ALL_ACCESS)? {
        Some(s) => {
            // SAFETY: valid handle; null pointers leave fields unchanged.
            let ok = unsafe {
                ChangeServiceConfigW(
                    s.0,
                    SERVICE_WIN32_OWN_PROCESS,
                    start,
                    SERVICE_ERROR_NORMAL,
                    cmd.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    acc_ptr,
                    std::ptr::null(),
                    display.as_ptr(),
                )
            };
            if ok == 0 {
                return Err(err(&format!("configure service {}", spec.name)));
            }
            (s, false)
        }
        None => {
            // SAFETY: valid strings; no dependencies, no password (virtual
            // accounts and LocalSystem have none).
            let h = unsafe {
                CreateServiceW(
                    m.0,
                    name.as_ptr(),
                    display.as_ptr(),
                    SERVICE_ALL_ACCESS,
                    SERVICE_WIN32_OWN_PROCESS,
                    start,
                    SERVICE_ERROR_NORMAL,
                    cmd.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    acc_ptr,
                    std::ptr::null(),
                )
            };
            if h.is_null() {
                return Err(err(&format!("create service {}", spec.name)));
            }
            (Sc(h), true)
        }
    };
    let mut desc = wide(spec.description);
    let d = SERVICE_DESCRIPTIONW { lpDescription: desc.as_mut_ptr() };
    // Privileges: a REG_MULTI_SZ, double-NUL terminated.
    let mut privs: Vec<u16> = spec.privileges.iter().flat_map(wide).collect();
    privs.push(0);
    let p = SERVICE_REQUIRED_PRIVILEGES_INFOW { pmszRequiredPrivileges: privs.as_mut_ptr() };
    let sid = SERVICE_SID_INFO { dwServiceSidType: if spec.restricted { SERVICE_SID_TYPE_RESTRICTED } else { 1 } };
    let mut actions = [
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 5_000 },
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 30_000 },
        SC_ACTION { Type: SC_ACTION_NONE, Delay: 0 },
    ];
    let fa = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 3600,
        lpRebootMsg: std::ptr::null_mut(),
        lpCommand: std::ptr::null_mut(),
        cActions: if spec.restart { actions.len() as u32 } else { 0 },
        lpsaActions: if spec.restart { actions.as_mut_ptr() } else { std::ptr::null_mut() },
    };
    // SAFETY: each struct matches its info level.
    unsafe {
        if ChangeServiceConfig2W(svc.0, SERVICE_CONFIG_DESCRIPTION, (&d as *const SERVICE_DESCRIPTIONW).cast()) == 0 {
            return Err(err("set the service description"));
        }
        if ChangeServiceConfig2W(svc.0, SERVICE_CONFIG_SERVICE_SID_INFO, (&sid as *const SERVICE_SID_INFO).cast()) == 0
        {
            return Err(err("set the service SID type"));
        }
        if !spec.privileges.is_empty()
            && ChangeServiceConfig2W(
                svc.0,
                SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO,
                (&p as *const SERVICE_REQUIRED_PRIVILEGES_INFOW).cast(),
            ) == 0
        {
            return Err(err("limit the service's privileges"));
        }
        if ChangeServiceConfig2W(svc.0, SERVICE_CONFIG_FAILURE_ACTIONS, (&fa as *const SERVICE_FAILURE_ACTIONSW).cast())
            == 0
        {
            return Err(err("set the restart policy"));
        }
    }
    let _ = SERVICE_NO_CHANGE;
    Ok(created)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub state: u32,
    pub pid: u32,
    pub exit: u32,
}

impl Status {
    pub fn running(&self) -> bool {
        self.state == SERVICE_RUNNING
    }
    pub fn word(&self) -> &'static str {
        match self.state {
            1 => "stopped",
            2 => "starting",
            3 => "stopping",
            4 => "running",
            _ => "changing",
        }
    }
}

/// The service's state, `None` when it isn't installed.
pub fn status(name: &str) -> Result<Option<Status>, String> {
    let m = manager(false)?;
    let Some(s) = open(&m, name, SERVICE_QUERY_STATUS)? else { return Ok(None) };
    query(&s).map(Some)
}

fn query(s: &Sc) -> Result<Status, String> {
    let mut st: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
    let mut need = 0u32;
    // SAFETY: a sized struct for SC_STATUS_PROCESS_INFO.
    let ok = unsafe {
        QueryServiceStatusEx(
            s.0,
            SC_STATUS_PROCESS_INFO,
            (&mut st as *mut SERVICE_STATUS_PROCESS).cast(),
            std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut need,
        )
    };
    if ok == 0 {
        return Err(err("query service status"));
    }
    Ok(Status { state: st.dwCurrentState, pid: st.dwProcessId, exit: st.dwWin32ExitCode })
}

/// The service's command line, `None` when it isn't installed.
pub fn command(name: &str) -> Result<Option<String>, String> {
    let m = manager(false)?;
    let Some(s) = open(&m, name, SERVICE_QUERY_CONFIG)? else { return Ok(None) };
    let mut buf = vec![0u64; 1024];
    let mut need = 0u32;
    // SAFETY: an 8 KiB buffer, aligned for the struct.
    let ok = unsafe {
        QueryServiceConfigW(s.0, buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW, (buf.len() * 8) as u32, &mut need)
    };
    if ok == 0 {
        return Err(err("query service config"));
    }
    // SAFETY: Windows filled the struct; strings point into `buf`.
    let c = unsafe { &*(buf.as_ptr() as *const QUERY_SERVICE_CONFIGW) };
    Ok(Some(unsafe { super::from_pwstr(c.lpBinaryPathName) }))
}

/// Starts the service and waits until it runs (or stops again).
pub fn start(name: &str, timeout: Duration) -> Result<(), String> {
    let m = manager(false)?;
    let s = open(&m, name, 0x0010 | SERVICE_QUERY_STATUS)?.ok_or_else(|| format!("{name} is not installed"))?;
    // SAFETY: valid handle, no arguments.
    if unsafe { StartServiceW(s.0, 0, std::ptr::null()) } == 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_SERVICE_ALREADY_RUNNING) {
            return Err(format!("start {name}: {e}"));
        }
    }
    let until = Instant::now() + timeout;
    loop {
        let st = query(&s)?;
        if st.running() {
            return Ok(());
        }
        if st.state == SERVICE_STOPPED {
            return Err(format!("{name} stopped right after starting (exit code {})", st.exit));
        }
        if Instant::now() > until {
            return Err(format!("{name} did not start within {} s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Stops the service and waits until it has stopped. Not installed or not
/// running is fine.
pub fn stop(name: &str, timeout: Duration) -> Result<(), String> {
    let m = manager(false)?;
    let Some(s) = open(&m, name, 0x0020 | SERVICE_QUERY_STATUS)? else { return Ok(()) };
    let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    // SAFETY: valid handle and out-struct.
    if unsafe { ControlService(s.0, SERVICE_CONTROL_STOP, &mut st) } == 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_SERVICE_NOT_ACTIVE) {
            return Err(format!("stop {name}: {e}"));
        }
    }
    let until = Instant::now() + timeout;
    loop {
        if query(&s)?.state == SERVICE_STOPPED {
            return Ok(());
        }
        if Instant::now() > until {
            return Err(format!("{name} did not stop within {} s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Marks the service for deletion (it goes once its handles close).
pub fn delete(name: &str) -> Result<bool, String> {
    let m = manager(true)?;
    let Some(s) = open(&m, name, 0x0001_0000)? else { return Ok(false) };
    // SAFETY: valid handle with DELETE access.
    if unsafe { DeleteService(s.0) } == 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(1072) {
            // 1072: already marked for deletion.
            return Err(format!("delete service {name}: {e}"));
        }
    }
    Ok(true)
}

/// What the helper does for `unit.restart`, `unit.start` and `unit.stop`:
/// the SCM's form of `systemctl restart|start|stop -- name`. Waits up to
/// a minute for each step, like systemd's default job timeout would.
pub fn control(verb: &str, name: &str) -> Result<(), String> {
    let wait = Duration::from_secs(60);
    if status(name)?.is_none() {
        return Err(format!("no service named {name}"));
    }
    match verb {
        "start" => start(name, wait),
        "stop" => stop(name, wait),
        "restart" => {
            stop(name, wait)?;
            start(name, wait)
        }
        _ => Err(format!("unknown verb {verb:?}")),
    }
}

/// Replaces a service's DACL (who may query, start or stop it).
pub fn set_dacl(name: &str, sddl: &str) -> Result<(), String> {
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::System::Services::SetServiceObjectSecurity;
    let m = manager(true)?;
    // WRITE_DAC
    let s = open(&m, name, 0x0004_0000)?.ok_or_else(|| format!("{name} is not installed"))?;
    let sd = super::acl::Sddl::new(sddl).map_err(|e| format!("{sddl}: {e}"))?;
    // SAFETY: valid handle with WRITE_DAC and a valid descriptor.
    if unsafe { SetServiceObjectSecurity(s.0, DACL_SECURITY_INFORMATION, sd.descriptor()) } == 0 {
        return Err(err(&format!("set the access list of {name}")));
    }
    Ok(())
}
