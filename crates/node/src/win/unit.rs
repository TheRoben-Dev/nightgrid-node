//! The node's network units on Windows (decisions N-091, N-093): what the
//! extra systemd units with `IPAddressAllow=` are on Linux.
//!
//! - `allow checks` installs the check runner, `nightgrid-node-checker`
//!   ([`CHECKER`]): outbound TCP to `[checks] networks`.
//! - `allow peers` installs the node-to-node link, `nightgrid-node-peer`
//!   ([`PEER`]): inbound on its listen port and outbound, both only with
//!   `[peers] networks`; its TLS key in a folder only it can read.
//! - `allow statuspage` installs the status page,
//!   `nightgrid-node-status-page` ([`STATUS`]): inbound only, on its port
//!   from `[statuspage] networks`; its secret link in a folder only it can
//!   read.
//!
//! Each runs the same exe as its own virtual account
//! (`NT SERVICE\nightgrid-node-<key>`): no administrator, only
//! `SeChangeNotifyPrivilege`, a write-restricted service SID, auto start.
//! It reads the config, talks to the node over the pipe (which names its
//! SID ahead of time, see [`crate::compat::service_sid`]), and the node
//! takes its internal reports only from that SID.
//!
//! Its network is a Windows Service Hardening restriction: everything is
//! blocked except the allow rules here, enforced by Windows even with the
//! firewall profiles off. Visible rules in the group `Nightgrid Node
//! <label>` say the same in wf.msc (and open the listen port, which
//! the firewall would otherwise block). The main service stays offline.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows_sys::Win32::Foundation::{ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, NO_ERROR};
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, SERVICE_ACCEPT_SHUTDOWN,
    SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};

use super::scm;
use crate::config::Config;
use crate::install::Report;

/// One network unit.
pub struct Unit {
    /// Service name, also its virtual account.
    pub service: &'static str,
    /// `nightgrid-node <key>` runs it; `compat::is_unit(uid, key)` knows it.
    pub key: &'static str,
    /// The `allow` word.
    pub grant: &'static str,
    /// What it is called in messages.
    pub label: &'static str,
    pub display: &'static str,
    pub description: &'static str,
    /// The warning `allow` prints.
    pub warning: &'static str,
    /// It keeps a private folder `data\<key>` (the link's TLS key).
    pub state: bool,
    /// `deny` keeps that folder (the link's pairings survive; the status
    /// page's link must die).
    pub keep_state: bool,
    /// It may open connections (the status page only answers).
    pub outbound: bool,
    /// The networks it may talk to.
    pub networks: fn(&Config) -> Vec<String>,
    /// The TCP port it listens on, if any.
    pub listen_port: fn(&Config) -> Option<u16>,
    /// Why `allow` refuses with this config.
    pub check: fn(&Config) -> Result<(), String>,
    /// The loop, until the stop flag says so.
    pub run: fn(&Path, &dyn Fn() -> bool) -> Result<(), String>,
    /// Extra `allow` work once its private folder exists.
    pub prepare: fn(&Unit, &Config, &mut Report) -> Result<(), String>,
}

fn nothing(_: &Unit, _: &Config, _: &mut Report) -> Result<(), String> {
    Ok(())
}

pub static CHECKER: Unit = Unit {
    service: "nightgrid-node-checker",
    key: "checker",
    grant: "checks",
    label: "check runner",
    display: "Nightgrid Node check runner",
    description: "Runs the Nightgrid app's service checks (HTTP(S), certificates, TCP) and reads NUT servers \
                  from this host and hands the results to the node. Outbound only, to [checks] networks.",
    warning: "the check runner can now open connections to the networks in [checks] networks (private ranges by \
              default) to run the app's service checks. It runs as its own service account with no administrator \
              rights; the main service still has no network access. Run allow checks again after changing the \
              networks.",
    state: false,
    keep_state: true,
    outbound: true,
    networks: |c| c.checks.networks.clone(),
    listen_port: |_| None,
    check: |c| if c.checks.enabled { Ok(()) } else { Err("[checks] enabled is false".into()) },
    run: crate::checks::checker_until,
    prepare: nothing,
};

pub static PEER: Unit = Unit {
    service: "nightgrid-node-peer",
    key: "peer",
    grant: "peers",
    label: "node-to-node link",
    display: "Nightgrid Node link",
    description: "The Nightgrid node-to-node link: mutual TLS with the nodes the app paired, only with [peers] \
                  networks. Holds its own key; the node itself stays offline.",
    warning: "the node-to-node link now listens on [peers] listen (the tailnet address by default) and talks to \
              the nodes the app pairs with this one, over mutual TLS with pinned certificates. Nothing is \
              trusted until the app pairs a node. It may reach only [peers] networks and answers only what \
              [peers] serve allows; the main service still has no network access. Run allow peers again \
              after changing the networks or the port.",
    state: true,
    keep_state: true,
    outbound: true,
    networks: |c| c.peers.networks.clone(),
    listen_port: peer_port,
    check: |_| Ok(()),
    run: crate::peers::link_until,
    prepare: nothing,
};

pub static STATUS: Unit = Unit {
    service: "nightgrid-node-status-page",
    key: "status-page",
    grant: "statuspage",
    label: "status page",
    display: "Nightgrid Node status page",
    description: "A read-only status page about this host at a secret link, for a phone on the tailnet. Answers \
                  only [statuspage] networks on its port and opens no connections.",
    warning: "a read-only status page now listens on [statuspage] listen (the tailnet address by default) for \
              [statuspage] networks. It is plain HTTP inside the tailnet's encryption, at a secret link: anyone \
              with the link and a tailnet address can read this host's summary. Run deny statuspage to remove \
              it and the link.",
    state: true,
    keep_state: false,
    outbound: false,
    networks: |c| c.statuspage.networks.clone(),
    listen_port: status_port,
    check: |_| Ok(()),
    run: crate::statuspage::serve_until,
    prepare: status_link,
};

pub static ALL: [&Unit; 3] = [&CHECKER, &PEER, &STATUS];

/// The page's port: `listen = "ip:port"` carries its own, "tailnet" uses `port`.
fn status_port(c: &Config) -> Option<u16> {
    match c.statuspage.listen.as_str() {
        "tailnet" => Some(c.statuspage.port),
        a => a.parse::<std::net::SocketAddr>().ok().map(|s| s.port()),
    }
}

/// Keeps the page's token (or makes one) in its private folder and puts the
/// link in the report.
fn status_link(_: &Unit, cfg: &Config, rep: &mut Report) -> Result<(), String> {
    let path = crate::statuspage::token_path();
    let old =
        std::fs::read_to_string(&path).ok().map(|t| t.trim().to_owned()).filter(|t| crate::statuspage::token_ok(t));
    let token = match old {
        Some(t) => {
            step(rep, format!("kept the link in {}", path.display()));
            t
        }
        None => {
            let t = crate::statuspage::new_token().map_err(|e| format!("random token: {e}"))?;
            std::fs::write(&path, format!("{t}\n")).map_err(|e| format!("{}: {e}", path.display()))?;
            step(rep, format!("wrote a new link to {}, which only the status page can read", path.display()));
            t
        }
    };
    rep.url = Some(crate::statuspage::url(cfg, &token));
    Ok(())
}

/// The link's port: `listen = "ip:port"` carries its own, "tailnet" and
/// "lan" use `port`, "" listens nowhere.
fn peer_port(c: &Config) -> Option<u16> {
    match c.peers.listen.as_str() {
        "" => None,
        "tailnet" | "lan" => Some(c.peers.port),
        a => a.parse::<std::net::SocketAddr>().ok().map(|s| s.port()),
    }
}

/// `FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE`.
const MODIFY: &str = "0x1301bf";

impl Unit {
    pub fn by_grant(g: &str) -> Option<&'static Unit> {
        ALL.iter().copied().find(|u| u.grant == g)
    }

    pub fn by_key(k: &str) -> Option<&'static Unit> {
        ALL.iter().copied().find(|u| u.key == k)
    }

    pub fn account(&self) -> String {
        format!("NT SERVICE\\{}", self.service)
    }

    pub fn sid(&self) -> String {
        crate::compat::service_sid(self.service)
    }

    pub fn firewall_group(&self) -> String {
        format!("Nightgrid Node {}", self.label)
    }

    /// Its log folder, `data\log\<key>`.
    pub fn log_dir(&self) -> PathBuf {
        super::data_dir().join("log").join(self.key)
    }

    /// Its private folder, `data\<key>` (only it, SYSTEM and Administrators).
    pub fn state_dir(&self) -> PathBuf {
        super::data_dir().join(self.key)
    }

    pub fn installed(&self) -> bool {
        scm::status(self.service).ok().flatten().is_some()
    }

    pub fn command(&self, bin: &Path, config: &Path) -> String {
        format!("\"{}\" {} --config \"{}\"", bin.display(), self.key, config.display())
    }
}

/// The log folder's DACL: SYSTEM and Administrators, the node (to read it)
/// and the unit (to write and rotate its file).
pub fn log_sddl(node_sid: &str, unit_sid: &str) -> String {
    format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FR;;;{node_sid})(A;OICI;{MODIFY};;;{unit_sid})")
}

/// The private folder's DACL: the node can't read the link's key.
pub fn state_sddl(unit_sid: &str) -> String {
    format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;{MODIFY};;;{unit_sid})")
}

/// The networks Windows Firewall accepts, normalised.
pub fn firewall_networks(networks: &[String]) -> Vec<String> {
    networks
        .iter()
        .filter_map(|n| crate::firewall::parse_cidr(n.trim()).map(|(ip, bits)| format!("{ip}/{bits}")))
        .collect()
}

/// The unit's Service Hardening rules: everything blocked except outbound
/// TCP to `networks` and, with a port, inbound TCP on it from `networks`;
/// plus visible rules saying the same.
pub fn firewall_script(u: &Unit, on: bool, exe: &str, networks: &[String], port: Option<u16>) -> String {
    let exe = exe.replace('\'', "''");
    let svc = u.service;
    let group = u.firewall_group();
    let out_name = format!("{group}: out");
    let in_name = format!("{group}: in");
    if !on {
        return format!(
            "$ErrorActionPreference='Continue'\n\
             $sr = (New-Object -ComObject HNetCfg.FwPolicy2).ServiceRestriction\n\
             foreach ($n in '{out_name}', '{in_name}') {{ try {{ $sr.Rules.Remove($n) }} catch {{}} }}\n\
             try {{ $sr.RestrictService('{svc}', '{exe}', $false, $false) }} catch {{}}\n\
             Get-NetFirewallRule -Group '{group}' -ErrorAction SilentlyContinue | Remove-NetFirewallRule\n\
             exit 0\n"
        );
    }
    let nets = firewall_networks(networks).join(",");
    let port = port.map_or(String::new(), |p| p.to_string());
    let outbound = if u.outbound { "true" } else { "false" };
    format!(
        "$ErrorActionPreference='Stop'\n\
         $sr = (New-Object -ComObject HNetCfg.FwPolicy2).ServiceRestriction\n\
         $sr.RestrictService('{svc}', '{exe}', $true, $false)\n\
         if (-not $sr.ServiceRestricted('{svc}', '{exe}')) {{\n\
           $sr.RestrictService('{svc}', '{exe}', $false, $false)\n\
           $sr.RestrictService('{svc}', '{exe}', $true, $false)\n\
         }}\n\
         if (-not $sr.ServiceRestricted('{svc}', '{exe}')) {{ throw 'not restricted' }}\n\
         foreach ($n in '{out_name}', '{in_name}') {{ try {{ $sr.Rules.Remove($n) }} catch {{}} }}\n\
         # Windows refuses some ranges in a rule (it says which only by\n\
         # failing): keep the ones it takes and name the rest.\n\
         $ok = @(); $bad = @()\n\
         foreach ($n in ('{nets}' -split ',')) {{\n\
           if (-not $n) {{ continue }}\n\
           $t = New-Object -ComObject HNetCfg.FWRule\n\
           $t.Protocol = 6\n\
           $t.Direction = 2\n\
           try {{ $t.RemoteAddresses = $n; $ok += $n }} catch {{ $bad += $n }}\n\
         }}\n\
         $nets = $ok -join ','\n\
         $port = '{port}'\n\
         function Wsh($name, $dir) {{\n\
           $r = New-Object -ComObject HNetCfg.FWRule\n\
           $r.Name = $name\n\
           $r.ApplicationName = '{exe}'\n\
           $r.ServiceName = '{svc}'\n\
           $r.Protocol = 6\n\
           $r.Direction = $dir\n\
           if ($dir -eq 1) {{ $r.LocalPorts = $port }}\n\
           $r.RemoteAddresses = $nets\n\
           $r.Action = 1\n\
           $r.Enabled = $true\n\
           $sr.Rules.Add($r)\n\
         }}\n\
         $outbound = ${outbound}\n\
         if ($nets) {{\n\
           if ($outbound) {{ Wsh '{out_name}' 2 }}\n\
           if ($port) {{ Wsh '{in_name}' 1 }}\n\
         }}\n\
         Get-NetFirewallRule -Group '{group}' -ErrorAction SilentlyContinue | Remove-NetFirewallRule\n\
         if ($nets -and $port) {{\n\
           New-NetFirewallRule -DisplayName '{group}: in on port {port}' -Group '{group}' -Direction Inbound -Service '{svc}' -Protocol TCP -LocalPort $port -RemoteAddress $ok -Action Allow -Profile Any | Out-Null\n\
         }} else {{\n\
           New-NetFirewallRule -DisplayName '{group}: nothing inbound' -Group '{group}' -Direction Inbound -Service '{svc}' -Action Block -Profile Any | Out-Null\n\
         }}\n\
         if ($nets -and $outbound) {{\n\
           New-NetFirewallRule -DisplayName '{group}: out' -Group '{group}' -Direction Outbound -Service '{svc}' -Protocol TCP -RemoteAddress $ok -Action Allow -Profile Any | Out-Null\n\
         }}\n\
         \"allowed: $nets\"\n\
         \"refused: $($bad -join ',')\"\n"
    )
}

/// The `allowed:` and `refused:` lines the rule script prints.
pub fn parse_rule_output(out: &str) -> (Vec<String>, Vec<String>) {
    let list = |key: &str| -> Vec<String> {
        out.lines()
            .filter_map(|l| l.trim().strip_prefix(key))
            .next_back()
            .map(|v| v.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned).collect())
            .unwrap_or_default()
    };
    (list("allowed:"), list("refused:"))
}

/// True when the unit's restriction is in place (for `doctor`).
pub fn restricted(u: &Unit) -> Option<bool> {
    let exe = super::installed_binary().to_string_lossy().replace('\'', "''");
    let out = super::install::powershell(&format!(
        "(New-Object -ComObject HNetCfg.FwPolicy2).ServiceRestriction.ServiceRestricted('{}', '{exe}')\n",
        u.service
    ))
    .ok()?;
    Some(out.trim().eq_ignore_ascii_case("true"))
}

fn step(r: &mut Report, s: impl Into<String>) {
    r.steps.push(s.into());
}

/// `allow checks|peers` / `deny checks|peers`.
pub fn allow(u: &Unit, on: bool, config_path: &Path, no_start: bool) -> Result<Report, String> {
    super::install::preflight()?;
    let mut rep = Report { host: None, ..super::install::report(if on { "allow" } else { "deny" }) };
    let bin = super::installed_binary();
    if !on {
        if u.installed() {
            remove(u, &mut rep);
        } else {
            step(&mut rep, format!("{} was not allowed", u.grant));
        }
        return Ok(rep);
    }
    let config = Config::load(config_path).map_err(|e| format!("config: {e}"))?;
    (u.check)(&config).map_err(|e| format!("{e} in {}", config_path.display()))?;
    let node = super::pipe::sid_of(&format!("NT SERVICE\\{}", super::SERVICE))
        .ok_or("the node's service isn't installed (run: nightgrid-node install)")?;
    if !bin.is_file() {
        return Err(format!("{} is missing (run: nightgrid-node install)", bin.display()));
    }
    let networks = (u.networks)(&config);
    if firewall_networks(&networks).is_empty() {
        return Err(format!("[{}] networks has no usable address range", u.grant));
    }
    let port = (u.listen_port)(&config);

    let m = scm::manager(true)?;
    let created = scm::ensure(
        &m,
        &scm::Spec {
            name: u.service,
            display: u.display,
            description: u.description,
            command: u.command(&bin, config_path),
            account: Some(u.account()),
            auto_start: true,
            privileges: &["SeChangeNotifyPrivilege"],
            restricted: true,
            restart: true,
        },
    )?;
    drop(m);
    step(&mut rep, format!("{} the {} service", if created { "created" } else { "updated" }, u.label));

    // The account exists now: its folders.
    let me = super::pipe::sid_of(&u.account()).ok_or_else(|| format!("the {}'s account did not resolve", u.label))?;
    super::install::lock_etc()?;
    let log = u.log_dir();
    std::fs::create_dir_all(&log).map_err(|e| format!("{}: {e}", log.display()))?;
    super::install::set_dacl(&log, &log_sddl(&node, &me))?;
    if u.state {
        let st = u.state_dir();
        std::fs::create_dir_all(&st).map_err(|e| format!("{}: {e}", st.display()))?;
        super::install::set_dacl(&st, &state_sddl(&me))?;
        step(&mut rep, format!("the {} keeps its secrets in {}, which the node can't read", u.label, st.display()));
    }
    (u.prepare)(u, &config, &mut rep)?;
    step(&mut rep, format!("the {} may read the config and write only its own folders", u.label));

    let out = super::install::powershell(&firewall_script(u, true, &bin.to_string_lossy(), &networks, port))
        .map_err(|e| format!("could not add the {}'s firewall restriction: {e}", u.label))?;
    let (allowed, refused) = parse_rule_output(&out);
    if allowed.is_empty() {
        rep.warnings
            .push(format!("Windows took none of [{}] networks: the {} can't connect anywhere", u.grant, u.label));
    } else {
        let how = match (port, u.outbound) {
            (Some(p), true) => format!("may connect to and be reached on port {p} from only"),
            (Some(p), false) => format!("opens no connections and may be reached on port {p} from only"),
            (None, _) => "may connect only to".into(),
        };
        step(&mut rep, format!("the {} {how} {} (Windows Service Hardening)", u.label, allowed.join(", ")));
    }
    if !refused.is_empty() {
        rep.warnings.push(format!(
            "Windows Firewall refused {} in a rule, so the {} can't reach {}",
            refused.join(", "),
            u.label,
            if refused.len() == 1 { "it" } else { "them" }
        ));
    }

    if no_start {
        step(&mut rep, "enabled at boot, not started (--no-start)");
    } else {
        // A running unit keeps its old settings until it starts again.
        scm::stop(u.service, Duration::from_secs(15))?;
        scm::start(u.service, Duration::from_secs(15))?;
        step(&mut rep, format!("started the {}", u.label));
    }
    rep.warnings.push(u.warning.into());
    Ok(rep)
}

/// Takes a unit away (deny, uninstall): its rules while Windows can still
/// resolve its SID, then the service. The link's key stays, as on Linux,
/// so pairings survive a deny and allow; `uninstall --purge` removes it.
/// The status page's folder goes, so its link dies.
pub fn remove(u: &Unit, rep: &mut Report) {
    let bin = super::installed_binary();
    if let Err(e) = super::install::powershell(&firewall_script(u, false, &bin.to_string_lossy(), &[], None)) {
        rep.warnings.push(format!("{} firewall rules: {e}", u.label));
    }
    let r = scm::stop(u.service, Duration::from_secs(15)).and_then(|()| scm::delete(u.service));
    match r {
        Ok(_) => step(rep, format!("removed the {} service", u.label)),
        Err(e) => rep.warnings.push(format!("{}: {e}", u.label)),
    }
    let st = u.state_dir();
    if u.state && !u.keep_state && st.exists() {
        match std::fs::remove_dir_all(&st) {
            Ok(()) => step(rep, format!("removed {} (the old link no longer works)", st.display())),
            Err(e) => rep.warnings.push(format!("{}: {e}", st.display())),
        }
    }
}

/// Removes every installed unit (uninstall).
pub fn remove_all(rep: &mut Report) {
    for u in ALL {
        if u.installed() {
            remove(u, rep);
        }
    }
}

/// Stops the running units before their exe is replaced.
pub fn stop_all() -> Result<(), String> {
    for u in ALL {
        if scm::status(u.service)?.is_some_and(|s| s.running()) {
            scm::stop(u.service, Duration::from_secs(15))?;
        }
    }
    Ok(())
}

/// Starts the installed units again (after an upgrade).
pub fn start_installed(rep: &mut Report) {
    for u in ALL {
        if scm::auto_start(u.service).ok().flatten().unwrap_or(false) {
            match scm::start(u.service, Duration::from_secs(15)) {
                Ok(()) => step(rep, format!("started the {} again", u.label)),
                Err(e) => rep.warnings.push(format!("{}: {e}", u.label)),
            }
        }
    }
}

// ---- running under the SCM ----

static HANDLE: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static STOP: AtomicBool = AtomicBool::new(false);
static CONFIG: Mutex<Option<PathBuf>> = Mutex::new(None);
static UNIT: OnceLock<&'static Unit> = OnceLock::new();
static RESULT: OnceLock<Result<(), String>> = OnceLock::new();

fn set(state: u32, exit: u32, wait_ms: u32) {
    let h = HANDLE.load(Ordering::SeqCst);
    if h.is_null() {
        return;
    }
    let accept = if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 };
    let st = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accept,
        dwWin32ExitCode: exit,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: wait_ms,
    };
    // SAFETY: a handle from RegisterServiceCtrlHandlerExW and a full struct.
    unsafe { SetServiceStatus(h as SERVICE_STATUS_HANDLE, &st) };
}

unsafe extern "system" fn handler(
    control: u32,
    _t: u32,
    _d: *mut core::ffi::c_void,
    _c: *mut core::ffi::c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            set(SERVICE_STOP_PENDING, NO_ERROR, 15_000);
            if !STOP.swap(true, Ordering::SeqCst) {
                // A round in progress has short timeouts; past 12 s the
                // process just ends.
                let _ = std::thread::Builder::new().name("stop".into()).spawn(|| {
                    std::thread::sleep(Duration::from_secs(12));
                    set(SERVICE_STOPPED, NO_ERROR, 0);
                    std::process::exit(0);
                });
            }
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => 120, // ERROR_CALL_NOT_IMPLEMENTED
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let Some(u) = UNIT.get().copied() else { return };
    let name = super::wide(u.service);
    // SAFETY: a static handler; the name lives for the call.
    let h = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(handler), std::ptr::null()) };
    if h.is_null() {
        return;
    }
    HANDLE.store(h as *mut _, Ordering::SeqCst);
    set(SERVICE_START_PENDING, NO_ERROR, 10_000);
    let _ = crate::log::to_file(&u.log_dir().join(format!("{}.log", u.key)));
    let config = CONFIG.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default();
    set(SERVICE_RUNNING, NO_ERROR, 0);
    crate::info!("{} started", u.label);
    let r = (u.run)(&config, &|| STOP.load(Ordering::SeqCst));
    if let Err(e) = &r {
        crate::error!("{}: {e}", u.label);
    }
    let exit = if r.is_ok() { NO_ERROR } else { 1066 }; // ERROR_SERVICE_SPECIFIC_ERROR
    let _ = RESULT.set(r);
    set(SERVICE_STOPPED, exit, 0);
}

/// `nightgrid-node checker` / `peer`: a unit service's command line. From a
/// console it runs the same loop until stopped (as whoever started it, so
/// the node takes its reports only from an elevated prompt or the service).
pub fn serve(u: &'static Unit, config_path: &Path) -> Result<(), String> {
    *CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = Some(config_path.to_owned());
    let _ = UNIT.set(u);
    let mut name = super::wide(u.service);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: std::ptr::null_mut(), lpServiceProc: None },
    ];
    // SAFETY: a NULL-terminated table whose strings outlive the call.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } != 0 {
        return RESULT.get().cloned().unwrap_or(Ok(()));
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT as i32) {
        return Err(format!("service dispatcher: {err}"));
    }
    (u.run)(config_path, &|| false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_and_acls() {
        assert_eq!(
            firewall_networks(&["10.0.0.0/8".into(), " fe80::/10".into(), "nope".into(), "1.2.3.4".into()]),
            ["10.0.0.0/8", "fe80::/10", "1.2.3.4/32"]
        );
        let s = firewall_script(&CHECKER, true, r"C:\Program Files\it's\n.exe", &["192.168.0.0/16".into()], None);
        assert!(s.contains("foreach ($n in ('192.168.0.0/16' -split ','))"), "{s}");
        assert!(s.contains(r"'C:\Program Files\it''s\n.exe'"), "{s}");
        assert!(s.contains("$port = ''") && s.contains("Wsh 'Nightgrid Node check runner: out' 2"), "{s}");
        let s = firewall_script(&PEER, true, "x", &["100.64.0.0/10".into()], Some(7419));
        assert!(s.contains("$port = '7419'") && s.contains("in on port 7419"), "{s}");
        assert!(!firewall_script(&PEER, false, "x", &[], None).contains("Rules.Add"));
        assert_eq!(
            parse_rule_output("noise\r\nallowed: 10.0.0.0/8,fc00::/7\r\nrefused: \r\n"),
            (vec!["10.0.0.0/8".to_owned(), "fc00::/7".to_owned()], vec![])
        );
        assert_eq!(parse_rule_output("allowed: \nrefused: ::1/128").1, ["::1/128"]);
        assert!(super::super::acl::Sddl::new(&log_sddl("S-1-5-80-1", "S-1-5-80-2")).is_ok());
        assert!(super::super::acl::Sddl::new(&state_sddl("S-1-5-80-2")).is_ok());
        assert!(CHECKER.command(Path::new(r"C:\P\n.exe"), Path::new(r"C:\c.toml")).contains("\" checker --config \""));
        assert_eq!(PEER.sid(), crate::compat::service_sid("nightgrid-node-peer"));
        assert_eq!(Unit::by_grant("peers").map(|u| u.key), Some("peer"));

        let mut c = Config::default();
        assert_eq!(peer_port(&c), Some(7419));
        c.peers.listen = "127.0.0.1:17419".into();
        assert_eq!(peer_port(&c), Some(17419));
        c.peers.listen = String::new();
        assert_eq!(peer_port(&c), None);

        assert_eq!(status_port(&c), Some(7420));
        c.statuspage.listen = "127.0.0.1:17420".into();
        assert_eq!(status_port(&c), Some(17420));
        let s = firewall_script(&STATUS, true, "x", &["100.64.0.0/10".into()], Some(17420));
        assert!(s.contains("$outbound = $false") && s.contains("in on port 17420"), "{s}");
        assert!(firewall_script(&CHECKER, true, "x", &["10.0.0.0/8".into()], None).contains("$outbound = $true"));
        assert_eq!(Unit::by_grant("statuspage").map(|u| u.service), Some("nightgrid-node-status-page"));
        assert!(crate::compat::is_unit(&STATUS.sid(), STATUS.key));
    }
}
