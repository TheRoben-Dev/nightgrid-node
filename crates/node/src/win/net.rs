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
