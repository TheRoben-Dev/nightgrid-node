// Vendored from Nightgrid (https://github.com/TheRoben-Dev/nightgrid),
// core/src/metrics/parse.rs at commit 8eedcf2, MIT License, (c) 2026 TheRoben.
// Kept byte-for-byte compatible in behaviour so node-side history matches the
// app's own; local changes are marked `NODE:`. See docs/DECISIONS.md N-003.

//! Parsers for the sections printed by [`super::COLLECT`]. Each one is
//! forgiving: unknown or malformed lines are skipped, so a host with an odd
//! kernel or busybox userland still yields what it can.

use std::collections::HashMap;

use super::{Gpu, MountUsage, Temp};

/// Jiffies from one `cpu` line of `/proc/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl CpuTimes {
    /// guest/guest_nice are already counted in user/nice.
    pub fn total(&self) -> u64 {
        self.user + self.nice + self.system + self.idle + self.iowait + self.irq + self.softirq + self.steal
    }
    pub fn idle(&self) -> u64 {
        self.idle + self.iowait
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
    pub free: u64,
    pub buffers: u64,
    pub cached: u64,
    pub sreclaimable: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

impl MemInfo {
    /// Bytes in use: total − MemAvailable (older kernels: free + buffers + cache).
    pub fn used(&self) -> u64 {
        let avail = if self.available > 0 {
            self.available
        } else {
            self.free + self.buffers + self.cached + self.sreclaimable
        };
        self.total.saturating_sub(avail)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetCounters {
    pub name: String,
    pub rx: u64,
    pub tx: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskCounters {
    pub name: String,
    pub read_sectors: u64,
    pub write_sectors: u64,
}

/// One parsed run of the collect script.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Raw {
    pub cpu: Option<CpuTimes>,
    pub cores: Vec<CpuTimes>,
    pub mem: MemInfo,
    pub load: [f32; 3],
    pub uptime: f64,
    pub kernel: String,
    pub hostname: String,
    pub net: Vec<NetCounters>,
    pub disks: Vec<DiskCounters>,
    /// Names under `/sys/block` (whole disks); empty if unreadable.
    pub blocks: Vec<String>,
    pub mounts: Vec<MountUsage>,
    pub temps: Vec<Temp>,
    pub gpus: Vec<Gpu>,
}

/// Splits the script output into `@@name` sections.
fn sections(out: &str) -> HashMap<&str, Vec<&str>> {
    let mut map: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut cur: Option<&str> = None;
    for line in out.lines() {
        if let Some(name) = line.strip_prefix("@@") {
            cur = Some(name.trim());
            map.entry(name.trim()).or_default();
        } else if let Some(c) = cur {
            map.entry(c).or_default().push(line);
        }
    }
    map
}

pub fn parse(out: &str) -> Raw {
    let s = sections(out);
    let get = |k: &str| s.get(k).map(Vec::as_slice).unwrap_or(&[]);
    let (cpu, cores) = parse_stat(get("stat"));
    let df = parse_df(get("df"));
    let mut temps = parse_hwmon(get("hwmon"));
    if temps.is_empty() {
        temps = parse_thermal(get("thermal"));
    }
    Raw {
        cpu,
        cores,
        mem: parse_meminfo(get("meminfo")),
        load: parse_loadavg(get("loadavg")),
        uptime: get("uptime").first().and_then(|l| l.split_whitespace().next()?.parse().ok()).unwrap_or(0.0),
        kernel: get("kernel").first().map(|l| l.trim().to_owned()).unwrap_or_default(),
        hostname: get("hostname").first().map(|l| l.trim().to_owned()).unwrap_or_default(),
        net: parse_netdev(get("netdev")),
        disks: parse_diskstats(get("diskstats")),
        blocks: get("blocks").iter().flat_map(|l| l.split_whitespace()).map(str::to_owned).collect(),
        mounts: join_mounts(get("mounts"), &df),
        temps,
        gpus: [parse_nvidia(get("nvidia")), parse_drm(get("drm"))].concat(),
    }
}

pub fn parse_stat(lines: &[&str]) -> (Option<CpuTimes>, Vec<CpuTimes>) {
    let mut total = None;
    let mut cores = Vec::new();
    for l in lines {
        let mut it = l.split_whitespace();
        let Some(name) = it.next() else { continue };
        if !name.starts_with("cpu") {
            continue;
        }
        let v: Vec<u64> = it.map(|x| x.parse().unwrap_or(0)).collect();
        let at = |i: usize| v.get(i).copied().unwrap_or(0);
        let t = CpuTimes {
            user: at(0),
            nice: at(1),
            system: at(2),
            idle: at(3),
            iowait: at(4),
            irq: at(5),
            softirq: at(6),
            steal: at(7),
        };
        if name == "cpu" {
            total = Some(t);
        } else {
            cores.push(t);
        }
    }
    (total, cores)
}

pub fn parse_meminfo(lines: &[&str]) -> MemInfo {
    let mut m = MemInfo::default();
    for l in lines {
        let Some((k, rest)) = l.split_once(':') else { continue };
        let kb: u64 = rest.split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let b = kb * 1024;
        match k.trim() {
            "MemTotal" => m.total = b,
            "MemAvailable" => m.available = b,
            "MemFree" => m.free = b,
            "Buffers" => m.buffers = b,
            "Cached" => m.cached = b,
            "SReclaimable" => m.sreclaimable = b,
            "SwapTotal" => m.swap_total = b,
            "SwapFree" => m.swap_free = b,
            _ => {}
        }
    }
    m
}

pub fn parse_loadavg(lines: &[&str]) -> [f32; 3] {
    let mut out = [0.0; 3];
    if let Some(l) = lines.first() {
        for (i, v) in l.split_whitespace().take(3).enumerate() {
            out[i] = v.parse().unwrap_or(0.0);
        }
    }
    out
}

pub fn parse_netdev(lines: &[&str]) -> Vec<NetCounters> {
    lines
        .iter()
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            let v: Vec<u64> = rest.split_whitespace().map(|x| x.parse().unwrap_or(0)).collect();
            (v.len() >= 9).then(|| NetCounters { name: name.trim().to_owned(), rx: v[0], tx: v[8] })
        })
        .collect()
}

pub fn parse_diskstats(lines: &[&str]) -> Vec<DiskCounters> {
    lines
        .iter()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 10 {
                return None;
            }
            Some(DiskCounters {
                name: f[2].to_owned(),
                read_sectors: f[5].parse().ok()?,
                write_sectors: f[9].parse().ok()?,
            })
        })
        .collect()
}

/// `df -kP` rows by mount point: (device, total, used, avail) in bytes.
pub fn parse_df(lines: &[&str]) -> HashMap<String, (String, u64, u64, u64)> {
    let mut out = HashMap::new();
    for l in lines.iter().skip_while(|l| l.starts_with("Filesystem")) {
        let f: Vec<&str> = l.split_whitespace().collect();
        // Devices and mount points may contain spaces; anchor on the "NN%" column.
        let Some(i) = f.iter().position(|x| x.ends_with('%') && x[..x.len() - 1].parse::<u32>().is_ok()) else {
            continue;
        };
        if i < 4 || i + 1 >= f.len() {
            continue;
        }
        let (Ok(total), Ok(used), Ok(avail)) =
            (f[i - 3].parse::<u64>(), f[i - 2].parse::<u64>(), f[i - 1].parse::<u64>())
        else {
            continue;
        };
        out.insert(f[i + 1..].join(" "), (f[..i - 3].join(" "), total * 1024, used * 1024, avail * 1024));
    }
    out
}

/// Filesystem types that are never interesting for "disk usage".
fn pseudo_fs(t: &str) -> bool {
    const SKIP: [&str; 24] = [
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
    ];
    SKIP.contains(&t) || t == "fuse.lxcfs" || t == "fuse.snapfuse" || t == "fuse.portal"
}

/// Real filesystems from `/proc/mounts`, with sizes from `df`, first mount
/// of each device only (bind mounts and Docker's per-container mounts of the
/// same disk are dropped).
pub fn join_mounts(mounts: &[&str], df: &HashMap<String, (String, u64, u64, u64)>) -> Vec<MountUsage> {
    let mut seen_dev = std::collections::HashSet::new();
    let mut out = Vec::new();
    for l in mounts {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 3 || pseudo_fs(f[2]) {
            continue;
        }
        let mount = f[1].replace("\\040", " ");
        if mount.starts_with("/snap/") || mount.starts_with("/var/lib/docker/") || mount.starts_with("/run/") {
            continue;
        }
        let Some((_, total, used, avail)) = df.get(&mount) else { continue };
        if *total == 0 || !seen_dev.insert(f[0].to_owned()) {
            continue;
        }
        out.push(MountUsage {
            device: f[0].to_owned(),
            mount,
            fstype: f[2].to_owned(),
            total: *total,
            used: *used,
            avail: *avail,
        });
    }
    out
}

fn millideg(v: &str) -> Option<f32> {
    let n: f32 = v.trim().parse().ok()?;
    let c = if n.abs() > 200.0 { n / 1000.0 } else { n };
    // Unplugged or bogus sensors report things like -273 or 255.
    ((-40.0..=150.0).contains(&c) && c != 0.0).then_some((c * 10.0).round() / 10.0)
}

pub fn parse_hwmon(lines: &[&str]) -> Vec<Temp> {
    lines
        .iter()
        .filter_map(|l| {
            let mut p = l.splitn(3, '|');
            let chip = p.next()?.trim();
            let celsius = millideg(p.next()?)?;
            let label = p.next().unwrap_or("").trim();
            Some(Temp { chip: chip.to_owned(), label: label.to_owned(), celsius })
        })
        .collect()
}

pub fn parse_thermal(lines: &[&str]) -> Vec<Temp> {
    lines
        .iter()
        .filter_map(|l| {
            let (kind, v) = l.split_once('|')?;
            Some(Temp { chip: kind.trim().to_owned(), label: String::new(), celsius: millideg(v)? })
        })
        .collect()
}

/// A number, or `None` for blanks and `[N/A]` / `[Not Supported]`.
fn opt_num(v: &str) -> Option<f64> {
    v.trim().parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0)
}

/// `nvidia-smi --format=csv,noheader,nounits` rows:
/// `index, name, util %, mem used MiB, mem total MiB, temp C, power W`.
pub fn parse_nvidia(lines: &[&str]) -> Vec<Gpu> {
    lines
        .iter()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            if f.len() < 7 || f[0].parse::<u32>().is_err() {
                return None;
            }
            let mib = |v: &str| opt_num(v).map(|n| (n * 1024.0 * 1024.0) as u64);
            Some(Gpu {
                name: f[1].to_owned(),
                util: opt_num(f[2]).map(|n| n as f32),
                mem_used: mib(f[3]),
                mem_total: mib(f[4]),
                temp: opt_num(f[5]).map(|n| n as f32),
                power: opt_num(f[6]).map(|n| n as f32),
            })
        })
        .collect()
}

/// `card|busy %|vram used B|vram total B|temp m°C|power µW|product name`
/// from amdgpu's sysfs files.
pub fn parse_drm(lines: &[&str]) -> Vec<Gpu> {
    lines
        .iter()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('|').collect();
            if f.len() < 7 {
                return None;
            }
            let util = opt_num(f[1])? as f32;
            let name =
                if f[6].trim().is_empty() { format!("AMD GPU ({})", f[0].trim()) } else { f[6].trim().to_owned() };
            Some(Gpu {
                name,
                util: Some(util),
                mem_used: opt_num(f[2]).map(|n| n as u64),
                mem_total: opt_num(f[3]).map(|n| n as u64).filter(|n| *n > 0),
                temp: millideg(f[4]),
                power: opt_num(f[5]).map(|n| (n / 1e6) as f32).filter(|w| *w > 0.0),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = include_str!("../../tests/fixtures/metrics/x86_server_a.txt");
    const PI: &str = include_str!("../../tests/fixtures/metrics/pi_busybox.txt");
    const OLD: &str = include_str!("../../tests/fixtures/metrics/old_kernel_partial.txt");

    #[test]
    fn gpus() {
        let n = parse_nvidia(&[
            "0, NVIDIA GeForce RTX 3060, 37, 2048, 12288, 61, 98.52",
            "1, Tesla P4, 0, 0, 7680, [N/A], [N/A]",
            "garbage",
        ]);
        assert_eq!(n.len(), 2);
        assert_eq!(n[0].name, "NVIDIA GeForce RTX 3060");
        assert_eq!(
            (n[0].util, n[0].mem_used, n[0].mem_total, n[0].temp),
            (Some(37.0), Some(2048 << 20), Some(12288 << 20), Some(61.0))
        );
        assert!((n[0].power.unwrap() - 98.52).abs() < 0.01);
        assert_eq!((n[1].temp, n[1].power), (None, None));
        let a = parse_drm(&["card1|12|1073741824|8573157376|54000|23000000|", "card0||||||"]);
        assert_eq!(a.len(), 1);
        assert_eq!(
            (a[0].name.as_str(), a[0].util, a[0].mem_used, a[0].temp, a[0].power),
            ("AMD GPU (card1)", Some(12.0), Some(1 << 30), Some(54.0), Some(23.0))
        );
        let raw = parse("@@nvidia\n0, A, 5, 1, 2, 30, 10\n@@drm\ncard0|50|||||Radeon RX 6600\n@@end\n");
        assert_eq!(raw.gpus.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["A", "Radeon RX 6600"]);
    }

    #[test]
    fn x86_server() {
        let r = parse(A);
        assert_eq!(r.cores.len(), 4);
        assert_eq!(r.hostname, "server01");
        assert_eq!(r.kernel, "Linux 6.8.0-49-generic");
        assert_eq!(r.load, [0.52, 0.61, 0.70]);
        assert_eq!(r.uptime, 864123.45);
        assert_eq!(r.mem.total, 32_000_000 * 1024);
        assert_eq!(r.mem.used(), (32_000_000 - 20_000_000) * 1024);
        assert_eq!(r.mem.swap_total - r.mem.swap_free, 1024 * 1024 * 1024);
        let mounts: Vec<_> = r.mounts.iter().map(|m| (m.mount.as_str(), m.fstype.as_str())).collect();
        assert_eq!(mounts, [("/", "ext4"), ("/boot/efi", "vfat"), ("/mnt/media pool", "nfs4")]);
        assert_eq!(r.mounts[0].used, 120_000_000 * 1024);
        // hwmon wins over thermal zones; bogus sensors dropped.
        assert!(r.temps.iter().any(|t| t.chip == "coretemp" && t.label == "Package id 0" && t.celsius == 48.0));
        assert!(r.temps.iter().all(|t| t.chip != "acpitz" || t.celsius > 0.0));
        assert!(!r.temps.iter().any(|t| t.chip == "x86_pkg_temp"));
    }

    #[test]
    fn pi_thermal_zone_fallback() {
        let r = parse(PI);
        assert_eq!(r.temps, vec![Temp { chip: "cpu-thermal".into(), label: String::new(), celsius: 51.6 }]);
        assert_eq!(r.cores.len(), 4);
        assert!(r.blocks.contains(&"mmcblk0".to_owned()));
    }

    #[test]
    fn missing_sections_and_no_memavailable() {
        let r = parse(OLD);
        assert_eq!(r.cpu.unwrap().user, 100);
        assert!(r.cores.is_empty());
        // No MemAvailable: free + buffers + cached.
        assert_eq!(r.mem.used(), (1000 - 200 - 100 - 300) * 1024);
        assert!(r.mounts.is_empty() && r.temps.is_empty() && r.net.is_empty());
        assert_eq!(parse(""), Raw::default());
    }
}
