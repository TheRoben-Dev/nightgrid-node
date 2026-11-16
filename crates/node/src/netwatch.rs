//! Network watch (N3): an inventory of the devices on the node's LAN, from
//! the kernel's IPv4 neighbour (ARP) table in `/proc/net/arp`.
//!
//! Reading it needs no privileges and sends nothing: the table holds the
//! devices this host has talked to recently. Each read is compared with what
//! the node already knows, and the differences become `network` events:
//!
//! - a MAC address never seen before: a new device;
//! - a known MAC on a new address: the device moved (a new DHCP lease);
//! - an address answering from a new MAC while its old owner still claims
//!   it: logged once, and if it keeps switching back and forth within ten
//!   minutes, an IP conflict (two devices with one address, or ARP
//!   spoofing). On the default gateway, any such change is a warning.
//!
//! The first read on a fresh install is the baseline and raises no events
//! for what is already there. Vendors are not looked up here: the app has
//! the OUI table, and the node stays small.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use nightgrid_node_proto::{EventKind, Level, NetDevice, NetDevicesResult};
use rusqlite::params;

use crate::daemon::{now_secs, Shared};
use crate::events::NewEvent;
use crate::store::{Result, Store};

const META_BASELINE: &str = "net_baseline";
/// An address switching MACs this often within this window is a conflict.
const CONFLICT_WINDOW: i64 = 600;
/// One conflict event per address per hour at most.
const CONFLICT_REPEAT: i64 = 3600;
/// `last_seen` is written at most this often for a device that didn't change.
const SEEN_WRITE_EVERY: i64 = 600;
/// Devices not seen for this long are forgotten.
const KEEP_SECS: i64 = 400 * 86_400;
/// At most this many devices are kept.
const MAX_DEVICES: usize = 4096;
/// More new devices than this in one read become one summary event.
const MAX_NEW_EVENTS: usize = 20;

/// One complete entry of the neighbour table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Neigh {
    pub ip: String,
    pub mac: String,
    pub iface: String,
}

/// `/proc/net/arp`:
/// `192.168.1.1  0x1  0x2  aa:bb:cc:dd:ee:ff  *  eth0`. Only complete
/// (`ATF_COM`) Ethernet entries with a real MAC are kept.
pub fn parse_arp(text: &str) -> Vec<Neigh> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 6 || f[1] != "0x1" {
                return None;
            }
            let flags = u32::from_str_radix(f[2].trim_start_matches("0x"), 16).ok()?;
            let ip: Ipv4Addr = f[0].parse().ok()?;
            let mac = f[3].to_ascii_lowercase();
            if flags & 0x2 == 0 || mac == "00:00:00:00:00:00" || mac.len() != 17 || ip.is_multicast() {
                return None;
            }
            Some(Neigh { ip: ip.to_string(), mac, iface: f[5].to_owned() })
        })
        .collect()
}

/// The IPv4 default gateway from `/proc/net/route` (lowest metric).
pub fn parse_gateway(text: &str) -> Option<String> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 8 || f[1] != "00000000" {
                return None;
            }
            let flags = u32::from_str_radix(f[3], 16).ok()?;
            // RTF_UP | RTF_GATEWAY
            if flags & 0x3 != 0x3 {
                return None;
            }
            // The kernel prints the address as a host-order number of a
            // network-order value.
            let gw = u32::from_str_radix(f[2], 16).ok()?;
            let metric: u32 = f[6].parse().ok()?;
            Some((metric, Ipv4Addr::from(gw.to_ne_bytes())))
        })
        .min()
        .map(|(_, ip)| ip.to_string())
}

// Vendored from Nightgrid core/src/netmap.rs (MIT, commit 8eedcf2).
/// Docker, Podman and Kubernetes bridges: their neighbours are containers.
pub fn is_container_bridge(name: &str) -> bool {
    name == "docker0"
        || name == "podman0"
        || name == "cni0"
        || name == "flannel.1"
        || name.starts_with("br-")
        || name.starts_with("veth")
        || name.starts_with("cali")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Dev {
    ip: String,
    iface: String,
    first: i64,
    last: i64,
    /// `last` as written to the database.
    written: i64,
}

/// What the node knows about the LAN, and how a new read changes it.
#[derive(Debug, Default)]
pub struct Watch {
    devices: BTreeMap<String, Dev>,
    /// The MAC last seen at each address.
    by_ip: HashMap<String, String>,
    /// Recent MAC switches per address: (when, the MAC it switched to).
    flips: HashMap<String, Vec<(i64, String)>>,
    conflict_said: HashMap<String, i64>,
    online: HashSet<String>,
    gateway: Option<String>,
    at: i64,
    /// False until the baseline is taken: the first read raises no events.
    baselined: bool,
    full_said: bool,
    /// MACs to write back.
    dirty: HashSet<String>,
}

impl Watch {
    /// Loads what earlier runs saw.
    pub fn load(store: &Store) -> Result<Self> {
        let mut w = Self { baselined: store.meta(META_BASELINE)?.is_some(), ..Self::default() };
        let mut st = store.conn().prepare("SELECT mac, ip, iface, first_seen, last_seen FROM net_devices")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Dev { ip: r.get(1)?, iface: r.get(2)?, first: r.get(3)?, last: r.get(4)?, written: r.get(4)? },
            ))
        })?;
        for row in rows {
            let (mac, d) = row?;
            w.by_ip.insert(d.ip.clone(), mac.clone());
            w.devices.insert(mac, d);
        }
        Ok(w)
    }

    /// Takes one read of the neighbour table; returns the events it raises.
    pub fn observe(&mut self, now: i64, table: &[Neigh], gateway: Option<String>, ignore: &[String]) -> Vec<NewEvent> {
        // One address per MAC: a device answering on several (aliases, two
        // interfaces) keeps the one it had, else the lowest.
        let mut seen: BTreeMap<&str, &Neigh> = BTreeMap::new();
        for n in table.iter().filter(|n| !is_container_bridge(&n.iface) && !ignore.contains(&n.iface)) {
            let keep = match seen.get(n.mac.as_str()) {
                None => true,
                Some(prev) => {
                    let known = self.devices.get(&n.mac).map(|d| d.ip.as_str());
                    known != Some(prev.ip.as_str())
                        && (known == Some(n.ip.as_str()) || ip_key(&n.ip) < ip_key(&prev.ip))
                }
            };
            if keep {
                seen.insert(&n.mac, n);
            }
        }

        let quiet = !self.baselined;
        let mut events = Vec::new();
        let mut new_devices = Vec::new();

        // Devices first, so the address checks below know who moved where.
        for (mac, n) in &seen {
            match self.devices.get_mut(*mac) {
                None => {
                    if self.devices.len() >= MAX_DEVICES {
                        if !self.full_said {
                            crate::warn!("network watch keeps at most {MAX_DEVICES} devices; ignoring new ones");
                            self.full_said = true;
                        }
                        continue;
                    }
                    self.devices.insert(
                        (*mac).to_owned(),
                        Dev { ip: n.ip.clone(), iface: n.iface.clone(), first: now, last: now, written: 0 },
                    );
                    self.dirty.insert((*mac).to_owned());
                    new_devices.push(*n);
                }
                Some(d) => {
                    if d.ip != n.ip {
                        if !quiet {
                            events.push(NewEvent::new(
                                now,
                                EventKind::Network,
                                Level::Info,
                                format!("device moved {} → {}", d.ip, n.ip),
                                format!("{mac} on {}", n.iface),
                            ));
                        }
                        d.ip = n.ip.clone();
                        self.dirty.insert((*mac).to_owned());
                    }
                    if d.iface != n.iface {
                        d.iface = n.iface.clone();
                        self.dirty.insert((*mac).to_owned());
                    }
                    d.last = now;
                    if now - d.written >= SEEN_WRITE_EVERY {
                        self.dirty.insert((*mac).to_owned());
                    }
                }
            }
        }

        if !quiet {
            if new_devices.len() > MAX_NEW_EVENTS {
                let first: Vec<&str> = new_devices.iter().take(5).map(|n| n.ip.as_str()).collect();
                events.push(NewEvent::new(
                    now,
                    EventKind::Network,
                    Level::Info,
                    format!("{} new devices", new_devices.len()),
                    format!("{}, …", first.join(", ")),
                ));
            } else {
                for n in &new_devices {
                    events.push(NewEvent::new(
                        now,
                        EventKind::Network,
                        Level::Info,
                        format!("new device {}", n.ip),
                        format!("{} on {}", n.mac, n.iface),
                    ));
                }
            }
        }

        // Addresses answering from a different MAC.
        let gw = gateway.as_deref();
        for (mac, n) in &seen {
            let prev = self.by_ip.insert(n.ip.clone(), (*mac).to_owned());
            let Some(prev) = prev.filter(|p| p != mac) else { continue };
            // The old owner now lives elsewhere: the address was reused.
            let old_still_here = self.devices.get(&prev).is_some_and(|d| d.ip == n.ip);
            if !old_still_here || quiet {
                continue;
            }
            let flips = self.flips.entry(n.ip.clone()).or_default();
            flips.retain(|(at, _)| now - at < CONFLICT_WINDOW);
            flips.push((now, (*mac).to_owned()));
            let is_gw = gw == Some(n.ip.as_str());
            if flips.len() >= 2 {
                if self.conflict_said.get(&n.ip).is_some_and(|at| now - at < CONFLICT_REPEAT) {
                    continue;
                }
                self.conflict_said.insert(n.ip.clone(), now);
                let mut macs: Vec<&str> = flips.iter().map(|(_, m)| m.as_str()).chain([prev.as_str()]).collect();
                macs.sort_unstable();
                macs.dedup();
                events.push(NewEvent::new(
                    now,
                    EventKind::Network,
                    if is_gw { Level::Critical } else { Level::Warn },
                    format!("IP conflict on {}{}", n.ip, if is_gw { " (gateway)" } else { "" }),
                    format!("{} keep taking turns answering for it", macs.join(" and ")),
                ));
            } else {
                events.push(NewEvent::new(
                    now,
                    EventKind::Network,
                    if is_gw { Level::Warn } else { Level::Info },
                    format!("{} {} answers from a new MAC", if is_gw { "gateway" } else { "address" }, n.ip),
                    format!(
                        "{prev} → {mac}{}",
                        if is_gw {
                            ": a replaced router, or another device answering for it (ARP spoofing)"
                        } else {
                            ""
                        }
                    ),
                ));
            }
        }

        if quiet {
            events.push(NewEvent::new(
                now,
                EventKind::Network,
                Level::Info,
                "network watch started",
                format!("{} devices in the neighbour table", self.devices.len()),
            ));
            self.baselined = true;
        }

        self.online = seen.keys().map(|m| (*m).to_owned()).collect();
        self.gateway = gateway;
        self.at = now;
        events
    }

    /// Writes changed devices (and the baseline mark) back.
    pub fn save(&mut self, store: &mut Store, now: i64) -> Result<()> {
        let tx = store.conn_mut().transaction()?;
        {
            let mut up = tx.prepare_cached(
                "INSERT INTO net_devices (mac, ip, iface, first_seen, last_seen) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (mac) DO UPDATE SET ip = excluded.ip, iface = excluded.iface, last_seen = excluded.last_seen",
            )?;
            for mac in self.dirty.drain() {
                if let Some(d) = self.devices.get_mut(&mac) {
                    up.execute(params![mac, d.ip, d.iface, d.first, d.last])?;
                    d.written = d.last;
                }
            }
            tx.execute("DELETE FROM net_devices WHERE last_seen < ?1", params![now - KEEP_SECS])?;
            tx.execute(
                "INSERT INTO meta (key, value) VALUES (?1, '1') ON CONFLICT (key) DO NOTHING",
                params![META_BASELINE],
            )?;
        }
        tx.commit()?;
        self.devices.retain(|_, d| d.last >= now - KEEP_SECS);
        Ok(())
    }

    /// The inventory for `net.devices`.
    pub fn result(&self, limit: usize) -> NetDevicesResult {
        let mut devices: Vec<NetDevice> = self
            .devices
            .iter()
            .map(|(mac, d)| NetDevice {
                mac: mac.clone(),
                ip: d.ip.clone(),
                iface: d.iface.clone(),
                first_seen: d.first,
                last_seen: d.last,
                online: self.online.contains(mac),
            })
            .collect();
        devices.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then_with(|| ip_key(&a.ip).cmp(&ip_key(&b.ip))));
        devices.truncate(limit);
        NetDevicesResult { at: self.at, gateway: self.gateway.clone(), devices }
    }
}

fn ip_key(ip: &str) -> u32 {
    ip.parse::<Ipv4Addr>().map(u32::from).unwrap_or(u32::MAX)
}

/// Handle to the network watcher thread.
pub struct NetWatcher {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Where the tables are read from (tests point these elsewhere).
#[derive(Debug, Clone)]
pub struct Sources {
    pub arp: PathBuf,
    pub route: PathBuf,
}

impl Default for Sources {
    fn default() -> Self {
        Self { arp: "/proc/net/arp".into(), route: "/proc/net/route".into() }
    }
}

impl NetWatcher {
    pub fn start(shared: Arc<Shared>, sources: Sources) -> std::io::Result<Self> {
        let (stop, rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new().name("network".into()).spawn(move || run(&shared, &sources, &rx))?;
        Ok(Self { stop, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Most devices `net.devices` returns.
pub const LIST_LIMIT: usize = 1000;

fn run(shared: &Shared, src: &Sources, stop: &mpsc::Receiver<()>) {
    let cfg = &shared.config.network;
    let interval = Duration::from_secs(cfg.interval_secs.into());
    let loaded = {
        let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        Watch::load(&st)
    };
    let mut watch = match loaded {
        Ok(w) => w,
        Err(e) => {
            crate::error!("network watch is off: {e}");
            let _ = stop.recv();
            return;
        }
    };
    loop {
        match std::fs::read_to_string(&src.arp) {
            Ok(arp) => {
                let gateway = std::fs::read_to_string(&src.route).ok().and_then(|r| parse_gateway(&r));
                let now = now_secs();
                let events = watch.observe(now, &parse_arp(&arp), gateway, &cfg.ignore_ifaces);
                let mut st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
                for e in &events {
                    if let Err(err) = shared.bus.record(&st, e) {
                        crate::warn!("could not record a network event: {err}");
                    }
                }
                if let Err(e) = watch.save(&mut st, now) {
                    crate::warn!("could not save the device inventory: {e}");
                }
                drop(st);
                *shared.net.lock().unwrap_or_else(|e| e.into_inner()) = watch.result(LIST_LIMIT);
            }
            Err(e) => crate::warn!("could not read {}: {e}", src.arp.display()),
        }
        match stop.recv_timeout(interval) {
            Err(RecvTimeoutError::Timeout) => continue,
            _ => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARP: &str = "IP address       HW type     Flags       HW address            Mask     Device
192.168.1.1      0x1         0x2         AA:BB:CC:00:00:01     *        eth0
192.168.1.20     0x1         0x2         aa:bb:cc:00:00:20     *        eth0
192.168.1.30     0x1         0x0         00:00:00:00:00:00     *        eth0
172.17.0.2       0x1         0x2         02:42:ac:11:00:02     *        docker0
192.168.1.40     0x1         0x6         aa:bb:cc:00:00:40     *        eth0
";

    fn n(ip: &str, mac: &str) -> Neigh {
        Neigh { ip: ip.into(), mac: mac.into(), iface: "eth0".into() }
    }

    fn titles(ev: &[NewEvent]) -> Vec<(&str, Level)> {
        ev.iter().map(|e| (e.title.as_str(), e.level)).collect()
    }

    #[test]
    fn parses_proc_tables() {
        let t = parse_arp(ARP);
        let ips: Vec<&str> = t.iter().map(|n| n.ip.as_str()).collect();
        assert_eq!(ips, ["192.168.1.1", "192.168.1.20", "172.17.0.2", "192.168.1.40"]);
        assert_eq!(t[0].mac, "aa:bb:cc:00:00:01");
        let route = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
wlan0\t00000000\t0102A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
eth0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
";
        if cfg!(target_endian = "little") {
            assert_eq!(parse_gateway(route).as_deref(), Some("192.168.1.1"));
        }
        assert_eq!(parse_gateway("Iface\tDestination\n"), None);
    }

    #[test]
    fn baseline_then_new_moved_and_reused() {
        let mut store = Store::open_in_memory().unwrap();
        let mut w = Watch::load(&store).unwrap();
        let gw = Some("192.168.1.1".to_owned());
        let ev = w.observe(100, &parse_arp(ARP), gw.clone(), &[]);
        assert_eq!(titles(&ev), [("network watch started", Level::Info)]);
        assert_eq!(ev[0].detail, "3 devices in the neighbour table");
        w.save(&mut store, 100).unwrap();

        // A restart keeps the inventory and the baseline.
        let mut w = Watch::load(&store).unwrap();
        let t = [n("192.168.1.1", "aa:bb:cc:00:00:01"), n("192.168.1.21", "aa:bb:cc:00:00:20")];
        let ev = w.observe(130, &t, gw.clone(), &[]);
        assert_eq!(titles(&ev), [("device moved 192.168.1.20 → 192.168.1.21", Level::Info)]);

        // A new phone gets the address the other device left: new, not a conflict.
        let t = [n("192.168.1.1", "aa:bb:cc:00:00:01"), n("192.168.1.20", "de:ad:be:ef:00:01")];
        let ev = w.observe(160, &t, gw.clone(), &[]);
        assert_eq!(titles(&ev), [("new device 192.168.1.20", Level::Info)]);
        assert_eq!(ev[0].detail, "de:ad:be:ef:00:01 on eth0");
        w.save(&mut store, 160).unwrap();

        let r = w.result(10);
        assert_eq!(r.gateway.as_deref(), Some("192.168.1.1"));
        assert_eq!(r.devices.len(), 4);
        let moved = r.devices.iter().find(|d| d.mac == "aa:bb:cc:00:00:20").unwrap();
        assert_eq!((moved.ip.as_str(), moved.online, moved.first_seen), ("192.168.1.21", false, 100));
        let n_rows: i64 = store.conn().query_row("SELECT count(*) FROM net_devices", [], |r| r.get(0)).unwrap();
        assert_eq!(n_rows, 4);
    }

    #[test]
    fn mac_switches_become_a_conflict_and_the_gateway_warns() {
        let store = Store::open_in_memory().unwrap();
        let mut w = Watch::load(&store).unwrap();
        let gw = Some("192.168.1.1".to_owned());
        let a = n("192.168.1.50", "aa:aa:aa:00:00:01");
        let b = n("192.168.1.50", "bb:bb:bb:00:00:02");
        w.observe(0, &[a.clone(), n("192.168.1.1", "aa:bb:cc:00:00:01")], gw.clone(), &[]);

        let ev = w.observe(30, std::slice::from_ref(&b), gw.clone(), &[]);
        assert_eq!(
            titles(&ev),
            [("new device 192.168.1.50", Level::Info), ("address 192.168.1.50 answers from a new MAC", Level::Info)]
        );
        let ev = w.observe(60, std::slice::from_ref(&a), gw.clone(), &[]);
        assert_eq!(titles(&ev), [("IP conflict on 192.168.1.50", Level::Warn)]);
        assert_eq!(ev[0].detail, "aa:aa:aa:00:00:01 and bb:bb:bb:00:00:02 keep taking turns answering for it");
        // Said once an hour, not every flip.
        assert!(w.observe(90, std::slice::from_ref(&b), gw.clone(), &[]).is_empty());
        assert!(w.observe(120, std::slice::from_ref(&a), gw.clone(), &[]).is_empty());

        // The gateway answering from another MAC is a warning straight away.
        let ev = w.observe(150, &[n("192.168.1.1", "66:66:66:00:00:66")], gw.clone(), &[]);
        let t = titles(&ev);
        assert!(t.contains(&("gateway 192.168.1.1 answers from a new MAC", Level::Warn)), "{t:?}");
        assert!(ev.iter().any(|e| e.detail.contains("ARP spoofing")));
    }

    #[test]
    fn ignores_bridges_and_summarises_many_new_devices() {
        let store = Store::open_in_memory().unwrap();
        let mut w = Watch::load(&store).unwrap();
        w.observe(0, &[], None, &[]);
        let many: Vec<Neigh> =
            (1..=30).map(|i| n(&format!("10.0.0.{i}"), &format!("aa:00:00:00:00:{i:02x}"))).collect();
        let mut t = many.clone();
        t.push(Neigh { ip: "172.18.0.2".into(), mac: "02:42:ac:12:00:02".into(), iface: "br-abc".into() });
        t.push(Neigh { ip: "10.9.0.2".into(), mac: "02:00:00:00:00:99".into(), iface: "wg0".into() });
        let ev = w.observe(30, &t, None, &["wg0".into()]);
        assert_eq!(titles(&ev), [("30 new devices", Level::Info)]);
        assert_eq!(w.result(1000).devices.len(), 30);
        assert_eq!(w.result(5).devices.len(), 5);
    }

    #[test]
    fn one_address_per_mac() {
        let store = Store::open_in_memory().unwrap();
        let mut w = Watch::load(&store).unwrap();
        let t = [n("192.168.1.9", "aa:00:00:00:00:01"), n("192.168.1.8", "aa:00:00:00:00:01")];
        w.observe(0, &t, None, &[]);
        assert_eq!(w.result(10).devices[0].ip, "192.168.1.8");
        // Both still there: no "moved" back and forth.
        assert!(w.observe(30, &t, None, &[]).is_empty());
        assert!(w.observe(60, &[t[1].clone(), t[0].clone()], None, &[]).is_empty());
    }
}
