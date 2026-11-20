//! Reads one metrics snapshot on Windows and prints it in the `@@section`
//! format, exactly as the app's own Windows sampler does
//! (`core/src/win/metrics.rs` in the app, D-191), so history synced from the
//! node lines up with samples the app took over SSH:
//!
//! - `stat`: per CPU, user, privileged and idle time from the raw
//!   `Processor` counters in 10 ms units (`_Total` first as `cpu`);
//! - `meminfo`: physical memory and page files in kB;
//! - `uptime`, `kernel` (`Windows Server 2022 Standard 10.0.20348`),
//!   `hostname`; `loadavg` stays empty (Windows has none);
//! - `netdev`: cumulative bytes per `Network Interface` instance;
//! - `diskstats`/`blocks`: cumulative bytes per `PhysicalDisk` as 512-byte
//!   sectors, named `disk0`, `disk1`…;
//! - `mounts`/`df`: fixed volumes (`C:` → `C:\`, file system in lower case);
//! - `thermal`: ACPI thermal zones; `nvidia`: `nvidia-smi` when installed.
//!
//! No PowerShell and no WMI: counters come from PDH, which the service
//! account may read, and the rest from Win32 calls. `nvidia-smi` is the only
//! child process, at most every [`GPU_EVERY`] and with a timeout.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, GetVolumePathNameW,
};
use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetRawCounterArrayW, PDH_HCOUNTER, PDH_HQUERY,
    PDH_RAW_COUNTER_ITEM_W,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

use super::super::win::{from_pwstr, from_wide, sys, wide};

/// How often `nvidia-smi` runs at most; samples in between reuse its output.
pub const GPU_EVERY: Duration = Duration::from_secs(30);
const NVIDIA_TIMEOUT: Duration = Duration::from_secs(3);
const NVIDIA_QUERY: &str = "--query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw";
const DRIVE_FIXED: u32 = 3;
const PDH_MORE_DATA: u32 = 0x800007D2;

/// The raw counters the sampler reads, in one PDH query.
struct Pdh {
    query: PDH_HQUERY,
    user: PDH_HCOUNTER,
    privileged: PDH_HCOUNTER,
    idle: PDH_HCOUNTER,
    net_rx: PDH_HCOUNTER,
    net_tx: PDH_HCOUNTER,
    disk_read: PDH_HCOUNTER,
    disk_write: PDH_HCOUNTER,
    thermal: PDH_HCOUNTER,
    /// A failed collection is logged once.
    warned: std::sync::atomic::AtomicBool,
}

// SAFETY: PDH handles may be used from any thread, one at a time; the
// reader is owned by the sampler thread.
unsafe impl Send for Pdh {}

impl Pdh {
    fn open() -> Option<Self> {
        let mut query: PDH_HQUERY = std::ptr::null_mut();
        // SAFETY: a fresh query handle.
        let r = unsafe { windows_sys::Win32::System::Performance::PdhOpenQueryW(std::ptr::null(), 0, &mut query) };
        if r != 0 {
            crate::warn!("performance counters unavailable: PDH status {r:#010x}");
            return None;
        }
        let add = |path: &str| -> PDH_HCOUNTER {
            let w = wide(path);
            let mut c: PDH_HCOUNTER = std::ptr::null_mut();
            // SAFETY: valid query and path. A counter missing on this host
            // (no thermal zones, say) stays null and reads as nothing.
            let r = unsafe { PdhAddEnglishCounterW(query, w.as_ptr(), 0, &mut c) };
            if r != 0 {
                crate::warn!("performance counter {path}: PDH status {r:#010x}");
                return std::ptr::null_mut();
            }
            c
        };
        Some(Self {
            query,
            user: add(r"\Processor(*)\% User Time"),
            privileged: add(r"\Processor(*)\% Privileged Time"),
            idle: add(r"\Processor(*)\% Processor Time"),
            net_rx: add(r"\Network Interface(*)\Bytes Received/sec"),
            net_tx: add(r"\Network Interface(*)\Bytes Sent/sec"),
            disk_read: add(r"\PhysicalDisk(*)\Disk Read Bytes/sec"),
            disk_write: add(r"\PhysicalDisk(*)\Disk Write Bytes/sec"),
            thermal: add(r"\Thermal Zone Information(*)\Temperature"),
            warned: Default::default(),
        })
    }

    fn collect(&self) -> bool {
        // SAFETY: valid query.
        let r = unsafe { PdhCollectQueryData(self.query) };
        if r != 0 && !self.warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
            crate::warn!("reading performance counters failed: PDH status {r:#010x}");
        }
        r == 0
    }

    /// Instance name → raw first value.
    fn raw(&self, c: PDH_HCOUNTER) -> Vec<(String, i64)> {
        if c.is_null() {
            return Vec::new();
        }
        let (mut size, mut count) = (0u32, 0u32);
        // SAFETY: size query.
        let r = unsafe { PdhGetRawCounterArrayW(c, &mut size, &mut count, std::ptr::null_mut()) };
        if r != PDH_MORE_DATA || size == 0 {
            return Vec::new();
        }
        // u64 storage keeps the items aligned.
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        let items = buf.as_mut_ptr() as *mut PDH_RAW_COUNTER_ITEM_W;
        // SAFETY: a buffer of the size PDH asked for.
        if unsafe { PdhGetRawCounterArrayW(c, &mut size, &mut count, items) } != 0 {
            return Vec::new();
        }
        (0..count as usize)
            .map(|i| {
                // SAFETY: PDH wrote `count` items; names point into `buf`.
                let it = unsafe { &*items.add(i) };
                (unsafe { from_pwstr(it.szName) }, it.RawValue.FirstValue)
            })
            .collect()
    }
}

impl Drop for Pdh {
    fn drop(&mut self) {
        // SAFETY: we own the query.
        unsafe { PdhCloseQuery(self.query) };
    }
}

pub struct Reader {
    pdh: Option<Pdh>,
    nvidia: Option<PathBuf>,
    gpu_cache: Option<(Instant, String)>,
}

/// 100 ns ticks to the 10 ms "jiffies" `/proc/stat` uses.
fn jiffies(v: i64) -> u64 {
    (v.max(0) as u64) / 100_000
}

impl Reader {
    pub fn system() -> Self {
        let sys32 = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let nvidia = [
            sys32.join(r"System32\nvidia-smi.exe"),
            super::super::win::program_files().join(r"NVIDIA Corporation\NVSMI\nvidia-smi.exe"),
        ]
        .into_iter()
        .find(|p| p.is_file());
        Self { pdh: Pdh::open(), nvidia, gpu_cache: None }
    }

    /// One snapshot in `@@section` format.
    pub fn read(&mut self) -> String {
        let mut o = String::with_capacity(8 * 1024);
        let ok = self.pdh.as_ref().is_some_and(Pdh::collect);
        let pdh = self.pdh.as_ref().filter(|_| ok);
        let raw = |c: fn(&Pdh) -> PDH_HCOUNTER| pdh.map(|p| p.raw(c(p))).unwrap_or_default();

        o.push_str("@@stat\n");
        o.push_str(&stat(&raw(|p| p.user), &raw(|p| p.privileged), &raw(|p| p.idle)));
        o.push_str("@@meminfo\n");
        o.push_str(&meminfo());
        o.push_str("@@loadavg\n@@uptime\n");
        let _ = writeln!(o, "{}", sys::uptime_secs().floor() as u64);
        let _ = writeln!(o, "@@kernel\n{}", sys::os_caption());
        let _ = writeln!(o, "@@hostname\n{}", std::env::var("COMPUTERNAME").unwrap_or_else(|_| sys::hostname()));
        o.push_str("@@netdev\n");
        o.push_str(&netdev(&raw(|p| p.net_rx), &raw(|p| p.net_tx)));
        let (diskstats, names) = diskstats(&raw(|p| p.disk_read), &raw(|p| p.disk_write));
        o.push_str("@@diskstats\n");
        o.push_str(&diskstats);
        let _ = writeln!(o, "@@blocks\n{}", names.join(" "));
        let vols = volumes();
        o.push_str("@@mounts\n");
        for v in &vols {
            let _ = writeln!(o, "{} {} {}", v.device, v.root, v.fs);
        }
        o.push_str("@@df\n");
        for v in &vols {
            if let Some((t, u, a)) = volume_kib(Path::new(&v.root)) {
                let pc = if t > 0 { ((u as f64) * 100.0 / t as f64).round() as u64 } else { 0 };
                let _ = writeln!(o, "{} {t} {u} {a} {pc}% {}", v.device, v.root);
            }
        }
        o.push_str("@@hwmon\n@@thermal\n");
        for (_, k) in raw(|p| p.thermal) {
            if k > 0 {
                // Kelvin, like MSAcpi_ThermalZoneTemperature / 10.
                let _ = writeln!(o, "acpitz|{}", ((k as f64 - 273.15) * 1000.0) as i64);
            }
        }
        o.push_str("@@nvidia\n");
        o.push_str(&self.nvidia_smi());
        o.push_str("@@end\n");
        o
    }

    fn nvidia_smi(&mut self) -> String {
        let Some(bin) = self.nvidia.clone() else { return String::new() };
        if let Some((at, out)) = &self.gpu_cache {
            if at.elapsed() < GPU_EVERY {
                return out.clone();
            }
        }
        let out =
            run_with_timeout(Command::new(bin).args([NVIDIA_QUERY, "--format=csv,noheader,nounits"]), NVIDIA_TIMEOUT)
                .unwrap_or_default();
        self.gpu_cache = Some((Instant::now(), out.clone()));
        out
    }
}

/// The `@@stat` body: `_Total` as `cpu`, then each CPU by number.
pub fn stat(user: &[(String, i64)], privileged: &[(String, i64)], idle: &[(String, i64)]) -> String {
    let get = |v: &[(String, i64)], n: &str| v.iter().find(|(k, _)| k == n).map(|(_, x)| *x).unwrap_or(0);
    let mut names: Vec<&String> = user.iter().map(|(k, _)| k).filter(|k| *k != "_Total").collect();
    names.sort_by_key(|k| k.parse::<u32>().unwrap_or(u32::MAX));
    names.dedup();
    let mut o = String::new();
    if user.iter().any(|(k, _)| k == "_Total") {
        let n = "_Total";
        let _ = writeln!(
            o,
            "cpu {} 0 {} {} 0 0 0 0",
            jiffies(get(user, n)),
            jiffies(get(privileged, n)),
            jiffies(get(idle, n))
        );
    }
    for n in names {
        let _ = writeln!(
            o,
            "cpu{n} {} 0 {} {} 0 0 0 0",
            jiffies(get(user, n)),
            jiffies(get(privileged, n)),
            jiffies(get(idle, n))
        );
    }
    o
}

/// The `@@netdev` body: `NAME: rx 0… tx 0…`, `:` in names replaced by `_`.
pub fn netdev(rx: &[(String, i64)], tx: &[(String, i64)]) -> String {
    let mut o = String::new();
    for (name, r) in rx {
        let t = tx.iter().find(|(k, _)| k == name).map(|(_, v)| *v).unwrap_or(0);
        let name = name.replace(':', "_");
        let _ = writeln!(o, "{}: {} 0 0 0 0 0 0 0 {} 0 0 0 0 0 0 0", name.trim(), r.max(&0), t.max(0));
    }
    o
}

/// The `@@diskstats` body and the disk names for `@@blocks`.
pub fn diskstats(read: &[(String, i64)], write: &[(String, i64)]) -> (String, Vec<String>) {
    let mut o = String::new();
    let mut names = Vec::new();
    for (inst, r) in read.iter().filter(|(k, _)| k != "_Total") {
        let w = write.iter().find(|(k, _)| k == inst).map(|(_, v)| *v).unwrap_or(0);
        let id = format!("disk{}", inst.split(' ').next().unwrap_or(inst));
        let _ = writeln!(o, "0 0 {id} 0 0 {} 0 0 0 {}", r.max(&0) / 512, w.max(0) / 512);
        names.push(id);
    }
    (o, names)
}

fn meminfo() -> String {
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized struct.
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
        return String::new();
    }
    let (total, avail) = (m.ullTotalPhys / 1024, m.ullAvailPhys / 1024);
    let (swap_total, swap_used) = page_files_kib();
    format!(
        "MemTotal: {total} kB\nMemFree: {avail} kB\nMemAvailable: {avail} kB\nSwapTotal: {swap_total} kB\nSwapFree: {} kB\n",
        swap_total.saturating_sub(swap_used)
    )
}

/// Page files: (allocated, in use), kB, like `Win32_PageFileUsage`.
fn page_files_kib() -> (u64, u64) {
    #[repr(C)]
    struct PageFile {
        next: u32,
        total: u32,
        in_use: u32,
        peak: u32,
        name_len: u16,
        name_max: u16,
        name: *mut u16,
    }
    const SYSTEM_PAGEFILE_INFORMATION: i32 = 18;
    let mut buf = vec![0u64; 512];
    let mut len = 0u32;
    // SAFETY: a buffer of the given size; Windows writes a chain of entries.
    let st = unsafe {
        windows_sys::Wdk::System::SystemInformation::NtQuerySystemInformation(
            SYSTEM_PAGEFILE_INFORMATION,
            buf.as_mut_ptr().cast(),
            (buf.len() * 8) as u32,
            &mut len,
        )
    };
    if st != 0 || len == 0 {
        return (0, 0);
    }
    let page_kib = 4u64;
    let (mut total, mut used) = (0u64, 0u64);
    let base = buf.as_ptr() as *const u8;
    let mut off = 0usize;
    loop {
        if off + std::mem::size_of::<PageFile>() > len as usize {
            break;
        }
        // SAFETY: inside what Windows wrote; entries are aligned.
        let e = unsafe { &*(base.add(off) as *const PageFile) };
        total += u64::from(e.total) * page_kib;
        used += u64::from(e.in_use) * page_kib;
        if e.next == 0 {
            break;
        }
        off += e.next as usize;
    }
    (total, used)
}

/// A fixed volume with a drive letter.
#[derive(Debug, Clone)]
pub struct Volume {
    /// `C:`
    pub device: String,
    /// `C:\`
    pub root: String,
    /// `ntfs`, `refs`
    pub fs: String,
}

/// Fixed volumes with a drive letter, like `Win32_LogicalDisk` with
/// `DriveType=3`.
pub fn volumes() -> Vec<Volume> {
    // SAFETY: no preconditions.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u32)
        .filter(|i| mask & (1 << i) != 0)
        .filter_map(|i| {
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            let w = wide(&root);
            // SAFETY: valid string.
            if unsafe { GetDriveTypeW(w.as_ptr()) } != DRIVE_FIXED {
                return None;
            }
            let mut fs = vec![0u16; 64];
            // SAFETY: buffer and its length match; other outputs are skipped.
            let ok = unsafe {
                GetVolumeInformationW(
                    w.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    fs.as_mut_ptr(),
                    fs.len() as u32,
                )
            };
            let fs = if ok != 0 { from_wide(&fs).to_lowercase() } else { String::new() };
            Some(Volume { device: format!("{letter}:"), root, fs })
        })
        .collect()
}

/// (total, used, available) KiB of the volume holding `path`.
pub fn volume_kib(path: &Path) -> Option<(u64, u64, u64)> {
    let w = wide(path.as_os_str());
    let mut root = vec![0u16; 520];
    // SAFETY: buffer and length match.
    let w = if unsafe { GetVolumePathNameW(w.as_ptr(), root.as_mut_ptr(), root.len() as u32) } != 0 { root } else { w };
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: valid string and out-params.
    if unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) } == 0 || total == 0 {
        return None;
    }
    let (t, a) = (total / 1024, free / 1024);
    Some((t, t.saturating_sub(a), avail / 1024))
}

/// Runs a command with a timeout, returning its stdout if it exited 0.
pub fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut out, &mut s);
        s
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                let s = reader.join().ok()?;
                return st.success().then_some(s);
            }
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(p: &[(&str, i64)]) -> Vec<(String, i64)> {
        p.iter().map(|(k, x)| ((*k).to_owned(), *x)).collect()
    }

    #[test]
    fn sections_match_the_apps_sampler() {
        let s = stat(
            &v(&[("1", 500_000), ("_Total", 1_000_000), ("0", 500_000), ("10", 1), ("2", 1)]),
            &v(&[("_Total", 2_000_000), ("0", 1_000_000)]),
            &v(&[("_Total", 100_000_000), ("0", 50_000_000)]),
        );
        assert_eq!(
            s,
            "cpu 10 0 20 1000 0 0 0 0\ncpu0 5 0 10 500 0 0 0 0\ncpu1 5 0 0 0 0 0 0 0\ncpu2 0 0 0 0 0 0 0 0\ncpu10 0 0 0 0 0 0 0 0\n"
        );
        let n = netdev(&v(&[("Intel[R] Ethernet: x", 1000)]), &v(&[("Intel[R] Ethernet: x", 2000)]));
        assert_eq!(n, "Intel[R] Ethernet_ x: 1000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\n");
        let (d, names) = diskstats(&v(&[("0 C:", 1_048_576), ("_Total", 9)]), &v(&[("0 C:", 2_097_152)]));
        assert_eq!(d, "0 0 disk0 0 0 2048 0 0 0 4096\n");
        assert_eq!(names, ["disk0"]);
        // The whole thing goes through the vendored parser like the app's.
        let out = format!(
            "@@stat\n{s}@@meminfo\nMemTotal: 1024 kB\nMemFree: 512 kB\nMemAvailable: 512 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n@@loadavg\n@@uptime\n5\n@@kernel\nWindows Server 2022 Standard 10.0.20348\n@@hostname\nSRV01\n@@netdev\n{n}@@diskstats\n{d}@@blocks\ndisk0\n@@mounts\nC: C:\\ ntfs\n@@df\nC: 100 50 50 50% C:\\\n@@hwmon\n@@thermal\n@@nvidia\n@@end\n"
        );
        let r = crate::metrics::parse(&out);
        assert_eq!(r.cores.len(), 4);
        assert_eq!(r.hostname, "SRV01");
        assert_eq!(r.disks[0].read_sectors, 2048);
        assert_eq!(r.mounts[0].mount, "C:\\");
    }

    /// The real counters on this machine (Windows CI).
    #[test]
    fn reads_this_machine() {
        let mut r = Reader::system();
        let a = crate::metrics::parse(&r.read());
        std::thread::sleep(Duration::from_millis(300));
        let b = crate::metrics::parse(&r.read());
        assert!(a.cpu.is_some() && !a.cores.is_empty());
        assert!(a.mem.total > 0 && a.mem.used() > 0 && a.mem.used() < a.mem.total);
        assert!(!a.hostname.is_empty() && a.kernel.contains("Windows"), "{}", a.kernel);
        assert!(a.uptime > 0.0);
        assert!(!a.mounts.is_empty() && a.mounts.iter().any(|m| m.total > 0));
        assert!(!a.disks.is_empty());
        let mut c = crate::metrics::Collector::new();
        c.sample(a, 0.0);
        let s = c.sample(b, 300.0);
        let cpu = s.cpu.expect("cpu % from two samples");
        assert!((0.0..=100.0).contains(&cpu), "{cpu}");
    }
}
