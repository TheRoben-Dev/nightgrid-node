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
    pub fn sysusers(&self) -> PathBuf {
        self.at("/etc/sysusers.d/nightgrid-node.conf")
    }
    pub fn config_dir(&self) -> PathBuf {
        self.at("/etc/nightgrid-node")
    }
    pub fn config(&self) -> PathBuf {
        self.config_dir().join("config.toml")
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
    match sys.run("systemctl", &["disable", "--now", SERVICE]) {
        Ok(_) => rep.step("service stopped and disabled".to_string()),
        Err(e) => rep.warnings.push(e),
    }
    remove(&layout.unit(), &mut rep)?;
    if let Err(e) = sys.run("systemctl", &["daemon-reload"]) {
        rep.warnings.push(e);
    }
    remove(&layout.bin(), &mut rep)?;
    if purge {
        remove_dir(&layout.data_dir(), &mut rep)?;
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
    fn no_start_leaves_it_stopped() {
        let (_d, layout, exe) = setup();
        let mut sys = FakeSys::default();
        install(&layout, &mut sys, &exe, &InstallOptions { no_start: true, ..Default::default() }).unwrap();
        assert!(!sys.log.iter().any(|l| l.contains("restart")));
    }
}
