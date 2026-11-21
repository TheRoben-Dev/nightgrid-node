//! Blocking addresses (N6c): `firewall.block` and `firewall.unblock`, run by
//! the root helper with nftables, and the optional auto-block of addresses
//! that keep failing SSH logins.
//!
//! The node keeps its own table, `inet nightgrid_node`, with one set per
//! address family whose elements carry a timeout, so every block ends by
//! itself in the kernel even if the node stops. Rules drop input and
//! forwarded traffic (published container ports) from those addresses and
//! touch nothing else; `deny helper` and `uninstall` delete the table.
//!
//! Safety, checked on the helper's side from root's config: the action must
//! be in `[actions] allow`, the duration is root's (`[block] minutes`), and
//! loopback, private, link-local and CGNAT/Tailscale addresses are never
//! blocked (unless root sets `[block] private`), nor anything in
//! `[block] never`. The service can't widen any of that.
//!
//! On Windows (W2e) a block is an inbound Windows Firewall rule in the
//! group `Nightgrid Node blocks`, named after the address, whose description
//! holds its end time. Windows has no expiring rules, so the helper removes
//! the ones past their time whenever it touches the group, and the node asks
//! it to (`firewall.sweep`) once a minute after one is due.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use nightgrid_node_proto::{action, ActionRequest, BlockEntry};

use crate::config::BlockConfig;
use crate::daemon::{now_secs, Shared};
use crate::helper::{Reply, Runner};

pub const TABLE: &str = "nightgrid_node";
const META_BLOCKS: &str = "blocks";

/// `a.b.c.d/n`, `x::/n` or a bare address.
pub fn parse_cidr(s: &str) -> Option<(IpAddr, u8)> {
    let (a, n) = match s.trim().split_once('/') {
        Some((a, n)) => (a, n.parse::<u8>().ok()?),
        None => (s.trim(), 255),
    };
    let ip: IpAddr = a.parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let n = if n == 255 { max } else { n };
    (n <= max).then_some((ip, n))
}

pub fn in_cidr(ip: IpAddr, (net, bits): (IpAddr, u8)) -> bool {
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let m = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            (u32::from(a) & m) == (u32::from(b) & m)
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let m = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
            (u128::from(a) & m) == (u128::from(b) & m)
        }
        _ => false,
    }
}

const PROTECTED: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "224.0.0.0/3",
    "::/127",
    "fc00::/7",
    "fe80::/10",
    "ff00::/8",
];

/// An IPv4-mapped IPv6 address is judged as the IPv4 one.
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// Why `ip` may not be blocked under root's config, if so.
pub fn refuse(cfg: &BlockConfig, ip: IpAddr) -> Option<String> {
    let ip = canonical(ip);
    if !cfg.private && PROTECTED.iter().filter_map(|c| parse_cidr(c)).any(|c| in_cidr(ip, c)) {
        return Some(format!("{ip} is a local, private or VPN address (root can allow that with [block] private)"));
    }
    if cfg.never.iter().filter_map(|c| parse_cidr(c)).any(|c| in_cidr(ip, c)) {
        return Some(format!("{ip} is in [block] never"));
    }
    None
}

fn set_for(ip: IpAddr) -> &'static str {
    if ip.is_ipv4() {
        "v4"
    } else {
        "v6"
    }
}

/// Creates the table, sets and chains if missing (idempotent; existing
/// elements stay), as one nft transaction.
pub fn ensure_script() -> String {
    let t = format!("inet {TABLE}");
    let mut s = format!(
        "add table {t} ; \
         add set {t} v4 {{ type ipv4_addr ; flags timeout ; }} ; \
         add set {t} v6 {{ type ipv6_addr ; flags timeout ; }}"
    );
    for (chain, hook) in [("input", "input"), ("forward", "forward")] {
        s.push_str(&format!(
            " ; add chain {t} {chain} {{ type filter hook {hook} priority -5 ; policy accept ; }} ; \
             flush chain {t} {chain} ; \
             add rule {t} {chain} ip saddr @v4 drop ; \
             add rule {t} {chain} ip6 saddr @v6 drop"
        ));
    }
    s
}

/// The Windows Firewall rule group that holds the node's blocks.
pub const WIN_GROUP: &str = "Nightgrid Node blocks";
/// The helper-only request that removes expired Windows rules.
pub const SWEEP: &str = "firewall.sweep";

fn win_rule(ip: IpAddr) -> String {
    format!("Nightgrid Node block {ip}")
}

/// PowerShell that removes the group's rules past their time (and the rule
/// named `also`, if given). `IpAddr`'s display is digits, dots, hex and
/// colons only, so nothing here needs quoting beyond the fixed names.
fn win_prune(now: i64, also: Option<&str>) -> String {
    let also = also.map(|n| format!("$r.Name -eq '{n}' -or ")).unwrap_or_default();
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         $fw = New-Object -ComObject HNetCfg.FwPolicy2\n\
         $gone = 0\n\
         foreach ($r in @($fw.Rules | Where-Object {{ $_.Grouping -eq '{WIN_GROUP}' }})) {{\n\
         \x20 if ({also}($r.Description -match '^until (\\d+)$' -and [int64]$Matches[1] -le {now})) {{ $fw.Rules.Remove($r.Name); $gone++ }}\n\
         }}\n"
    )
}

/// PowerShell for one block: an inbound rule for the address, any protocol
/// and profile, that ends at `until`. Says `firewall-off` when a profile has
/// Windows Firewall turned off (the rule then does nothing there).
pub fn win_block_script(ip: IpAddr, now: i64, until: i64) -> String {
    let name = win_rule(ip);
    format!(
        "{}$n = New-Object -ComObject HNetCfg.FWRule\n\
         $n.Name = '{name}'; $n.Description = 'until {until}'; $n.Grouping = '{WIN_GROUP}'\n\
         $n.Direction = 1; $n.Action = 0; $n.Protocol = 256; $n.RemoteAddresses = '{ip}'; $n.Profiles = 2147483647; $n.Enabled = $true\n\
         $fw.Rules.Add($n)\n\
         if (@(1, 2, 4 | Where-Object {{ -not $fw.FirewallEnabled($_) }}).Count) {{ 'firewall-off' }}\n",
        win_prune(now, Some(&name))
    )
}

/// PowerShell for an unblock: says `removed` when there was a rule.
pub fn win_unblock_script(ip: IpAddr, now: i64) -> String {
    let name = win_rule(ip);
    format!(
        "{}$had = @($fw.Rules | Where-Object {{ $_.Name -eq '{name}' }}).Count\n\
         if ($had) {{ $fw.Rules.Remove('{name}') }}\n\
         if ($had -or $gone) {{ 'removed' }}\n",
        win_prune(now, None)
    )
}

/// PowerShell that removes the expired rules: says how many.
pub fn win_sweep_script(now: i64) -> String {
    format!("{}$gone\n", win_prune(now, None))
}

/// PowerShell that removes every block (`deny helper`, `uninstall`).
pub fn win_remove_all_script() -> String {
    format!(
        "$fw = New-Object -ComObject HNetCfg.FwPolicy2\n\
         foreach ($r in @($fw.Rules | Where-Object {{ $_.Grouping -eq '{WIN_GROUP}' }})) {{ $fw.Rules.Remove($r.Name) }}\n"
    )
}

/// The Windows side of `firewall.*` and `firewall.sweep`.
fn handle_windows(cfg: &BlockConfig, req_action: &str, ip: Option<IpAddr>, now: i64, run: &mut dyn Runner) -> Reply {
    let no = |detail: String| Reply { ok: false, detail };
    match (req_action, ip) {
        (SWEEP, _) => match run.powershell(&win_sweep_script(now)) {
            Ok(out) => Reply { ok: true, detail: format!("removed {} expired blocks", out.trim()) },
            Err(e) => no(e),
        },
        (action::FIREWALL_UNBLOCK, Some(ip)) => match run.powershell(&win_unblock_script(ip, now)) {
            Ok(out) if out.contains("removed") => Reply { ok: true, detail: format!("unblocked {ip}") },
            Ok(_) => no(format!("{ip} is not blocked")),
            Err(e) => no(e),
        },
        (_, Some(ip)) => {
            let m = cfg.minutes;
            match run.powershell(&win_block_script(ip, now, now + i64::from(m) * 60)) {
                Ok(out) if out.contains("firewall-off") => Reply {
                    ok: true,
                    detail: format!(
                        "blocked {ip} for {m} minutes (Windows Firewall is off for some network profile, where this does nothing)"
                    ),
                },
                Ok(_) => Reply { ok: true, detail: format!("blocked {ip} for {m} minutes") },
                Err(e) => no(e),
            }
        }
        (_, None) => no("no address".into()),
    }
}

/// The helper's side of `firewall.*` (after the `[actions] allow` check).
pub fn handle(cfg: &BlockConfig, req_action: &str, target: &str, run: &mut dyn Runner) -> Reply {
    if cfg!(windows) && req_action == SWEEP {
        return handle_windows(cfg, req_action, None, now_secs(), run);
    }
    let Ok(ip) = target.parse::<IpAddr>().map(canonical) else {
        return Reply { ok: false, detail: format!("not an IP address: {target:?}") };
    };
    if cfg!(windows) {
        if req_action == action::FIREWALL_BLOCK {
            if let Some(why) = refuse(cfg, ip) {
                return Reply { ok: false, detail: why };
            }
        }
        return handle_windows(cfg, req_action, Some(ip), now_secs(), run);
    }
    let t = format!("inet {TABLE}");
    if req_action == action::FIREWALL_UNBLOCK {
        return match run.run("nft", &[&format!("delete element {t} {} {{ {ip} }}", set_for(ip))]) {
            Ok(()) => Reply { ok: true, detail: format!("unblocked {ip}") },
            Err(_) => Reply { ok: false, detail: format!("{ip} is not blocked") },
        };
    }
    if let Some(why) = refuse(cfg, ip) {
        return Reply { ok: false, detail: why };
    }
    if let Err(e) = run.run("nft", &[&ensure_script()]) {
        return Reply {
            ok: false,
            detail: if e.is_empty() { "nft failed (is nftables installed?)".into() } else { e },
        };
    }
    let m = cfg.minutes;
    match run.run("nft", &[&format!("add element {t} {} {{ {ip} timeout {m}m }}", set_for(ip))]) {
        Ok(()) => Reply { ok: true, detail: format!("blocked {ip} for {m} minutes") },
        Err(e) => Reply { ok: false, detail: e },
    }
}

/// What the node believes is blocked (the kernel holds the truth; entries
/// expire on their own there and here).
#[derive(Debug, Default)]
pub struct Blocks {
    pub active: BTreeMap<String, BlockEntry>,
}

impl Blocks {
    pub fn prune(&mut self, now: i64) {
        self.active.retain(|_, e| e.until > now);
    }

    pub fn list(&mut self, now: i64) -> Vec<BlockEntry> {
        self.prune(now);
        let mut v: Vec<BlockEntry> = self.active.values().cloned().collect();
        v.sort_by(|a, b| b.since.cmp(&a.since).then(a.addr.cmp(&b.addr)));
        v
    }
}

fn save(shared: &Shared) {
    let list = shared.blocks.lock().unwrap_or_else(|e| e.into_inner()).list(now_secs());
    let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = st.set_meta(META_BLOCKS, &serde_json::to_string(&list).unwrap_or_default()) {
        crate::warn!("could not save blocks: {e}");
    }
}

/// Loads the saved list at start (still-running blocks survive a restart).
pub fn load(shared: &Shared) {
    let saved: Vec<BlockEntry> = {
        let st = shared.store.lock().unwrap_or_else(|e| e.into_inner());
        st.meta(META_BLOCKS).ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
    };
    let mut b = shared.blocks.lock().unwrap_or_else(|e| e.into_inner());
    for e in saved {
        b.active.insert(e.addr.clone(), e);
    }
    b.prune(now_secs());
}

/// Notes the outcome of a block or unblock that went through `actions::run`.
pub fn note(shared: &Shared, req: &ActionRequest, ok: bool, source: &str) {
    if !ok || req.dry_run {
        return;
    }
    let Some(ip) = req.target.as_deref().and_then(|t| t.parse::<IpAddr>().ok()).map(canonical) else { return };
    let now = now_secs();
    {
        let mut b = shared.blocks.lock().unwrap_or_else(|e| e.into_inner());
        if req.action == action::FIREWALL_UNBLOCK {
            b.active.remove(&ip.to_string());
        } else {
            let until = now + i64::from(shared.config.block.minutes) * 60;
            b.active.insert(
                ip.to_string(),
                BlockEntry { addr: ip.to_string(), since: now, until, source: source.to_owned() },
            );
        }
    }
    save(shared);
}

/// Handle to the auto-block thread, which the login watcher feeds with
/// addresses that crossed `[block] threshold`.
pub struct AutoBlocker {
    tx: mpsc::Sender<String>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AutoBlocker {
    pub fn start(shared: Arc<Shared>) -> std::io::Result<(Self, mpsc::Sender<String>)> {
        let (tx, rx) = mpsc::channel::<String>();
        let t2 = tx.clone();
        let thread = std::thread::Builder::new().name("autoblock".into()).spawn(move || run(&shared, &rx))?;
        Ok((Self { tx, thread: Some(thread) }, t2))
    }

    pub fn stop(mut self) {
        // An empty address is the stop signal.
        let _ = self.tx.send(String::new());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Windows: asks the helper to remove the rules past their time (not an
/// action; nothing is logged unless it fails).
pub fn sweep(shared: &Shared) {
    let req = crate::helper::HelperRequest { action: SWEEP.into(), ..Default::default() };
    match crate::helper::call(&shared.helper_socket, &req) {
        Ok(r) if r.ok => crate::debug!("{}", r.detail),
        Ok(r) => crate::warn!("removing expired blocks: {}", r.detail),
        Err(e) => crate::warn!("removing expired blocks: {e}"),
    }
}

/// Whether the block thread should run: auto-block on Linux, and on
/// Windows also whenever blocks can be made, to remove them on time.
pub fn thread_wanted(shared: &Shared) -> bool {
    let allowed = shared.config.actions.allow.iter().any(|a| a == action::FIREWALL_BLOCK)
        && crate::helper::installed(&shared.helper_socket);
    (auto_enabled(shared) && shared.config.logins.enabled) || (cfg!(windows) && allowed)
}

/// Auto-block is on and can work: root turned it on, allowed the action and
/// installed the helper.
pub fn auto_enabled(shared: &Shared) -> bool {
    shared.config.block.auto
        && shared.config.actions.allow.iter().any(|a| a == action::FIREWALL_BLOCK)
        && crate::helper::installed(&shared.helper_socket)
}

/// Whether an address should be auto-blocked now (the node's side; the
/// helper checks root's limits again).
pub fn should_block(shared: &Shared, addr: &str, now: i64) -> Result<(), String> {
    let cfg = &shared.config.block;
    let Ok(ip) = addr.parse::<IpAddr>().map(canonical) else { return Err("not an address".into()) };
    if let Some(why) = refuse(cfg, ip) {
        return Err(why);
    }
    if shared.logins.lock().unwrap_or_else(|e| e.into_inner()).trusted(&ip.to_string(), now) {
        return Err(format!("{ip} logged in successfully in the last day"));
    }
    let mut b = shared.blocks.lock().unwrap_or_else(|e| e.into_inner());
    b.prune(now);
    if b.active.contains_key(&ip.to_string()) {
        return Err("already blocked".into());
    }
    if b.active.len() >= cfg.max_active as usize {
        return Err(format!("{} addresses are blocked already ([block] max_active)", b.active.len()));
    }
    Ok(())
}

fn run(shared: &Shared, rx: &mpsc::Receiver<String>) {
    if cfg!(windows) {
        // Rules that ran out while the node was stopped.
        sweep(shared);
    }
    loop {
        let addr = match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(a) if a.is_empty() => return,
            Ok(a) => a,
            Err(RecvTimeoutError::Timeout) => {
                let expired = {
                    let mut b = shared.blocks.lock().unwrap_or_else(|e| e.into_inner());
                    let before = b.active.len();
                    b.prune(now_secs());
                    before - b.active.len()
                };
                if cfg!(windows) && expired > 0 {
                    sweep(shared);
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if !shared.config.block.auto {
            continue;
        }
        let now = now_secs();
        if let Err(why) = should_block(shared, &addr, now) {
            crate::debug!("not auto-blocking {addr}: {why}");
            continue;
        }
        let req = ActionRequest { action: action::FIREWALL_BLOCK.into(), target: Some(addr), dry_run: false };
        crate::actions::run(shared, &req, "auto-block");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[derive(Default)]
    struct Fake {
        log: Vec<String>,
        fail: bool,
    }

    #[cfg(unix)]
    impl Runner for Fake {
        fn run(&mut self, prog: &str, args: &[&str]) -> Result<(), String> {
            self.log.push(format!("{prog} {}", args.join(" ")));
            if self.fail {
                Err("Error: Could not process rule: No such file or directory".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn never_blocks_local_private_vpn_or_listed() {
        let mut cfg = BlockConfig::default();
        for a in [
            "127.0.0.1",
            "10.1.2.3",
            "172.20.0.5",
            "192.168.1.9",
            "100.101.102.103",
            "::1",
            "fd7a:115c:a1e0::1",
            "fe80::1",
            "169.254.1.1",
            "::ffff:192.168.1.9",
            "0.0.0.0",
            "224.0.0.251",
        ] {
            assert!(refuse(&cfg, a.parse().unwrap()).is_some(), "{a}");
        }
        for a in ["203.0.113.7", "2001:db8::7", "::ffff:198.51.100.1", "172.32.0.1"] {
            assert!(refuse(&cfg, a.parse().unwrap()).is_none(), "{a}");
        }
        cfg.never = vec!["203.0.113.0/24".into(), "2001:db8::/32".into(), "bogus".into()];
        assert!(refuse(&cfg, "203.0.113.7".parse().unwrap()).unwrap().contains("[block] never"));
        assert!(refuse(&cfg, "2001:db8:1::1".parse().unwrap()).is_some());
        assert!(refuse(&cfg, "203.0.114.1".parse().unwrap()).is_none());
        cfg.private = true;
        assert!(refuse(&cfg, "10.1.2.3".parse().unwrap()).is_none());
    }

    #[test]
    #[cfg(unix)] // Windows takes the firewall-rule path (tested below).
    fn helper_runs_fixed_nft_commands() {
        let cfg = BlockConfig { minutes: 30, ..Default::default() };
        let mut f = Fake::default();
        let r = handle(&cfg, action::FIREWALL_BLOCK, "203.0.113.7", &mut f);
        assert_eq!((r.ok, r.detail.as_str()), (true, "blocked 203.0.113.7 for 30 minutes"));
        assert_eq!(f.log.len(), 2);
        assert!(f.log[0].starts_with("nft add table inet nightgrid_node ;") && f.log[0].contains("hook forward"));
        assert_eq!(f.log[1], "nft add element inet nightgrid_node v4 { 203.0.113.7 timeout 30m }");
        let r = handle(&cfg, action::FIREWALL_BLOCK, "::ffff:203.0.113.8", &mut f);
        assert!(r.ok && f.log[3].contains("v4 { 203.0.113.8 timeout"), "{:?}", f.log);
        let r = handle(&cfg, action::FIREWALL_BLOCK, "2001:db8::7", &mut f);
        assert!(r.ok && f.log[5] == "nft add element inet nightgrid_node v6 { 2001:db8::7 timeout 30m }");
        // Nothing that isn't an address reaches nft.
        let n = f.log.len();
        for bad in ["1.2.3.4 } ; flush ruleset", "", "example.com", "10.0.0.1"] {
            assert!(!handle(&cfg, action::FIREWALL_BLOCK, bad, &mut f).ok, "{bad}");
        }
        assert_eq!(f.log.len(), n);
        let r = handle(&cfg, action::FIREWALL_UNBLOCK, "203.0.113.7", &mut f);
        assert_eq!(
            (r.ok, f.log.last().unwrap().as_str()),
            (true, "nft delete element inet nightgrid_node v4 { 203.0.113.7 }")
        );
        f.fail = true;
        assert_eq!(handle(&cfg, action::FIREWALL_UNBLOCK, "203.0.113.7", &mut f).detail, "203.0.113.7 is not blocked");
        assert!(!handle(&cfg, action::FIREWALL_BLOCK, "203.0.113.9", &mut f).ok);
    }

    #[derive(Default)]
    struct Ps(Vec<String>, String);

    impl Runner for Ps {
        fn run(&mut self, _: &str, _: &[&str]) -> Result<(), String> {
            Err("no programs".into())
        }
        fn powershell(&mut self, script: &str) -> Result<String, String> {
            self.0.push(script.to_owned());
            Ok(self.1.clone())
        }
    }

    #[test]
    fn windows_rules_carry_their_end_and_old_ones_go() {
        let cfg = BlockConfig::default();
        let ip: IpAddr = "203.0.113.9".parse().unwrap();
        let s = win_block_script(ip, 1000, 4600);
        assert!(s.contains("$n.Name = 'Nightgrid Node block 203.0.113.9'; $n.Description = 'until 4600'"));
        assert!(s.contains("$n.RemoteAddresses = '203.0.113.9'") && s.contains("$n.Direction = 1; $n.Action = 0"));
        assert!(
            s.contains("[int64]$Matches[1] -le 1000")
                && s.contains("$r.Name -eq 'Nightgrid Node block 203.0.113.9' -or")
        );
        assert!(win_unblock_script(ip, 5).contains("$fw.Rules.Remove('Nightgrid Node block 203.0.113.9')"));
        assert!(win_sweep_script(7).ends_with("$gone\n") && win_remove_all_script().contains(WIN_GROUP));

        let mut ps = Ps::default();
        let r = handle_windows(&cfg, action::FIREWALL_BLOCK, Some(ip), 1000, &mut ps);
        assert!(r.ok && r.detail == format!("blocked 203.0.113.9 for {} minutes", cfg.minutes), "{}", r.detail);
        let r = handle_windows(&cfg, action::FIREWALL_UNBLOCK, Some(ip), 1000, &mut ps);
        assert!(!r.ok && r.detail.contains("is not blocked"));
        ps.1 = "removed\r\n".into();
        assert!(handle_windows(&cfg, action::FIREWALL_UNBLOCK, Some(ip), 1000, &mut ps).ok);
        ps.1 = "firewall-off\r\n".into();
        assert!(handle_windows(&cfg, action::FIREWALL_BLOCK, Some(ip), 1000, &mut ps)
            .detail
            .contains("Firewall is off"));
        ps.1 = "2\r\n".into();
        assert_eq!(handle_windows(&cfg, SWEEP, None, 1000, &mut ps).detail, "removed 2 expired blocks");
    }

    #[test]
    fn list_expires_entries() {
        let mut b = Blocks::default();
        let e = |a: &str, since, until| BlockEntry { addr: a.into(), since, until, source: "auto-block".into() };
        b.active.insert("a".into(), e("a", 10, 100));
        b.active.insert("b".into(), e("b", 20, 300));
        assert_eq!(b.list(50).iter().map(|e| e.addr.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(b.list(100).len(), 1);
    }
}
