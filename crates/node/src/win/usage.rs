//! Per-process and per-service counters, what `/proc` and the systemd
//! cgroups give on Linux.
//!
//! Both come without opening any process: the process list from
//! `NtQuerySystemInformation(SystemProcessInformation)` (what Task Manager
//! reads, so the service account sees every process), and which process
//! hosts which service from the Service Control Manager.

use windows_sys::Win32::System::Services::{
    EnumServicesStatusExW, ENUM_SERVICE_STATUS_PROCESSW, SC_ENUM_PROCESS_INFO, SERVICE_ACTIVE, SERVICE_WIN32,
};

/// One process.
#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    /// When it started (100 ns since 1601), so a reused pid is a new process.
    pub created: i64,
    /// `svchost.exe`, `System`.
    pub name: String,
    /// User plus kernel time, 100 ns units.
    pub cpu: u64,
    /// Working set, bytes.
    pub mem: u64,
    /// Bytes read and written through I/O calls (files, devices, network).
    pub read: u64,
    pub write: u64,
}

/// The x64 layout of `SYSTEM_PROCESS_INFORMATION` (the documented struct
/// hides most of these fields as reserved).
#[repr(C)]
struct Spi {
    next: u32,
    threads: u32,
    ws_private: i64,
    hard_faults: u32,
    threads_peak: u32,
    cycles: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    name_len: u16,
    name_max: u16,
    name: *const u16,
    priority: i32,
    pid: usize,
    parent: usize,
    handles: u32,
    session: u32,
    key: usize,
    peak_virtual: usize,
    virtual_size: usize,
    page_faults: u32,
    peak_ws: usize,
    ws: usize,
    peak_paged: usize,
    paged: usize,
    peak_nonpaged: usize,
    nonpaged: usize,
    pagefile: usize,
    peak_pagefile: usize,
    private: usize,
    read_ops: i64,
    write_ops: i64,
    other_ops: i64,
    read_bytes: i64,
    write_bytes: i64,
    other_bytes: i64,
}

const SYSTEM_PROCESS_INFORMATION: i32 = 5;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;

/// Every process but the idle one.
pub fn processes() -> Vec<Proc> {
    let mut buf: Vec<u64> = vec![0; 64 * 1024];
    let mut len = 0u32;
    for _ in 0..4 {
        // SAFETY: a buffer of the given size; Windows writes a chain of entries.
        let st = unsafe {
            windows_sys::Wdk::System::SystemInformation::NtQuerySystemInformation(
                SYSTEM_PROCESS_INFORMATION,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &mut len,
            )
        };
        if st == STATUS_INFO_LENGTH_MISMATCH {
            // Processes come and go between the calls: leave room.
            buf = vec![0; (len as usize).div_ceil(8) + 8 * 1024];
            continue;
        }
        if st != 0 {
            return Vec::new();
        }
        break;
    }
    let mut out = Vec::new();
    let base = buf.as_ptr() as *const u8;
    let mut off = 0usize;
    while off + std::mem::size_of::<Spi>() <= len as usize {
        // SAFETY: inside what Windows wrote; entries are 8-byte aligned.
        let e = unsafe { &*(base.add(off) as *const Spi) };
        if e.pid != 0 {
            let name = if e.name.is_null() || e.name_len == 0 {
                if e.pid == 4 { "System" } else { "?" }.to_owned()
            } else {
                // SAFETY: `name_len` bytes of UTF-16 inside the buffer.
                let s = unsafe { std::slice::from_raw_parts(e.name, usize::from(e.name_len) / 2) };
                String::from_utf16_lossy(s)
            };
            out.push(Proc {
                pid: e.pid as u32,
                created: e.create_time,
                name,
                cpu: (e.user_time.max(0) + e.kernel_time.max(0)) as u64,
                mem: e.ws as u64,
                read: e.read_bytes.max(0) as u64,
                write: e.write_bytes.max(0) as u64,
            });
        }
        if e.next == 0 {
            break;
        }
        off += e.next as usize;
    }
    out
}

/// Running Win32 services and the process each runs in: (name, pid).
pub fn services() -> Vec<(String, u32)> {
    let Ok(m) = super::scm::manager_enumerate() else { return Vec::new() };
    let mut out = Vec::new();
    let mut buf: Vec<u64> = vec![0; 32 * 1024];
    let mut resume = 0u32;
    loop {
        let (mut needed, mut n) = (0u32, 0u32);
        // SAFETY: a buffer of the given size, an open manager handle.
        let ok = unsafe {
            EnumServicesStatusExW(
                m.0,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_ACTIVE,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &mut needed,
                &mut n,
                &mut resume,
                std::ptr::null(),
            )
        };
        let more = ok == 0 && std::io::Error::last_os_error().raw_os_error() == Some(234); // ERROR_MORE_DATA
        if ok == 0 && !more {
            break;
        }
        // SAFETY: `n` entries at the start of the buffer, their strings after them.
        let rows =
            unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW, n as usize) };
        for r in rows {
            let pid = r.ServiceStatusProcess.dwProcessId;
            if pid != 0 && !r.lpServiceName.is_null() {
                // SAFETY: a NUL-terminated string inside the buffer.
                out.push((unsafe { super::from_pwstr(r.lpServiceName) }, pid));
            }
        }
        if !more || out.len() > 4096 {
            break;
        }
        if needed as usize > buf.len() * 8 {
            buf = vec![0; (needed as usize).div_ceil(8)];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_processes_and_services() {
        let p = processes();
        assert!(p.iter().any(|p| p.name.eq_ignore_ascii_case("svchost.exe") && p.mem > 0), "{}", p.len());
        let me = p.iter().find(|p| p.pid == std::process::id()).expect("this process");
        assert!(me.cpu > 0 && me.mem > 0);
        let s = services();
        assert!(s.iter().any(|(n, _)| n.eq_ignore_ascii_case("EventLog")), "{s:?}");
    }
}
