//! Reads one metrics snapshot straight from `/proc`, `/sys` and `statvfs(3)`
//! and prints it in the `@@section` format of the app's `COLLECT` script, so
//! the vendored parser gives the same result as the app's SSH path.
//!
//! No shell is spawned per sample. The only child process is `nvidia-smi`
//! (when present), at most every [`GPU_EVERY`] and with a timeout.
//! Network filesystems are `statvfs`ed on a helper thread with a timeout so
//! a hung NFS server can't stall sampling; a mount that timed out is skipped
//! until that call returns. Sensors (hwmon, thermal, GPU sysfs) are read the
//! same way, keeping the last readings while a slow read is still running.

use std::collections::HashMap;
use std::ffi::CString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// How often `nvidia-smi` runs at most; samples in between reuse its output.
pub const GPU_EVERY: Duration = Duration::from_secs(30);
const NVIDIA_TIMEOUT: Duration = Duration::from_secs(3);
const NETFS_TIMEOUT: Duration = Duration::from_secs(2);
const SENSORS_TIMEOUT: Duration = Duration::from_secs(1);

const NVIDIA_QUERY: &str = "--query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw";

pub struct Reader {
    root: PathBuf,
    nvidia: Option<PathBuf>,
    gpu_cache: Option<(Instant, String)>,
    /// Network mounts with a `statvfs` still running on a helper thread.
    busy: HashMap<String, Arc<AtomicBool>>,
    /// Sensor reads still running on a helper thread, and the last result.
    sensors_rx: Option<mpsc::Receiver<[String; 3]>>,
    sensors_last: [String; 3],
}

impl Reader {
    /// Reads the real system.
    pub fn system() -> Self {
        let nvidia = ["/usr/bin/nvidia-smi", "/usr/local/bin/nvidia-smi", "/bin/nvidia-smi", "/opt/bin/nvidia-smi"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file());
        Self { root: PathBuf::from("/"), nvidia, ..Self::at("/") }
    }

    /// Reads a fake tree (tests): `root/proc/stat`, `root/sys/...`.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            nvidia: None,
            gpu_cache: None,
            busy: HashMap::new(),
            sensors_rx: None,
            sensors_last: Default::default(),
        }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel.trim_start_matches('/'))
    }

    fn cat(&self, rel: &str) -> String {
        fs::read_to_string(self.p(rel)).unwrap_or_default()
    }

    /// One snapshot in `@@section` format.
    pub fn read(&mut self) -> String {
        let mut o = String::with_capacity(16 * 1024);
        let section = |o: &mut String, name: &str, body: &str| {
            o.push_str("@@");
            o.push_str(name);
            o.push('\n');
            o.push_str(body);
            if !body.is_empty() && !body.ends_with('\n') {
                o.push('\n');
            }
        };
        section(&mut o, "stat", &self.cat("proc/stat"));
        section(&mut o, "meminfo", &self.cat("proc/meminfo"));
        section(&mut o, "loadavg", &self.cat("proc/loadavg"));
        section(&mut o, "uptime", &self.cat("proc/uptime"));
        let release = self.cat("proc/sys/kernel/osrelease");
        let kernel = if release.trim().is_empty() { String::new() } else { format!("Linux {}", release.trim()) };
        section(&mut o, "kernel", &kernel);
        section(&mut o, "hostname", self.cat("proc/sys/kernel/hostname").trim());
        section(&mut o, "netdev", &self.cat("proc/net/dev"));
        section(&mut o, "diskstats", &self.cat("proc/diskstats"));
        section(&mut o, "blocks", &list_dir(&self.p("sys/block")).join(" "));
        let mounts = self.cat("proc/mounts");
        section(&mut o, "mounts", &mounts);
        let df = self.df(&mounts);
        section(&mut o, "df", &df);
        let [hwmon, thermal, drm] = self.sensors();
        section(&mut o, "hwmon", &hwmon);
        section(&mut o, "thermal", &thermal);
        let nv = self.nvidia_smi();
        section(&mut o, "nvidia", &nv);
        section(&mut o, "drm", &drm);
        o.push_str("@@end\n");
        o
    }

    /// `df -kP`-style rows for the mounts worth measuring.
    fn df(&mut self, mounts: &str) -> String {
        let mut out = String::from("Filesystem 1024-blocks Used Available Capacity Mounted on\n");
        let mut done = std::collections::HashSet::new();
        for l in mounts.lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 3 {
                continue;
            }
            let (dev, mount, fstype) = (f[0], unescape(f[1]), f[2]);
            if skip_mount(&mount, fstype) || !done.insert(mount.clone()) {
                continue;
            }
            let path = if self.root == Path::new("/") { PathBuf::from(&mount) } else { self.p(&mount) };
            let st = if is_netfs(fstype) { self.statvfs_guarded(&mount, path) } else { statvfs(&path) };
            let Some((total, used, avail)) = st else { continue };
            let pct = if used + avail == 0 { 0 } else { (used * 100).div_ceil(used + avail) };
            let _ = writeln!(out, "{dev} {total} {used} {avail} {pct}% {mount}");
        }
        out
    }

    /// hwmon, thermal zones and GPU sysfs files, read on a helper thread.
    /// Some drivers answer a temperature read by asking the device (NVMe
    /// drives, ACPI firmware) and can take a minute or hang; sampling then
    /// carries on with the last good readings until that read returns.
    fn sensors(&mut self) -> [String; 3] {
        if self.sensors_rx.is_none() {
            let (tx, rx) = mpsc::channel();
            let root = self.root.clone();
            let spawned = std::thread::Builder::new().name("sensors".into()).spawn(move || {
                let _ = tx.send([Self::hwmon(&root), Self::thermal(&root), Self::drm(&root)]);
            });
            if spawned.is_err() {
                return self.sensors_last.clone();
            }
            self.sensors_rx = Some(rx);
        }
        if let Some(rx) = &self.sensors_rx {
            match rx.recv_timeout(SENSORS_TIMEOUT) {
                Ok(v) => {
                    self.sensors_last = v;
                    self.sensors_rx = None;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    crate::debug!("sensor reads are slow; reusing the last readings");
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => self.sensors_rx = None,
            }
        }
        self.sensors_last.clone()
    }

    fn statvfs_guarded(&mut self, mount: &str, path: PathBuf) -> Option<(u64, u64, u64)> {
        if let Some(flag) = self.busy.get(mount) {
            if flag.load(Ordering::Acquire) {
                return None;
            }
            self.busy.remove(mount);
        }
        let flag = Arc::new(AtomicBool::new(true));
        let (tx, rx) = mpsc::channel();
        let f = flag.clone();
        let spawned = std::thread::Builder::new().name("statvfs".into()).spawn(move || {
            let r = statvfs(&path);
            f.store(false, Ordering::Release);
            let _ = tx.send(r);
        });
        if spawned.is_err() {
            return None;
        }
        match rx.recv_timeout(NETFS_TIMEOUT) {
            Ok(r) => r,
            Err(_) => {
                crate::warn!("statvfs on {mount} is not answering; skipping it until it does");
                self.busy.insert(mount.to_owned(), flag);
                None
            }
        }
    }

    fn hwmon(root: &Path) -> String {
        let mut out = String::new();
        let base = root.join("sys/class/hwmon");
        for h in list_dir(&base).into_iter().filter(|n| n.starts_with("hwmon")) {
            let dir = base.join(&h);
            let name = fs::read_to_string(dir.join("name")).unwrap_or_default();
            let mut inputs: Vec<String> =
                list_dir(&dir).into_iter().filter(|f| f.starts_with("temp") && f.ends_with("_input")).collect();
            inputs.sort_by_key(|f| sensor_index(f));
            for t in inputs {
                let Ok(v) = fs::read_to_string(dir.join(&t)) else { continue };
                let label_file = format!("{}_label", t.trim_end_matches("_input"));
                let label = fs::read_to_string(dir.join(label_file)).unwrap_or_default();
                let _ = writeln!(out, "{}|{}|{}", name.trim(), v.trim(), label.trim());
            }
        }
        out
    }

    fn thermal(root: &Path) -> String {
        let mut out = String::new();
        let base = root.join("sys/class/thermal");
        for z in list_dir(&base).into_iter().filter(|n| n.starts_with("thermal_zone")) {
            let Ok(t) = fs::read_to_string(base.join(&z).join("temp")) else { continue };
            let kind = fs::read_to_string(base.join(&z).join("type")).unwrap_or_default();
            let _ = writeln!(out, "{}|{}", kind.trim(), t.trim());
        }
        out
    }

    fn drm(root: &Path) -> String {
        let mut out = String::new();
        let base = root.join("sys/class/drm");
        for c in list_dir(&base) {
            let Some(n) = c.strip_prefix("card") else { continue };
            if n.is_empty() || n.len() > 2 || !n.chars().all(|ch| ch.is_ascii_digit()) {
                continue;
            }
            let d = base.join(&c).join("device");
            let Ok(busy) = fs::read_to_string(d.join("gpu_busy_percent")) else { continue };
            let rd = |f: &str| fs::read_to_string(d.join(f)).unwrap_or_default().trim().to_owned();
            let h = list_dir(&d.join("hwmon")).into_iter().find(|x| x.starts_with("hwmon"));
            let hr = |f: &str| {
                h.as_ref()
                    .and_then(|h| fs::read_to_string(d.join("hwmon").join(h).join(f)).ok())
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            };
            let _ = writeln!(
                out,
                "{c}|{}|{}|{}|{}|{}|{}",
                busy.trim(),
                rd("mem_info_vram_used"),
                rd("mem_info_vram_total"),
                hr("temp1_input"),
                hr("power1_average"),
                rd("product_name")
            );
        }
        out
    }

    fn nvidia_smi(&mut self) -> String {
        let Some(bin) = self.nvidia.clone() else { return String::new() };
        if let Some((at, out)) = &self.gpu_cache {
            if at.elapsed() < GPU_EVERY {
                return out.clone();
            }
        }
        let out =
            run_with_timeout(Command::new(bin).arg(NVIDIA_QUERY).arg("--format=csv,noheader,nounits"), NVIDIA_TIMEOUT)
                .unwrap_or_default();
        self.gpu_cache = Some((Instant::now(), out.clone()));
        out
    }
}

/// `/proc/mounts` escapes spaces, tabs, newlines and backslashes as octal.
fn unescape(s: &str) -> String {
    s.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\")
}

/// Mounts never worth a `statvfs` (the parser drops them anyway).
fn skip_mount(mount: &str, fstype: &str) -> bool {
    const PSEUDO: [&str; 26] = [
        "proc",
        "sysfs",
        "devtmpfs",
        "devpts",
        "tmpfs",
        "securityfs",
        "cgroup",
        "cgroup2",
        "pstore",
        "bpf",
        "debugfs",
        "tracefs",
        "configfs",
        "fusectl",
        "mqueue",
        "hugetlbfs",
        "autofs",
        "rpc_pipefs",
        "binfmt_misc",
        "overlay",
        "squashfs",
        "nsfs",
        "ramfs",
        "efivarfs",
        "fuse.lxcfs",
        "fuse.snapfuse",
    ];
    PSEUDO.contains(&fstype)
        || ["/proc", "/sys", "/dev", "/run/"].iter().any(|p| mount.starts_with(p))
        || mount.starts_with("/snap/")
        || mount.starts_with("/var/lib/docker/")
}

fn is_netfs(fstype: &str) -> bool {
    matches!(fstype, "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs" | "9p" | "ceph" | "glusterfs" | "davfs")
        || fstype.starts_with("fuse.")
}

/// (total, used, avail) in KiB, like `df -kP`.
fn statvfs(path: &Path) -> Option<(u64, u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a writable statvfs.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)] // c_ulong is 32-bit on 32-bit targets.
    let frsize = if st.f_frsize > 0 { st.f_frsize as u64 } else { st.f_bsize as u64 };
    #[allow(clippy::unnecessary_cast)]
    Some(usage_kib(st.f_blocks as u64, st.f_bfree as u64, st.f_bavail as u64, frsize))
}

/// (total, used, avail) in KiB from statvfs block counts, all in u64 so a
/// disk over 4 GiB (or 2^32 blocks) adds up on 32-bit hosts too.
pub fn usage_kib(blocks: u64, bfree: u64, bavail: u64, frsize: u64) -> (u64, u64, u64) {
    let kib = |b: u64| b.saturating_mul(frsize) / 1024;
    (kib(blocks), kib(blocks.saturating_sub(bfree)), kib(bavail))
}

fn list_dir(p: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(p)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned())).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// `temp12_input` → 12, so sensors keep the kernel's order.
fn sensor_index(f: &str) -> u32 {
    f.trim_start_matches("temp").trim_end_matches("_input").parse().unwrap_or(u32::MAX)
}

/// Runs a command, killing it after `timeout`; stdout on success.
pub fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                let mut out = String::new();
                use std::io::Read;
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return st.success().then_some(out);
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
    use crate::metrics::{parse, Collector};

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    /// A fake tree read through the collector gives what the app's script would.
    #[test]
    fn fake_tree_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        write(r, "proc/stat", "cpu  100 0 50 1000 0 0 0 0\ncpu0 100 0 50 1000 0 0 0 0\n");
        write(r, "proc/meminfo", "MemTotal: 1000 kB\nMemAvailable: 600 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n");
        write(r, "proc/loadavg", "0.10 0.20 0.30 1/100 42\n");
        write(r, "proc/uptime", "123.45 100.00\n");
        write(r, "proc/sys/kernel/osrelease", "6.1.0-test\n");
        write(r, "proc/sys/kernel/hostname", "node01\n");
        write(r, "proc/net/dev", "Inter-|\n face |\n  eth0: 100 0 0 0 0 0 0 0 200 0 0 0 0 0 0 0\n");
        write(r, "proc/diskstats", "   8       0 sda 1 0 64 0 2 0 128 0 0 0 0\n");
        write(r, "sys/block/sda/size", "1\n");
        write(r, "proc/mounts", "/dev/sda1 / ext4 rw 0 0\nproc /proc proc rw 0 0\n");
        write(r, "sys/class/hwmon/hwmon0/name", "coretemp\n");
        write(r, "sys/class/hwmon/hwmon0/temp1_input", "45000\n");
        write(r, "sys/class/hwmon/hwmon0/temp1_label", "Package id 0\n");
        write(r, "sys/class/hwmon/hwmon0/temp10_input", "40000\n");
        write(r, "sys/class/hwmon/hwmon0/temp2_input", "41000\n");
        write(r, "sys/class/thermal/thermal_zone0/type", "acpitz\n");
        write(r, "sys/class/thermal/thermal_zone0/temp", "30000\n");
        write(r, "sys/class/drm/card0/device/gpu_busy_percent", "7\n");
        write(r, "sys/class/drm/card0/device/product_name", "Test GPU\n");
        write(r, "sys/class/drm/card0-DP-1/device/gpu_busy_percent", "99\n");

        let text = Reader::at(r).read();
        assert!(text.ends_with("@@end\n"), "{text}");
        let raw = parse(&text);
        assert_eq!(raw.hostname, "node01");
        assert_eq!(raw.kernel, "Linux 6.1.0-test");
        assert_eq!(raw.uptime, 123.45);
        assert_eq!(raw.mem.used(), 400 * 1024);
        assert_eq!(raw.cores.len(), 1);
        assert_eq!(raw.blocks, ["sda"]);
        // `/` was statvfs'ed (the temp dir's filesystem) and joined with /proc/mounts.
        assert_eq!(raw.mounts.len(), 1, "{text}");
        assert_eq!((raw.mounts[0].mount.as_str(), raw.mounts[0].fstype.as_str()), ("/", "ext4"));
        assert!(raw.mounts[0].total > 0);
        // hwmon in kernel order (temp1, temp2, temp10), thermal ignored when hwmon exists.
        let temps: Vec<f32> = raw.temps.iter().map(|t| t.celsius).collect();
        assert_eq!(temps, [45.0, 41.0, 40.0]);
        // Only real cards (not connectors).
        assert_eq!(raw.gpus.len(), 1);
        assert_eq!((raw.gpus[0].name.as_str(), raw.gpus[0].util), ("Test GPU", Some(7.0)));
        let s = Collector::new().sample(raw, 0.0);
        assert_eq!(s.cpu_temp, Some(45.0));
    }

    #[test]
    fn empty_tree_still_parses() {
        let d = tempfile::tempdir().unwrap();
        let raw = parse(&Reader::at(d.path()).read());
        assert_eq!(raw, crate::metrics::Raw::default());
    }

    #[test]
    fn mount_escapes_and_filters() {
        assert_eq!(unescape("/mnt/media\\040pool"), "/mnt/media pool");
        assert!(skip_mount("/proc", "proc"));
        assert!(skip_mount("/run/user/1000", "ext4"));
        assert!(!skip_mount("/srv", "xfs"));
        assert!(is_netfs("nfs4") && is_netfs("fuse.sshfs") && !is_netfs("zfs"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_this_machine() {
        let raw = parse(&Reader::system().read());
        assert!(raw.cpu.is_some() && !raw.cores.is_empty());
        assert!(raw.mem.total > 0);
        assert!(raw.kernel.starts_with("Linux "));
    }

    #[test]
    fn a_hanging_sensor_does_not_stall_sampling() {
        use std::os::unix::ffi::OsStrExt;
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "proc/stat", "cpu  1 0 1 10 0 0 0 0 0 0\n");
        write(d.path(), "sys/class/hwmon/hwmon0/name", "nvme\n");
        // A FIFO with no writer blocks the reader in open(), like a drive
        // that doesn't answer a temperature query.
        let fifo = d.path().join("sys/class/hwmon/hwmon0/temp1_input");
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let mut r = Reader::at(d.path());
        for _ in 0..3 {
            let t = Instant::now();
            let out = r.read();
            assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
            assert!(out.contains("@@hwmon\n@@thermal"), "{out}");
        }
        // Unblock the stuck read (the writer's close is EOF), swap in a real
        // file, and the sensor comes back within a couple of samples.
        drop(fs::OpenOptions::new().write(true).open(&fifo).unwrap());
        fs::remove_file(&fifo).unwrap();
        write(d.path(), "sys/class/hwmon/hwmon0/temp1_input", "41000\n");
        assert!((0..5).any(|_| r.read().contains("nvme|41000|")));
    }

    #[test]
    fn timeout_kills_slow_commands() {
        let t = Instant::now();
        assert_eq!(run_with_timeout(Command::new("sleep").arg("5"), Duration::from_millis(200)), None);
        assert!(t.elapsed() < Duration::from_secs(2));
        assert_eq!(run_with_timeout(Command::new("echo").arg("hi"), Duration::from_secs(2)).as_deref(), Some("hi\n"));
    }
}
