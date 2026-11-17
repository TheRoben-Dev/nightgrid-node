//! The network watch helper (`nightgrid-node netwatch`, N3b).
//!
//! Runs as its own unit, `nightgrid-node-netwatch.service`, only after root
//! runs `nightgrid-node allow netwatch` (decision N-021). It holds one
//! capability, `CAP_NET_RAW`, opens one `AF_PACKET` socket with a kernel
//! filter that passes only ARP and DHCP (UDP 67/68), and never sends a
//! packet. What it sees goes to the main service over its unix socket as
//! `net.report` calls; the main service decides what is news.
//!
//! - ARP from any device: who is on the LAN, even devices this host never
//!   talks to.
//! - DHCP offers and acks: which servers answer, and what they hand out
//!   (router, DNS, subnet, lease). A second server is how a rogue DHCP
//!   server shows up.
//! - DHCP requests: a client's host name, and the server it accepted.

use std::collections::{BTreeMap, HashMap};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use nightgrid_node_proto::{method, ArpSeen, DhcpSeen, NetReport};

use crate::config::Config;
use crate::daemon::now_secs;

/// Reports go out at least this often while there is something to say.
const FLUSH_EVERY: Duration = Duration::from_secs(5);
/// An empty report this often tells the service the helper is alive.
const HEARTBEAT: Duration = Duration::from_secs(60);
/// At most this many DHCP messages are kept between reports.
const MAX_DHCP: usize = 256;
/// At most this many ARP sightings are kept between reports.
const MAX_ARP: usize = 4096;

fn mac_str(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(":")
}

fn ip4(b: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(b[0], b[1], b[2], b[3])
}

fn usable_mac(b: &[u8]) -> bool {
    b.len() == 6 && b.iter().any(|x| *x != 0) && b.iter().any(|x| *x != 0xff) && b[0] & 1 == 0
}

/// What one frame told us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    Arp { ip: String, mac: String },
    Dhcp(Box<DhcpSeen>),
}

/// Parses an Ethernet frame the filter let through.
pub fn parse_frame(f: &[u8], iface: &str, at: i64) -> Option<Seen> {
    if f.len() < 14 {
        return None;
    }
    let ethertype = u16::from_be_bytes([f[12], f[13]]);
    let p = &f[14..];
    match ethertype {
        0x0806 => {
            // htype 1, ptype IPv4, hlen 6, plen 4.
            if p.len() < 28 || p[0..2] != [0, 1] || p[2..4] != [8, 0] || p[4] != 6 || p[5] != 4 {
                return None;
            }
            let (sha, spa) = (&p[8..14], ip4(&p[14..18]));
            if !usable_mac(sha) || spa.is_unspecified() || spa.is_broadcast() || spa.is_multicast() {
                return None;
            }
            Some(Seen::Arp { ip: spa.to_string(), mac: mac_str(sha) })
        }
        0x0800 => {
            if p.len() < 20 || p[0] >> 4 != 4 || p[9] != 17 {
                return None;
            }
            let ihl = usize::from(p[0] & 0xf) * 4;
            let udp = p.get(ihl..)?;
            if udp.len() < 8 {
                return None;
            }
            let (sport, dport) = (u16::from_be_bytes([udp[0], udp[1]]), u16::from_be_bytes([udp[2], udp[3]]));
            if !matches!((sport, dport), (67 | 68, 67 | 68)) {
                return None;
            }
            let mut d = parse_dhcp(&udp[8..], at, iface)?;
            if d.server_mac.is_some() {
                // Set only for server replies below: use the frame's source.
                d.server_mac = Some(mac_str(&f[6..12]));
            }
            Some(Seen::Dhcp(Box::new(d)))
        }
        _ => None,
    }
}

const MSG: [&str; 9] = ["", "discover", "offer", "request", "decline", "ack", "nak", "release", "inform"];

fn host_name(b: &[u8]) -> Option<String> {
    let s: String = b
        .iter()
        .take(63)
        .map(|c| *c as char)
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        .collect();
    (!s.is_empty()).then_some(s)
}

/// A BOOTP/DHCP payload (after the UDP header).
pub fn parse_dhcp(b: &[u8], at: i64, iface: &str) -> Option<DhcpSeen> {
    if b.len() < 240 || b[1] != 1 || b[2] != 6 || b[236..240] != [99, 130, 83, 99] {
        return None;
    }
    let reply = b[0] == 2;
    let mut d = DhcpSeen {
        at,
        iface: iface.to_owned(),
        client_mac: mac_str(&b[28..34]),
        server_mac: reply.then(String::new),
        ..Default::default()
    };
    let yiaddr = ip4(&b[16..20]);
    if !yiaddr.is_unspecified() {
        d.ip = Some(yiaddr.to_string());
    }
    let mut o = &b[240..];
    while let Some((&code, rest)) = o.split_first() {
        match code {
            0 => {
                o = rest;
                continue;
            }
            255 => break,
            _ => {}
        }
        let (&len, rest) = rest.split_first()?;
        let v = rest.get(..usize::from(len))?;
        o = &rest[usize::from(len)..];
        match code {
            53 if len == 1 => d.msg = MSG.get(usize::from(v[0])).copied().unwrap_or("").to_owned(),
            54 if len == 4 => d.server = Some(ip4(v).to_string()),
            3 if len >= 4 => d.router = Some(ip4(v).to_string()),
            6 => d.dns = v.as_chunks::<4>().0.iter().take(4).map(|c| ip4(c).to_string()).collect(),
            1 if len == 4 => d.subnet = Some(ip4(v).to_string()),
            51 if len == 4 => d.lease_secs = Some(u32::from_be_bytes([v[0], v[1], v[2], v[3]])),
            12 => d.hostname = host_name(v),
            _ => {}
        }
    }
    (!d.msg.is_empty()).then_some(d)
}

/// The kernel filter: accept ARP, and unfragmented IPv4 UDP from or to port
/// 67 or 68; drop everything else before it reaches this process.
fn filter() -> [libc::sock_filter; 16] {
    let op = |code: u16, jt: u8, jf: u8, k: u32| libc::sock_filter { code, jt, jf, k };
    [
        op(0x28, 0, 0, 12),      // 0: ldh [12]           ethertype
        op(0x15, 12, 0, 0x0806), // 1: ARP → accept
        op(0x15, 0, 12, 0x0800), // 2: not IPv4 → drop
        op(0x30, 0, 0, 23),      // 3: ldb [23]           protocol
        op(0x15, 0, 10, 17),     // 4: not UDP → drop
        op(0x28, 0, 0, 20),      // 5: ldh [20]           fragment offset
        op(0x45, 8, 0, 0x1fff),  // 6: a fragment → drop
        op(0xb1, 0, 0, 14),      // 7: ldxb 4*([14]&0xf)  header length
        op(0x48, 0, 0, 14),      // 8: ldh [x+14]         source port
        op(0x15, 4, 0, 67),      // 9
        op(0x15, 3, 0, 68),      // 10
        op(0x48, 0, 0, 16),      // 11: ldh [x+16]        destination port
        op(0x15, 1, 0, 67),      // 12
        op(0x15, 0, 1, 68),      // 13
        op(0x06, 0, 0, 0xffff),  // 14: accept
        op(0x06, 0, 0, 0),       // 15: drop
    ]
}

struct Socket(libc::c_int);

impl Drop for Socket {
    fn drop(&mut self) {
        // SAFETY: we own the descriptor.
        unsafe { libc::close(self.0) };
    }
}

fn last_err(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

fn open() -> Result<Socket, String> {
    let proto = (libc::ETH_P_ALL as u16).to_be();
    // SAFETY: plain socket(2).
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW | libc::SOCK_CLOEXEC, i32::from(proto)) };
    if fd < 0 {
        return Err(last_err("packet socket (does the unit have CAP_NET_RAW?)"));
    }
    let s = Socket(fd);
    let prog = filter();
    let fprog = libc::sock_fprog { len: prog.len() as u16, filter: prog.as_ptr().cast_mut() };
    let tv = libc::timeval { tv_sec: 1, tv_usec: 0 };
    // SAFETY: the pointers and sizes describe live values of the right types.
    unsafe {
        if libc::setsockopt(
            s.0,
            libc::SOL_SOCKET,
            libc::SO_ATTACH_FILTER,
            (&fprog as *const libc::sock_fprog).cast(),
            std::mem::size_of::<libc::sock_fprog>() as u32,
        ) != 0
        {
            return Err(last_err("attach filter"));
        }
        if libc::setsockopt(
            s.0,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&tv as *const libc::timeval).cast(),
            std::mem::size_of::<libc::timeval>() as u32,
        ) != 0
        {
            return Err(last_err("receive timeout"));
        }
    }
    Ok(s)
}

/// Interface index → name, from /sys/class/net.
#[derive(Default)]
struct Ifaces {
    names: HashMap<i32, String>,
    read: Option<Instant>,
}

impl Ifaces {
    fn name(&mut self, index: i32) -> Option<&str> {
        if !self.names.contains_key(&index) && self.read.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
            self.read = Some(Instant::now());
            self.names = std::fs::read_dir("/sys/class/net")
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| {
                    let i = std::fs::read_to_string(e.path().join("ifindex")).ok()?.trim().parse().ok()?;
                    Some((i, e.file_name().to_string_lossy().into_owned()))
                })
                .collect();
        }
        self.names.get(&index).map(String::as_str)
    }
}

/// Sightings waiting to be reported.
#[derive(Default)]
pub struct Batch {
    arp: BTreeMap<(String, String), String>,
    dhcp: Vec<DhcpSeen>,
}

impl Batch {
    pub fn add(&mut self, s: Seen, iface: &str) {
        match s {
            Seen::Arp { ip, mac } => {
                if self.arp.len() < MAX_ARP || self.arp.contains_key(&(ip.clone(), mac.clone())) {
                    self.arp.insert((ip, mac), iface.to_owned());
                }
            }
            Seen::Dhcp(d) => {
                if self.dhcp.len() < MAX_DHCP {
                    self.dhcp.push(*d);
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.arp.is_empty() && self.dhcp.is_empty()
    }

    pub fn take(&mut self) -> NetReport {
        let arp =
            std::mem::take(&mut self.arp).into_iter().map(|((ip, mac), iface)| ArpSeen { ip, mac, iface }).collect();
        NetReport { arp, dhcp: std::mem::take(&mut self.dhcp) }
    }
}

/// `nightgrid-node netwatch`: runs until killed.
pub fn run(config: &Config) -> Result<(), String> {
    if !config.network.enabled {
        return Err("network watch is switched off in the config ([network] enabled = false)".into());
    }
    let sock = open()?;
    crate::info!("network watch helper listening for ARP and DHCP (sends nothing)");
    let ignore = &config.network.ignore_ifaces;
    let mut ifaces = Ifaces::default();
    let mut batch = Batch::default();
    let mut client: Option<crate::client::Client> = None;
    let mut last_sent = Instant::now() - HEARTBEAT;
    let mut retry_at = Instant::now();
    let mut buf = vec![0u8; 2048];
    loop {
        // SAFETY: sockaddr_ll is plain data; buf and addr are valid for the sizes given.
        let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        let mut alen = std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
        let n = unsafe {
            libc::recvfrom(
                sock.0,
                buf.as_mut_ptr().cast(),
                buf.len(),
                0,
                (&mut addr as *mut libc::sockaddr_ll).cast(),
                &mut alen,
            )
        };
        if n > 0 && addr.sll_pkttype != libc::PACKET_OUTGOING {
            if let Some(name) = ifaces.name(addr.sll_ifindex) {
                if name != "lo" && !crate::netwatch::is_container_bridge(name) && !ignore.iter().any(|i| i == name) {
                    let name = name.to_owned();
                    if let Some(s) = parse_frame(&buf[..n as usize], &name, now_secs()) {
                        batch.add(s, &name);
                    }
                }
            }
        } else if n < 0 {
            let e = std::io::Error::last_os_error();
            if !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) {
                return Err(format!("receive: {e}"));
            }
        }

        let due = (!batch.is_empty() && last_sent.elapsed() >= FLUSH_EVERY)
            || (!batch.dhcp.is_empty() && last_sent.elapsed() >= Duration::from_secs(1))
            || last_sent.elapsed() >= HEARTBEAT;
        if !due || Instant::now() < retry_at {
            continue;
        }
        if client.is_none() {
            match crate::client::Client::connect(&config.server.socket, "nightgrid-node netwatch") {
                Ok(c) => client = Some(c),
                Err(e) => {
                    crate::warn!("cannot reach the node at {}: {e}", config.server.socket.display());
                    retry_at = Instant::now() + Duration::from_secs(10);
                    continue;
                }
            }
        }
        let report = batch.take();
        let params = serde_json::to_value(&report).unwrap_or_default();
        if let Some(c) = client.as_mut() {
            if let Err(e) = c.call(method::NET_REPORT, params) {
                crate::warn!("report failed: {e}");
                client = None;
                retry_at = Instant::now() + Duration::from_secs(5);
            }
        }
        last_sent = Instant::now();
    }
}

/// True when this process may open packet sockets (for `status` messages).
pub fn can_sniff() -> bool {
    open().is_ok()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// An Ethernet + IPv4 + UDP frame carrying a DHCP message (for tests).
    pub fn dhcp_frame(src_mac: [u8; 6], op: u8, chaddr: [u8; 6], yiaddr: [u8; 4], opts: &[u8]) -> Vec<u8> {
        let mut bootp = vec![0u8; 240];
        bootp[0] = op;
        bootp[1] = 1;
        bootp[2] = 6;
        bootp[16..20].copy_from_slice(&yiaddr);
        bootp[28..34].copy_from_slice(&chaddr);
        bootp[236..240].copy_from_slice(&[99, 130, 83, 99]);
        bootp.extend_from_slice(opts);
        bootp.push(255);
        let (sport, dport) = if op == 2 { (67u16, 68u16) } else { (68, 67) };
        let mut udp = Vec::new();
        udp.extend_from_slice(&sport.to_be_bytes());
        udp.extend_from_slice(&dport.to_be_bytes());
        udp.extend_from_slice(&((8 + bootp.len()) as u16).to_be_bytes());
        udp.extend_from_slice(&[0, 0]);
        udp.extend_from_slice(&bootp);
        let mut ip = vec![0x45, 0, 0, 0, 0, 0, 0, 0, 64, 17, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255];
        let total = (20 + udp.len()) as u16;
        ip[2..4].copy_from_slice(&total.to_be_bytes());
        ip.extend_from_slice(&udp);
        let mut f = vec![0xff; 6];
        f.extend_from_slice(&src_mac);
        f.extend_from_slice(&[8, 0]);
        f.extend_from_slice(&ip);
        f
    }

    #[test]
    fn parses_offer_request_and_arp() {
        let server = [0x02, 0, 0, 0, 0, 0x01];
        let client = [0xaa, 0xbb, 0xcc, 0, 0, 0x02];
        let opts = [
            53, 1, 2, // offer
            54, 4, 10, 99, 0, 1, // server id
            1, 4, 255, 255, 255, 0, // subnet
            3, 4, 10, 99, 0, 1, // router
            6, 8, 10, 99, 0, 1, 1, 1, 1, 1, // dns
            51, 4, 0, 1, 81, 128, // lease 86400
            0, 0, // padding
        ];
        let f = dhcp_frame(server, 2, client, [10, 99, 0, 50], &opts);
        let Some(Seen::Dhcp(d)) = parse_frame(&f, "eth0", 5) else { panic!() };
        assert_eq!(d.msg, "offer");
        assert_eq!(d.server.as_deref(), Some("10.99.0.1"));
        assert_eq!(d.server_mac.as_deref(), Some("02:00:00:00:00:01"));
        assert_eq!(d.client_mac, "aa:bb:cc:00:00:02");
        assert_eq!(d.ip.as_deref(), Some("10.99.0.50"));
        assert_eq!(d.router.as_deref(), Some("10.99.0.1"));
        assert_eq!(d.dns, ["10.99.0.1", "1.1.1.1"]);
        assert_eq!((d.subnet.as_deref(), d.lease_secs), (Some("255.255.255.0"), Some(86_400)));

        let opts = [53, 1, 3, 54, 4, 10, 99, 0, 1, 12, 9, b'n', b'g', b'n', b'-', b'p', b'h', b'o', b'n', 0x07];
        let f = dhcp_frame(client, 1, client, [0; 4], &opts);
        let Some(Seen::Dhcp(d)) = parse_frame(&f, "eth0", 6) else { panic!() };
        assert_eq!(
            (d.msg.as_str(), d.hostname.as_deref(), d.server_mac.as_deref()),
            ("request", Some("ngn-phon"), None)
        );

        // Truncated options and a non-DHCP BOOTP payload are ignored.
        let mut bad = dhcp_frame(server, 2, client, [0; 4], &[53, 4, 2]);
        bad.pop();
        assert!(parse_frame(&bad, "eth0", 0).is_none());
        assert!(parse_frame(&dhcp_frame(server, 2, client, [0; 4], &[]), "eth0", 0).is_none());

        let mut arp = vec![0xff; 6];
        arp.extend_from_slice(&client);
        arp.extend_from_slice(&[8, 6, 0, 1, 8, 0, 6, 4, 0, 1]);
        arp.extend_from_slice(&client);
        arp.extend_from_slice(&[192, 168, 1, 77]);
        arp.extend_from_slice(&[0; 6]);
        arp.extend_from_slice(&[192, 168, 1, 1]);
        assert_eq!(
            parse_frame(&arp, "eth0", 0),
            Some(Seen::Arp { ip: "192.168.1.77".into(), mac: "aa:bb:cc:00:00:02".into() })
        );
        // An ARP probe (sender 0.0.0.0) says nothing about the sender's address.
        arp[28..32].copy_from_slice(&[0; 4]);
        assert_eq!(parse_frame(&arp, "eth0", 0), None);
    }

    #[test]
    fn batches_dedupe_arp() {
        let mut b = Batch::default();
        for _ in 0..3 {
            b.add(Seen::Arp { ip: "10.0.0.2".into(), mac: "aa:00:00:00:00:02".into() }, "eth0");
        }
        let r = b.take();
        assert_eq!(r.arp.len(), 1);
        assert!(b.is_empty());
    }
}
