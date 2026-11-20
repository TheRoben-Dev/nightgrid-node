//! Fine-grained usage (N6a): CPU, memory, disk I/O and network per systemd
//! service and per container from cgroup v2, the busiest process names from
//! `/proc`, and power draw from RAPL. Read every 10 s, every second while
//! anyone subscribes to the `usage` stream, and kept as one-minute averages
//! for a day.
//!
//! Everything here is readable without privileges except RAPL's
//! `energy_uj`, which the kernel keeps root-only (it can leak what a CPU is
//! doing); `allow power` lets the service's group read it.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nightgrid_node_proto::{PowerDomain, PowerReading, UsageItem, UsagePoint, UsageSample};

use crate::daemon::{now_millis, now_secs, Shared};

/// Process names reported per sample, busiest first.
const TOP_PROCESSES: usize = 15;
/// Minute points kept, in seconds.
pub const KEEP_SECS: i64 = 86_400;
const EVERY: Duration = Duration::from_secs(10);
const LIVE_EVERY: Duration = Duration::from_secs(1);

/// Raw counters of one cgroup at one moment.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Counters {
    cpu_usec: u64,
    io_read: u64,
    io_write: u64,
    net_rx: Option<u64>,
    net_tx: Option<u64>,
}

fn read(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_default()
}

fn list_dir(p: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(p)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// `usage_usec` from `cpu.stat`.
pub fn parse_cpu_stat(s: &str) -> u64 {
    s.lines().find_map(|l| l.strip_prefix("usage_usec ")).and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// Sum of `rbytes` and `wbytes` over the devices in `io.stat`.
pub fn parse_io_stat(s: &str) -> (u64, u64) {
    let mut r = 0u64;
    let mut w = 0u64;
    for tok in s.split_whitespace() {
        if let Some(v) = tok.strip_prefix("rbytes=") {
            r = r.saturating_add(v.parse().unwrap_or(0));
        } else if let Some(v) = tok.strip_prefix("wbytes=") {
            w = w.saturating_add(v.parse().unwrap_or(0));
        }
    }
    (r, w)
}

/// Received and sent bytes over every interface but `lo` in `/proc/<pid>/net/dev`.
pub fn parse_net_dev(s: &str) -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    for l in s.lines().skip(2) {
        let Some((iface, rest)) = l.split_once(':') else { continue };
        if iface.trim() == "lo" {
            continue;
        }
        let f: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        if f.len() >= 9 {
            rx = rx.saturating_add(f[0]);
            tx = tx.saturating_add(f[8]);
        }
    }
    (rx, tx)
}

/// `docker-<id>.scope`, `libpod-<id>.scope` (systemd driver) or `<id>`
/// (cgroupfs driver, under `docker/`): the container id.
pub fn container_id(dir: &str) -> Option<&str> {
    let id = dir
        .strip_prefix("docker-")
        .or_else(|| dir.strip_prefix("libpod-"))
        .and_then(|d| d.strip_suffix(".scope"))
        .unwrap_or(dir);
    (id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

/// `/proc/<pid>/stat`: (comm, utime + stime in ticks, rss in pages).
pub fn parse_proc_stat(s: &str) -> Option<(String, u64, u64)> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_owned();
    let f: Vec<&str> = s.get(close + 2..)?.split_whitespace().collect();
    // After the comm: state is field 3, utime 14, stime 15, rss 24 (1-based).
    let n = |i: usize| f.get(i - 3).and_then(|v| v.parse::<u64>().ok());
    Some((comm, n(14)? + n(15)?, n(24).unwrap_or(0)))
}

/// One RAPL zone: (name, energy in µJ, wrap range in µJ, is top level).
fn rapl_zones(root: &Path) -> Result<Vec<(String, u64, u64, bool)>, &'static str> {
    let base = root.join("sys/class/powercap");
    let mut out = Vec::new();
    let mut denied = false;
    for z in list_dir(&base) {
        if !z.starts_with("intel-rapl:") {
            continue;
        }
        let d = base.join(&z);
        let name = read(&d.join("name")).trim().to_owned();
        match fs::read_to_string(d.join("energy_uj")) {
            Ok(e) => {
                let Ok(e) = e.trim().parse::<u64>() else { continue };
                let range = read(&d.join("max_energy_range_uj")).trim().parse().unwrap_or(u64::MAX);
                let top = z.matches(':').count() == 1;
                let label = if top { name } else { format!("{name} ({z})") };
                out.push((label, e, range, top));
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied = true,
            Err(_) => {}
        }
    }
    if out.is_empty() {
        return Err(if denied { "denied" } else { "absent" });
    }
    Ok(out)
}

/// The item name of services that share one process: up to three names
/// joined with `+`, or two and how many more.
pub fn service_group_name(names: &[String]) -> String {
    if names.len() <= 3 {
        names.join("+")
    } else {
        format!("{}+{} more", names[..2].join("+"), names.len() - 2)
    }
}

/// Turns two samples of a monotonically growing counter into a rate.
fn rate(new: u64, old: u64, secs: f64) -> f64 {
    if secs <= 0.0 {
        return 0.0;
    }
    new.saturating_sub(old) as f64 / secs
}

/// Reads usage; keeps the previous counters to turn them into rates.
pub struct UsageReader {
    root: PathBuf,
    ticks: f64,
    page: u64,
    cores: f64,
    prev_at: Option<Instant>,
    prev_groups: HashMap<(String, String), Counters>,
    prev_procs: HashMap<u32, u64>,
    prev_rapl: HashMap<String, u64>,
    /// Windows: CPU time per (pid, start time).
    #[cfg(windows)]
    prev_win_procs: HashMap<(u32, i64), u64>,
    /// Windows: counters per service, with the pid they were read from.
    #[cfg(windows)]
    prev_win_services: HashMap<String, (u32, Counters)>,
}

impl UsageReader {
    pub fn system() -> Self {
        Self::at("/")
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        // SAFETY: sysconf only reads system constants.
        #[cfg(unix)]
        let (ticks, page, cores) = unsafe {
            (
                libc::sysconf(libc::_SC_CLK_TCK),
                libc::sysconf(libc::_SC_PAGESIZE),
                libc::sysconf(libc::_SC_NPROCESSORS_ONLN),
            )
        };
        #[cfg(windows)]
        let (ticks, page, cores): (i64, i64, i64) =
            (100, 4096, std::thread::available_parallelism().map(|n| n.get() as i64).unwrap_or(1));
        Self {
            root: root.into(),
            ticks: if ticks > 0 { ticks as f64 } else { 100.0 },
            page: if page > 0 { page as u64 } else { 4096 },
            cores: if cores > 0 { cores as f64 } else { 1.0 },
            prev_at: None,
            prev_groups: HashMap::new(),
            prev_procs: HashMap::new(),
            prev_rapl: HashMap::new(),
            #[cfg(windows)]
            prev_win_procs: HashMap::new(),
            #[cfg(windows)]
            prev_win_services: HashMap::new(),
        }
    }

    /// Tests: pretend the machine has this many cores.
    pub fn with_cores(mut self, cores: f64) -> Self {
        self.cores = cores;
        self
    }

    /// cgroups to measure: (kind, name, dir).
    fn groups(&self, names: &HashMap<String, String>) -> Vec<(String, String, PathBuf)> {
        let cg = self.root.join("sys/fs/cgroup");
        let mut out = Vec::new();
        let sys = cg.join("system.slice");
        for d in list_dir(&sys) {
            if d.ends_with(".service") {
                out.push(("service".to_owned(), d.clone(), sys.join(&d)));
            } else if let Some(id) = container_id(&d) {
                out.push((
                    "container".into(),
                    names.get(id).cloned().unwrap_or_else(|| id[..12].to_owned()),
                    sys.join(&d),
                ));
            }
        }
        for parent in [cg.join("machine.slice"), cg.join("docker")] {
            for d in list_dir(&parent) {
                if let Some(id) = container_id(&d) {
                    out.push((
                        "container".into(),
                        names.get(id).cloned().unwrap_or_else(|| id[..12].to_owned()),
                        parent.join(&d),
                    ));
                }
            }
        }
        out
    }

    fn counters(&self, kind: &str, dir: &Path) -> Counters {
        let (io_read, io_write) = parse_io_stat(&read(&dir.join("io.stat")));
        let mut c = Counters {
            cpu_usec: parse_cpu_stat(&read(&dir.join("cpu.stat"))),
            io_read,
            io_write,
            ..Default::default()
        };
        if kind == "container" {
            // The container's own network namespace, through its first process.
            if let Some(pid) = read(&dir.join("cgroup.procs")).lines().next().map(str::trim).filter(|p| !p.is_empty()) {
                let dev = read(&self.root.join(format!("proc/{pid}/net/dev")));
                // A container on the host's network sees the host's
                // interfaces: its traffic isn't its own, so leave it out.
                let ifaces = |s: &str| -> Vec<String> {
                    s.lines().skip(2).filter_map(|l| l.split_once(':')).map(|(i, _)| i.trim().to_owned()).collect()
                };
                let host = ifaces(&read(&self.root.join("proc/net/dev")));
                if !dev.is_empty() && ifaces(&dev) != host {
                    let (rx, tx) = parse_net_dev(&dev);
                    (c.net_rx, c.net_tx) = (Some(rx), Some(tx));
                }
            }
        }
        c
    }

    /// One sample. `names` maps container ids to names. The first call has
    /// zero rates.
    pub fn read(&mut self, names: &HashMap<String, String>) -> UsageSample {
        let now = Instant::now();
        let secs = self.prev_at.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);
        let first = self.prev_at.is_none();
        let cgroups = self.root.join("sys/fs/cgroup/cgroup.controllers").exists();
        let mut items = Vec::new();
        let mut groups = HashMap::new();
        if cgroups {
            for (kind, name, dir) in self.groups(names) {
                let c = self.counters(&kind, &dir);
                let mem = read(&dir.join("memory.current")).trim().parse().unwrap_or(0);
                let key = (kind.clone(), name.clone());
                let p = self.prev_groups.get(&key).copied();
                let r = |new: u64, old: Option<u64>| (!first).then(|| rate(new, old.unwrap_or(new), secs));
                let cpu = p.map_or(0.0, |p| rate(c.cpu_usec, p.cpu_usec, secs) / 1e6 / self.cores * 100.0);
                items.push(UsageItem {
                    kind: kind.clone(),
                    name,
                    cpu: cpu.clamp(0.0, 100.0) as f32,
                    mem,
                    io_read: r(c.io_read, p.map(|p| p.io_read)),
                    io_write: r(c.io_write, p.map(|p| p.io_write)),
                    net_rx: c.net_rx.and_then(|n| r(n, p.and_then(|p| p.net_rx))),
                    net_tx: c.net_tx.and_then(|n| r(n, p.and_then(|p| p.net_tx))),
                    count: None,
                });
                groups.insert(key, c);
            }
        }
        self.prev_groups = groups;
        items.extend(self.processes(secs, first));
        #[cfg(windows)]
        items.extend(self.windows(secs, first));
        let power = self.power(secs);
        self.prev_at = Some(now);
        // Windows measures every service (by its process), like cgroup v2.
        UsageSample { ts: now_millis(), items, power, cgroups: cgroups || cfg!(windows) }
    }

    fn processes(&mut self, secs: f64, first: bool) -> Vec<UsageItem> {
        let proc_dir = self.root.join("proc");
        let mut by_name: BTreeMap<String, (f64, u64, u32)> = BTreeMap::new();
        let mut seen = HashMap::new();
        for d in list_dir(&proc_dir) {
            let Ok(pid) = d.parse::<u32>() else { continue };
            let Some((comm, ticks, rss)) = parse_proc_stat(&read(&proc_dir.join(&d).join("stat"))) else { continue };
            let used = self.prev_procs.get(&pid).map_or(0, |old| ticks.saturating_sub(*old));
            seen.insert(pid, ticks);
            let e = by_name.entry(comm).or_default();
            e.0 += used as f64;
            e.1 += rss * self.page;
            e.2 += 1;
        }
        self.prev_procs = seen;
        let mut v: Vec<UsageItem> = by_name
            .into_iter()
            .map(|(name, (ticks, mem, n))| {
                let cpu = if first || secs <= 0.0 { 0.0 } else { ticks / self.ticks / secs / self.cores * 100.0 };
                UsageItem {
                    kind: "process".into(),
                    name,
                    cpu: cpu.clamp(0.0, 100.0) as f32,
                    mem,
                    io_read: None,
                    io_write: None,
                    net_rx: None,
                    net_tx: None,
                    count: Some(n),
                }
            })
            .collect();
        v.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.mem.cmp(&a.mem)));
        v.truncate(TOP_PROCESSES);
        v
    }

    /// Windows: one item per running service (the process it runs in;
    /// services sharing a process are named together) and the busiest
    /// process names. I/O is what the processes read and wrote through
    /// any handle, network and devices included.
    #[cfg(windows)]
    fn windows(&mut self, secs: f64, first: bool) -> Vec<UsageItem> {
        let procs = crate::win::usage::processes();
        let by_pid: HashMap<u32, &crate::win::usage::Proc> = procs.iter().map(|p| (p.pid, p)).collect();
        let cpu_pct = |used_100ns: f64| {
            if first || secs <= 0.0 {
                0.0
            } else {
                (used_100ns / 1e7 / secs / self.cores * 100.0).clamp(0.0, 100.0) as f32
            }
        };
        let mut hosts: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for (name, pid) in crate::win::usage::services() {
            hosts.entry(pid).or_default().push(name);
        }
        let mut items = Vec::new();
        let mut services = HashMap::new();
        for (pid, mut names) in hosts {
            let Some(p) = by_pid.get(&pid) else { continue };
            names.sort_by_key(|n| n.to_ascii_lowercase());
            let name = service_group_name(&names);
            let c = Counters { cpu_usec: p.cpu / 10, io_read: p.read, io_write: p.write, net_rx: None, net_tx: None };
            let prev = self.prev_win_services.get(&name).filter(|(old, _)| *old == pid).map(|(_, c)| *c);
            let r = |new: u64, old: Option<u64>| (!first).then(|| rate(new, old.unwrap_or(new), secs));
            items.push(UsageItem {
                kind: "service".into(),
                name: name.clone(),
                cpu: prev.map_or(0.0, |o| cpu_pct(c.cpu_usec.saturating_sub(o.cpu_usec) as f64 * 10.0)),
                mem: p.mem,
                io_read: r(c.io_read, prev.map(|o| o.io_read)),
                io_write: r(c.io_write, prev.map(|o| o.io_write)),
                net_rx: None,
                net_tx: None,
                count: None,
            });
            services.insert(name, (pid, c));
        }
        self.prev_win_services = services;
        let mut by_name: BTreeMap<String, (f64, u64, u32)> = BTreeMap::new();
        let mut seen = HashMap::new();
        for p in &procs {
            let key = (p.pid, p.created);
            let used = self.prev_win_procs.get(&key).map_or(0, |old| p.cpu.saturating_sub(*old));
            seen.insert(key, p.cpu);
            let e = by_name.entry(p.name.clone()).or_default();
            e.0 += used as f64;
            e.1 += p.mem;
            e.2 += 1;
        }
        self.prev_win_procs = seen;
        let mut v: Vec<UsageItem> = by_name
            .into_iter()
            .map(|(name, (used, mem, n))| UsageItem {
                kind: "process".into(),
                name,
                cpu: cpu_pct(used),
                mem,
                io_read: None,
                io_write: None,
                net_rx: None,
                net_tx: None,
                count: Some(n),
            })
            .collect();
        v.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.mem.cmp(&a.mem)));
        v.truncate(TOP_PROCESSES);
        items.extend(v);
        items
    }

    fn power(&mut self, secs: f64) -> PowerReading {
        let zones = match rapl_zones(&self.root) {
            Ok(z) => z,
            Err(access) => return PowerReading { access: access.into(), ..Default::default() },
        };
        let mut domains = Vec::new();
        let mut now = HashMap::new();
        for (name, e, range, top) in zones {
            if let Some(old) = self.prev_rapl.get(&name) {
                // The counter wraps at max_energy_range_uj.
                let used = if e >= *old { e - old } else { range.saturating_sub(*old) + e };
                if secs > 0.0 {
                    domains.push((PowerDomain { name: name.clone(), watts: used as f64 / 1e6 / secs }, top));
                }
            }
            now.insert(name, e);
        }
        self.prev_rapl = now;
        let psys = domains.iter().find(|(d, top)| *top && d.name == "psys").map(|(d, _)| d.watts);
        let packages: f64 = domains.iter().filter(|(d, top)| *top && d.name != "psys").map(|(d, _)| d.watts).sum();
        let watts = if domains.is_empty() { None } else { Some(psys.unwrap_or(packages)) };
        PowerReading { access: "ok".into(), watts, domains: domains.into_iter().map(|(d, _)| d).collect() }
    }
}

/// (kind, name, minute point).
pub type KeyedPoint = (String, String, UsagePoint);

/// Minute averages per entity.
#[derive(Debug, Default)]
pub struct MinuteUsage {
    minute: i64,
    sums: BTreeMap<(String, String), (UsagePoint, u32)>,
}

fn add(a: &mut Option<f64>, b: Option<f64>) {
    if let Some(b) = b {
        *a = Some(a.unwrap_or(0.0) + b);
    }
}

impl MinuteUsage {
    /// Adds a sample; returns the finished minute's points when a new minute starts.
    pub fn push(&mut self, s: &UsageSample, now: i64) -> Option<(i64, Vec<KeyedPoint>)> {
        let minute = now - now.rem_euclid(60);
        let done = (self.minute != 0 && minute != self.minute).then(|| self.flush()).flatten();
        self.minute = minute;
        for i in &s.items {
            let e = self.sums.entry((i.kind.clone(), i.name.clone())).or_insert_with(|| (empty(minute), 0));
            e.0.cpu = Some(e.0.cpu.unwrap_or(0.0) + i.cpu);
            e.0.mem = Some(e.0.mem.unwrap_or(0) + i.mem);
            add(&mut e.0.io_read, i.io_read);
            add(&mut e.0.io_write, i.io_write);
            add(&mut e.0.net_rx, i.net_rx);
            add(&mut e.0.net_tx, i.net_tx);
            e.1 += 1;
        }
        let mut power = vec![("total".to_owned(), s.power.watts)];
        power.extend(s.power.domains.iter().map(|d| (d.name.clone(), Some(d.watts))));
        for (name, w) in power {
            if let Some(w) = w {
                let e = self.sums.entry(("power".into(), name)).or_insert_with(|| (empty(minute), 0));
                add(&mut e.0.watts, Some(w));
                e.1 += 1;
            }
        }
        done
    }

    /// The current minute's averages, if any.
    pub fn flush(&mut self) -> Option<(i64, Vec<KeyedPoint>)> {
        if self.sums.is_empty() {
            return None;
        }
        let minute = self.minute;
        let points = std::mem::take(&mut self.sums)
            .into_iter()
            .map(|((kind, name), (p, n))| {
                let d = f64::from(n.max(1));
                let avg = |v: Option<f64>| v.map(|x| x / d);
                let point = UsagePoint {
                    ts: minute,
                    cpu: p.cpu.map(|x| x / n.max(1) as f32),
                    mem: p.mem.map(|x| x / u64::from(n.max(1))),
                    io_read: avg(p.io_read),
                    io_write: avg(p.io_write),
                    net_rx: avg(p.net_rx),
                    net_tx: avg(p.net_tx),
                    watts: avg(p.watts),
                };
                (kind, name, point)
            })
            .collect();
        Some((minute, points))
    }
}

fn empty(ts: i64) -> UsagePoint {
    UsagePoint { ts, cpu: None, mem: None, io_read: None, io_write: None, net_rx: None, net_tx: None, watts: None }
}

/// Handle to the usage thread.
pub struct UsageWatcher {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl UsageWatcher {
    pub fn start(shared: Arc<Shared>, mut reader: UsageReader) -> std::io::Result<Self> {
        let (stop, rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new().name("usage".into()).spawn(move || {
            let mut minutes = MinuteUsage::default();
            let mut last_prune = 0i64;
            loop {
                let started = Instant::now();
                let names: HashMap<String, String> = shared
                    .containers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .result
                    .containers
                    .iter()
                    .map(|c| (c.id.clone(), c.name.clone()))
                    .collect();
                let s = reader.read(&names);
                let now = now_secs();
                if let Some((_, points)) = minutes.push(&s, now) {
                    store(&shared, &points);
                }
                if now - last_prune > 3600 {
                    last_prune = now;
                    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
                    if let Err(e) = st.prune_usage(now - KEEP_SECS) {
                        crate::warn!("could not prune usage history: {e}");
                    }
                }
                if shared.bus.usage.receiver_count() > 0 {
                    let _ = shared.bus.usage.send(s.clone());
                }
                *shared.usage.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
                let stop = loop {
                    let every = if shared.bus.usage.receiver_count() > 0 { LIVE_EVERY } else { EVERY };
                    let left = every.saturating_sub(started.elapsed());
                    if left.is_zero() {
                        break false;
                    }
                    match rx.recv_timeout(left.min(Duration::from_secs(1))) {
                        Err(RecvTimeoutError::Timeout) => continue,
                        _ => break true,
                    }
                };
                if stop {
                    break;
                }
            }
            if let Some((_, points)) = minutes.flush() {
                store(&shared, &points);
            }
        })?;
        Ok(Self { stop, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Keeps a minute's points: every service and container that used CPU or
/// memory, the process names, and power.
fn store(shared: &Shared, points: &[KeyedPoint]) {
    let mut st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    let keep: Vec<&KeyedPoint> = points
        .iter()
        .filter(|(kind, _, p)| kind == "power" || p.cpu.unwrap_or(0.0) > 0.0 || p.mem.unwrap_or(0) > 0)
        .collect();
    if let Err(e) = st.insert_usage(&keep) {
        crate::warn!("could not store usage: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn tree(root: &Path, cpu_usec: u64, rbytes: u64, rx: u64, ticks: u64, energy: u64) {
        write(root, "sys/fs/cgroup/cgroup.controllers", "cpu io memory\n");
        let svc = "sys/fs/cgroup/system.slice/nginx.service";
        write(root, &format!("{svc}/cpu.stat"), &format!("usage_usec {cpu_usec}\nuser_usec 1\n"));
        write(root, &format!("{svc}/memory.current"), "52428800\n");
        write(root, &format!("{svc}/io.stat"), &format!("8:0 rbytes={rbytes} wbytes=10 rios=1 wios=1\n"));
        let ct = format!("sys/fs/cgroup/system.slice/docker-{ID}.scope");
        write(root, &format!("{ct}/cpu.stat"), &format!("usage_usec {}\n", cpu_usec / 2));
        write(root, &format!("{ct}/memory.current"), "1048576\n");
        write(root, &format!("{ct}/cgroup.procs"), "4242\n");
        write(
            root,
            "proc/4242/net/dev",
            &format!(
                "Inter-|   Receive\n face |bytes\n    lo: 999 1 0 0 0 0 0 0 999 1 0 0 0 0 0 0\n  eth0: {rx} 10 0 0 0 0 0 0 {} 5 0 0 0 0 0 0\n",
                rx / 2
            ),
        );
        write(
            root,
            "proc/4242/stat",
            &format!("4242 (nginx: worker) S 1 1 1 0 -1 0 0 0 0 0 {ticks} 0 0 0 20 0 1 0 1 100 25"),
        );
        write(root, "proc/1/stat", "1 (systemd) S 0 1 1 0 -1 0 0 0 0 0 5 5 0 0 20 0 1 0 1 100 10");
        write(root, "sys/class/powercap/intel-rapl:0/name", "package-0\n");
        write(root, "sys/class/powercap/intel-rapl:0/energy_uj", &format!("{energy}\n"));
        write(root, "sys/class/powercap/intel-rapl:0/max_energy_range_uj", "262143328850\n");
        write(root, "sys/class/powercap/intel-rapl:0:0/name", "core\n");
        write(root, "sys/class/powercap/intel-rapl:0:0/energy_uj", &format!("{}\n", energy / 2));
    }

    #[test]
    fn parsers() {
        assert_eq!(parse_cpu_stat("usage_usec 1234\nuser_usec 1000\n"), 1234);
        assert_eq!(parse_io_stat("8:0 rbytes=100 wbytes=5\n259:0 rbytes=1 wbytes=2 dbytes=9\n"), (101, 7));
        assert_eq!(container_id(&format!("docker-{ID}.scope")), Some(ID));
        assert_eq!(container_id(&format!("libpod-{ID}.scope")), Some(ID));
        assert_eq!(container_id("libpod-conmon-abc.scope"), None);
        let (comm, ticks, rss) =
            parse_proc_stat("7 (a (b) c) S 1 1 1 0 -1 0 0 0 0 0 30 12 0 0 20 0 1 0 1 100 9").unwrap();
        assert_eq!((comm.as_str(), ticks, rss), ("a (b) c", 42, 9));
    }

    #[test]
    #[cfg_attr(windows, ignore = "cgroups and /proc")]
    fn rates_from_two_reads() {
        let d = tempfile::tempdir().unwrap();
        tree(d.path(), 1_000_000, 0, 0, 0, 1_000_000);
        let mut r = UsageReader::at(d.path()).with_cores(2.0);
        let names = HashMap::from([(ID.to_owned(), "web".to_owned())]);
        let first = r.read(&names);
        assert!(first.cgroups);
        assert!(first.items.iter().all(|i| i.cpu == 0.0));
        assert_eq!(first.power.access, "ok");
        assert_eq!(first.power.watts, None);
        std::thread::sleep(Duration::from_millis(200));
        let t = Instant::now();
        tree(d.path(), 1_200_000, 1_000_000, 500_000, 20, 3_000_000);
        let s = r.read(&names);
        let secs = t.elapsed().as_secs_f64() + 0.2;
        let svc = s.items.iter().find(|i| i.name == "nginx.service").unwrap();
        // 0.2 s of CPU in ~0.2 s on 2 cores ≈ 50 %.
        assert!((20.0..=55.0).contains(&svc.cpu), "{}", svc.cpu);
        assert_eq!(svc.mem, 50 << 20);
        assert!(svc.io_read.unwrap() > 1_000_000.0 / secs / 2.0);
        assert_eq!(svc.net_rx, None);
        let ct = s.items.iter().find(|i| i.kind == "container").unwrap();
        assert_eq!(ct.name, "web");
        assert!(ct.net_rx.unwrap() > 0.0 && ct.net_tx.unwrap() > 0.0);
        let p = s.items.iter().find(|i| i.name == "nginx: worker").unwrap();
        assert_eq!((p.kind.as_str(), p.count), ("process", Some(1)));
        assert!(p.cpu > 0.0);
        let w = s.power.watts.unwrap();
        assert!((3.0..=11.0).contains(&w), "{w}");
        assert_eq!(s.power.domains.len(), 2);
    }

    #[test]
    #[cfg_attr(windows, ignore = "cgroups and /proc")]
    fn reads_this_machine() {
        let mut r = UsageReader::system();
        r.read(&HashMap::new());
        std::thread::sleep(Duration::from_millis(100));
        let s = r.read(&HashMap::new());
        assert!(s.items.iter().any(|i| i.kind == "process" && i.mem > 0), "{:?}", s.items);
        assert!(["ok", "denied", "absent"].contains(&s.power.access.as_str()));
        eprintln!("cgroups {} power {:?} items {}", s.cgroups, s.power.access, s.items.len());
    }

    #[test]
    fn power_access_words() {
        let d = tempfile::tempdir().unwrap();
        let mut r = UsageReader::at(d.path());
        assert_eq!(r.read(&HashMap::new()).power.access, "absent");
        // No cgroup tree under the root; Windows always measures services.
        assert_eq!(r.read(&HashMap::new()).cgroups, cfg!(windows));
    }

    #[test]
    fn minute_averages() {
        let mut m = MinuteUsage::default();
        let item = |cpu: f32| UsageItem {
            kind: "service".into(),
            name: "a.service".into(),
            cpu,
            mem: 100,
            io_read: Some(10.0),
            io_write: None,
            net_rx: None,
            net_tx: None,
            count: None,
        };
        let s = |cpu, w| UsageSample {
            ts: 0.0,
            items: vec![item(cpu)],
            power: PowerReading { access: "ok".into(), watts: Some(w), domains: vec![] },
            cgroups: true,
        };
        assert!(m.push(&s(10.0, 20.0), 120).is_none());
        assert!(m.push(&s(30.0, 40.0), 150).is_none());
        let (minute, points) = m.push(&s(0.0, 0.0), 185).unwrap();
        assert_eq!(minute, 120);
        let a = points.iter().find(|p| p.1 == "a.service").unwrap();
        assert_eq!((a.2.cpu, a.2.mem, a.2.io_read), (Some(20.0), Some(100), Some(10.0)));
        let w = points.iter().find(|p| p.0 == "power" && p.1 == "total").unwrap();
        assert_eq!(w.2.watts, Some(30.0));
    }

    #[test]
    fn names_services_sharing_a_process() {
        let n = |v: &[&str]| service_group_name(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(n(&["W32Time"]), "W32Time");
        assert_eq!(n(&["a", "b", "c"]), "a+b+c");
        assert_eq!(n(&["a", "b", "c", "d"]), "a+b+2 more");
    }
}
