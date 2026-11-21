//! The check runner on Windows (decision N-091): what
//! `nightgrid-node-checks.service` is on Linux.
//!
//! `allow checks` installs the service `nightgrid-node-checker`, which runs
//! `nightgrid-node checker` as its own virtual account
//! `NT SERVICE\nightgrid-node-checker`: no administrator, only
//! `SeChangeNotifyPrivilege`, a write-restricted service SID, auto start.
//! It reads the config (read-only), asks the node for the app's checks over
//! the pipe, runs them and reports back; the node takes `checks.report` only
//! from that SID.
//!
//! Its network is a Windows Service Hardening restriction: all traffic is
//! blocked except outbound TCP to `[checks] networks`, enforced by Windows
//! even with the firewall profiles off (the Linux unit's `IPAddressDeny=any`
//! plus `IPAddressAllow=`). Name lookups go through the DNS Client service,
//! not the runner. The main service stays without any network.

use std::path::Path;
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
use crate::install::Report;

/// Service name, also its virtual account (`NT SERVICE\nightgrid-node-checker`).
pub const SERVICE: &str = "nightgrid-node-checker";
const DISPLAY: &str = "Nightgrid Node check runner";
/// Group of its firewall rules (shown in wf.msc).
pub const FIREWALL_GROUP: &str = "Nightgrid Node check runner";
/// `FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE`.
const MODIFY: &str = "0x1301bf";

/// Its own log folder, `data\log\checker`: the only place it may write.
pub fn log_dir() -> std::path::PathBuf {
    super::data_dir().join("log").join("checker")
}

/// The log folder's DACL: SYSTEM and Administrators, the node (to read it)
/// and the runner (to write and rotate its file).
pub fn log_sddl(node_sid: &str, runner_sid: &str) -> String {
    format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FR;;;{node_sid})(A;OICI;{MODIFY};;;{runner_sid})")
}

pub fn service_command(bin: &Path, config: &Path) -> String {
    format!("\"{}\" checker --config \"{}\"", bin.display(), config.display())
}

pub fn installed() -> bool {
    scm::status(SERVICE).ok().flatten().is_some()
}

/// The `[checks] networks` entries Windows Firewall accepts, normalised.
pub fn firewall_networks(networks: &[String]) -> Vec<String> {
    networks
        .iter()
        .filter_map(|n| crate::firewall::parse_cidr(n.trim()).map(|(ip, bits)| format!("{ip}/{bits}")))
        .collect()
}

/// The runner's Service Hardening rule: everything blocked except outbound
/// TCP to `networks`; plus two visible rules saying the same.
pub fn firewall_script(on: bool, exe: &str, networks: &[String]) -> String {
    let exe = exe.replace('\'', "''");
    let wsh_name = format!("{FIREWALL_GROUP}: out to [checks] networks");
    if !on {
        return format!(
            "$ErrorActionPreference='Continue'\n\
             $sr = (New-Object -ComObject HNetCfg.FwPolicy2).ServiceRestriction\n\
             try {{ $sr.Rules.Remove('{wsh_name}') }} catch {{}}\n\
             try {{ $sr.RestrictService('{SERVICE}', '{exe}', $false, $false) }} catch {{}}\n\
             Get-NetFirewallRule -Group '{FIREWALL_GROUP}' -ErrorAction SilentlyContinue | Remove-NetFirewallRule\n\
             exit 0\n"
        );
    }
    let nets = firewall_networks(networks).join(",");
    format!(
        "$ErrorActionPreference='Stop'\n\
         $sr = (New-Object -ComObject HNetCfg.FwPolicy2).ServiceRestriction\n\
         $sr.RestrictService('{SERVICE}', '{exe}', $true, $false)\n\
         if (-not $sr.ServiceRestricted('{SERVICE}', '{exe}')) {{\n\
           $sr.RestrictService('{SERVICE}', '{exe}', $false, $false)\n\
           $sr.RestrictService('{SERVICE}', '{exe}', $true, $false)\n\
         }}\n\
         if (-not $sr.ServiceRestricted('{SERVICE}', '{exe}')) {{ throw 'not restricted' }}\n\
         try {{ $sr.Rules.Remove('{wsh_name}') }} catch {{}}\n\
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
         if ($nets) {{\n\
           $r = New-Object -ComObject HNetCfg.FWRule\n\
           $r.Name = '{wsh_name}'\n\
           $r.ApplicationName = '{exe}'\n\
           $r.ServiceName = '{SERVICE}'\n\
           $r.Protocol = 6\n\
           $r.Direction = 2\n\
           $r.RemoteAddresses = $nets\n\
           $r.Action = 1\n\
           $r.Enabled = $true\n\
           $sr.Rules.Add($r)\n\
         }}\n\
         Get-NetFirewallRule -Group '{FIREWALL_GROUP}' -ErrorAction SilentlyContinue | Remove-NetFirewallRule\n\
         New-NetFirewallRule -DisplayName '{FIREWALL_GROUP}: nothing inbound' -Group '{FIREWALL_GROUP}' -Direction Inbound -Service '{SERVICE}' -Action Block -Profile Any | Out-Null\n\
         if ($nets) {{\n\
           New-NetFirewallRule -DisplayName '{FIREWALL_GROUP}: out to [checks] networks' -Group '{FIREWALL_GROUP}' -Direction Outbound -Service '{SERVICE}' -Protocol TCP -RemoteAddress $ok -Action Allow -Profile Any | Out-Null\n\
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

/// True when the runner's restriction is in place (for `doctor`).
pub fn restricted() -> Option<bool> {
    let exe = super::installed_binary().to_string_lossy().replace('\'', "''");
    let out = super::install::powershell(&format!(
        "(New-Object -ComObject HNetCfg.FwPolicy2).ServiceRestriction.ServiceRestricted('{SERVICE}', '{exe}')\n"
    ))
    .ok()?;
    Some(out.trim().eq_ignore_ascii_case("true"))
}

fn step(r: &mut Report, s: impl Into<String>) {
    r.steps.push(s.into());
}

/// `allow checks` / `deny checks`.
pub fn allow(on: bool, config_path: &Path, no_start: bool) -> Result<Report, String> {
    super::install::preflight()?;
    let mut rep = Report { host: None, ..super::install::report(if on { "allow" } else { "deny" }) };
    let bin = super::installed_binary();
    if !on {
        if installed() {
            remove(&mut rep);
        } else {
            step(&mut rep, "checks was not allowed");
        }
        return Ok(rep);
    }
    let config = crate::config::Config::load(config_path).map_err(|e| format!("config: {e}"))?;
    if !config.checks.enabled {
        return Err(format!("[checks] enabled is false in {}", config_path.display()));
    }
    let node = super::pipe::sid_of(&format!("NT SERVICE\\{}", super::SERVICE))
        .ok_or("the node's service isn't installed (run: nightgrid-node install)")?;
    if !bin.is_file() {
        return Err(format!("{} is missing (run: nightgrid-node install)", bin.display()));
    }
    let nets = firewall_networks(&config.checks.networks);
    if nets.is_empty() {
        return Err("[checks] networks has no usable address range".into());
    }

    let m = scm::manager(true)?;
    let created = scm::ensure(
        &m,
        &scm::Spec {
            name: SERVICE,
            display: DISPLAY,
            description: "Runs the Nightgrid app's service checks (HTTP(S), certificates, TCP) from this host and \
                          hands the results to the node. Outbound only, to [checks] networks.",
            command: service_command(&bin, config_path),
            account: Some(format!("NT SERVICE\\{SERVICE}")),
            auto_start: true,
            privileges: &["SeChangeNotifyPrivilege"],
            restricted: true,
            restart: true,
        },
    )?;
    drop(m);
    step(&mut rep, if created { "created the check runner service" } else { "updated the check runner service" });

    // The account exists now: its folders.
    let runner =
        super::pipe::sid_of(&format!("NT SERVICE\\{SERVICE}")).ok_or("the runner's account did not resolve")?;
    super::install::lock_etc()?;
    let log = log_dir();
    std::fs::create_dir_all(&log).map_err(|e| format!("{}: {e}", log.display()))?;
    super::install::set_dacl(&log, &log_sddl(&node, &runner))?;
    step(&mut rep, format!("the runner may read the config and write only {}", log.display()));

    let out = super::install::powershell(&firewall_script(true, &bin.to_string_lossy(), &config.checks.networks))
        .map_err(|e| format!("could not add the runner's firewall restriction: {e}"))?;
    let (allowed, refused) = parse_rule_output(&out);
    if allowed.is_empty() {
        rep.warnings.push("Windows took none of [checks] networks: the runner can't connect anywhere".into());
    } else {
        step(&mut rep, format!("the runner may connect only to {} (Windows Service Hardening)", allowed.join(", ")));
    }
    if !refused.is_empty() {
        rep.warnings.push(format!(
            "Windows Firewall refused {} in a rule, so the runner can't reach {}",
            refused.join(", "),
            if refused.len() == 1 { "it" } else { "them" }
        ));
    }

    if no_start {
        step(&mut rep, "enabled at boot, not started (--no-start)");
    } else {
        // A running runner keeps its old networks and interval until it starts again.
        scm::stop(SERVICE, Duration::from_secs(15))?;
        scm::start(SERVICE, Duration::from_secs(15))?;
        step(&mut rep, "started the check runner");
    }
    rep.warnings.push(
        "the check runner can now open connections to the networks in [checks] networks (private ranges by \
         default) to run the app's service checks. It runs as its own service account with no administrator \
         rights; the main service still has no network access. Run allow checks again after changing the \
         networks."
            .into(),
    );
    Ok(rep)
}

/// Takes the runner away (deny, uninstall): its rules while Windows can
/// still resolve its SID, then the service.
pub fn remove(rep: &mut Report) {
    let bin = super::installed_binary();
    if let Err(e) = super::install::powershell(&firewall_script(false, &bin.to_string_lossy(), &[])) {
        rep.warnings.push(format!("check runner firewall rules: {e}"));
    }
    let r = scm::stop(SERVICE, Duration::from_secs(15)).and_then(|()| scm::delete(SERVICE));
    match r {
        Ok(_) => step(rep, "removed the check runner service"),
        Err(e) => rep.warnings.push(format!("check runner: {e}")),
    }
}

/// Stops the runner before its exe is replaced; returns whether it ran.
pub fn stop() -> Result<bool, String> {
    let running = scm::status(SERVICE)?.is_some_and(|s| s.running());
    if running {
        scm::stop(SERVICE, Duration::from_secs(15))?;
    }
    Ok(running)
}

/// Starts the runner again if it is installed for auto start.
pub fn start_if_installed() -> Result<(), String> {
    if scm::auto_start(SERVICE)?.unwrap_or(false) {
        scm::start(SERVICE, Duration::from_secs(15))?;
    }
    Ok(())
}

// ---- running under the SCM ----

static STATUS: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static STOP: AtomicBool = AtomicBool::new(false);
static CONFIG: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);
static RESULT: OnceLock<Result<(), String>> = OnceLock::new();

fn set(state: u32, exit: u32, wait_ms: u32) {
    let h = STATUS.load(Ordering::SeqCst);
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
                // A round in progress has a 10 s timeout per check; past
                // 12 s the process just ends.
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
    let name = super::wide(SERVICE);
    // SAFETY: a static handler; the name lives for the call.
    let h = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(handler), std::ptr::null()) };
    if h.is_null() {
        return;
    }
    STATUS.store(h as *mut _, Ordering::SeqCst);
    set(SERVICE_START_PENDING, NO_ERROR, 10_000);
    let _ = crate::log::to_file(&log_dir().join("checker.log"));
    let config = CONFIG.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default();
    set(SERVICE_RUNNING, NO_ERROR, 0);
    crate::info!("check runner started");
    let r = crate::checks::checker_until(&config, &|| STOP.load(Ordering::SeqCst));
    if let Err(e) = &r {
        crate::error!("check runner: {e}");
    }
    let exit = if r.is_ok() { NO_ERROR } else { 1066 }; // ERROR_SERVICE_SPECIFIC_ERROR
    let _ = RESULT.set(r);
    set(SERVICE_STOPPED, exit, 0);
}

/// `nightgrid-node checker`: the runner service's command line. From a
/// console it runs the same loop until Ctrl+C (as whoever started it, so
/// the node only takes its reports from an elevated prompt or the service).
pub fn serve(config_path: &Path) -> Result<(), String> {
    *CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = Some(config_path.to_owned());
    let mut name = super::wide(SERVICE);
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
    crate::checks::checker(config_path)
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
        let s = firewall_script(true, r"C:\Program Files\it's\n.exe", &["192.168.0.0/16".into()]);
        assert!(s.contains("$nets = '192.168.0.0/16'"), "{s}");
        assert!(s.contains(r"'C:\Program Files\it''s\n.exe'"), "{s}");
        assert!(s.contains("$r.Direction = 2") && s.contains("$r.Action = 1"));
        assert!(!firewall_script(false, "x", &[]).contains("Rules.Add"));
        assert_eq!(
            parse_rule_output("noise\r\nallowed: 10.0.0.0/8,fc00::/7\r\nrefused: \r\n"),
            (vec!["10.0.0.0/8".to_owned(), "fc00::/7".to_owned()], vec![])
        );
        assert_eq!(parse_rule_output("allowed: \nrefused: ::1/128").1, ["::1/128"]);
        assert!(super::super::acl::Sddl::new(&log_sddl("S-1-5-80-1", "S-1-5-80-2")).is_ok());
        assert!(service_command(Path::new(r"C:\P\n.exe"), Path::new(r"C:\c.toml")).contains("\" checker --config \""));
    }
}
