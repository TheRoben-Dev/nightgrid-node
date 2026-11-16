//! DHCP servers on the LAN (N3b), from what the network watch helper saw.
//!
//! A server is known by its identifier (option 54). The first one the node
//! ever sees is noted; any further one is a warning unless root listed it
//! in `[network] dhcp_servers`, because two DHCP servers on one LAN hand out
//! conflicting addresses, and an unexpected one may be rogue (sending
//! clients to a different router or DNS). A known server that starts
//! handing out a different router or DNS is a warning too.

use std::collections::BTreeMap;

use nightgrid_node_proto::{DhcpResult, DhcpSeen, DhcpServer, EventKind, Level};
use serde::{Deserialize, Serialize};

use crate::events::NewEvent;
use crate::store::{Result, Store};

const META_SERVERS: &str = "dhcp_servers";
/// Servers not seen for this long are forgotten (and would be new again).
const FORGET_SECS: i64 = 30 * 86_400;
/// The helper counts as running if it reported this recently.
const ALIVE_SECS: i64 = 150;
const MAX_SERVERS: usize = 32;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Server {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mac: Option<String>,
    first: i64,
    last: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    router: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    dns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subnet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lease: Option<u32>,
}

/// What the node knows about DHCP on its LAN.
#[derive(Debug, Default)]
pub struct Dhcp {
    servers: BTreeMap<String, Server>,
    trusted: Vec<String>,
    /// When the helper last reported.
    pub last_report: Option<i64>,
    dirty: bool,
}

fn words(d: &[String]) -> String {
    if d.is_empty() {
        "none".into()
    } else {
        d.join(", ")
    }
}

fn hands_out(s: &Server) -> String {
    let mut parts = Vec::new();
    if let Some(r) = &s.router {
        parts.push(format!("router {r}"));
    }
    if !s.dns.is_empty() {
        parts.push(format!("DNS {}", s.dns.join(", ")));
    }
    if let Some(l) = s.lease {
        parts.push(format!("lease {}", crate::uptime::span_words(i64::from(l))));
    }
    parts.join(", ")
}

impl Dhcp {
    pub fn load(store: &Store, trusted: &[String]) -> Self {
        let servers =
            store.meta(META_SERVERS).ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
        Self { servers, trusted: trusted.to_vec(), last_report: None, dirty: false }
    }

    pub fn watching(&self, now: i64) -> bool {
        self.last_report.is_some_and(|t| now - t <= ALIVE_SECS)
    }

    /// Takes the DHCP messages of one report; returns the events.
    pub fn observe(&mut self, now: i64, msgs: &[DhcpSeen]) -> Vec<NewEvent> {
        self.last_report = Some(now);
        let mut events = Vec::new();
        for m in msgs {
            let Some(id) = m.server.as_deref() else { continue };
            if id.parse::<std::net::Ipv4Addr>().is_err() {
                continue;
            }
            // Only a server's own replies say what it hands out; a client's
            // request only names the server it chose.
            let reply = matches!(m.msg.as_str(), "offer" | "ack" | "nak");
            let known = self.servers.contains_key(id);
            if !known {
                if self.servers.len() >= MAX_SERVERS {
                    continue;
                }
                let s = Server {
                    mac: m.server_mac.clone().filter(|_| reply),
                    first: now,
                    last: now,
                    router: m.router.clone().filter(|_| reply),
                    dns: if reply { m.dns.clone() } else { Vec::new() },
                    subnet: m.subnet.clone().filter(|_| reply),
                    lease: m.lease_secs.filter(|_| reply),
                };
                let mac = s.mac.as_deref().map(|x| format!(" ({x})")).unwrap_or_default();
                let what = hands_out(&s);
                let others: Vec<&str> = self.servers.keys().map(String::as_str).collect();
                let trusted = self.trusted.iter().any(|t| t == id);
                let e = if others.is_empty() || trusted {
                    NewEvent::new(
                        now,
                        EventKind::Network,
                        Level::Info,
                        format!("DHCP server {id}"),
                        format!(
                            "answering on {}{mac}{}",
                            m.iface,
                            if what.is_empty() { String::new() } else { format!(": {what}") }
                        ),
                    )
                } else {
                    NewEvent::new(
                        now,
                        EventKind::Network,
                        Level::Warn,
                        format!("another DHCP server on the LAN: {id}"),
                        format!(
                            "seen on {}{mac}{}; already answering: {}. Two DHCP servers hand out conflicting \
                             addresses, and one you didn't set up may be rogue. If it's expected, list it in \
                             [network] dhcp_servers.",
                            m.iface,
                            if what.is_empty() { String::new() } else { format!(", {what}") },
                            others.join(", ")
                        ),
                    )
                };
                events.push(e);
                self.servers.insert(id.to_owned(), s);
                self.dirty = true;
                continue;
            }
            let s = self.servers.get_mut(id).expect("known");
            if now - s.last >= 600 {
                self.dirty = true;
            }
            s.last = now;
            if !reply {
                continue;
            }
            if s.mac.is_none() && m.server_mac.is_some() {
                s.mac = m.server_mac.clone();
                self.dirty = true;
            }
            // Fill in what wasn't known yet without calling it a change.
            let mut changes = Vec::new();
            if let Some(r) = &m.router {
                match &s.router {
                    Some(old) if old != r => changes.push(format!("router {old} → {r}")),
                    None => {}
                    _ => {}
                }
                s.router = Some(r.clone());
            }
            if !m.dns.is_empty() {
                if !s.dns.is_empty() && s.dns != m.dns {
                    changes.push(format!("DNS {} → {}", words(&s.dns), words(&m.dns)));
                }
                s.dns = m.dns.clone();
            }
            if m.subnet.is_some() {
                s.subnet = m.subnet.clone();
            }
            if m.lease_secs.is_some() {
                s.lease = m.lease_secs;
            }
            self.dirty = true;
            if !changes.is_empty() {
                events.push(NewEvent::new(
                    now,
                    EventKind::Network,
                    Level::Warn,
                    format!(
                        "DHCP server {id} hands out a different {}",
                        if changes.len() > 1 {
                            "router and DNS"
                        } else if changes[0].starts_with("router") {
                            "router"
                        } else {
                            "DNS"
                        }
                    ),
                    changes.join("; "),
                ));
            }
        }
        let before = self.servers.len();
        self.servers.retain(|_, s| now - s.last < FORGET_SECS);
        if self.servers.len() != before {
            self.dirty = true;
        }
        events
    }

    pub fn save(&mut self, store: &Store) -> Result<()> {
        if self.dirty {
            store.set_meta(META_SERVERS, &serde_json::to_string(&self.servers).unwrap_or_default())?;
            self.dirty = false;
        }
        Ok(())
    }

    pub fn result(&self, now: i64) -> DhcpResult {
        let mut servers: Vec<DhcpServer> = self
            .servers
            .iter()
            .map(|(ip, s)| DhcpServer {
                ip: ip.clone(),
                mac: s.mac.clone(),
                first_seen: s.first,
                last_seen: s.last,
                router: s.router.clone(),
                dns: s.dns.clone(),
                subnet: s.subnet.clone(),
                lease_secs: s.lease,
                trusted: self.trusted.iter().any(|t| t == ip),
            })
            .collect();
        servers.sort_by_key(|s| std::cmp::Reverse(s.last_seen));
        DhcpResult { watching: self.watching(now), servers }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(server: &str, mac: &str, router: &str, dns: &[&str]) -> DhcpSeen {
        DhcpSeen {
            at: 0,
            msg: "offer".into(),
            iface: "eth0".into(),
            client_mac: "aa:00:00:00:00:01".into(),
            server: Some(server.into()),
            server_mac: Some(mac.into()),
            ip: Some("192.168.1.50".into()),
            router: Some(router.into()),
            dns: dns.iter().map(|s| (*s).to_owned()).collect(),
            subnet: Some("255.255.255.0".into()),
            lease_secs: Some(86_400),
            hostname: None,
        }
    }

    fn t(ev: &[NewEvent]) -> Vec<(&str, Level)> {
        ev.iter().map(|e| (e.title.as_str(), e.level)).collect()
    }

    #[test]
    fn first_server_then_a_rogue_one_then_changes() {
        let store = Store::open_in_memory().unwrap();
        let mut d = Dhcp::load(&store, &[]);
        assert!(!d.watching(0));
        let ev = d.observe(10, &[offer("192.168.1.1", "02:00:00:00:00:01", "192.168.1.1", &["192.168.1.1"])]);
        assert_eq!(t(&ev), [("DHCP server 192.168.1.1", Level::Info)]);
        assert!(ev[0].detail.contains("router 192.168.1.1, DNS 192.168.1.1, lease 1d 0h"), "{}", ev[0].detail);
        assert!(d.watching(100) && !d.watching(1000));
        // Same server again: nothing.
        assert!(d
            .observe(20, &[offer("192.168.1.1", "02:00:00:00:00:01", "192.168.1.1", &["192.168.1.1"])])
            .is_empty());

        let ev = d.observe(30, &[offer("192.168.1.66", "02:00:00:00:00:66", "192.168.1.66", &["192.168.1.66"])]);
        assert_eq!(t(&ev), [("another DHCP server on the LAN: 192.168.1.66", Level::Warn)]);
        assert!(ev[0].detail.contains("already answering: 192.168.1.1"));

        let ev = d.observe(40, &[offer("192.168.1.1", "02:00:00:00:00:01", "192.168.1.1", &["9.9.9.9"])]);
        assert_eq!(t(&ev), [("DHCP server 192.168.1.1 hands out a different DNS", Level::Warn)]);
        assert_eq!(ev[0].detail, "DNS 192.168.1.1 → 9.9.9.9");

        d.save(&store).unwrap();
        let d2 = Dhcp::load(&store, &["192.168.1.66".into()]);
        let r = d2.result(40);
        assert_eq!(r.servers.len(), 2);
        assert!(r.servers.iter().any(|s| s.ip == "192.168.1.66" && s.trusted));
        assert!(!r.watching);
    }

    #[test]
    fn requests_name_servers_but_trusted_ones_stay_quiet() {
        let store = Store::open_in_memory().unwrap();
        let mut d = Dhcp::load(&store, &["10.0.0.2".into()]);
        d.observe(0, &[offer("10.0.0.1", "02:00:00:00:00:01", "10.0.0.1", &[])]);
        let req = DhcpSeen {
            msg: "request".into(),
            server: Some("10.0.0.2".into()),
            iface: "eth0".into(),
            ..Default::default()
        };
        let ev = d.observe(5, &[req]);
        assert_eq!(t(&ev), [("DHCP server 10.0.0.2", Level::Info)]);
        let r = d.result(5);
        let s = r.servers.iter().find(|s| s.ip == "10.0.0.2").unwrap();
        assert!(s.mac.is_none() && s.router.is_none() && s.trusted);
    }
}
