//! `nightgrid-node install` / `uninstall`: set the node up as a hardened
//! systemd service, or take it away again.
//!
//! The app runs these over SSH with sudo after uploading and verifying the
//! binary; a person can run them by hand just the same. Both are idempotent:
//! installing again upgrades in place and restarts the service, and
//! uninstalling something already gone succeeds. Every step goes through
//! [`Sys`] and every path through [`Layout`], so tests can run the whole flow
//! in a temporary directory without root.
//!
//! Works on Debian/Ubuntu, Fedora (including rpm-ostree systems like Bazzite,
//! where `/usr/local` and `/etc` are writable) and Proxmox VE: it only needs
//! systemd (with `systemd-sysusers`) and `usermod`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

pub const SERVICE: &str = "nightgrid-node";
pub const USER: &str = "nightgrid-node";
pub const UNIT: &str = include_str!("../../../dist/systemd/nightgrid-node.service");
/// The opt-in network watch helper (`allow netwatch`).
pub const NETWATCH_UNIT: &str = include_str!("../../../dist/systemd/nightgrid-node-netwatch.service");
pub const NETWATCH_SERVICE: &str = "nightgrid-node-netwatch";
/// The opt-in root helper (`allow helper`): a socket and a per-connection service.
pub const HELPER_SOCKET_UNIT: &str = include_str!("../../../dist/systemd/nightgrid-node-helper.socket");
pub const HELPER_SERVICE_UNIT: &str = include_str!("../../../dist/systemd/nightgrid-node-helper@.service");
pub const HELPER_SOCKET: &str = "nightgrid-node-helper.socket";
/// The opt-in read-only health helper (`allow health`).
pub const HEALTH_SOCKET_UNIT: &str = include_str!("../../../dist/systemd/nightgrid-node-health.socket");
pub const HEALTH_SERVICE_UNIT: &str = include_str!("../../../dist/systemd/nightgrid-node-health@.service");
pub const HEALTH_SOCKET: &str = "nightgrid-node-health.socket";
/// `allow power`: a udev rule that lets the service's group read RAPL.
pub const POWER_RULE: &str = "/etc/udev/rules.d/70-nightgrid-node-power.rules";
const POWER_RULE_BODY: &str = "# Written by `nightgrid-node allow power`; removed by `nightgrid-node deny power`.\n\
# Lets the nightgrid-node group read the CPU energy counters (RAPL) for power draw.\n\
SUBSYSTEM==\"powercap\", ACTION==\"add\", RUN+=\"/bin/chgrp nightgrid-node /sys%p/energy_uj\", RUN+=\"/bin/chmod 0440 /sys%p/energy_uj\"\n";
/// Applies the rule's effect now, or takes it back (`root:root 0400`, the kernel's default).
fn power_now(group: &str, mode: &str) -> String {
    format!(
        "for f in /sys/class/powercap/intel-rapl:*/energy_uj; do [ -e \"$f\" ] || continue; \
         chgrp {group} \"$f\" && chmod {mode} \"$f\"; done; true"
    )
}

/// An opt-in grant that installs units of its own rather than a drop-in.
struct Extra {
    name: &'static str,
    /// (file in /etc/systemd/system, contents); the first marks it installed.
    files: &'static [(&'static str, &'static str)],
    /// The unit to enable and start.
    unit: &'static str,
    warning: &'static str,
    what: &'static str,
    /// A drop-in written from the config next to the second file's unit.
    dropin: Option<fn(&Layout) -> String>,
}

const NETWATCH: Extra = Extra {
    name: "netwatch",
    files: &[("nightgrid-node-netwatch.service", NETWATCH_UNIT)],
    unit: NETWATCH_SERVICE,
    warning: "the helper may read every packet on this host's network interfaces (CAP_NET_RAW). A kernel filter \
              passes it only ARP and DHCP, it sends nothing, and it keeps only addresses, MACs, DHCP settings \
              and host names.",
    what: "network watch helper",
    dropin: None,
};

const HELPER: Extra = Extra {
    name: "helper",
    files: &[
        ("nightgrid-node-helper.socket", HELPER_SOCKET_UNIT),
        ("nightgrid-node-helper@.service", HELPER_SERVICE_UNIT),
    ],
    unit: HELPER_SOCKET,
    warning: "the service can now ask a root helper to restart, start or stop the systemd units listed in \
              [actions] units and to reboot or power off, but only for the actions you list in [actions] allow \
              in /etc/nightgrid-node/config.toml (none by default). The helper checks that file itself.",
    what: "root helper",
    dropin: Some(helper_dropin),
};

const HEALTH: Extra = Extra {
    name: "health",
    files: &[
        ("nightgrid-node-health.socket", HEALTH_SOCKET_UNIT),
        ("nightgrid-node-health@.service", HEALTH_SERVICE_UNIT),
    ],
    unit: HEALTH_SOCKET,
    warning: "the service can now ask a read-only root helper for disk SMART data, storage pool status, pending \
              updates (from the package manager's local cache) and the UPS targets in [health] ups. The helper \
              runs fixed commands only and changes nothing.",
    what: "health helper",
    dropin: None,
};

/// Lets the helper write where root allows rollbacks.
fn helper_dropin(layout: &Layout) -> String {
    let paths = crate::config::Config::load(&layout.config()).map(|c| c.rollback.paths).unwrap_or_default();
    let mut s = String::from(
        "# Written by `nightgrid-node allow helper` from [rollback] paths in /etc/nightgrid-node/config.toml.\n\
         # Run that command again after changing the list.\n[Service]\n",
    );
    for p in paths.iter().filter(|p| crate::config::rollback_path_ok(p)) {
        s.push_str(&format!("ReadWritePaths=-{p}\n"));
    }
    s
}

impl Layout {
    fn extra_dropin(&self, e: &Extra) -> PathBuf {
        self.system_unit(&format!("{}.d", e.files[e.files.len() - 1].0)).join("rollback.conf")
    }
    fn write_extra(&self, e: &Extra, rep: &mut Report, verb: &str) -> Result<(), String> {
        for (name, body) in e.files {
            write_file(&self.system_unit(name), body, 0o644)?;
            rep.step(format!("{verb} {}", self.system_unit(name).display()));
        }
        if let Some(f) = e.dropin {
            let path = self.extra_dropin(e);
            std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")))
                .map_err(|err| format!("{}: {err}", path.display()))?;
            write_file(&path, &f(self), 0o644)?;
            rep.step(format!("{verb} {}", path.display()));
        }
        Ok(())
    }
    fn remove_extra(&self, e: &Extra, rep: &mut Report) -> Result<(), String> {
        for (name, _) in e.files {
            remove(&self.system_unit(name), rep)?;
        }
        if e.dropin.is_some() {
            remove_dir(self.extra_dropin(e).parent().unwrap_or(Path::new("/nonexistent")), rep)?;
        }
        Ok(())
    }
}

const EXTRAS: [&Extra; 3] = [&NETWATCH, &HELPER, &HEALTH];
pub const SYSUSERS: &str = "# Nightgrid Node service account (no login shell, no password).\n\
                            u nightgrid-node - \"Nightgrid Node\" /var/lib/nightgrid-node\n";

/// Where things go, under `root` ("/" for real).
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn system() -> Self {
        Self { root: PathBuf::from("/") }
    }
    fn at(&self, p: &str) -> PathBuf {
        self.root.join(p.trim_start_matches('/'))
    }
    pub fn bin(&self) -> PathBuf {
        self.at("/usr/local/bin/nightgrid-node")
    }
    pub fn unit(&self) -> PathBuf {
        self.at("/etc/systemd/system/nightgrid-node.service")
    }
    pub fn netwatch_unit(&self) -> PathBuf {
        self.system_unit("nightgrid-node-netwatch.service")
    }
    pub fn system_unit(&self, name: &str) -> PathBuf {
        self.at("/etc/systemd/system").join(name)
    }
    fn has(&self, e: &Extra) -> bool {
        self.system_unit(e.files[0].0).exists()
    }
    pub fn sysusers(&self) -> PathBuf {
        self.at("/etc/sysusers.d/nightgrid-node.conf")
    }
    pub fn config_dir(&self) -> PathBuf {
        self.at("/etc/nightgrid-node")
    }
    pub fn config(&self) -> PathBuf {
        self.config_dir().join("config.toml")
    }
    /// systemd drop-ins written by `allow`.
    pub fn dropin_dir(&self) -> PathBuf {
        self.at("/etc/systemd/system/nightgrid-node.service.d")
    }
    pub fn data_dir(&self) -> PathBuf {
        self.at(crate::config::DEFAULT_DATA_DIR)
    }
    fn is_system(&self) -> bool {
        self.root == Path::new("/")
    }
}

/// The commands install and uninstall run.
pub trait Sys {
    /// Runs a program; `Err` with its stderr when it fails.
    fn run(&mut self, prog: &str, args: &[&str]) -> Result<String, String>;
}

/// Runs real commands.
pub struct RealSys;

impl Sys for RealSys {
    fn run(&mut self, prog: &str, args: &[&str]) -> Result<String, String> {
        let out = Command::new(prog).args(args).output().map_err(|e| format!("{prog}: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!("{prog} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// Login users allowed to talk to the node (added to its group).
    pub allow_users: Vec<String>,
    /// Leave the service stopped (enabled at boot all the same).
    pub no_start: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub action: &'static str,
    pub version: &'static str,
    /// Steps taken, in order, for the app's log and the person's terminal.
    pub steps: Vec<String>,
    /// Users added to the node's group. They need a new SSH login to use it.
    pub users: Vec<String>,
    /// Problems that didn't stop the action.
    pub warnings: Vec<String>,
}

impl Report {
    fn step(&mut self, s: impl Into<String>) {
        let s = s.into();
        crate::info!("{s}");
        self.steps.push(s);
    }
}

/// A login name `usermod` will take and a shell can't misread.
pub fn valid_user(u: &str) -> bool {
    !u.is_empty()
        && u.len() <= 32
        && !u.starts_with('-')
        && u.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && u != USER
}

fn write_file(path: &Path, body: &str, mode: u32) -> Result<(), String> {
    let dir = path.parent().ok_or("no parent dir")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = path.with_extension("ngn-tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Copies the binary into place: a new file in the target directory (so it
/// gets that directory's SELinux type) renamed over the old one, so a running
/// service keeps its old inode until restarted.
fn copy_binary(from: &Path, to: &Path) -> Result<bool, String> {
    if let (Ok(a), Ok(b)) = (from.canonicalize(), to.canonicalize()) {
        if a == b {
            return Ok(false);
        }
    }
    let dir = to.parent().ok_or("no parent dir")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = to.with_extension("ngn-tmp");
    std::fs::copy(from, &tmp).map_err(|e| format!("copy to {}: {e}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, to).map_err(|e| format!("{}: {e}", to.display()))?;
    Ok(true)
}

fn preflight(layout: &Layout) -> Result<(), String> {
    if !layout.is_system() {
        return Ok(());
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err("run this as root (sudo nightgrid-node ...)".into());
    }
    if !Path::new("/run/systemd/system").is_dir() {
        return Err("systemd is not running on this host; Nightgrid Node needs it".into());
    }
    Ok(())
}

pub fn install(layout: &Layout, sys: &mut dyn Sys, self_exe: &Path, opts: &InstallOptions) -> Result<Report, String> {
    preflight(layout)?;
    for u in &opts.allow_users {
        if !valid_user(u) {
            return Err(format!("not a usable login name: {u:?}"));
        }
    }
    let mut rep = Report { action: "install", version: env!("CARGO_PKG_VERSION"), ..Default::default() };

    if copy_binary(self_exe, &layout.bin())? {
        rep.step(format!("installed {}", layout.bin().display()));
    } else {
        rep.step(format!("{} already in place", layout.bin().display()));
    }
    // Best effort: fix the SELinux label where SELinux is in use.
    if layout.is_system() && Path::new("/sys/fs/selinux/enforce").exists() {
        if let Err(e) = sys.run("restorecon", &["-F", &layout.bin().to_string_lossy()]) {
            rep.warnings.push(format!("restorecon: {e}"));
        }
    }

    write_file(&layout.sysusers(), SYSUSERS, 0o644)?;
    sys.run("systemd-sysusers", &[&layout.sysusers().to_string_lossy()])?;
    rep.step(format!("service account {USER} ready"));

    if !layout.config().exists() {
        write_file(&layout.config(), crate::config::TEMPLATE, 0o644)?;
        rep.step(format!("wrote {}", layout.config().display()));
    } else {
        crate::config::Config::load(&layout.config())?;
        rep.step(format!("kept {}", layout.config().display()));
    }

    write_file(&layout.unit(), UNIT, 0o644)?;
    rep.step(format!("wrote {}", layout.unit().display()));
    let extras: Vec<&Extra> = EXTRAS.into_iter().filter(|e| layout.has(e)).collect();
    for e in &extras {
        layout.write_extra(e, &mut rep, "updated")?;
    }

    for u in &opts.allow_users {
        sys.run("id", &["-u", u]).map_err(|_| format!("no such user: {u}"))?;
        sys.run("usermod", &["-a", "-G", USER, u])?;
        rep.users.push(u.clone());
        rep.step(format!("{u} may use the node (from their next login)"));
    }

    sys.run("systemctl", &["daemon-reload"])?;
    sys.run("systemctl", &["enable", SERVICE])?;
    if opts.no_start {
        rep.step("enabled at boot, not started".to_string());
    } else {
        sys.run("systemctl", &["restart", SERVICE])?;
        rep.step("service started".to_string());
        if layout.is_system() {
            wait_ready(&mut rep);
        }
        for e in &extras {
            match sys.run("systemctl", &["restart", e.unit]) {
                Ok(_) => rep.step(format!("{} restarted", e.what)),
                Err(err) => rep.warnings.push(err),
            }
        }
    }
    Ok(rep)
}

fn wait_ready(rep: &mut Report) {
    let sock = crate::config::Config::load(Path::new(crate::config::DEFAULT_CONFIG))
        .map(|c| c.server.socket)
        .unwrap_or_else(|_| PathBuf::from(nightgrid_node_proto::DEFAULT_SOCKET));
    for _ in 0..50 {
        if let Ok(c) = crate::client::Client::connect(&sock, "nightgrid-node install") {
            rep.step(format!("answering on {} (node {})", sock.display(), c.welcome.node.node_id));
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    rep.warnings.push(format!("not answering on {} yet; see: journalctl -u {SERVICE}", sock.display()));
}

/// Stops and removes the service. `purge` also removes history, config and
/// the service account (so a later install starts with a new node id).
pub fn uninstall(layout: &Layout, sys: &mut dyn Sys, purge: bool) -> Result<Report, String> {
    preflight(layout)?;
    let mut rep = Report { action: "uninstall", version: env!("CARGO_PKG_VERSION"), ..Default::default() };
    for e in EXTRAS.into_iter().filter(|e| layout.has(e)) {
        match sys.run("systemctl", &["disable", "--now", e.unit]) {
            Ok(_) => rep.step(format!("{} stopped and disabled", e.what)),
            Err(err) => rep.warnings.push(err),
        }
        layout.remove_extra(e, &mut rep)?;
        if std::ptr::eq(e, &HELPER) {
            drop_blocks(sys, &mut rep);
        }
    }
    match sys.run("systemctl", &["disable", "--now", SERVICE]) {
        Ok(_) => rep.step("service stopped and disabled".to_string()),
        Err(e) => rep.warnings.push(e),
    }
    remove(&layout.unit(), &mut rep)?;
    if let Err(e) = sys.run("systemctl", &["daemon-reload"]) {
        rep.warnings.push(e);
    }
    remove(&layout.bin(), &mut rep)?;
    remove_dir(&layout.dropin_dir(), &mut rep)?;
    if layout.at(POWER_RULE).exists() {
        remove(&layout.at(POWER_RULE), &mut rep)?;
        if let Err(e) = sys.run("sh", &["-c", &power_now("root", "0400")]) {
            rep.warnings.push(e);
        }
    }
    if purge {
        remove_dir(&layout.data_dir(), &mut rep)?;
        remove_dir(&layout.at(crate::helper::ROLLBACK_DIR), &mut rep)?;
        remove_dir(&layout.config_dir(), &mut rep)?;
        remove(&layout.sysusers(), &mut rep)?;
        // userdel also drops the group and every membership in it.
        if sys.run("id", &["-u", USER]).is_ok() {
            sys.run("userdel", &[USER])?;
            rep.step(format!("removed the {USER} account"));
        }
        if sys.run("getent", &["group", USER]).is_ok() {
            sys.run("groupdel", &[USER])?;
            rep.step(format!("removed the {USER} group"));
        }
    } else {
        rep.step(format!(
            "kept history in {} and config in {} (uninstall --purge removes them)",
            layout.data_dir().display(),
            layout.config_dir().display()
        ));
    }
    Ok(rep)
}

/// Extra access root can give the service, one drop-in each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    /// The container engine's socket group: root-equivalent.
    Docker,
    /// The journal's group: reads every log on the host.
    Journal,
    /// The network watch helper: a separate unit with `CAP_NET_RAW`.
    Netwatch,
    /// The root helper for unit and power actions.
    Helper,
    /// The read-only health helper (SMART, pools, updates, UPS).
    Health,
    /// Group read access to RAPL energy counters (power draw).
    Power,
}

impl Grant {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "docker" => Some(Self::Docker),
            "journal" => Some(Self::Journal),
            "netwatch" => Some(Self::Netwatch),
            "helper" => Some(Self::Helper),
            "health" => Some(Self::Health),
            "power" => Some(Self::Power),
            _ => None,
        }
    }
    fn file(self) -> &'static str {
        match self {
            Self::Docker => "docker.conf",
            Self::Journal => "journal.conf",
            Self::Netwatch => "netwatch",
            Self::Helper => "helper",
            Self::Health => "health",
            Self::Power => "power",
        }
    }
    fn extra(self) -> Option<&'static Extra> {
        match self {
            Self::Netwatch => Some(&NETWATCH),
            Self::Helper => Some(&HELPER),
            Self::Health => Some(&HEALTH),
            _ => None,
        }
    }
}

/// Group that owns `path`, by name.
fn socket_group(sys: &mut dyn Sys, path: &Path, system: bool) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e} (is the engine installed and running?)", path.display()))?;
    if md.gid() == 0 && system {
        return Err(format!(
            "{} belongs to group root; giving the service the root group is not supported. \
             Use an engine socket with its own group (Docker's default is `docker`).",
            path.display()
        ));
    }
    let line = sys.run("getent", &["group", &md.gid().to_string()])?;
    let name = line.split(':').next().unwrap_or("").trim().to_owned();
    if name.is_empty() || !valid_user(&name) {
        return Err(format!("no usable group name for gid {}", md.gid()));
    }
    Ok(name)
}

/// `allow docker`: let the service read the container engine.
pub fn allow(
    layout: &Layout,
    sys: &mut dyn Sys,
    grant: Grant,
    socket: &Path,
    no_start: bool,
) -> Result<Report, String> {
    preflight(layout)?;
    let mut rep = Report { action: "allow", version: env!("CARGO_PKG_VERSION"), ..Default::default() };
    if let Some(e) = grant.extra() {
        rep.warnings.push(e.warning.into());
        layout.write_extra(e, &mut rep, "wrote")?;
        sys.run("systemctl", &["daemon-reload"])?;
        if no_start {
            sys.run("systemctl", &["enable", e.unit])?;
            rep.step("enabled at boot, not started".to_string());
        } else {
            sys.run("systemctl", &["enable", "--now", e.unit])?;
            rep.step(format!("{} started", e.what));
        }
        return Ok(rep);
    }
    if grant == Grant::Power {
        rep.warnings.push(
            "the service can now read the CPU's energy counters. The kernel keeps them root-only because \
             reading them very often can hint at what other programs compute; the node reads them every 10 s \
             (every second while the app watches) and keeps one-minute averages."
                .into(),
        );
        let path = layout.at(POWER_RULE);
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        write_file(&path, POWER_RULE_BODY, 0o644)?;
        rep.step(format!("wrote {}", path.display()));
        sys.run("sh", &["-c", &power_now(USER, "0440")])?;
        rep.step("energy counters readable by the service now".to_string());
        return Ok(rep);
    }
    let body = match grant {
        Grant::Netwatch | Grant::Helper | Grant::Health | Grant::Power => unreachable!("handled above"),
        Grant::Docker => {
            let group = socket_group(sys, socket, layout.is_system())?;
            rep.warnings.push(format!(
                "members of {group} can control the container engine, which is root-equivalent. \
                 The node only reads container lists and events, but a flaw in it could be used for more."
            ));
            format!(
                "# Written by `nightgrid-node allow docker`; removed by `nightgrid-node deny docker`.\n\
                 # Lets the service read containers and events through {}.\n\
                 # Group {group} can control the engine: this is root-equivalent access.\n\
                 [Service]\nSupplementaryGroups={group}\n",
                socket.display()
            )
        }
        Grant::Journal => {
            let dir = ["/var/log/journal", "/run/log/journal"]
                .into_iter()
                .map(|p| layout.at(p))
                .find(|p| p.is_dir())
                .ok_or("no journal directory (/var/log/journal or /run/log/journal)")?;
            let group = socket_group(sys, &dir, layout.is_system())?;
            rep.warnings.push(format!(
                "members of {group} can read every log on this host, which may include secrets that \
                 programs logged. The node only keeps sshd's login lines, through the same secret filter as the app."
            ));
            format!(
                "# Written by `nightgrid-node allow journal`; removed by `nightgrid-node deny journal`.\n\
                 # Lets the service read the system journal (for SSH logins).\n\
                 [Service]\nSupplementaryGroups={group}\n"
            )
        }
    };
    let path = layout.dropin_dir().join(grant.file());
    write_file(&path, &body, 0o644)?;
    rep.step(format!("wrote {}", path.display()));
    restart(sys, &mut rep, no_start)?;
    Ok(rep)
}

/// `deny docker`: take the access away again.
pub fn deny(layout: &Layout, sys: &mut dyn Sys, grant: Grant, no_start: bool) -> Result<Report, String> {
    preflight(layout)?;
    let mut rep = Report { action: "deny", version: env!("CARGO_PKG_VERSION"), ..Default::default() };
    if let Some(e) = grant.extra() {
        if !layout.has(e) {
            rep.step(format!("{} was not allowed", e.name));
            return Ok(rep);
        }
        if let Err(err) = sys.run("systemctl", &["disable", "--now", e.unit]) {
            rep.warnings.push(err);
        }
        layout.remove_extra(e, &mut rep)?;
        if std::ptr::eq(e, &HELPER) {
            drop_blocks(sys, &mut rep);
        }
        sys.run("systemctl", &["daemon-reload"])?;
        rep.step(format!("{} stopped", e.what));
        return Ok(rep);
    }
    if grant == Grant::Power {
        let path = layout.at(POWER_RULE);
        if !path.exists() {
            rep.step("power was not allowed".to_string());
            return Ok(rep);
        }
        remove(&path, &mut rep)?;
        sys.run("sh", &["-c", &power_now("root", "0400")])?;
        rep.step("energy counters root-only again".to_string());
        return Ok(rep);
    }
    let path = layout.dropin_dir().join(grant.file());
    if !path.exists() {
        rep.step(format!("{} was not allowed", grant.file().trim_end_matches(".conf")));
        return Ok(rep);
    }
    remove(&path, &mut rep)?;
    restart(sys, &mut rep, no_start)?;
    Ok(rep)
}

/// Removes the node's nftables table (blocks from `firewall.block`), if any.
fn drop_blocks(sys: &mut dyn Sys, rep: &mut Report) {
    let t = crate::firewall::TABLE;
    let script = format!("if command -v nft >/dev/null && nft list table inet {t} >/dev/null 2>&1; then nft delete table inet {t} && echo dropped; fi");
    if let Ok(out) = sys.run("sh", &["-c", &script]) {
        if out.contains("dropped") {
            rep.step("removed the node's firewall blocks".to_string());
        }
    }
}

fn restart(sys: &mut dyn Sys, rep: &mut Report, no_start: bool) -> Result<(), String> {
    sys.run("systemctl", &["daemon-reload"])?;
    if no_start {
        rep.step("takes effect at the next service start".to_string());
    } else if sys.run("systemctl", &["is-active", "--quiet", SERVICE]).is_ok() {
        sys.run("systemctl", &["restart", SERVICE])?;
        rep.step("service restarted".to_string());
    }
    Ok(())
}

fn remove(p: &Path, rep: &mut Report) -> Result<(), String> {
    match std::fs::remove_file(p) {
        Ok(()) => {
            rep.step(format!("removed {}", p.display()));
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

fn remove_dir(p: &Path, rep: &mut Report) -> Result<(), String> {
    match std::fs::remove_dir_all(p) {
        Ok(()) => {
            rep.step(format!("removed {}", p.display()));
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeSys {
        log: Vec<String>,
        users: Vec<&'static str>,
        account: bool,
        groups: bool,
    }

    impl Sys for FakeSys {
        fn run(&mut self, prog: &str, args: &[&str]) -> Result<String, String> {
            self.log.push(format!("{prog} {}", args.join(" ")));
            match (prog, args) {
                ("id", [_, u]) if *u == USER => {
                    if self.account {
                        Ok("990".into())
                    } else {
                        Err("no".into())
                    }
                }
                ("id", [_, u]) => {
                    if self.users.contains(u) {
                        Ok("1000".into())
                    } else {
                        Err("no".into())
                    }
                }
                ("getent", ["group", g]) if self.groups && g.parse::<u32>().is_ok() => Ok(format!("docker:x:{g}:")),
                ("getent", _) => Err("no".into()),
                ("systemd-sysusers", _) => {
                    self.account = true;
                    Ok(String::new())
                }
                ("userdel", _) => {
                    self.account = false;
                    Ok(String::new())
                }
                _ => Ok(String::new()),
            }
        }
    }

    fn setup() -> (tempfile::TempDir, Layout, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("download/nightgrid-node");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"\x7fELF fake").unwrap();
        let layout = Layout { root: d.path().join("root") };
        (d, layout, exe)
    }

    #[test]
    fn allow_and_deny_netwatch() {
        let (_d, layout, exe) = setup();
        let mut sys = FakeSys::default();
        install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        let rep = allow(&layout, &mut sys, Grant::Netwatch, Path::new("/x"), false).unwrap();
        assert!(rep.warnings[0].contains("CAP_NET_RAW"));
        let unit = std::fs::read_to_string(layout.netwatch_unit()).unwrap();
        assert!(
            unit.contains("CapabilityBoundingSet=CAP_NET_RAW")
                && unit.contains("RestrictAddressFamilies=AF_UNIX AF_PACKET")
        );
        assert!(sys.log.ends_with(&["systemctl enable --now nightgrid-node-netwatch".to_string()]));
        // A reinstall refreshes and restarts it.
        install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        assert!(sys.log.ends_with(&["systemctl restart nightgrid-node-netwatch".to_string()]));
        deny(&layout, &mut sys, Grant::Netwatch, false).unwrap();
        assert!(!layout.netwatch_unit().exists());
        assert!(sys.log.contains(&"systemctl disable --now nightgrid-node-netwatch".to_string()));
        assert_eq!(deny(&layout, &mut sys, Grant::Netwatch, false).unwrap().steps, ["netwatch was not allowed"]);
        // Uninstall takes it away too.
        allow(&layout, &mut sys, Grant::Netwatch, Path::new("/x"), true).unwrap();
        uninstall(&layout, &mut sys, false).unwrap();
        assert!(!layout.netwatch_unit().exists());
    }

    #[test]
    fn allow_and_deny_helper() {
        let (_d, layout, exe) = setup();
        let mut sys = FakeSys::default();
        install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        let cfg = std::fs::read_to_string(layout.config()).unwrap().replace("paths = []", "paths = [\"/srv/compose\"]");
        std::fs::write(layout.config(), cfg).unwrap();
        let rep = allow(&layout, &mut sys, Grant::Helper, Path::new("/x"), false).unwrap();
        let dropin =
            std::fs::read_to_string(layout.system_unit("nightgrid-node-helper@.service.d/rollback.conf")).unwrap();
        assert!(dropin.contains("ReadWritePaths=-/srv/compose"), "{dropin}");
        assert!(rep.warnings[0].contains("[actions] allow"));
        let sock = std::fs::read_to_string(layout.system_unit("nightgrid-node-helper.socket")).unwrap();
        assert!(
            sock.contains("SocketMode=0600")
                && sock.contains("SocketUser=nightgrid-node")
                && sock.contains("Accept=yes")
        );
        let svc = std::fs::read_to_string(layout.system_unit("nightgrid-node-helper@.service")).unwrap();
        assert!(svc.contains("ExecStart=/usr/local/bin/nightgrid-node helper") && svc.contains("NoNewPrivileges=yes"));
        assert!(sys.log.ends_with(&["systemctl enable --now nightgrid-node-helper.socket".to_string()]));
        install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        assert!(sys.log.ends_with(&["systemctl restart nightgrid-node-helper.socket".to_string()]));
        deny(&layout, &mut sys, Grant::Helper, false).unwrap();
        assert!(!layout.system_unit("nightgrid-node-helper.socket").exists());
        assert!(!layout.system_unit("nightgrid-node-helper@.service").exists());
        assert!(!layout.system_unit("nightgrid-node-helper@.service.d").exists());
        assert_eq!(deny(&layout, &mut sys, Grant::Helper, false).unwrap().steps, ["helper was not allowed"]);
        allow(&layout, &mut sys, Grant::Helper, Path::new("/x"), true).unwrap();
        uninstall(&layout, &mut sys, false).unwrap();
        assert!(!layout.system_unit("nightgrid-node-helper.socket").exists());
    }

    #[test]
    fn allow_and_deny_health() {
        let (_d, layout, exe) = setup();
        let mut sys = FakeSys::default();
        install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        let rep = allow(&layout, &mut sys, Grant::Health, Path::new("/x"), false).unwrap();
        assert!(rep.warnings[0].contains("read-only"));
        let sock = std::fs::read_to_string(layout.system_unit("nightgrid-node-health.socket")).unwrap();
        assert!(sock.contains("SocketMode=0600") && sock.contains("SocketUser=nightgrid-node"));
        let svc = std::fs::read_to_string(layout.system_unit("nightgrid-node-health@.service")).unwrap();
        assert!(
            svc.contains("ExecStart=/usr/local/bin/nightgrid-node health-helper")
                && svc.contains("ProtectSystem=strict")
        );
        assert!(sys.log.ends_with(&["systemctl enable --now nightgrid-node-health.socket".to_string()]));
        deny(&layout, &mut sys, Grant::Health, false).unwrap();
        assert!(!layout.system_unit("nightgrid-node-health.socket").exists());
        assert!(!layout.system_unit("nightgrid-node-health@.service").exists());
        allow(&layout, &mut sys, Grant::Health, Path::new("/x"), true).unwrap();
        uninstall(&layout, &mut sys, false).unwrap();
        assert!(!layout.system_unit("nightgrid-node-health.socket").exists());
    }

    #[test]
    fn allow_and_deny_power() {
        let (_d, layout, exe) = setup();
        let mut sys = FakeSys::default();
        install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        let rep = allow(&layout, &mut sys, Grant::Power, Path::new("/x"), false).unwrap();
        assert!(rep.warnings[0].contains("energy counters"));
        let rule = std::fs::read_to_string(layout.at(POWER_RULE)).unwrap();
        assert!(rule.contains("SUBSYSTEM==\"powercap\"") && rule.contains("chgrp nightgrid-node /sys%p/energy_uj"));
        assert!(sys.log.last().unwrap().contains("chgrp nightgrid-node"), "{:?}", sys.log);
        deny(&layout, &mut sys, Grant::Power, false).unwrap();
        assert!(!layout.at(POWER_RULE).exists());
        assert!(sys.log.last().unwrap().contains("chgrp root") && sys.log.last().unwrap().contains("0400"));
        assert_eq!(deny(&layout, &mut sys, Grant::Power, false).unwrap().steps, ["power was not allowed"]);
        allow(&layout, &mut sys, Grant::Power, Path::new("/x"), true).unwrap();
        uninstall(&layout, &mut sys, false).unwrap();
        assert!(!layout.at(POWER_RULE).exists());
    }

    #[test]
    fn install_then_reinstall_then_uninstall() {
        let (_d, layout, exe) = setup();
        let mut sys = FakeSys { users: vec!["alice"], ..Default::default() };
        let opts = InstallOptions { allow_users: vec!["alice".into()], no_start: false };
        let rep = install(&layout, &mut sys, &exe, &opts).unwrap();
        assert_eq!(rep.users, ["alice"]);
        assert_eq!(std::fs::read(layout.bin()).unwrap(), b"\x7fELF fake");
        let mode = std::fs::metadata(layout.bin()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert!(std::fs::read_to_string(layout.unit()).unwrap().contains("IPAddressDeny=any"));
        assert_eq!(std::fs::read_to_string(layout.config()).unwrap(), crate::config::TEMPLATE);
        assert!(sys.log.contains(&"usermod -a -G nightgrid-node alice".to_string()));
        assert!(sys.log.ends_with(&[
            "systemctl daemon-reload".to_string(),
            "systemctl enable nightgrid-node".to_string(),
            "systemctl restart nightgrid-node".to_string()
        ]));

        // Reinstall keeps an edited config and upgrades the binary.
        std::fs::write(layout.config(), "[metrics]\ninterval_secs = 5\n").unwrap();
        std::fs::write(&exe, b"\x7fELF newer").unwrap();
        install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        assert_eq!(std::fs::read(layout.bin()).unwrap(), b"\x7fELF newer");
        assert!(std::fs::read_to_string(layout.config()).unwrap().contains("interval_secs = 5"));

        // Plain uninstall keeps data and config.
        std::fs::create_dir_all(layout.data_dir()).unwrap();
        std::fs::write(layout.data_dir().join("node.db"), b"x").unwrap();
        uninstall(&layout, &mut sys, false).unwrap();
        assert!(!layout.bin().exists() && !layout.unit().exists());
        assert!(layout.data_dir().join("node.db").exists() && layout.config().exists());
        assert!(sys.account);

        // Purge removes everything, and running it again is fine.
        uninstall(&layout, &mut sys, true).unwrap();
        assert!(!layout.data_dir().exists() && !layout.config_dir().exists() && !layout.sysusers().exists());
        assert!(!sys.account);
        uninstall(&layout, &mut sys, true).unwrap();
    }

    #[test]
    fn refuses_bad_users_and_broken_config() {
        let (_d, layout, exe) = setup();
        let mut sys = FakeSys::default();
        for bad in ["-rf", "a b", "x;reboot", "", "nightgrid-node"] {
            let opts = InstallOptions { allow_users: vec![bad.into()], no_start: true };
            assert!(install(&layout, &mut sys, &exe, &opts).is_err(), "{bad:?}");
        }
        let opts = InstallOptions { allow_users: vec!["ghost".into()], no_start: true };
        assert!(install(&layout, &mut sys, &exe, &opts).unwrap_err().contains("no such user"));
        std::fs::write(layout.config(), "listen = 1\n").unwrap();
        assert!(install(&layout, &mut sys, &exe, &InstallOptions::default()).is_err());
    }

    #[test]
    fn allow_and_deny_docker() {
        let (d, layout, exe) = setup();
        let mut sys = FakeSys::default();
        install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        let sock = d.path().join("docker.sock");
        assert!(allow(&layout, &mut sys, Grant::Docker, &sock, false).unwrap_err().contains("docker.sock"));
        std::fs::write(&sock, b"").unwrap();
        // The fake getent knows no groups.
        assert!(allow(&layout, &mut sys, Grant::Docker, &sock, false).is_err());
        sys.groups = true;
        let r = allow(&layout, &mut sys, Grant::Docker, &sock, false).unwrap();
        assert!(r.warnings[0].contains("root-equivalent"));
        let body = std::fs::read_to_string(layout.dropin_dir().join("docker.conf")).unwrap();
        assert!(body.contains("SupplementaryGroups=docker\n"), "{body}");
        assert!(sys.log.iter().any(|l| l == "systemctl restart nightgrid-node"));
        deny(&layout, &mut sys, Grant::Docker, false).unwrap();
        assert!(!layout.dropin_dir().join("docker.conf").exists());
        assert!(deny(&layout, &mut sys, Grant::Docker, false).unwrap().steps[0].contains("not allowed"));
        uninstall(&layout, &mut sys, false).unwrap();
        assert!(!layout.dropin_dir().exists());
    }

    #[test]
    fn no_start_leaves_it_stopped() {
        let (_d, layout, exe) = setup();
        let mut sys = FakeSys::default();
        install(&layout, &mut sys, &exe, &InstallOptions { no_start: true, ..Default::default() }).unwrap();
        assert!(!sys.log.iter().any(|l| l.contains("restart")));
    }
}
