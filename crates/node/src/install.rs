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

use crate::compat::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

pub use crate::host::Init;

pub const SERVICE: &str = "nightgrid-node";
/// The service script for hosts with OpenRC instead of systemd.
pub const OPENRC_SCRIPT: &str = include_str!("../../../dist/openrc/nightgrid-node");
/// The service script for hosts with sysvinit (and start-stop-daemon).
pub const SYSVINIT_SCRIPT: &str = include_str!("../../../dist/sysvinit/nightgrid-node");
pub const USER: &str = "nightgrid-node";
pub const UNIT: &str = include_str!("../../../dist/systemd/nightgrid-node.service");
/// The opt-in network watch helper (`allow netwatch`).
pub const NETWATCH_UNIT: &str = include_str!("../../../dist/systemd/nightgrid-node-netwatch.service");
pub const NETWATCH_SERVICE: &str = "nightgrid-node-netwatch";
pub const CHECKS_UNIT: &str = include_str!("../../../dist/systemd/nightgrid-node-checks.service");
pub const CHECKS_SERVICE: &str = "nightgrid-node-checks";
/// The opt-in node-to-node link (`allow peers`).
pub const PEER_UNIT: &str = include_str!("../../../dist/systemd/nightgrid-node-peer.service");
pub const PEER_SERVICE: &str = "nightgrid-node-peer";
/// The opt-in status page (`allow statuspage`).
pub const STATUS_UNIT: &str = include_str!("../../../dist/systemd/nightgrid-node-status.service");
pub const STATUS_SERVICE: &str = "nightgrid-node-status";
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

/// Writes a drop-in's body from root's config.
type DropinFn = fn(&Layout) -> String;

/// An opt-in grant that installs units of its own rather than a drop-in.
struct Extra {
    name: &'static str,
    /// (file in /etc/systemd/system, contents); the first marks it installed.
    files: &'static [(&'static str, &'static str)],
    /// The unit to enable and start.
    unit: &'static str,
    warning: &'static str,
    what: &'static str,
    /// A drop-in (file name, body from the config) next to the last file's unit.
    dropin: Option<(&'static str, DropinFn)>,
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
    dropin: Some(("rollback.conf", helper_dropin)),
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
        let name = e.dropin.map_or("rollback.conf", |(n, _)| n);
        self.system_unit(&format!("{}.d", e.files[e.files.len() - 1].0)).join(name)
    }
    fn write_extra(&self, e: &Extra, rep: &mut Report, verb: &str, sys: &mut dyn Sys) -> Result<(), String> {
        let version = self.systemd_version();
        for (name, body) in e.files {
            let dropped = write_unit(&self.system_unit(name), body, version, &self.root, sys)?;
            rep.step(format!("{verb} {}", self.system_unit(name).display()));
            note_unsupported(rep, name, version, dropped);
        }
        if let Some((_, f)) = e.dropin {
            let path = self.extra_dropin(e);
            make_dirs(path.parent().unwrap_or(Path::new("/")))?;
            let a = crate::units::adapt(&f(self), version);
            write_file(&path, &a.text, 0o644)?;
            rep.step(format!("{verb} {}", path.display()));
            note_unsupported(rep, &path.to_string_lossy(), version, a.dropped);
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

const CHECKS: Extra = Extra {
    name: "checks",
    files: &[("nightgrid-node-checks.service", CHECKS_UNIT)],
    unit: CHECKS_SERVICE,
    warning: "the check runner can now open connections to the networks in [checks] networks (private ranges \
              by default) to run the app's service checks. It runs as the service's account with no \
              capabilities; the main service still has no network access. Run allow checks again after \
              changing the networks.",
    what: "check runner",
    dropin: Some(("networks.conf", checks_dropin)),
};

/// The runner may reach exactly root's networks.
fn checks_dropin(layout: &Layout) -> String {
    let nets = crate::config::Config::load(&layout.config()).map(|c| c.checks.networks).unwrap_or_default();
    let mut s = String::from(
        "# Written by `nightgrid-node allow checks` from [checks] networks in /etc/nightgrid-node/config.toml.\n\
         # Run that command again after changing the list.\n[Service]\n",
    );
    for n in nets.iter().filter(|n| crate::firewall::parse_cidr(n).is_some()) {
        s.push_str(&format!("IPAddressAllow={}\n", n.trim()));
    }
    s
}

const PEERS: Extra = Extra {
    name: "peers",
    files: &[("nightgrid-node-peer.service", PEER_UNIT)],
    unit: PEER_SERVICE,
    warning: "the node-to-node link now listens on [peers] listen (the tailnet address by default) and talks to \
              the nodes the app pairs with this one, over mutual TLS with pinned certificates. Nothing is \
              trusted until the app pairs a node. It may reach only [peers] networks and answers only what \
              [peers] serve allows; the main service still has no network access. Run allow peers again \
              after changing the networks.",
    what: "node-to-node link",
    dropin: Some(("networks.conf", peers_dropin)),
};

/// The link may reach (and be reached from) exactly root's networks.
fn peers_dropin(layout: &Layout) -> String {
    let nets = crate::config::Config::load(&layout.config()).map(|c| c.peers.networks).unwrap_or_default();
    let mut s = String::from(
        "# Written by `nightgrid-node allow peers` from [peers] networks in /etc/nightgrid-node/config.toml.\n\
         # Run that command again after changing the list.\n[Service]\n",
    );
    for n in nets.iter().filter(|n| crate::firewall::parse_cidr(n).is_some()) {
        s.push_str(&format!("IPAddressAllow={}\n", n.trim()));
    }
    s
}

const STATUSPAGE: Extra = Extra {
    name: "statuspage",
    files: &[("nightgrid-node-status.service", STATUS_UNIT)],
    unit: STATUS_SERVICE,
    warning: "a read-only status page now listens on [statuspage] listen (the tailnet address by default) for \
              [statuspage] networks. It is plain HTTP inside the tailnet's encryption, at a secret link: \
              anyone with the link and tailnet access can read this host's status, alerts and linked hosts. \
              Run deny statuspage to remove it and the link.",
    what: "status page",
    dropin: Some(("networks.conf", statuspage_dropin)),
};

fn statuspage_dropin(layout: &Layout) -> String {
    let nets = crate::config::Config::load(&layout.config()).map(|c| c.statuspage.networks).unwrap_or_default();
    let mut s = String::from(
        "# Written by `nightgrid-node allow statuspage` from [statuspage] networks in /etc/nightgrid-node/config.toml.\n\
         # Run that command again after changing the list.\n[Service]\n",
    );
    for n in nets.iter().filter(|n| crate::firewall::parse_cidr(n).is_some()) {
        s.push_str(&format!("IPAddressAllow={}\n", n.trim()));
    }
    s
}

const EXTRAS: [&Extra; 6] = [&NETWATCH, &HELPER, &HEALTH, &CHECKS, &PEERS, &STATUSPAGE];
pub const SYSUSERS: &str = "# Nightgrid Node service account (no login shell, no password).\n\
                            u nightgrid-node - \"Nightgrid Node\" /var/lib/nightgrid-node\n";

/// Keeps the status page's token (or makes one) and returns its link.
fn status_token(layout: &Layout, sys: &mut dyn Sys, rep: &mut Report) -> Result<String, String> {
    let path = layout.at(crate::statuspage::TOKEN_FILE);
    let old =
        std::fs::read_to_string(&path).ok().map(|t| t.trim().to_owned()).filter(|t| crate::statuspage::token_ok(t));
    let token = match old {
        Some(t) => {
            rep.step(format!("kept the link in {}", path.display()));
            t
        }
        None => {
            let t = crate::statuspage::new_token().map_err(|e| format!("random token: {e}"))?;
            write_file(&path, &format!("{t}\n"), 0o640)?;
            if layout.is_system() {
                sys.run("chgrp", &[USER, &path.to_string_lossy()])?;
            }
            rep.step(format!("wrote a new link to {}", path.display()));
            t
        }
    };
    let cfg = crate::config::Config::load(&layout.config()).unwrap_or_default();
    Ok(crate::statuspage::url(&cfg, &token))
}

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
    /// The init system. A test root without markers counts as systemd.
    pub fn init(&self) -> Init {
        match crate::host::init_at(&self.root) {
            Init::Other if !self.is_system() => Init::Systemd,
            i => i,
        }
    }
    /// The running systemd's version (a test root: `run/systemd/version`).
    pub fn systemd_version(&self) -> Option<u32> {
        if self.is_system() {
            crate::host::systemd_version()
        } else {
            std::fs::read_to_string(self.at("/run/systemd/version")).ok()?.trim().parse().ok()
        }
    }
    /// What kind of host this is, for the reports.
    pub fn facts(&self) -> crate::host::HostFacts {
        if self.is_system() {
            crate::host::facts()
        } else {
            crate::host::facts_at(&self.root, "x86_64", "test", self.systemd_version())
        }
    }
    /// `/etc/init.d/nightgrid-node` on hosts without systemd.
    pub fn init_script(&self) -> PathBuf {
        self.at("/etc/init.d/nightgrid-node")
    }
    /// A file under the root (for the account files and markers).
    pub fn path(&self, p: &str) -> PathBuf {
        self.at(p)
    }
}

/// The error for something that needs systemd on a host without it. `install
/// --json` and the others print it as `{"error":{"code":"no_systemd",...}}`.
pub fn no_systemd(what: &str, init: Init) -> String {
    format!(
        "{what} needs systemd, and this host's init is {} (Nightgrid Node installs on systemd, OpenRC and sysvinit \
         with start-stop-daemon; the extra access and the hub need systemd)",
        init.name()
    )
}

/// What a command that isn't on Windows yet says (docs/WINDOWS.md). JSON
/// errors carry it as `not_on_windows`.
pub fn not_on_windows(what: &str) -> String {
    format!("{what} is not available on Windows yet (see docs/WINDOWS.md for what comes when)")
}

/// The `code` for a failed command's JSON error: `no_systemd`,
/// `not_on_windows` (a command that comes in a later Windows milestone),
/// `not_root` or `failed`.
pub fn error_code(message: &str) -> &'static str {
    if message.contains(" needs systemd, and this host") {
        "no_systemd"
    } else if message.contains(" is not available on Windows yet") {
        "not_on_windows"
    } else if message.starts_with("run this as root") {
        "not_root"
    } else {
        "failed"
    }
}

/// `{"error":{"code":...,"message":...}}`, one line.
pub fn error_json(message: &str) -> String {
    serde_json::json!({ "error": { "code": error_code(message), "message": message } }).to_string()
}

/// Whether `prog` is on the PATH.
pub fn has(sys: &mut dyn Sys, prog: &str) -> bool {
    sys.run("sh", &["-c", &format!("command -v {prog} >/dev/null 2>&1")]).is_ok()
}

/// Whether `/etc/passwd` (or `/etc/group`) under `root` lists `name`.
fn listed(root: &Path, file: &str, name: &str) -> bool {
    std::fs::read_to_string(root.join(file))
        .map(|t| t.lines().any(|l| l.split(':').next() == Some(name)))
        .unwrap_or(false)
}

/// Makes the service account (no shell, no password) and its group:
/// `systemd-sysusers` with systemd, else `useradd`, else BusyBox `adduser`.
/// Returns the step to report.
#[allow(clippy::too_many_arguments)]
pub fn ensure_account(
    root: &Path,
    sys: &mut dyn Sys,
    init: Init,
    user: &str,
    home: &str,
    sysusers: &Path,
    sysusers_body: &str,
) -> Result<String, String> {
    if init == Init::Systemd && has(sys, "systemd-sysusers") {
        write_file(sysusers, sysusers_body, 0o644)?;
        sys.run("systemd-sysusers", &[&sysusers.to_string_lossy()])?;
        return Ok(format!("service account {user} ready"));
    }
    if listed(root, "etc/passwd", user) {
        return Ok(format!("service account {user} ready (already there)"));
    }
    let shell = ["/usr/sbin/nologin", "/sbin/nologin", "/bin/false"]
        .into_iter()
        .find(|p| root.join(p.trim_start_matches('/')).exists())
        .unwrap_or("/bin/false");
    if has(sys, "useradd") {
        if !listed(root, "etc/group", user) {
            sys.run("groupadd", &["-r", user])?;
        }
        sys.run("useradd", &["-r", "-M", "-d", home, "-s", shell, "-g", user, user])?;
    } else if has(sys, "adduser") {
        // BusyBox.
        if !listed(root, "etc/group", user) {
            sys.run("addgroup", &["-S", user])?;
        }
        sys.run("adduser", &["-S", "-D", "-H", "-h", home, "-s", shell, "-G", user, user])?;
    } else {
        return Err("no way to make the service account here (no systemd-sysusers, useradd or adduser)".into());
    }
    Ok(format!("made the service account {user}"))
}

/// Adds a login user to the service's group: `usermod`, else BusyBox `addgroup`.
pub fn add_to_group(sys: &mut dyn Sys, login: &str, group: &str) -> Result<(), String> {
    if has(sys, "usermod") {
        sys.run("usermod", &["-a", "-G", group, login]).map(|_| ())
    } else if has(sys, "addgroup") {
        sys.run("addgroup", &[login, group]).map(|_| ())
    } else {
        sys.run("gpasswd", &["-a", login, group]).map(|_| ())
    }
}

/// Writes a systemd unit fitted to the running systemd, creating the state
/// directories an old systemd won't. Returns the settings left out.
pub fn write_unit(
    path: &Path,
    body: &str,
    version: Option<u32>,
    root: &Path,
    sys: &mut dyn Sys,
) -> Result<Vec<String>, String> {
    let a = crate::units::adapt(body, version);
    write_file(path, &a.text, 0o644)?;
    for (dir, mode, user) in &a.state_dirs {
        let d = root.join(dir.trim_start_matches('/'));
        make_dirs(&d)?;
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(*mode)).map_err(|e| e.to_string())?;
        if root == Path::new("/") {
            sys.run("chown", &[&format!("{user}:{user}"), dir])?;
        }
    }
    Ok(a.dropped)
}

/// Records settings an old systemd lacks; for the link and the checks
/// runner that includes the kernel-enforced network limit, which is worth a
/// warning of its own.
fn note_unsupported(rep: &mut Report, unit: &str, version: Option<u32>, dropped: Vec<String>) {
    if dropped.iter().any(|d| d.starts_with("IPAddress")) {
        rep.warnings.push(format!(
            "systemd {} can't limit {unit} to its networks (IPAddressAllow needs 235); it only talks to the \
             nodes the app paired, over mutual TLS",
            version.unwrap_or(0)
        ));
    }
    for d in dropped {
        if !rep.unsupported.contains(&d) {
            rep.unsupported.push(d);
        }
    }
}

/// The commands install and uninstall run.
pub trait Sys {
    /// Runs a program; `Err` with its stderr when it fails.
    fn run(&mut self, prog: &str, args: &[&str]) -> Result<String, String>;

    /// Runs an init script that starts a daemon. Like `run`, but the output
    /// must not go through pipes this process waits on: the daemon (and
    /// OpenRC's `output_logger`) inherit them and would hold them open for
    /// as long as the service runs.
    fn run_starter(&mut self, prog: &str, args: &[&str]) -> Result<String, String> {
        self.run(prog, args)
    }
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

    fn run_starter(&mut self, prog: &str, args: &[&str]) -> Result<String, String> {
        // Output goes to an unlinked temp file, read back after the script exits.
        use crate::compat::fs::OpenOptionsExt;
        let path = std::env::temp_dir().join(format!("nightgrid-node-start.{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let _ = std::fs::remove_file(&path);
        let status = Command::new(prog)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(f.try_clone().map_err(|e| format!("temp file: {e}"))?)
            .stderr(f.try_clone().map_err(|e| format!("temp file: {e}"))?)
            .status()
            .map_err(|e| format!("{prog}: {e}"))?;
        use std::io::{Read, Seek};
        let mut out = String::new();
        let _ = f.seek(std::io::SeekFrom::Start(0)).and_then(|_| f.read_to_string(&mut out));
        if status.success() {
            Ok(out)
        } else {
            Err(format!("{prog} {}: {}", args.join(" "), out.trim()))
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
    /// `allow statuspage`: the page's secret link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `install`: what kind of host this is (`arch`, `assetArch`, `libc`,
    /// `init`, `initVersion`, `kernel`, `busybox`, `limits`).
    #[serde(flatten)]
    pub host: Option<crate::host::HostFacts>,
    /// systemd settings left out because this systemd is too old for them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unsupported: Vec<String>,
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

/// Creates `dir` and any missing parents as 0755. `create_dir_all` alone
/// takes the umask of the shell that ran sudo: with a 0077 or 0027 umask the
/// config directory came out 0700/0750 root:root and the service account
/// couldn't read its own config.
fn make_dirs(dir: &Path) -> Result<(), String> {
    let mut missing = Vec::new();
    let mut d = Some(dir);
    while let Some(p) = d {
        if p.as_os_str().is_empty() || p.exists() {
            break;
        }
        missing.push(p.to_path_buf());
        d = p.parent();
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for p in missing {
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("{}: {e}", p.display()))?;
    }
    Ok(())
}

/// Makes an existing directory the service reads from (its config directory)
/// traversable and listable by everyone, as /etc directories normally are.
/// Repairs installs made by 0.26.0 under a strict umask. Returns true if it
/// changed anything.
fn open_dir(dir: &Path) -> Result<bool, String> {
    let meta = std::fs::metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mode = meta.permissions().mode() & 0o7777;
    if mode & 0o755 == 0o755 {
        return Ok(false);
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(true)
}

fn write_file(path: &Path, body: &str, mode: u32) -> Result<(), String> {
    let dir = path.parent().ok_or("no parent dir")?;
    make_dirs(dir)?;
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
    make_dirs(to.parent().ok_or("no parent dir")?)?;
    let tmp = to.with_extension("ngn-tmp");
    std::fs::copy(from, &tmp).map_err(|e| format!("copy to {}: {e}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, to).map_err(|e| format!("{}: {e}", to.display()))?;
    Ok(true)
}

/// Root and systemd: for everything but install and uninstall, which also
/// work with OpenRC and sysvinit.
pub fn preflight(layout: &Layout) -> Result<(), String> {
    preflight_root(layout)?;
    match layout.init() {
        Init::Systemd => Ok(()),
        other => Err(no_systemd("this", other)),
    }
}

/// Only root.
pub fn preflight_root(layout: &Layout) -> Result<(), String> {
    if !layout.is_system() {
        return Ok(());
    }
    if !crate::compat::am_root() {
        return Err("run this as root (sudo nightgrid-node ...)".into());
    }
    Ok(())
}

pub fn install(layout: &Layout, sys: &mut dyn Sys, self_exe: &Path, opts: &InstallOptions) -> Result<Report, String> {
    preflight_root(layout)?;
    for u in &opts.allow_users {
        if !valid_user(u) {
            return Err(format!("not a usable login name: {u:?}"));
        }
    }
    let init = layout.init();
    if init == Init::Other {
        return Err(no_systemd("installing the service", init));
    }
    let mut rep = Report {
        action: "install",
        version: env!("CARGO_PKG_VERSION"),
        host: Some(layout.facts()),
        ..Default::default()
    };

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

    let step =
        ensure_account(&layout.root, sys, init, USER, crate::config::DEFAULT_DATA_DIR, &layout.sysusers(), SYSUSERS)?;
    rep.step(step);

    if !layout.config().exists() {
        write_file(&layout.config(), crate::config::TEMPLATE, 0o644)?;
        rep.step(format!("wrote {}", layout.config().display()));
    } else {
        crate::config::Config::load(&layout.config())?;
        rep.step(format!("kept {}", layout.config().display()));
    }
    if open_dir(&layout.config_dir())? {
        rep.step(format!("made {} readable to the service (was closed by the umask)", layout.config_dir().display()));
    }

    if init != Init::Systemd {
        return install_rc(layout, sys, init, opts, rep);
    }
    let version = layout.systemd_version();
    rep.unsupported = write_unit(&layout.unit(), UNIT, version, &layout.root, sys)?;
    rep.step(format!("wrote {}", layout.unit().display()));
    if !rep.unsupported.is_empty() {
        rep.warnings.push(format!(
            "systemd {} is older than some of the unit's sandbox settings; left out: {}",
            version.unwrap_or(0),
            rep.unsupported.join(", ")
        ));
    }
    let extras: Vec<&Extra> = EXTRAS.into_iter().filter(|e| layout.has(e)).collect();
    for e in &extras {
        layout.write_extra(e, &mut rep, "updated", sys)?;
    }

    add_users(sys, opts, &mut rep)?;

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

fn add_users(sys: &mut dyn Sys, opts: &InstallOptions, rep: &mut Report) -> Result<(), String> {
    for u in &opts.allow_users {
        sys.run("id", &["-u", u]).map_err(|_| format!("no such user: {u}"))?;
        add_to_group(sys, u, USER)?;
        rep.users.push(u.clone());
        rep.step(format!("{u} may use the node (from their next login)"));
    }
    Ok(())
}

/// The rest of `install` on a host with OpenRC or sysvinit: the same
/// account, config, socket and data directory, started by a service script
/// instead of a unit, and without systemd's sandbox.
fn install_rc(
    layout: &Layout,
    sys: &mut dyn Sys,
    init: Init,
    opts: &InstallOptions,
    mut rep: Report,
) -> Result<Report, String> {
    let script = if init == Init::Openrc { OPENRC_SCRIPT } else { SYSVINIT_SCRIPT };
    write_file(&layout.init_script(), script, 0o755)?;
    rep.step(format!("wrote {} ({})", layout.init_script().display(), init.name()));
    let data = layout.data_dir();
    make_dirs(&data)?;
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    if layout.is_system() {
        sys.run("chown", &[&format!("{USER}:{USER}"), &data.to_string_lossy()])?;
    }
    rep.warnings.push(format!(
        "no systemd here ({}): the service runs as {USER} without systemd's sandbox (no kernel-enforced network \
         ban or memory cap). It still opens no network ports. The extra access (allow ...) and the hub need systemd.",
        init.name()
    ));
    add_users(sys, opts, &mut rep)?;
    let script = layout.init_script();
    let script = script.to_string_lossy();
    match init {
        Init::Openrc => {
            sys.run("rc-update", &["add", SERVICE, "default"])?;
        }
        _ => enable_sysv(layout, sys)?,
    }
    if opts.no_start {
        rep.step("enabled at boot, not started".to_string());
        return Ok(rep);
    }
    let started = if init == Init::Openrc {
        // OpenRC's `restart` of a service it doesn't count as started can
        // trip over its own state ("already starting"); start it plainly
        // unless it runs.
        let running = sys.run("rc-service", &[SERVICE, "status"]).is_ok();
        sys.run_starter("rc-service", &[SERVICE, if running { "restart" } else { "start" }])
    } else {
        sys.run_starter(&script, &["restart"])
    };
    match started {
        Ok(_) => {}
        // Someone (or OpenRC itself) is starting it already: the socket
        // check below tells whether it came up.
        Err(e) if e.contains("already start") => rep.warnings.push(e),
        Err(e) => return Err(e),
    }
    rep.step("service started".to_string());
    if layout.is_system() {
        wait_ready(&mut rep);
    }
    Ok(rep)
}

/// Enables the sysvinit script at boot: `update-rc.d`, `chkconfig`, or the
/// rc*.d links by hand.
fn enable_sysv(layout: &Layout, sys: &mut dyn Sys) -> Result<(), String> {
    if has(sys, "update-rc.d") {
        return sys.run("update-rc.d", &[SERVICE, "defaults"]).map(|_| ());
    }
    if has(sys, "chkconfig") {
        return sys.run("chkconfig", &["--add", SERVICE]).map(|_| ());
    }
    let base = ["/etc", "/etc/rc.d"].into_iter().find(|b| layout.at(&format!("{b}/rc2.d")).is_dir());
    let Some(base) = base else {
        return Err("can't enable the service at boot: no update-rc.d, chkconfig or /etc/rc2.d".into());
    };
    for (lvl, name) in
        [("2", "S90"), ("3", "S90"), ("4", "S90"), ("5", "S90"), ("0", "K10"), ("1", "K10"), ("6", "K10")]
    {
        let dir = layout.at(&format!("{base}/rc{lvl}.d"));
        if dir.is_dir() {
            let link = dir.join(format!("{name}{SERVICE}"));
            let _ = std::fs::remove_file(&link);
            crate::compat::fs::symlink("../init.d/nightgrid-node", &link)
                .map_err(|e| format!("{}: {e}", link.display()))?;
        }
    }
    Ok(())
}

/// Takes the sysvinit/OpenRC service away again.
fn uninstall_rc(layout: &Layout, sys: &mut dyn Sys, init: Init, rep: &mut Report) {
    let script = layout.init_script();
    if !script.exists() {
        return;
    }
    let s = script.to_string_lossy().into_owned();
    let stopped = if init == Init::Openrc { sys.run("rc-service", &[SERVICE, "stop"]) } else { sys.run(&s, &["stop"]) };
    match stopped {
        Ok(_) => rep.step("service stopped".to_string()),
        Err(e) => rep.warnings.push(e),
    }
    let disabled = match init {
        Init::Openrc => sys.run("rc-update", &["del", SERVICE, "default"]).map(|_| ()),
        _ if has(sys, "update-rc.d") => {
            let _ = std::fs::remove_file(&script);
            sys.run("update-rc.d", &[SERVICE, "remove"]).map(|_| ())
        }
        _ if has(sys, "chkconfig") => sys.run("chkconfig", &["--del", SERVICE]).map(|_| ()),
        _ => {
            for base in ["/etc", "/etc/rc.d"] {
                for lvl in ["0", "1", "2", "3", "4", "5", "6"] {
                    for p in ["S90", "K10"] {
                        let _ = std::fs::remove_file(layout.at(&format!("{base}/rc{lvl}.d/{p}{SERVICE}")));
                    }
                }
            }
            Ok(())
        }
    };
    match disabled {
        Ok(()) => rep.step("disabled at boot".to_string()),
        Err(e) => rep.warnings.push(e),
    }
    let _ = remove(&script, rep);
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
    preflight_root(layout)?;
    let mut rep = Report { action: "uninstall", version: env!("CARGO_PKG_VERSION"), ..Default::default() };
    let init = layout.init();
    if init != Init::Systemd {
        uninstall_rc(layout, sys, init, &mut rep);
        remove(&layout.bin(), &mut rep)?;
        remove(&crate::upgrade::previous(layout), &mut rep)?;
        return finish_uninstall(layout, sys, purge, rep);
    }
    for e in EXTRAS.into_iter().filter(|e| layout.has(e)) {
        match sys.run("systemctl", &["disable", "--now", e.unit]) {
            Ok(_) => rep.step(format!("{} stopped and disabled", e.what)),
            Err(err) => rep.warnings.push(err),
        }
        layout.remove_extra(e, &mut rep)?;
        if std::ptr::eq(e, &HELPER) {
            drop_blocks(sys, &mut rep);
        }
        if std::ptr::eq(e, &STATUSPAGE) {
            remove(&layout.at(crate::statuspage::TOKEN_FILE), &mut rep)?;
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
    remove(&crate::upgrade::previous(layout), &mut rep)?;
    remove_dir(&layout.dropin_dir(), &mut rep)?;
    if layout.at(POWER_RULE).exists() {
        remove(&layout.at(POWER_RULE), &mut rep)?;
        if let Err(e) = sys.run("sh", &["-c", &power_now("root", "0400")]) {
            rep.warnings.push(e);
        }
    }
    finish_uninstall(layout, sys, purge, rep)
}

/// History, config and the account: kept, or gone with `--purge`.
fn finish_uninstall(layout: &Layout, sys: &mut dyn Sys, purge: bool, mut rep: Report) -> Result<Report, String> {
    if purge {
        remove_dir(&layout.data_dir(), &mut rep)?;
        remove_dir(&layout.at(crate::helper::ROLLBACK_DIR), &mut rep)?;
        remove_dir(&layout.at("/var/lib/nightgrid-node-peer"), &mut rep)?;
        remove_dir(&layout.config_dir(), &mut rep)?;
        remove(&layout.sysusers(), &mut rep)?;
        // userdel also drops the group and every membership in it. BusyBox
        // has deluser/delgroup instead (and maybe no getent).
        let busybox = !has(sys, "userdel");
        if sys.run("id", &["-u", USER]).is_ok() {
            sys.run(if busybox { "deluser" } else { "userdel" }, &[USER])?;
            rep.step(format!("removed the {USER} account"));
        }
        let group = if layout.at("/etc/group").exists() {
            listed(&layout.root, "etc/group", USER)
        } else {
            sys.run("getent", &["group", USER]).is_ok()
        };
        if group {
            sys.run(if busybox { "delgroup" } else { "groupdel" }, &[USER])?;
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
    /// The service check runner: a separate unit with network access to
    /// `[checks] networks` only.
    Checks,
    /// The node-to-node link: a separate unit, mutual TLS with paired nodes,
    /// network access to `[peers] networks` only.
    Peers,
    /// The read-only status page on the tailnet.
    StatusPage,
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
            "checks" => Some(Self::Checks),
            "peers" => Some(Self::Peers),
            "statuspage" => Some(Self::StatusPage),
            _ => None,
        }
    }
    /// Every grant, in the order `help` lists them.
    pub const ALL: [Grant; 9] = [
        Self::Docker,
        Self::Journal,
        Self::Netwatch,
        Self::Helper,
        Self::Health,
        Self::Power,
        Self::Checks,
        Self::Peers,
        Self::StatusPage,
    ];
    /// The name `allow` takes.
    pub fn name(self) -> &'static str {
        self.file().trim_end_matches(".conf")
    }
    /// The systemd unit a separate-unit grant runs as.
    pub fn unit(self) -> Option<&'static str> {
        self.extra().map(|e| e.unit)
    }
    /// Whether root has allowed it (its unit, rule or drop-in is installed).
    pub fn is_allowed(self, layout: &Layout) -> bool {
        match (self, self.extra()) {
            (_, Some(e)) => layout.has(e),
            (Self::Power, None) => layout.at(POWER_RULE).exists(),
            (_, None) => layout.dropin_dir().join(self.file()).exists(),
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
            Self::Checks => "checks",
            Self::Peers => "peers",
            Self::StatusPage => "statuspage",
        }
    }
    fn extra(self) -> Option<&'static Extra> {
        match self {
            Self::Netwatch => Some(&NETWATCH),
            Self::Helper => Some(&HELPER),
            Self::Health => Some(&HEALTH),
            Self::Checks => Some(&CHECKS),
            Self::Peers => Some(&PEERS),
            Self::StatusPage => Some(&STATUSPAGE),
            _ => None,
        }
    }
}

/// Rewrites the `serve = [...]` line of `[peers]` in the config text.
/// Comments and every other line are kept as they are.
pub fn edit_serve(text: &str, name: &str, add: bool) -> Result<(String, bool), String> {
    if !crate::config::PEER_SERVES.contains(&name) {
        return Err(format!("unknown {name:?} (one of {})", crate::config::PEER_SERVES.join(", ")));
    }
    let mut out = Vec::new();
    let (mut section, mut found, mut changed) = (String::new(), false, false);
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if section == "[peers]" && !found && add {
                // No serve line yet: the default is ["ping"].
                out.push(format!("serve = [\"ping\", \"{name}\"]"));
                found = true;
                changed = true;
            }
            section = t.split('#').next().unwrap_or("").trim().to_owned();
        }
        if section == "[peers]" && !found && t.starts_with("serve") && t[5..].trim_start().starts_with('=') {
            found = true;
            let v: toml::Value = toml::from_str(t).map_err(|e| format!("[peers] serve: {e}"))?;
            let mut list: Vec<String> = v
                .get("serve")
                .and_then(|x| x.as_array())
                .ok_or("[peers] serve is not a list")?
                .iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect();
            let had = list.iter().any(|x| x == name);
            if add && !had {
                list.push(name.to_owned());
            } else if !add && had {
                list.retain(|x| x != name);
            }
            changed = had != add;
            let items: Vec<String> = list.iter().map(|x| format!("\"{x}\"")).collect();
            out.push(format!("serve = [{}]", items.join(", ")));
            continue;
        }
        out.push(line.to_owned());
    }
    if section == "[peers]" && !found && add {
        out.push(format!("serve = [\"ping\", \"{name}\"]"));
        found = true;
        changed = true;
    }
    if !found && add {
        out.push(String::new());
        out.push("[peers]".into());
        out.push(format!("serve = [\"ping\", \"{name}\"]"));
        changed = true;
    }
    let mut s = out.join("\n");
    s.push('\n');
    Ok((s, changed))
}

/// `serve add|remove NAME`: changes what paired peers may ask this node,
/// then restarts the service and the link so both read it.
pub fn serve(layout: &Layout, sys: &mut dyn Sys, name: &str, add: bool) -> Result<Report, String> {
    preflight(layout)?;
    let mut rep = Report { action: "serve", version: env!("CARGO_PKG_VERSION"), ..Default::default() };
    let path = layout.config();
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (new, changed) = edit_serve(&text, name, add)?;
    if !changed {
        rep.step(format!("[peers] serve already {} {name:?}", if add { "lists" } else { "leaves out" }));
        return Ok(rep);
    }
    let cfg: crate::config::Config = toml::from_str(&new).map_err(|e| format!("config: {e}"))?;
    cfg.validate()?;
    write_file(&path, &new, 0o644)?;
    rep.step(format!("[peers] serve is now {:?}", cfg.peers.serve));
    if name == "sync" && add {
        rep.warnings.push(
            "the peer the app names with hub.member can now read this host's metrics, events, uptime, disk \
             usage and active alerts over the link. It can't change or run anything here."
                .into(),
        );
    }
    if layout.has(&PEERS) {
        if layout.is_system() {
            sys.run("systemctl", &["try-restart", SERVICE, PEER_SERVICE])?;
        }
        rep.step("restarted the service and the link (where running)".to_string());
    } else {
        if layout.is_system() {
            sys.run("systemctl", &["try-restart", SERVICE])?;
        }
        rep.step("restarted the service (where running)".to_string());
        if add {
            rep.warnings.push(
                "the node-to-node link is off, so no peer can ask for this yet. Turn it on with: \
                 sudo nightgrid-node allow peers"
                    .into(),
            );
        }
    }
    Ok(rep)
}

/// Group that owns `path`, by name.
fn socket_group(sys: &mut dyn Sys, path: &Path, system: bool) -> Result<String, String> {
    use crate::compat::fs::MetadataExt;
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
        layout.write_extra(e, &mut rep, "wrote", sys)?;
        if std::ptr::eq(e, &STATUSPAGE) {
            rep.url = Some(status_token(layout, sys, &mut rep)?);
        }
        sys.run("systemctl", &["daemon-reload"])?;
        if no_start {
            sys.run("systemctl", &["enable", e.unit])?;
            rep.step("enabled at boot, not started".to_string());
        } else {
            sys.run("systemctl", &["enable", "--now", e.unit])?;
            if [&CHECKS, &PEERS, &STATUSPAGE].iter().any(|x| std::ptr::eq(e, *x)) {
                // Already running: pick up new networks.
                sys.run("systemctl", &["restart", e.unit])?;
            }
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
        write_file(&path, POWER_RULE_BODY, 0o644)?;
        rep.step(format!("wrote {}", path.display()));
        sys.run("sh", &["-c", &power_now(USER, "0440")])?;
        rep.step("energy counters readable by the service now".to_string());
        return Ok(rep);
    }
    let body = match grant {
        Grant::Netwatch
        | Grant::Helper
        | Grant::Health
        | Grant::Power
        | Grant::Checks
        | Grant::Peers
        | Grant::StatusPage => {
            unreachable!("handled above")
        }
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
        if std::ptr::eq(e, &STATUSPAGE) {
            remove(&layout.at(crate::statuspage::TOKEN_FILE), &mut rep)?;
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
mod serve_tests {
    use super::edit_serve;

    #[test]
    fn edits_only_the_serve_line_of_peers() {
        let t = "[server]\nserve = [\"x\"] # not this one\n\n[peers]\n# what peers may ask\nserve = [\"ping\"]\nport = 7419\n";
        let (s, changed) = edit_serve(t, "sync", true).unwrap();
        assert!(changed);
        assert!(s.contains("[peers]\n# what peers may ask\nserve = [\"ping\", \"sync\"]\nport = 7419"), "{s}");
        assert!(s.contains("serve = [\"x\"] # not this one"), "other sections untouched");
        assert!(!edit_serve(&s, "sync", true).unwrap().1, "already there");
        let (back, changed) = edit_serve(&s, "sync", false).unwrap();
        assert!(changed && back.contains("serve = [\"ping\"]\nport"));
        assert!(edit_serve(t, "everything", true).is_err());
        let (s, _) = edit_serve("[peers]\nport = 1\n\n[ups]\n", "sync", true).unwrap();
        assert!(
            s.contains("[peers]\nport = 1\n\nserve = [\"ping\", \"sync\"]\n[ups]")
                || s.contains("serve = [\"ping\", \"sync\"]"),
            "{s}"
        );
        let (s, _) = edit_serve("", "sync", true).unwrap();
        assert!(s.contains("[peers]\nserve = [\"ping\", \"sync\"]"));
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
        /// Programs `command -v` doesn't find.
        missing: Vec<&'static str>,
    }

    impl Sys for FakeSys {
        fn run(&mut self, prog: &str, args: &[&str]) -> Result<String, String> {
            if let ("sh", ["-c", cmd]) = (prog, args) {
                if let Some(p) = cmd.strip_prefix("command -v ") {
                    let p = p.split_whitespace().next().unwrap_or("");
                    return if self.missing.contains(&p) { Err("not found".into()) } else { Ok(String::new()) };
                }
            }
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
                ("rc-service", [_, "status"]) => Err(" * status: stopped".into()),
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

    #[test]
    fn a_starter_that_leaves_a_daemon_behind_returns() {
        // The child keeps stdout open, as a daemon or OpenRC's logger does.
        let t = std::time::Instant::now();
        let out = RealSys.run_starter("sh", &["-c", "sleep 20 & echo started"]).unwrap();
        assert!(t.elapsed() < std::time::Duration::from_secs(10), "waited for the daemon");
        assert_eq!(out.trim(), "started");
        let err = RealSys.run_starter("sh", &["-c", "echo already starting >&2; exit 1"]).unwrap_err();
        assert!(err.contains("already starting"), "{err}");
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
    #[cfg_attr(windows, ignore = "the Linux installer")]
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
    #[cfg_attr(windows, ignore = "the Linux installer")]
    fn allow_and_deny_checks() {
        let (_d, layout, exe) = setup();
        let mut sys = FakeSys::default();
        install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        let rep = allow(&layout, &mut sys, Grant::Checks, Path::new("/x"), false).unwrap();
        assert!(rep.warnings[0].contains("no capabilities"));
        let unit = std::fs::read_to_string(layout.system_unit("nightgrid-node-checks.service")).unwrap();
        assert!(
            unit.contains("CapabilityBoundingSet=\n")
                && unit.contains("IPAddressDeny=any")
                && unit.contains("User=nightgrid-node")
        );
        let dropin = std::fs::read_to_string(layout.extra_dropin(&CHECKS)).unwrap();
        assert!(
            dropin.contains("IPAddressAllow=192.168.0.0/16\n") && dropin.contains("IPAddressAllow=::1/128\n"),
            "{dropin}"
        );
        assert!(dropin.ends_with("networks.conf") || layout.extra_dropin(&CHECKS).ends_with("networks.conf"));
        assert!(sys.log.ends_with(&[
            "systemctl enable --now nightgrid-node-checks".to_string(),
            "systemctl restart nightgrid-node-checks".to_string()
        ]));
        deny(&layout, &mut sys, Grant::Checks, false).unwrap();
        assert!(!layout.system_unit("nightgrid-node-checks.service").exists());
        assert!(!layout.extra_dropin(&CHECKS).exists());
    }

    #[test]
    #[cfg_attr(windows, ignore = "the Linux installer")]
    fn allow_and_deny_peers() {
        let (_d, layout, exe) = setup();
        let mut sys = FakeSys::default();
        install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        let rep = allow(&layout, &mut sys, Grant::Peers, Path::new("/x"), false).unwrap();
        assert!(rep.warnings[0].contains("pinned certificates"));
        let unit = std::fs::read_to_string(layout.system_unit("nightgrid-node-peer.service")).unwrap();
        assert!(unit.contains("CapabilityBoundingSet=\n") && unit.contains("IPAddressDeny=any"));
        assert!(unit.contains("ExecStart=/usr/local/bin/nightgrid-node peer\n"));
        let dropin = std::fs::read_to_string(layout.extra_dropin(&PEERS)).unwrap();
        assert!(dropin.contains("IPAddressAllow=100.64.0.0/10\n"), "{dropin}");
        assert!(sys.log.contains(&"systemctl enable --now nightgrid-node-peer".to_string()));
        deny(&layout, &mut sys, Grant::Peers, false).unwrap();
        assert!(!layout.system_unit("nightgrid-node-peer.service").exists());
        assert!(!layout.extra_dropin(&PEERS).exists());
    }

    #[test]
    #[cfg_attr(windows, ignore = "the Linux installer")]
    fn allow_and_deny_statuspage() {
        let (_d, layout, exe) = setup();
        let mut sys = FakeSys::default();
        install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        let cfg = std::fs::read_to_string(layout.config()).unwrap().replace(
            "listen = \"tailnet\"\n# Port for \"tailnet\".\nport = 7420",
            "listen = \"127.0.0.1:7420\"\n# Port for \"tailnet\".\nport = 7420",
        );
        std::fs::write(layout.config(), cfg).unwrap();
        let rep = allow(&layout, &mut sys, Grant::StatusPage, Path::new("/x"), false).unwrap();
        let url = rep.url.clone().unwrap();
        assert!(url.starts_with("http://127.0.0.1:7420/") && url.len() == "http://127.0.0.1:7420/".len() + 64, "{url}");
        let token = layout.at(crate::statuspage::TOKEN_FILE);
        use crate::compat::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&token).unwrap().permissions().mode() & 0o777, 0o640);
        // Again: same link.
        assert_eq!(allow(&layout, &mut sys, Grant::StatusPage, Path::new("/x"), false).unwrap().url.unwrap(), url);
        let unit = std::fs::read_to_string(layout.system_unit("nightgrid-node-status.service")).unwrap();
        assert!(
            unit.contains("IPAddressDeny=any")
                && unit.contains("ExecStart=/usr/local/bin/nightgrid-node status-page\n")
        );
        deny(&layout, &mut sys, Grant::StatusPage, false).unwrap();
        assert!(!token.exists(), "deny kills the link");
        assert!(!layout.system_unit("nightgrid-node-status.service").exists());
    }

    #[test]
    #[cfg_attr(windows, ignore = "the Linux installer")]
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
    #[cfg_attr(windows, ignore = "the Linux installer")]
    fn serve_without_the_link_says_how_to_turn_it_on() {
        let (_d, layout, exe) = setup();
        let mut sys = FakeSys::default();
        install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        let rep = serve(&layout, &mut sys, "sync", true).unwrap();
        assert!(std::fs::read_to_string(layout.config()).unwrap().contains(r#"serve = ["ping", "sync"]"#));
        assert!(rep.warnings.iter().any(|w| w.contains("sudo nightgrid-node allow peers")), "{:?}", rep.warnings);
        assert!(rep.steps.last().unwrap().contains("restarted the service (where"), "{:?}", rep.steps);
        allow(&layout, &mut sys, Grant::Peers, Path::new("/x"), true).unwrap();
        let rep = serve(&layout, &mut sys, "ups", true).unwrap();
        assert!(!rep.warnings.iter().any(|w| w.contains("allow peers")), "{:?}", rep.warnings);
        assert!(rep.steps.last().unwrap().contains("and the link"), "{:?}", rep.steps);
    }

    #[test]
    #[cfg_attr(windows, ignore = "the Linux installer")]
    fn install_opens_a_config_dir_the_umask_closed() {
        // 0.26.0 created /etc/nightgrid-node with the sudo shell's umask; under
        // 0077 the service account couldn't read its config. Reinstalling fixes it.
        let (_d, layout, exe) = setup();
        let mut sys = FakeSys::default();
        install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&layout.config_dir()), 0o755);
        std::fs::set_permissions(layout.config_dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let rep = install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        assert_eq!(mode(&layout.config_dir()), 0o755);
        assert!(rep.steps.iter().any(|s| s.contains("readable to the service")), "{:?}", rep.steps);
        let rep = install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        assert!(!rep.steps.iter().any(|s| s.contains("readable to the service")));
    }

    #[test]
    #[cfg_attr(windows, ignore = "the Linux installer")]
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

    #[test]
    #[cfg_attr(windows, ignore = "the Linux installer")]
    fn openrc_host_gets_a_service_script() {
        let (_d, layout, exe) = setup();
        std::fs::create_dir_all(layout.path("/run/openrc")).unwrap();
        let mut sys = FakeSys { users: vec!["alice"], ..Default::default() };
        let opts = InstallOptions { allow_users: vec!["alice".into()], no_start: false };
        let rep = install(&layout, &mut sys, &exe, &opts).unwrap();
        let script = std::fs::read_to_string(layout.init_script()).unwrap();
        assert!(
            script.starts_with("#!/sbin/openrc-run")
                && script.contains("command_user=\"nightgrid-node:nightgrid-node\"")
        );
        assert!(!layout.unit().exists(), "no systemd unit");
        assert_eq!(rep.host.as_ref().unwrap().init, "openrc");
        assert!(rep.warnings.iter().any(|w| w.contains("without systemd's sandbox")), "{:?}", rep.warnings);
        assert!(sys.log.contains(&"rc-update add nightgrid-node default".to_string()), "{:?}", sys.log);
        assert!(
            sys.log.ends_with(&[
                "rc-service nightgrid-node status".to_string(),
                "rc-service nightgrid-node start".to_string()
            ]),
            "{:?}",
            sys.log
        );
        // No systemd-sysusers here: useradd makes the account.
        assert!(sys.log.iter().any(|l| l.starts_with("useradd -r -M -d /var/lib/nightgrid-node")), "{:?}", sys.log);
        let json = serde_json::to_value(&rep).unwrap();
        assert_eq!((json["init"].as_str(), json["arch"].as_str()), (Some("openrc"), Some("x86_64")), "{json}");
        assert!(json["kernel"].is_string() && json["libc"].is_string());

        // The extra access needs systemd and says so in a way the app can tell.
        let e = allow(&layout, &mut sys, Grant::Peers, Path::new("/x"), false).unwrap_err();
        assert_eq!(error_code(&e), "no_systemd", "{e}");
        let j: serde_json::Value = serde_json::from_str(&error_json(&e)).unwrap();
        assert_eq!(j["error"]["code"], "no_systemd");

        let rep = uninstall(&layout, &mut sys, true).unwrap();
        assert!(!layout.init_script().exists() && !layout.bin().exists(), "{:?}", rep.steps);
        assert!(sys.log.contains(&"rc-update del nightgrid-node default".to_string()));
    }

    #[test]
    #[cfg_attr(windows, ignore = "the Linux installer")]
    fn busybox_sysvinit_host_without_shadow_tools() {
        let (_d, layout, exe) = setup();
        for d in ["/etc/init.d", "/etc/rc2.d", "/etc/rc3.d", "/etc/rc0.d"] {
            std::fs::create_dir_all(layout.path(d)).unwrap();
        }
        std::fs::create_dir_all(layout.path("/sbin")).unwrap();
        std::fs::write(layout.path("/sbin/start-stop-daemon"), "").unwrap();
        let mut sys = FakeSys {
            users: vec!["admin"],
            missing: vec!["systemd-sysusers", "useradd", "usermod", "userdel", "update-rc.d", "chkconfig"],
            ..Default::default()
        };
        let opts = InstallOptions { allow_users: vec!["admin".into()], no_start: false };
        let rep = install(&layout, &mut sys, &exe, &opts).unwrap();
        assert_eq!(rep.host.as_ref().unwrap().init, "sysvinit");
        let script = std::fs::read_to_string(layout.init_script()).unwrap();
        assert!(script.contains("start-stop-daemon -S -q -b -m -p"));
        assert!(sys.log.contains(&"addgroup -S nightgrid-node".to_string()), "{:?}", sys.log);
        assert!(sys.log.iter().any(|l| l.starts_with("adduser -S -D -H -h /var/lib/nightgrid-node")));
        assert!(sys.log.contains(&"addgroup admin nightgrid-node".to_string()), "{:?}", sys.log);
        let link = layout.path("/etc/rc2.d/S90nightgrid-node");
        assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("../init.d/nightgrid-node"));
        assert!(std::fs::read_link(layout.path("/etc/rc0.d/K10nightgrid-node")).is_ok());
        let restart = format!("{} restart", layout.init_script().display());
        assert!(sys.log.ends_with(&[restart]), "{:?}", sys.log);
        uninstall(&layout, &mut sys, false).unwrap();
        assert!(std::fs::read_link(&link).is_err() && !layout.init_script().exists());
    }

    #[test]
    #[cfg_attr(windows, ignore = "the Linux installer")]
    fn old_systemd_gets_a_unit_it_understands() {
        let (_d, layout, exe) = setup();
        std::fs::create_dir_all(layout.path("/run/systemd/system")).unwrap();
        std::fs::write(layout.path("/run/systemd/version"), "215\n").unwrap();
        let mut sys = FakeSys::default();
        let rep = install(&layout, &mut sys, &exe, &InstallOptions::default()).unwrap();
        let unit = std::fs::read_to_string(layout.unit()).unwrap();
        assert!(unit.contains("# needs systemd 235: IPAddressDeny=any") && unit.contains("\nMemoryLimit=64M\n"));
        assert!(rep.unsupported.iter().any(|u| u.starts_with("StateDirectory=")), "{:?}", rep.unsupported);
        assert!(layout.data_dir().is_dir(), "made the state directory itself");
        assert_eq!(rep.host.as_ref().unwrap().init_version, Some(215));
        // The link's network limit can't be enforced: said out loud.
        let rep = allow(&layout, &mut sys, Grant::Peers, Path::new("/x"), true).unwrap();
        assert!(rep.warnings.iter().any(|w| w.contains("IPAddressAllow needs 235")), "{:?}", rep.warnings);
        let dropin = std::fs::read_to_string(layout.extra_dropin(&PEERS)).unwrap();
        assert!(dropin.contains("# needs systemd 235: IPAddressAllow="), "{dropin}");
    }

    #[test]
    fn error_codes_for_the_app() {
        assert_eq!(error_code(&no_systemd("installing the service", Init::Other)), "no_systemd");
        assert_eq!(error_code("run this as root (sudo nightgrid-node ...)"), "not_root");
        assert_eq!(error_code("usermod: boom"), "failed");
        assert_eq!(error_code(&not_on_windows("the fix menu")), "not_on_windows");
    }
}
