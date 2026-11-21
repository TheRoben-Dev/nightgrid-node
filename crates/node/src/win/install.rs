//! `install` and `uninstall` on Windows Server: one elevated command each,
//! idempotent, with the same `--json` report as on Linux.
//!
//! Install:
//! 1. copies the binary to `%ProgramFiles%\Nightgrid Node` and puts that
//!    folder on the machine `PATH`, so `nightgrid-node bridge` works over SSH;
//! 2. creates the local group `Nightgrid Node Users` and adds the login
//!    users (`--user`, default: whoever runs the install);
//! 3. creates the service `nightgrid-node` as the virtual account
//!    `NT SERVICE\nightgrid-node`: auto start, restart on failure, only
//!    `SeChangeNotifyPrivilege`, a write-restricted service SID;
//! 4. locks down `%ProgramData%\Nightgrid Node` (config: Administrators
//!    write, the service and the group read) and its `data` folder (only
//!    the service, Administrators and SYSTEM);
//! 5. writes the config if there is none;
//! 6. blocks all network traffic for the service with a Windows Service
//!    Hardening rule (and two visible firewall rules saying the same);
//! 7. starts the service and waits until the pipe answers.

use std::path::{Path, PathBuf};
use std::time::Duration;

use windows_sys::Win32::NetworkManagement::NetManagement::{
    NetLocalGroupAdd, NetLocalGroupAddMembers, NetLocalGroupDel, NetLocalGroupDelMembers, LOCALGROUP_INFO_1,
    LOCALGROUP_MEMBERS_INFO_3,
};
use windows_sys::Win32::Security::Authorization::{SetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE,
    REG_EXPAND_SZ, REG_MULTI_SZ,
};

use super::{acl, pipe, scm, wide, DISPLAY, GROUP, SERVICE};
use crate::install::{InstallOptions, Report};

const ENV_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
/// `FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE`
/// ("Modify"): enough for SQLite, not enough to change the folder's ACL.
const MODIFY: &str = "0x1301bf";
/// Name of the firewall rules and their group.
pub const FIREWALL_GROUP: &str = "Nightgrid Node";

/// The Windows template: everything not listed takes its default.
pub const TEMPLATE: &str = r#"# Nightgrid Node configuration (Administrators may change it; the service
# only reads it). Restart the service after a change:
#   Restart-Service nightgrid-node

[metrics]
# Seconds between samples (2-60). Minute points average the samples in them.
interval_secs = 10
# Days minute points are kept (1-365). 15-minute rollups are kept 90 days
# and hourly ones 400 days, like the app.
minute_retention_days = 14

[server]
# Named pipe the app reaches through SSH (`nightgrid-node bridge`).
# Administrators and members of "Nightgrid Node Users" may connect; remote
# clients are rejected and nothing listens on the network.
socket = '\\.\pipe\nightgrid-node'
max_connections = 8

[health]
# Disks, Storage Spaces and Windows Update, after an administrator runs
#   nightgrid-node allow health
enabled = true
# UPS to read: "battery" is a UPS on this host's USB port (Windows shows it
# as a battery). [ups] shutdown can then power this host off on battery.
ups = []

[rollback]
# Files, or folders of files, that a change from the app may snapshot and
# that are put back (contents, ACL and read-only bit) unless the app
# confirms the change in time. Needs files.rollback in [actions] allow and
# the admin helper (nightgrid-node allow helper). Keep this narrow, e.g.
#   paths = ['D:\srv\app', 'C:\ProgramData\ssh\sshd_config']
paths = []
"#;

pub(crate) fn report(action: &'static str) -> Report {
    Report { action, version: env!("CARGO_PKG_VERSION"), host: Some(facts()), ..Default::default() }
}

fn step(r: &mut Report, s: impl Into<String>) {
    let s = s.into();
    crate::info!("{s}");
    r.steps.push(s);
}

/// What kind of host this is, for the install report.
pub fn facts() -> crate::host::HostFacts {
    crate::host::HostFacts {
        arch: std::env::consts::ARCH.to_owned(),
        asset_arch: cfg!(target_arch = "x86_64").then_some("x86_64"),
        libc: "windows",
        init: "scm",
        init_version: None,
        kernel: super::sys::os_version(),
        busybox: false,
        limits: limits(),
    }
}

/// What the Windows node doesn't do yet (docs/WINDOWS.md), one sentence each.
pub fn limits() -> Vec<String> {
    [
        "UPS readings only from a UPS on USB.",
        "No service checks, node-to-node link or status page yet.",
        "No rogue DHCP watch: Windows can't listen for DHCP without a packet capture driver.",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

pub fn preflight() -> Result<(), String> {
    if !crate::compat::am_root() {
        return Err("run this as root (from an elevated Administrator session)".into());
    }
    Ok(())
}

/// A local or domain account name `NetLocalGroupAddMembers` will take.
pub fn valid_user(u: &str) -> bool {
    let name = u.rsplit('\\').next().unwrap_or(u);
    !u.is_empty()
        && u.len() <= 104
        && !name.is_empty()
        && name.len() <= 64
        && !u.starts_with('-')
        && u.matches('\\').count() <= 1
        && u.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ' ' | '\\' | '$'))
}

/// `"C:\Program Files\Nightgrid Node\nightgrid-node.exe" run`
pub fn service_command(bin: &Path) -> String {
    format!("\"{}\" run", bin.display())
}

pub fn install(self_exe: &Path, opts: &InstallOptions) -> Result<Report, String> {
    preflight()?;
    for u in &opts.allow_users {
        if !valid_user(u) {
            return Err(format!("not a usable account name: {u:?}"));
        }
    }
    let mut rep = report("install");
    let bin = super::installed_binary();

    // 1. Binary and PATH. A running service holds its exe open: stop it,
    // or the copy would fail; it starts again at the end.
    let was_running = scm::status(SERVICE)?.is_some_and(|s| s.running());
    let same = same_file(self_exe, &bin);
    if !same && was_running {
        scm::stop(SERVICE, Duration::from_secs(30))?;
        step(&mut rep, "stopped the running service for the upgrade");
    }
    if !same {
        // The helper runs the same exe; it starts again when next needed.
        super::helper::stop_all()?;
    }
    if same {
        step(&mut rep, format!("{} already in place", bin.display()));
    } else {
        copy_binary(self_exe, &bin)?;
        step(&mut rep, format!("installed {}", bin.display()));
    }
    if add_to_path(&super::bin_dir())? {
        step(&mut rep, format!("added {} to the machine PATH", super::bin_dir().display()));
    }

    // 2. Group and users.
    if ensure_group()? {
        step(&mut rep, format!("created the local group {GROUP:?}"));
    }
    let mut users = opts.allow_users.clone();
    if users.is_empty() {
        if let Some(me) = invoking_user() {
            users.push(me);
        }
    }
    for u in &users {
        match add_member(u) {
            Ok(true) => {
                step(&mut rep, format!("added {u} to {GROUP:?}"));
                rep.users.push(u.clone());
            }
            Ok(false) => step(&mut rep, format!("{u} already in {GROUP:?}")),
            Err(e) => rep.warnings.push(e),
        }
    }

    // 3. The service.
    let m = scm::manager(true)?;
    let created = scm::ensure(
        &m,
        &scm::Spec {
            name: SERVICE,
            display: DISPLAY,
            description: "Keeps Nightgrid's history and alerts going while the app is closed. No network access; the app reaches it over SSH.",
            command: service_command(&bin),
            account: Some(format!("NT SERVICE\\{SERVICE}")),
            auto_start: true,
            privileges: &["SeChangeNotifyPrivilege"],
            restricted: true,
            restart: true,
        },
    )?;
    drop(m);
    step(&mut rep, if created { "created the service" } else { "updated the service" });
    let account = format!("NT SERVICE\\{SERVICE}");
    match builtin_group(PERF_USERS).map(|g| (add_to_group(&g, &account), g)) {
        Some((Ok(true), g)) => step(&mut rep, format!("added the service to {g:?} (to read performance counters)")),
        Some((Ok(false), _)) => {}
        Some((Err(e), _)) => rep.warnings.push(format!("{e}: CPU, disk and network figures will be missing")),
        None => rep
            .warnings
            .push("no Performance Monitor Users group: CPU, disk and network figures will be missing".into()),
    }

    // 4. Folders and their ACLs (the service SID exists now).
    let etc = super::etc_dir();
    let data = super::data_dir();
    let tmp = data.join("tmp");
    std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let svc = pipe::sid_of(&format!("NT SERVICE\\{SERVICE}")).ok_or("the service's account did not resolve")?;
    let grp = pipe::sid_of(GROUP);
    set_dacl(&etc, &etc_sddl(&svc, grp.as_deref()))?;
    set_dacl(&data, &data_sddl(&svc))?;
    step(&mut rep, format!("locked down {}", etc.display()));
    set_service_env(&[format!("TEMP={}", tmp.display()), format!("TMP={}", tmp.display())])?;

    // 5. Config.
    let config = etc.join("config.toml");
    if !config.exists() {
        std::fs::write(&config, TEMPLATE).map_err(|e| format!("{}: {e}", config.display()))?;
        step(&mut rep, format!("wrote {}", config.display()));
    }

    // 6. No network.
    match firewall(true, &bin) {
        Ok(()) => step(&mut rep, "blocked all network traffic for the service (Windows Service Hardening)"),
        Err(e) => rep.warnings.push(format!("could not add the service's firewall restriction: {e}")),
    }

    // 7. Start and check.
    if opts.no_start {
        step(&mut rep, "enabled at boot, not started (--no-start)");
    } else {
        if was_running && same {
            scm::stop(SERVICE, Duration::from_secs(30))?;
        }
        scm::start(SERVICE, Duration::from_secs(30))?;
        wait_answering(Duration::from_secs(20))?;
        step(&mut rep, "started the service; it answers on its pipe");
    }
    if !rep.users.is_empty() {
        rep.warnings
            .push(format!("{} must log in again (a new SSH session) before the group applies", rep.users.join(", ")));
    }
    Ok(rep)
}

pub fn uninstall(purge: bool) -> Result<Report, String> {
    preflight()?;
    let mut rep = report("uninstall");
    super::helper::remove(&mut rep);
    scm::stop(SERVICE, Duration::from_secs(30))?;
    for sid in [PERF_USERS, super::EVENT_LOG_READERS] {
        if let Some(g) = builtin_group(sid) {
            if remove_from_group(&g, &format!("NT SERVICE\\{SERVICE}")) {
                step(&mut rep, format!("removed the service from {g:?}"));
            }
        }
    }
    // While the service still exists: Windows resolves its SID to drop the
    // hardening rule.
    if let Err(e) = firewall(false, &super::installed_binary()) {
        rep.warnings.push(format!("firewall rules: {e}"));
    }
    if scm::delete(SERVICE)? {
        step(&mut rep, "removed the service");
    }
    if remove_from_path(&super::bin_dir())? {
        step(&mut rep, format!("removed {} from the machine PATH", super::bin_dir().display()));
    }
    // The exe kept by the last upgrade, and leftovers of a replaced one.
    for f in [super::upgrade::previous(), super::upgrade::staged(), super::installed_binary().with_extension("exe.old")]
    {
        let _ = std::fs::remove_file(f);
    }
    remove_binary(&super::installed_binary(), &mut rep);
    if purge {
        let etc = super::etc_dir();
        if etc.exists() {
            std::fs::remove_dir_all(&etc).map_err(|e| format!("{}: {e}", etc.display()))?;
            step(&mut rep, format!("removed {} (history and config)", etc.display()));
        }
        let snaps = crate::helper::rollback_dir();
        if snaps.exists() {
            std::fs::remove_dir_all(&snaps).map_err(|e| format!("{}: {e}", snaps.display()))?;
            step(&mut rep, format!("removed {} (rollback snapshots)", snaps.display()));
        }
        let w = wide(GROUP);
        // SAFETY: valid string.
        if unsafe { NetLocalGroupDel(std::ptr::null(), w.as_ptr()) } == 0 {
            step(&mut rep, format!("removed the local group {GROUP:?}"));
        }
    } else {
        step(&mut rep, format!("kept history and config in {}", super::etc_dir().display()));
    }
    Ok(rep)
}

/// `allow eventlog` / `deny eventlog` (alias `journal`): the service joins
/// or leaves Event Log Readers, the counterpart of the journal's group, and
/// is restarted so its token picks the change up.
pub fn eventlog(on: bool, no_start: bool) -> Result<Report, String> {
    preflight()?;
    let mut rep = Report { host: None, ..report(if on { "allow" } else { "deny" }) };
    let group = builtin_group(super::EVENT_LOG_READERS).ok_or("this Windows has no Event Log Readers group")?;
    let account = format!("NT SERVICE\\{SERVICE}");
    let changed = if on {
        rep.warnings.push(format!(
            "members of {group:?} can read the System, Security and Application logs, which may include secrets \
             that programs logged. The node keeps only login lines and error patterns, through the same secret \
             filter as the app."
        ));
        add_to_group(&group, &account)?
    } else {
        remove_from_group(&group, &account)
    };
    match (on, changed) {
        (true, true) => step(&mut rep, format!("added the service to {group:?}")),
        (true, false) => step(&mut rep, format!("the service was already in {group:?}")),
        (false, true) => step(&mut rep, format!("removed the service from {group:?}")),
        (false, false) => step(&mut rep, "eventlog was not allowed"),
    }
    let running = scm::status(SERVICE).ok().flatten().is_some_and(|s| s.running());
    if changed && running && !no_start {
        scm::stop(SERVICE, Duration::from_secs(30))?;
        scm::start(SERVICE, Duration::from_secs(30))?;
        wait_answering(Duration::from_secs(20))?;
        step(&mut rep, "restarted the service");
    }
    Ok(rep)
}

/// Docker Engine's settings file, where `group` decides who besides
/// SYSTEM and Administrators may use its pipe.
pub fn docker_config() -> PathBuf {
    super::program_data().join(r"docker\config\daemon.json")
}

/// The `group` value that lets the node's service in: its own service SID,
/// because the service's write-restricted token can only write to a pipe
/// whose DACL names that SID (membership in a group such as docker-users
/// passes only half of the access check).
pub fn docker_group() -> String {
    format!("NT SERVICE\\{SERVICE}")
}

/// `daemon.json` with `group` set to the service (`on`) or that setting
/// taken out again. `Ok(None)`: nothing to change. An existing `group` for
/// someone else is left alone and reported.
pub fn docker_settings(text: Option<&str>, on: bool) -> Result<Option<String>, String> {
    let mut v: serde_json::Value = match text.map(str::trim) {
        None | Some("") => serde_json::json!({}),
        Some(t) => serde_json::from_str(t).map_err(|e| format!("daemon.json isn't valid JSON: {e}"))?,
    };
    let obj = v.as_object_mut().ok_or("daemon.json isn't a JSON object")?;
    let ours = docker_group();
    let current = obj.get("group").and_then(|g| g.as_str()).map(str::to_owned);
    match (on, current) {
        (true, Some(g)) if g.eq_ignore_ascii_case(&ours) => Ok(None),
        (true, Some(g)) => Err(format!(
            "daemon.json already gives Docker's pipe to {g:?}. The node's restricted token needs its own SID \
             there: change it to \"group\": {ours:?} (members of {g:?} then need Administrators to use docker), \
             or leave container monitoring off"
        )),
        (true, None) => {
            obj.insert("group".into(), serde_json::Value::String(ours));
            Ok(Some(serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"))
        }
        (false, Some(g)) if g.eq_ignore_ascii_case(&ours) => {
            obj.remove("group");
            Ok(Some(serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"))
        }
        (false, _) => Ok(None),
    }
}

/// `allow docker` / `deny docker`: names the service as Docker Engine's
/// pipe group in `daemon.json` (or takes it out). Docker reads it when it
/// starts, so nothing changes until Docker is restarted, which restarts
/// its containers too; the node never does that itself.
pub fn docker(on: bool) -> Result<Report, String> {
    preflight()?;
    let mut rep = Report { host: None, ..report(if on { "allow" } else { "deny" }) };
    let path = docker_config();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if on && text.is_none() && scm::status("docker").ok().flatten().is_none() {
        rep.warnings.push("Docker Engine (the docker service) isn't installed; the setting waits for it".into());
    }
    match docker_settings(text.as_deref(), on)? {
        None if on => step(&mut rep, format!("{} already lets the service use Docker's pipe", path.display())),
        None => step(&mut rep, "docker was not allowed"),
        Some(new) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            std::fs::write(&path, new).map_err(|e| format!("{}: {e}", path.display()))?;
            step(
                &mut rep,
                if on {
                    format!("set \"group\": {:?} in {}", docker_group(), path.display())
                } else {
                    format!("removed \"group\" from {}", path.display())
                },
            );
            rep.warnings.push(
                "Docker applies this when it restarts (Restart-Service docker), which restarts its containers \
                 too; the node doesn't restart Docker for you"
                    .into(),
            );
        }
    }
    if on {
        rep.warnings.push(
            "whoever can use Docker's pipe controls the engine, which is Administrator-equivalent. The node only \
             reads containers and events and runs the container actions an administrator allowed, but a flaw in it \
             could be used for more."
                .into(),
        );
    }
    Ok(rep)
}

/// Whether the service is in Event Log Readers (`allow eventlog`); `None`
/// when the group or the service's account can't be read.
pub fn eventlog_allowed() -> Option<bool> {
    use windows_sys::Win32::NetworkManagement::NetManagement::{
        NetApiBufferFree, NetLocalGroupGetMembers, LOCALGROUP_MEMBERS_INFO_0,
    };
    let group = builtin_group(super::EVENT_LOG_READERS)?;
    let svc = pipe::sid_of(&format!("NT SERVICE\\{SERVICE}"))?;
    let g = wide(&group);
    let mut buf: *mut u8 = std::ptr::null_mut();
    let (mut read, mut total, mut resume) = (0u32, 0u32, 0usize);
    // SAFETY: Windows allocates the buffer; freed below.
    let r = unsafe {
        NetLocalGroupGetMembers(std::ptr::null(), g.as_ptr(), 0, &mut buf, u32::MAX, &mut read, &mut total, &mut resume)
    };
    if r != 0 || buf.is_null() {
        return None;
    }
    let items = buf as *const LOCALGROUP_MEMBERS_INFO_0;
    let found = (0..read as usize).any(|i| {
        // SAFETY: `read` entries, each holding a valid SID.
        unsafe { acl::sid_string((*items.add(i)).lgrmi0_sid) }.as_deref() == Some(svc.as_str())
    });
    // SAFETY: the buffer NetLocalGroupGetMembers allocated.
    unsafe { NetApiBufferFree(buf.cast()) };
    Some(found)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Copies the binary in, moving an old one aside first (Windows lets a file
/// that is in use be renamed, not overwritten).
fn copy_binary(from: &Path, to: &Path) -> Result<(), String> {
    let dir = to.parent().ok_or("no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = to.with_extension("exe.new");
    std::fs::copy(from, &tmp).map_err(|e| format!("copy to {}: {e}", tmp.display()))?;
    if to.exists() {
        let old = to.with_extension("exe.old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(to, &old).map_err(|e| format!("{}: {e}", to.display()))?;
        let _ = std::fs::remove_file(&old);
    }
    std::fs::rename(&tmp, to).map_err(|e| format!("{}: {e}", to.display()))
}

/// Removes the installed binary; when that is the running program, it is
/// renamed and deleted at the next reboot.
fn remove_binary(bin: &Path, rep: &mut Report) {
    if !bin.exists() {
        return;
    }
    match std::fs::remove_file(bin) {
        Ok(()) => step(rep, format!("removed {}", bin.display())),
        Err(_) => {
            use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
            let gone = bin.with_extension("exe.remove");
            let _ = std::fs::remove_file(&gone);
            let moved = std::fs::rename(bin, &gone).is_ok();
            let target = if moved { gone } else { bin.to_path_buf() };
            let w = wide(target.as_os_str());
            // SAFETY: valid string; a null destination means delete.
            unsafe { MoveFileExW(w.as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
            step(rep, format!("{} is in use; it is removed at the next reboot", bin.display()));
        }
    }
    let dir = super::bin_dir();
    let _ = std::fs::remove_dir(&dir);
}

pub fn etc_sddl(service_sid: &str, group_sid: Option<&str>) -> String {
    let mut s = format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FR;;;{service_sid})");
    if let Some(g) = group_sid {
        s.push_str(&format!("(A;OICI;FR;;;{g})"));
    }
    s
}

pub fn data_sddl(service_sid: &str) -> String {
    format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;{MODIFY};;;{service_sid})")
}

/// Replaces a folder's DACL (protected: nothing inherited from above).
fn set_dacl(path: &Path, sddl: &str) -> Result<(), String> {
    let sd = acl::Sddl::new(sddl).map_err(|e| format!("{sddl}: {e}"))?;
    let (mut present, mut defaulted) = (0, 0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: a valid descriptor; `dacl` points into it.
    if unsafe { GetSecurityDescriptorDacl(sd.descriptor(), &mut present, &mut dacl, &mut defaulted) } == 0 {
        return Err(format!("{sddl}: {}", std::io::Error::last_os_error()));
    }
    let w = wide(path.as_os_str());
    // SAFETY: valid path and DACL; owner and group unchanged.
    let r = unsafe {
        SetNamedSecurityInfoW(
            w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null(),
        )
    };
    if r != 0 {
        return Err(format!("set the ACL of {}: {}", path.display(), std::io::Error::from_raw_os_error(r as i32)));
    }
    Ok(())
}

/// Creates the users' group. True when it was created.
fn ensure_group() -> Result<bool, String> {
    let mut name = wide(GROUP);
    let mut comment = wide("May use Nightgrid Node's pipe (nightgrid-node bridge)");
    let info = LOCALGROUP_INFO_1 { lgrpi1_name: name.as_mut_ptr(), lgrpi1_comment: comment.as_mut_ptr() };
    let mut parm = 0u32;
    // SAFETY: a valid level-1 struct.
    let r = unsafe { NetLocalGroupAdd(std::ptr::null(), 1, (&info as *const LOCALGROUP_INFO_1).cast(), &mut parm) };
    match r {
        0 => Ok(true),
        1379 | 2223 => Ok(false), // ERROR_ALIAS_EXISTS, NERR_GroupExists
        e => Err(format!("create the group {GROUP:?}: {}", std::io::Error::from_raw_os_error(e as i32))),
    }
}

/// Adds an account to the node's group. True when it was added.
fn add_member(user: &str) -> Result<bool, String> {
    add_to_group(GROUP, user)
}

/// Builtin "Performance Monitor Users": a service isn't an interactive
/// logon, so it may read performance counters only as a member.
const PERF_USERS: &str = "S-1-5-32-558";

/// The local name of a builtin group (the names are translated).
fn builtin_group(sid: &str) -> Option<String> {
    use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
    let w = wide(sid);
    let mut p: windows_sys::Win32::Security::PSID = std::ptr::null_mut();
    // SAFETY: Windows allocates the SID; `Local` frees it.
    if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut p) } == 0 {
        return None;
    }
    let _free = super::Local(p);
    // SAFETY: a valid SID from the call above.
    let name = unsafe { acl::account_name(p) }?;
    Some(name.rsplit('\\').next().unwrap_or(&name).to_owned())
}

/// Removes an account from a local group. True when it was a member.
fn remove_from_group(group: &str, user: &str) -> bool {
    let g = wide(group);
    let mut u = wide(user);
    let m = LOCALGROUP_MEMBERS_INFO_3 { lgrmi3_domainandname: u.as_mut_ptr() };
    // SAFETY: one valid level-3 entry.
    unsafe {
        NetLocalGroupDelMembers(std::ptr::null(), g.as_ptr(), 3, (&m as *const LOCALGROUP_MEMBERS_INFO_3).cast(), 1)
            == 0
    }
}

/// Adds an account to a local group. True when it was added.
fn add_to_group(group: &str, user: &str) -> Result<bool, String> {
    let g = wide(group);
    let mut u = wide(user);
    let m = LOCALGROUP_MEMBERS_INFO_3 { lgrmi3_domainandname: u.as_mut_ptr() };
    // SAFETY: one valid level-3 entry.
    let r = unsafe {
        NetLocalGroupAddMembers(std::ptr::null(), g.as_ptr(), 3, (&m as *const LOCALGROUP_MEMBERS_INFO_3).cast(), 1)
    };
    match r {
        0 => Ok(true),
        1378 => Ok(false), // ERROR_MEMBER_IN_ALIAS
        1332 => Err(format!("no account named {user}")),
        e => Err(format!("add {user} to {group:?}: {}", std::io::Error::from_raw_os_error(e as i32))),
    }
}

/// `DOMAIN\user` of whoever runs this, unless it is SYSTEM or a machine
/// account (which need no group).
fn invoking_user() -> Option<String> {
    let user = std::env::var("USERNAME").ok().filter(|u| !u.is_empty() && !u.ends_with('$'))?;
    match std::env::var("USERDOMAIN") {
        Ok(d) if !d.is_empty() => Some(format!("{d}\\{user}")),
        _ => Some(user),
    }
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: we own the key.
        unsafe { RegCloseKey(self.0) };
    }
}

fn open_key(path: &str, write: bool) -> Result<Key, String> {
    let w = wide(path);
    let mut k: HKEY = std::ptr::null_mut();
    let access = if write { KEY_READ | KEY_WRITE } else { KEY_READ };
    // SAFETY: valid strings and out-param.
    let r = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, w.as_ptr(), 0, access, &mut k) };
    if r != 0 {
        return Err(format!("HKLM\\{path}: {}", std::io::Error::from_raw_os_error(r as i32)));
    }
    Ok(Key(k))
}

fn machine_path() -> Result<String, String> {
    let k = open_key(ENV_KEY, false)?;
    let name = wide("Path");
    let mut len = 0u32;
    let mut ty = 0u32;
    // SAFETY: size query.
    unsafe { RegQueryValueExW(k.0, name.as_ptr(), std::ptr::null(), &mut ty, std::ptr::null_mut(), &mut len) };
    let mut buf = vec![0u16; (len as usize / 2) + 1];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: a buffer of `len` bytes.
    let r =
        unsafe { RegQueryValueExW(k.0, name.as_ptr(), std::ptr::null(), &mut ty, buf.as_mut_ptr().cast(), &mut len) };
    if r != 0 {
        return Err(format!("read the machine PATH: {}", std::io::Error::from_raw_os_error(r as i32)));
    }
    Ok(super::from_wide(&buf))
}

fn set_machine_path(v: &str) -> Result<(), String> {
    let k = open_key(ENV_KEY, true)?;
    let name = wide("Path");
    let data = wide(v);
    // SAFETY: REG_EXPAND_SZ data with its NUL, length in bytes.
    let r =
        unsafe { RegSetValueExW(k.0, name.as_ptr(), 0, REG_EXPAND_SZ, data.as_ptr().cast(), (data.len() * 2) as u32) };
    if r != 0 {
        return Err(format!("write the machine PATH: {}", std::io::Error::from_raw_os_error(r as i32)));
    }
    Ok(())
}

fn path_has(path: &str, dir: &Path) -> bool {
    let d = dir.to_string_lossy();
    path.split(';').any(|p| p.trim().trim_end_matches('\\').eq_ignore_ascii_case(d.trim_end_matches('\\')))
}

fn add_to_path(dir: &Path) -> Result<bool, String> {
    let p = machine_path()?;
    if path_has(&p, dir) {
        return Ok(false);
    }
    let sep = if p.is_empty() || p.ends_with(';') { "" } else { ";" };
    set_machine_path(&format!("{p}{sep}{}", dir.display()))?;
    Ok(true)
}

fn remove_from_path(dir: &Path) -> Result<bool, String> {
    let p = machine_path()?;
    if !path_has(&p, dir) {
        return Ok(false);
    }
    let d = dir.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let kept: Vec<&str> = p.split(';').filter(|x| x.trim().trim_end_matches('\\').to_lowercase() != d).collect();
    set_machine_path(&kept.join(";"))?;
    Ok(true)
}

/// The service's environment (`Environment`, REG_MULTI_SZ, under its key).
fn set_service_env(vars: &[String]) -> Result<(), String> {
    let k = open_key(&format!(r"SYSTEM\CurrentControlSet\Services\{SERVICE}"), true)?;
    let name = wide("Environment");
    let mut data: Vec<u16> = vars.iter().flat_map(wide).collect();
    data.push(0);
    // SAFETY: REG_MULTI_SZ, double-NUL terminated, length in bytes.
    let r =
        unsafe { RegSetValueExW(k.0, name.as_ptr(), 0, REG_MULTI_SZ, data.as_ptr().cast(), (data.len() * 2) as u32) };
    if r != 0 {
        return Err(format!("service environment: {}", std::io::Error::from_raw_os_error(r as i32)));
    }
    Ok(())
}

/// Adds (or removes) the service's network restriction: a Windows Service
/// Hardening rule that blocks all traffic for the service SID, which
/// Windows enforces even with the firewall profile off, plus two ordinary
/// block rules in the group "Nightgrid Node" so it shows in wf.msc.
pub fn firewall(on: bool, bin: &Path) -> Result<(), String> {
    let exe = bin.to_string_lossy().replace('\'', "''");
    let script = if on {
        format!(
            "$ErrorActionPreference='Stop'\n\
             $fw = New-Object -ComObject HNetCfg.FwPolicy2\n\
             $sr = $fw.ServiceRestriction\n\
             $sr.RestrictService('{SERVICE}', '{exe}', $true, $false)\n\
             if (-not $sr.ServiceRestricted('{SERVICE}', '{exe}')) {{\n\
               # A stale rule from an earlier install: drop it and add it again.\n\
               $sr.RestrictService('{SERVICE}', '{exe}', $false, $false)\n\
               $sr.RestrictService('{SERVICE}', '{exe}', $true, $false)\n\
             }}\n\
             if (-not $sr.ServiceRestricted('{SERVICE}', '{exe}')) {{ throw 'not restricted' }}\n\
             Get-NetFirewallRule -Group '{FIREWALL_GROUP}' -ErrorAction SilentlyContinue | Remove-NetFirewallRule\n\
             foreach ($d in 'Inbound','Outbound') {{\n\
               New-NetFirewallRule -DisplayName \"{FIREWALL_GROUP}: no network ($d)\" -Group '{FIREWALL_GROUP}' -Direction $d -Service '{SERVICE}' -Action Block -Profile Any | Out-Null\n\
             }}\n"
        )
    } else {
        format!(
            "$ErrorActionPreference='Continue'\n\
             $fw = New-Object -ComObject HNetCfg.FwPolicy2\n\
             try {{ $fw.ServiceRestriction.RestrictService('{SERVICE}', '{exe}', $false, $false) }} catch {{}}\n\
             Get-NetFirewallRule -Group '{FIREWALL_GROUP}' -ErrorAction SilentlyContinue | Remove-NetFirewallRule\n\
             exit 0\n"
        )
    };
    powershell(&script).map(|_| ())
}

/// True when the service's network restriction is in place (for `doctor`).
pub fn firewall_restricted() -> Option<bool> {
    let exe = super::installed_binary().to_string_lossy().replace('\'', "''");
    let out = powershell(&format!(
        "$fw = New-Object -ComObject HNetCfg.FwPolicy2\n$fw.ServiceRestriction.ServiceRestricted('{SERVICE}', '{exe}')\n"
    ))
    .ok()?;
    Some(out.trim().eq_ignore_ascii_case("true"))
}

/// Runs a short PowerShell script (Windows PowerShell 5.1, built in).
pub fn powershell(script: &str) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    // No progress records: they arrive on stderr as CLIXML.
    let encoded = encode_utf16_base64(&format!("$ProgressPreference='SilentlyContinue'\n{script}"));
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &encoded])
        .creation_flags(0x0800_0000)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("powershell: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first =
            err.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("#< CLIXML")).unwrap_or("failed");
        return Err(first.to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// PowerShell's `-EncodedCommand`: base64 of UTF-16LE.
pub fn encode_utf16_base64(s: &str) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n =
            (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Waits until the service answers `hello` on its pipe.
fn wait_answering(timeout: Duration) -> Result<(), String> {
    let until = std::time::Instant::now() + timeout;
    let pipe = PathBuf::from(super::PIPE);
    loop {
        match crate::client::Client::connect(&pipe, "nightgrid-node install") {
            Ok(_) => return Ok(()),
            Err(e) if std::time::Instant::now() > until => {
                return Err(format!("the service runs but its pipe does not answer: {e}"))
            }
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_settings_add_and_remove_only_our_group() {
        let ours = docker_group();
        let on = docker_settings(None, true).unwrap().unwrap();
        assert!(on.contains(&format!("\"group\": {}", serde_json::to_string(&ours).unwrap())), "{on}");
        assert_eq!(docker_settings(Some(&on), true).unwrap(), None);
        let kept = docker_settings(Some(r#"{"hosts": ["npipe://"], "debug": true}"#), true).unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(&kept).unwrap();
        assert_eq!((v["debug"].as_bool(), v["group"].as_str()), (Some(true), Some(ours.as_str())));
        let off = docker_settings(Some(&kept), false).unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(&off).unwrap();
        assert!(v.get("group").is_none() && v["debug"] == true);
        assert_eq!(docker_settings(Some(r#"{"group": "docker-users"}"#), false).unwrap(), None);
        assert!(docker_settings(Some(r#"{"group": "docker-users"}"#), true).unwrap_err().contains("docker-users"));
        assert!(docker_settings(Some("{nope"), true).is_err() && docker_settings(Some("[]"), true).is_err());
        assert_eq!(docker_settings(None, false).unwrap(), None);
    }

    #[test]
    fn accounts_and_acls() {
        assert!(valid_user("admin"));
        assert!(valid_user(r"CONTOSO\ops user"));
        assert!(!valid_user(r"a\b\c"));
        assert!(!valid_user("-x"));
        assert!(!valid_user("a;b"));
        assert_eq!(
            etc_sddl("S-1-5-80-1", Some("S-1-5-21-2")),
            "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FR;;;S-1-5-80-1)(A;OICI;FR;;;S-1-5-21-2)"
        );
        assert!(acl::Sddl::new(&data_sddl("S-1-5-18")).is_ok());
        assert!(path_has(
            r"C:\Windows;C:\Program Files\Nightgrid Node\;C:\x",
            Path::new(r"c:\program files\nightgrid node")
        ));
        assert_eq!(encode_utf16_base64("ab"), "YQBiAA==");
        assert!(crate::config::Config::parse(TEMPLATE).is_ok());
        assert_eq!(service_command(Path::new(r"C:\P F\n.exe")), r#""C:\P F\n.exe" run"#);
    }
}
