//! This host's addresses (what `getifaddrs` gives on Linux).

use std::net::Ipv4Addr;

use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

/// Every IPv4 address on an interface that is up.
pub fn ipv4_addresses() -> Vec<Ipv4Addr> {
    let mut len = 16 * 1024u32;
    let mut buf: Vec<u64> = Vec::new();
    for _ in 0..3 {
        buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: a buffer of `len` bytes, aligned for the structs.
        let r = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_INET),
                GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                std::ptr::null(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        match r {
            0 => break,
            111 => continue, // ERROR_BUFFER_OVERFLOW: `len` now says how much.
            _ => return Vec::new(),
        }
    }
    let mut out = Vec::new();
    let mut a = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    // SAFETY: a linked list inside `buf`, as Windows wrote it.
    unsafe {
        while !a.is_null() {
            if (*a).OperStatus == 1 {
                let mut u = (*a).FirstUnicastAddress;
                while !u.is_null() {
                    let sa = (*u).Address.lpSockaddr;
                    if !sa.is_null() && (*sa).sa_family == AF_INET {
                        let sin = &*(sa as *const SOCKADDR_IN);
                        out.push(Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr)));
                    }
                    u = (*u).Next;
                }
            }
            a = (*a).Next;
        }
    }
    out
}

/// A table GetExtendedTcpTable or GetExtendedUdpTable fills, read into an
/// aligned buffer (it says how much it needs on the first call).
fn table(fill: impl Fn(*mut core::ffi::c_void, &mut u32) -> u32) -> Option<Vec<u64>> {
    let mut len = 0u32;
    for _ in 0..4 {
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        match fill(buf.as_mut_ptr().cast(), &mut len) {
            0 => return Some(buf),
            122 => continue, // ERROR_INSUFFICIENT_BUFFER: `len` now says how much.
            _ => return None,
        }
    }
    None
}

/// Rows of a MIB_*TABLE_OWNER_PID: a u32 count, then the rows.
///
/// # Safety
/// `buf` holds such a table with rows of type `T`.
unsafe fn rows<T: Copy>(buf: &[u64]) -> Vec<T> {
    let base = buf.as_ptr() as *const u8;
    // SAFETY: the caller promises the table layout; the count comes first
    // and the rows start at the struct's alignment.
    unsafe {
        let n = *(base as *const u32) as usize;
        let first = base.add(std::mem::align_of::<T>().max(4)) as *const T;
        let room = (buf.len() * 8).saturating_sub(std::mem::align_of::<T>().max(4)) / std::mem::size_of::<T>();
        (0..n.min(room)).map(|i| *first.add(i)).collect()
    }
}

fn port(p: u32) -> u16 {
    u16::from_be(p as u16)
}

/// Listening TCP sockets and bound UDP sockets: (proto, address, port, pid).
pub fn listeners() -> Vec<(String, String, u16, u32)> {
    use std::net::Ipv6Addr;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, MIB_UDP6ROW_OWNER_PID,
        MIB_UDPROW_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER, UDP_TABLE_OWNER_PID,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET6;
    let (v4, v6) = (u32::from(AF_INET), u32::from(AF_INET6));
    let mut out = Vec::new();
    // SAFETY (each call): a buffer of `len` bytes for the table asked for.
    let tcp = |af| table(|p, len| unsafe { GetExtendedTcpTable(p, len, 0, af, TCP_TABLE_OWNER_PID_LISTENER, 0) });
    let udp = |af| table(|p, len| unsafe { GetExtendedUdpTable(p, len, 0, af, UDP_TABLE_OWNER_PID, 0) });
    if let Some(b) = tcp(v4) {
        // SAFETY: an AF_INET TCP_TABLE_OWNER_PID_* table.
        for r in unsafe { rows::<MIB_TCPROW_OWNER_PID>(&b) } {
            let a = Ipv4Addr::from(r.dwLocalAddr.to_le_bytes());
            out.push(("tcp".into(), a.to_string(), port(r.dwLocalPort), r.dwOwningPid));
        }
    }
    if let Some(b) = tcp(v6) {
        // SAFETY: an AF_INET6 TCP_TABLE_OWNER_PID_* table.
        for r in unsafe { rows::<MIB_TCP6ROW_OWNER_PID>(&b) } {
            out.push(("tcp".into(), Ipv6Addr::from(r.ucLocalAddr).to_string(), port(r.dwLocalPort), r.dwOwningPid));
        }
    }
    if let Some(b) = udp(v4) {
        // SAFETY: an AF_INET UDP_TABLE_OWNER_PID table.
        for r in unsafe { rows::<MIB_UDPROW_OWNER_PID>(&b) } {
            let a = Ipv4Addr::from(r.dwLocalAddr.to_le_bytes());
            out.push(("udp".into(), a.to_string(), port(r.dwLocalPort), r.dwOwningPid));
        }
    }
    if let Some(b) = udp(v6) {
        // SAFETY: an AF_INET6 UDP_TABLE_OWNER_PID table.
        for r in unsafe { rows::<MIB_UDP6ROW_OWNER_PID>(&b) } {
            out.push(("udp".into(), Ipv6Addr::from(r.ucLocalAddr).to_string(), port(r.dwLocalPort), r.dwOwningPid));
        }
    }
    out
}

/// Every process's executable name by pid (no privileges needed).
pub fn process_names() -> std::collections::HashMap<u32, String> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    let mut out = std::collections::HashMap::new();
    // SAFETY: a fresh snapshot handle, closed by `Handle`.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap.is_null() || snap as isize == -1 {
        return out;
    }
    let _snap = super::Handle(snap);
    let mut e: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    // SAFETY: `e` is sized and the handle is a process snapshot.
    let mut ok = unsafe { Process32FirstW(snap, &mut e) } != 0;
    while ok {
        out.insert(e.th32ProcessID, super::from_wide(&e.szExeFile));
        // SAFETY: as above.
        ok = unsafe { Process32NextW(snap, &mut e) } != 0;
    }
    out
}

/// The IPv4 neighbour (ARP) table: entries Windows has resolved (states
/// probe to reachable; permanent ones are the host's own broadcast and
/// multicast mappings) with a real Ethernet MAC, named by interface alias.
pub fn neighbours() -> std::io::Result<Vec<crate::netwatch::Neigh>> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIpNetTable2, MIB_IPNET_TABLE2};
    use windows_sys::Win32::Networking::WinSock::{NlnsProbe, NlnsReachable};
    let mut t: *mut MIB_IPNET_TABLE2 = std::ptr::null_mut();
    // SAFETY: Windows allocates the table; freed below.
    let r = unsafe { GetIpNetTable2(AF_INET, &mut t) };
    if r != 0 {
        return Err(std::io::Error::from_raw_os_error(r as i32));
    }
    let mut aliases = std::collections::HashMap::<u64, String>::new();
    let mut out = Vec::new();
    // SAFETY: `NumEntries` rows follow the header, as Windows wrote them.
    unsafe {
        let rows = std::slice::from_raw_parts((*t).Table.as_ptr(), (*t).NumEntries as usize);
        for row in rows {
            if !(NlnsProbe..=NlnsReachable).contains(&row.State) || row.PhysicalAddressLength != 6 {
                continue;
            }
            let mac = &row.PhysicalAddress[..6];
            let ip = Ipv4Addr::from(row.Address.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes());
            if mac.iter().all(|b| *b == 0) || mac.iter().all(|b| *b == 0xff) || ip.is_multicast() || ip.is_broadcast() {
                continue;
            }
            let luid = row.InterfaceLuid.Value;
            let iface = aliases.entry(luid).or_insert_with(|| alias(&row.InterfaceLuid)).clone();
            let mac = mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
            out.push(crate::netwatch::Neigh { ip: ip.to_string(), mac, iface });
        }
        FreeMibTable(t.cast());
    }
    Ok(out)
}

/// An interface's alias ("Ethernet 2"), or its LUID in hex.
fn alias(luid: &windows_sys::Win32::NetworkManagement::Ndis::NET_LUID_LH) -> String {
    use windows_sys::Win32::NetworkManagement::IpHelper::ConvertInterfaceLuidToAlias;
    let mut buf = [0u16; 257];
    // SAFETY: a buffer of 257 characters, as the docs ask for.
    if unsafe { ConvertInterfaceLuidToAlias(luid, buf.as_mut_ptr(), buf.len()) } == 0 {
        super::from_wide(&buf)
    } else {
        // SAFETY: the union's plain value.
        format!("{:x}", unsafe { luid.Value })
    }
}

/// The IPv4 default gateway: the 0.0.0.0/0 route with the lowest metric.
pub fn gateway() -> Option<String> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIpForwardTable2, MIB_IPFORWARD_TABLE2};
    let mut t: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
    // SAFETY: Windows allocates the table; freed below.
    if unsafe { GetIpForwardTable2(AF_INET, &mut t) } != 0 {
        return None;
    }
    // SAFETY: `NumEntries` rows follow the header, as Windows wrote them.
    unsafe {
        let rows = std::slice::from_raw_parts((*t).Table.as_ptr(), (*t).NumEntries as usize);
        let best = rows
            .iter()
            .filter(|r| r.DestinationPrefix.PrefixLength == 0 && !r.Loopback)
            .map(|r| (r.Metric, Ipv4Addr::from(r.NextHop.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes())))
            .filter(|(_, ip)| !ip.is_unspecified())
            .min();
        FreeMibTable(t.cast());
        best.map(|(_, ip)| ip.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_listeners_with_their_processes() {
        let l = listeners();
        // Every Windows Server listens on RPC (135) and SMB (445).
        assert!(l.iter().any(|(p, _, port, _)| p == "tcp" && *port == 135), "{l:?}");
        let names = process_names();
        assert!(names.values().any(|n| n.eq_ignore_ascii_case("svchost.exe")));
        assert!(names.values().any(|n| n.eq_ignore_ascii_case("System")) || names.contains_key(&4));
    }

    #[test]
    fn reads_the_neighbour_table_and_gateway() {
        // A runner has a default route; its gateway is usually resolved.
        let gw = gateway();
        assert!(gw.is_some());
        let n = neighbours().unwrap();
        assert!(n.iter().all(|x| x.mac.len() == 17 && !x.iface.is_empty()), "{n:?}");
    }
}
