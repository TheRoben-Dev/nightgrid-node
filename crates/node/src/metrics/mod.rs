//! Host metrics, computed the same way as the Nightgrid app does it over SSH.
//!
//! [`collect::read`] reads `/proc`, `/sys` and `statvfs` directly (no shell)
//! and prints the same `@@section` text as the app's `COLLECT` script, so the
//! vendored [`parse`] module and [`Collector`] give identical numbers. A
//! [`MinuteAverager`] turns samples into minute [`Point`]s for the store.
//!
// Collector, cpu_temp, is_virtual_iface, is_physical_disk, Point::from_sample
// and MinuteAverager are vendored from Nightgrid
// (https://github.com/TheRoben-Dev/nightgrid) core/src/metrics/mod.rs and
// core/src/metrics/history.rs at commit 8eedcf2, MIT License, (c) 2026 TheRoben.
// Local changes are marked `NODE:`.

pub mod collect;
pub mod parse;

pub use nightgrid_node_proto::{Gpu, MountUsage, Point, Rate, Sample, Temp};
pub use parse::{parse, Raw};

/// Interfaces left out of the network total (virtual / container plumbing).
pub fn is_virtual_iface(name: &str) -> bool {
    const PREFIXES: [&str; 11] =
        ["lo", "veth", "docker", "br-", "virbr", "vnet", "cni", "flannel", "cali", "kube", "lxcbr"];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Whole disks counted in the disk total (no loop/ram/zram/dm/md/optical).
pub fn is_physical_disk(name: &str) -> bool {
    const SKIP: [&str; 7] = ["loop", "ram", "zram", "dm-", "md", "sr", "fd"];
    !SKIP.iter().any(|p| name.starts_with(p))
}

/// Picks the sensor that best stands for "CPU temperature".
pub fn cpu_temp(temps: &[Temp]) -> Option<f32> {
    let score = |t: &Temp| -> Option<u8> {
        let chip = t.chip.as_str();
        let label = t.label.to_lowercase();
        match chip {
            "coretemp" if label.starts_with("package") => Some(0),
            "k10temp" | "zenpower" if label == "tctl" || label == "tdie" => Some(0),
            "cpu_thermal" | "cpu-thermal" | "soc_thermal" | "x86_pkg_temp" => Some(1),
            "coretemp" | "k10temp" | "zenpower" => Some(2),
            "acpitz" => Some(4),
            _ if chip.contains("cpu") => Some(3),
            _ => None,
        }
    };
    temps.iter().filter_map(|t| score(t).map(|s| (s, t.celsius))).min_by_key(|(s, _)| *s).map(|(_, c)| c)
}

/// Turns raw snapshots into samples with rates; keeps the previous snapshot.
#[derive(Default)]
pub struct Collector {
    prev: Option<(Raw, f64)>,
}

impl Collector {
    pub fn new() -> Self {
        Self::default()
    }

    /// `ts` is the sample time in unix milliseconds.
    pub fn sample(&mut self, raw: Raw, ts: f64) -> Sample {
        let prev = self.prev.take();
        let dt = prev.as_ref().map(|(_, t)| (ts - t) / 1000.0).filter(|d| *d > 0.05);

        let (cpu, cores) = match (&prev, dt) {
            (Some((p, _)), Some(_)) => {
                let busy = |now: &parse::CpuTimes, before: Option<&parse::CpuTimes>| -> Option<f32> {
                    let before = before?;
                    let total = now.total().checked_sub(before.total())?;
                    let idle = now.idle().checked_sub(before.idle())?;
                    (total > 0).then(|| ((total - idle.min(total)) as f64 / total as f64 * 100.0) as f32)
                };
                let cpu = raw.cpu.as_ref().and_then(|c| busy(c, p.cpu.as_ref()));
                let cores = raw.cores.iter().enumerate().map(|(i, c)| busy(c, p.cores.get(i)).unwrap_or(0.0)).collect();
                (cpu, cores)
            }
            _ => (None, Vec::new()),
        };

        let rate = |now: u64, before: Option<u64>| -> f64 {
            match (before, dt) {
                (Some(b), Some(dt)) if now >= b => (now - b) as f64 / dt,
                _ => 0.0,
            }
        };
        let net: Vec<Rate> = raw
            .net
            .iter()
            .filter(|n| n.name != "lo")
            .map(|n| {
                let p = prev.as_ref().and_then(|(p, _)| p.net.iter().find(|x| x.name == n.name));
                Rate { name: n.name.clone(), rx: rate(n.rx, p.map(|x| x.rx)), tx: rate(n.tx, p.map(|x| x.tx)) }
            })
            .collect();
        let (net_rx, net_tx) =
            net.iter().filter(|n| !is_virtual_iface(&n.name)).fold((0.0, 0.0), |(r, t), n| (r + n.rx, t + n.tx));
        let disks: Vec<Rate> = raw
            .disks
            .iter()
            .filter(|d| (raw.blocks.is_empty() || raw.blocks.contains(&d.name)) && is_physical_disk(&d.name))
            .map(|d| {
                let p = prev.as_ref().and_then(|(p, _)| p.disks.iter().find(|x| x.name == d.name));
                Rate {
                    name: d.name.clone(),
                    rx: rate(d.read_sectors * 512, p.map(|x| x.read_sectors * 512)),
                    tx: rate(d.write_sectors * 512, p.map(|x| x.write_sectors * 512)),
                }
            })
            .collect();
        let (disk_read, disk_write) = disks.iter().fold((0.0, 0.0), |(r, w), d| (r + d.rx, w + d.tx));

        let s = Sample {
            ts,
            hostname: raw.hostname.clone(),
            kernel: raw.kernel.clone(),
            uptime_secs: raw.uptime,
            cpu,
            cores,
            load: raw.load,
            mem_total: raw.mem.total,
            mem_used: raw.mem.used(),
            swap_total: raw.mem.swap_total,
            swap_used: raw.mem.swap_total.saturating_sub(raw.mem.swap_free),
            mounts: raw.mounts.clone(),
            net,
            net_rx,
            net_tx,
            disks,
            disk_read,
            disk_write,
            cpu_temp: cpu_temp(&raw.temps),
            temps: raw.temps.clone(),
            gpus: raw.gpus.clone(),
        };
        self.prev = Some((raw, ts));
        s
    }
}

/// The history point for one sample (`Point::from_sample` in the app).
pub fn point_from_sample(s: &Sample) -> Point {
    let pct = |used: u64, total: u64| (total > 0).then(|| (used as f64 / total as f64 * 100.0) as f32);
    Point {
        ts: (s.ts / 1000.0).floor(),
        cpu: s.cpu,
        mem: pct(s.mem_used, s.mem_total),
        swap: pct(s.swap_used, s.swap_total),
        load1: Some(s.load[0]),
        disk: s.mounts.iter().filter_map(|m| pct(m.used, m.used + m.avail)).reduce(f32::max),
        net_rx: Some(s.net_rx),
        net_tx: Some(s.net_tx),
        disk_read: Some(s.disk_read),
        disk_write: Some(s.disk_write),
        temp: s.cpu_temp,
        gpu: s.gpus.iter().filter_map(|g| g.util).reduce(f32::max),
        gpu_mem: s
            .gpus
            .iter()
            .filter_map(|g| match (g.mem_used, g.mem_total) {
                (Some(u), Some(t)) => pct(u, t),
                _ => None,
            })
            .reduce(f32::max),
        gpu_temp: s.gpus.iter().filter_map(|g| g.temp).reduce(f32::max),
        gpu_power: s.gpus.iter().filter_map(|g| g.power).reduce(|a, b| a + b),
        n: 1,
    }
}

/// Number of stored metric columns.
pub const N: usize = 14;

/// Stored columns in order (also the SQL column names).
pub const COLS: [&str; N] = [
    "cpu",
    "mem",
    "swap",
    "load1",
    "disk",
    "net_rx",
    "net_tx",
    "disk_read",
    "disk_write",
    "temp",
    "gpu",
    "gpu_mem",
    "gpu_temp",
    "gpu_power",
];

pub fn values(p: &Point) -> [Option<f64>; N] {
    let f = |v: Option<f32>| v.map(f64::from);
    [
        f(p.cpu),
        f(p.mem),
        f(p.swap),
        f(p.load1),
        f(p.disk),
        p.net_rx,
        p.net_tx,
        p.disk_read,
        p.disk_write,
        f(p.temp),
        f(p.gpu),
        f(p.gpu_mem),
        f(p.gpu_temp),
        f(p.gpu_power),
    ]
}

pub fn point(ts: i64, n: u32, v: &[Option<f64>]) -> Point {
    let f = |x: Option<f64>| x.map(|x| x as f32);
    Point {
        ts: ts as f64,
        cpu: f(v[0]),
        mem: f(v[1]),
        swap: f(v[2]),
        load1: f(v[3]),
        disk: f(v[4]),
        net_rx: v[5],
        net_tx: v[6],
        disk_read: v[7],
        disk_write: v[8],
        temp: f(v[9]),
        gpu: f(v[10]),
        gpu_mem: f(v[11]),
        gpu_temp: f(v[12]),
        gpu_power: f(v[13]),
        n,
    }
}

/// Averages samples into minute buckets; `push` returns the finished point
/// when a sample lands in a new minute.
#[derive(Default)]
pub struct MinuteAverager {
    bucket: i64,
    n: u32,
    sum: [f64; N],
    seen: [u32; N],
}

impl MinuteAverager {
    pub fn push(&mut self, p: &Point) -> Option<Point> {
        let bucket = (p.ts as i64).div_euclid(nightgrid_node_proto::MINUTE) * nightgrid_node_proto::MINUTE;
        let done = (self.n > 0 && bucket != self.bucket).then(|| self.take());
        if self.n == 0 {
            self.bucket = bucket;
        }
        for (i, v) in values(p).iter().enumerate() {
            if let Some(v) = v {
                self.sum[i] += v;
                self.seen[i] += 1;
            }
        }
        self.n += 1;
        done
    }

    /// The partial current minute, if any (e.g. on shutdown).
    pub fn flush(&mut self) -> Option<Point> {
        (self.n > 0).then(|| self.take())
    }

    fn take(&mut self) -> Point {
        let avg: Vec<Option<f64>> =
            (0..N).map(|i| (self.seen[i] > 0).then(|| self.sum[i] / self.seen[i] as f64)).collect();
        // NODE: keep the sample count so rollups can be merged with weights.
        let p = point(self.bucket, self.n, &avg);
        *self = Self::default();
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = include_str!("../../tests/fixtures/metrics/x86_server_a.txt");
    const B: &str = include_str!("../../tests/fixtures/metrics/x86_server_b.txt");
    const PI: &str = include_str!("../../tests/fixtures/metrics/pi_busybox.txt");

    #[test]
    fn rates_from_two_snapshots() {
        let mut c = Collector::new();
        let first = c.sample(parse(A), 1_000_000.0);
        assert_eq!(first.cpu, None);
        assert!(first.cores.is_empty());
        assert_eq!(first.net_rx, 0.0);
        let s = c.sample(parse(B), 1_002_000.0);
        assert_eq!(s.cpu, Some(75.0));
        assert_eq!(s.cores.len(), 4);
        assert_eq!(s.cores[0], 90.0);
        assert_eq!(s.net_rx, 1_000_000.0);
        assert_eq!(s.net_tx, 250_000.0);
        assert!(s.net.iter().any(|n| n.name == "docker0") && !s.net.iter().any(|n| n.name == "lo"));
        assert_eq!(s.disk_read, 1_048_576.0);
        assert_eq!(s.disks.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["nvme0n1", "sda"]);
        assert_eq!(s.cpu_temp, Some(48.0));
    }

    #[test]
    fn busybox_pi_without_hwmon_labels() {
        let s = Collector::new().sample(parse(PI), 0.0);
        assert_eq!(s.cpu_temp, Some(51.6));
        assert_eq!(s.hostname, "pi");
        assert_eq!(s.mounts.iter().map(|m| m.mount.as_str()).collect::<Vec<_>>(), ["/", "/boot/firmware"]);
    }

    #[test]
    fn averages_per_minute() {
        let p =
            |ts: f64, cpu: f32| Point { ts, cpu: Some(cpu), net_rx: Some(f64::from(cpu) * 10.0), ..Default::default() };
        let mut a = MinuteAverager::default();
        assert_eq!(a.push(&p(120.0, 10.0)), None);
        assert_eq!(a.push(&p(150.0, 30.0)), None);
        let done = a.push(&p(181.0, 50.0)).unwrap();
        assert_eq!((done.ts, done.cpu, done.net_rx, done.temp, done.n), (120.0, Some(20.0), Some(200.0), None, 2));
        assert_eq!(a.flush().unwrap().cpu, Some(50.0));
        assert_eq!(a.flush(), None);
    }

    #[test]
    fn point_from_two_samples() {
        let mut c = Collector::new();
        c.sample(parse(A), 1_000_000.0);
        let p = point_from_sample(&c.sample(parse(B), 1_002_000.0));
        assert_eq!((p.ts, p.cpu, p.net_rx, p.temp, p.n), (1002.0, Some(75.0), Some(1_000_000.0), Some(48.0), 1));
        assert!(p.mem.unwrap() > 0.0 && p.disk.unwrap() > 0.0);
    }
}
