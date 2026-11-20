//! Windows Server support (docs/WINDOWS.md): the pieces that take the place
//! of `/proc`, systemd and the unix socket.

pub mod acl;
pub mod doctor;
pub mod eventlog;
pub mod helper;
pub mod install;
pub mod net;
pub mod pipe;
pub mod scm;
pub mod service;
pub mod usage;

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};

/// Service name, also the virtual account (`NT SERVICE\nightgrid-node`).
pub const SERVICE: &str = "nightgrid-node";
/// The service's display name.
pub const DISPLAY: &str = "Nightgrid Node";
/// Builtin "Event Log Readers": may read the System, Security and
/// Application logs (`allow eventlog`, like the journal's group on Linux).
pub const EVENT_LOG_READERS: &str = "S-1-5-32-573";

/// Local group whose members may use the pipe (like the Linux group).
pub const GROUP: &str = "Nightgrid Node Users";
/// The node's pipe.
pub const PIPE: &str = r"\\.\pipe\nightgrid-node";

/// A NUL-terminated UTF-16 string.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// A UTF-16 buffer up to its first NUL.
pub fn from_wide(w: &[u16]) -> String {
    let end = w.iter().position(|c| *c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// A NUL-terminated UTF-16 string from a pointer.
///
/// # Safety
/// `p` is null or points to a NUL-terminated UTF-16 string.
pub unsafe fn from_pwstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    // SAFETY: the caller promises a terminator.
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: `n` u16s up to the terminator are readable.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

/// Closes a handle when dropped.
pub struct Handle(pub HANDLE);

// SAFETY: kernel handles may be used and closed from any thread.
unsafe impl Send for Handle {}
// SAFETY: as above; nothing here mutates through a shared reference.
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 as isize != -1 {
            // SAFETY: we own the handle.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// Frees a `LocalAlloc`ed pointer when dropped.
pub struct Local(pub *mut core::ffi::c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we own the allocation.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// `%ProgramData%` (normally `C:\ProgramData`).
pub fn program_data() -> PathBuf {
    std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
}

/// `%ProgramFiles%` (normally `C:\Program Files`).
pub fn program_files() -> PathBuf {
    std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
}

/// `%ProgramData%\Nightgrid Node`: config, policy and (in `data`) history.
pub fn etc_dir() -> PathBuf {
    program_data().join(DISPLAY)
}

/// Where history lives.
pub fn data_dir() -> PathBuf {
    etc_dir().join("data")
}

/// Where the binary is installed.
pub fn bin_dir() -> PathBuf {
    program_files().join(DISPLAY)
}

pub fn installed_binary() -> PathBuf {
    bin_dir().join("nightgrid-node.exe")
}

pub mod sys {
    //! Clocks, identity and memory: what the Linux code reads from `/proc`
    //! and libc.

    use windows_sys::Win32::Foundation::{FILETIME, HANDLE, SYSTEMTIME};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TokenUser, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNamePhysicalDnsHostname, GetComputerNameExW, GetTickCount64,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    use windows_sys::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTime;

    use super::{wide, Handle};

    /// Milliseconds since boot, including time asleep.
    pub fn ticks_ms() -> u64 {
        // SAFETY: no preconditions.
        unsafe { GetTickCount64() }
    }

    /// Seconds since boot.
    pub fn uptime_secs() -> f64 {
        ticks_ms() as f64 / 1000.0
    }

    /// Seconds the host spent asleep or hibernating since boot: the tick
    /// count runs on through sleep, the unbiased interrupt time doesn't.
    pub fn suspended_secs() -> i64 {
        let mut unbiased = 0u64;
        // SAFETY: writes one u64.
        if unsafe { QueryUnbiasedInterruptTime(&mut unbiased) } == 0 {
            return 0;
        }
        let awake_ms = unbiased / 10_000;
        (ticks_ms().saturating_sub(awake_ms) / 1000) as i64
    }

    /// A value under `HKLM`, as a string (REG_SZ) or number (REG_DWORD).
    pub fn reg_string(key: &str, name: &str) -> Option<String> {
        let (k, n) = (wide(key), wide(name));
        let mut buf = vec![0u16; 512];
        let mut len = (buf.len() * 2) as u32;
        // SAFETY: the buffer and its length in bytes match.
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                k.as_ptr(),
                n.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        (r == 0).then(|| super::from_wide(&buf))
    }

    pub fn reg_dword(key: &str, name: &str) -> Option<u32> {
        let (k, n) = (wide(key), wide(name));
        let mut v = 0u32;
        let mut len = 4u32;
        // SAFETY: a DWORD-sized buffer.
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                k.as_ptr(),
                n.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&mut v as *mut u32).cast(),
                &mut len,
            )
        };
        (r == 0).then_some(v)
    }

    /// Changes on every boot: the kernel's boot counter (Windows 8 and
    /// later). `None` if it can't be read; callers fall back to boot time.
    pub fn boot_id() -> Option<String> {
        reg_dword(r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters", "BootId")
            .map(|n| format!("windows-boot-{n}"))
    }

    const NT: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

    /// `10.0.20348`: the kernel version, in the form `uname -r` would give.
    pub fn os_version() -> String {
        let build = reg_string(NT, "CurrentBuildNumber").unwrap_or_default();
        let major = reg_dword(NT, "CurrentMajorVersionNumber").unwrap_or(10);
        let minor = reg_dword(NT, "CurrentMinorVersionNumber").unwrap_or(0);
        format!("{major}.{minor}.{build}")
    }

    /// `Windows Server 2022 Standard 10.0.20348`, like the app's sampler
    /// (`Win32_OperatingSystem` caption and version without "Microsoft ").
    pub fn os_caption() -> String {
        let mut name = reg_string(NT, "ProductName").unwrap_or_else(|| "Windows".into());
        // Windows 11 still says "Windows 10" in ProductName.
        let build: u32 = reg_string(NT, "CurrentBuildNumber").and_then(|b| b.parse().ok()).unwrap_or(0);
        if build >= 22000 && name.starts_with("Windows 10") {
            name = name.replacen("Windows 10", "Windows 11", 1);
        }
        let name = name.trim_start_matches("Microsoft ").to_owned();
        format!("{name} {}", os_version())
    }

    /// The host name (the DNS host name, like `hostname`).
    pub fn hostname() -> String {
        let mut buf = vec![0u16; 256];
        let mut len = buf.len() as u32;
        // SAFETY: buffer and length in characters match.
        if unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, buf.as_mut_ptr(), &mut len) } == 0 {
            return std::env::var("COMPUTERNAME").unwrap_or_default();
        }
        String::from_utf16_lossy(&buf[..len as usize])
    }

    /// Working set of this process, bytes.
    pub fn rss_bytes() -> u64 {
        let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: the pseudo handle needs no closing; the struct is sized.
        if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } == 0 {
            return 0;
        }
        c.WorkingSetSize as u64
    }

    fn token_info(token: HANDLE, class: i32) -> Option<Vec<u8>> {
        let mut len = 0u32;
        // SAFETY: a size query with no buffer.
        unsafe { GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut len) };
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        // SAFETY: buffer of the size Windows asked for.
        (unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) } != 0).then_some(buf)
    }

    fn process_token() -> Option<Handle> {
        let mut t: HANDLE = std::ptr::null_mut();
        // SAFETY: pseudo handle of this process; `t` receives a new handle.
        (unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut t) } != 0).then(|| Handle(t))
    }

    /// The SID of a token's user, as `S-1-5-…`.
    pub fn token_sid(token: HANDLE) -> Option<String> {
        let buf = token_info(token, TokenUser)?;
        // SAFETY: TokenUser fills a TOKEN_USER at the start of the buffer.
        let tu = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
        // SAFETY: the SID lives in `buf`.
        unsafe { super::acl::sid_string(tu.User.Sid) }
    }

    /// The SID this process runs as.
    pub fn my_sid() -> String {
        process_token().and_then(|t| token_sid(t.0)).unwrap_or_default()
    }

    /// Whether this process's token holds the group `sid` (string form),
    /// enabled: builtin groups such as Event Log Readers.
    pub fn in_group(sid: &str) -> bool {
        use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
        use windows_sys::Win32::Security::{CheckTokenMembership, PSID};
        let w = wide(sid);
        let mut p: PSID = std::ptr::null_mut();
        // SAFETY: Windows allocates the SID; `Local` frees it.
        if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut p) } == 0 {
            return false;
        }
        let _free = super::Local(p);
        let mut member = 0;
        // SAFETY: a null token means the calling thread's or process's token.
        (unsafe { CheckTokenMembership(std::ptr::null_mut(), p, &mut member) } != 0) && member != 0
    }

    /// True when this process has a full administrator token (elevated).
    pub fn is_elevated() -> bool {
        let Some(t) = process_token() else { return false };
        let Some(buf) = token_info(t.0, TokenElevation) else { return false };
        // SAFETY: TokenElevation fills a TOKEN_ELEVATION.
        let e = unsafe { &*(buf.as_ptr() as *const TOKEN_ELEVATION) };
        e.TokenIsElevated != 0
    }

    /// `2026-11-17 21:04` in the host's time zone.
    pub fn local_time(t: i64) -> Option<String> {
        // Unix seconds to FILETIME (100 ns since 1601).
        let ft100 = (t.checked_add(11_644_473_600)?).checked_mul(10_000_000)?;
        let ft100 = u64::try_from(ft100).ok()?;
        let ft = FILETIME { dwLowDateTime: ft100 as u32, dwHighDateTime: (ft100 >> 32) as u32 };
        let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
        let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
        // SAFETY: plain structs in and out.
        unsafe {
            if FileTimeToSystemTime(&ft, &mut utc) == 0
                || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0
            {
                return None;
            }
        }
        Some(format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
        ))
    }
}
