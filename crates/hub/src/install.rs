//! `nightgrid-hub install` / `uninstall` / `allow collect` / `deny collect`.
//! Same shape as the node's: idempotent, every step through [`Sys`], every
//! path through [`Layout`], so tests run the flow in a temporary directory.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use nightgrid_node::install::{valid_user, Sys};
use serde::Serialize;

pub const SERVICE: &str = "nightgrid-hub";
pub const COLLECT_SERVICE: &str = "nightgrid-hub-collect";
pub const USER: &str = "nightgrid-hub";
pub const UNIT: &str = include_str!("../../../dist/systemd/nightgrid-hub.service");
pub const COLLECT_UNIT: &str = include_str!("../../../dist/systemd/nightgrid-hub-collect.service");
pub const SYSUSERS: &str = "# Nightgrid Hub service account (no login shell, no password).\n\
                            u nightgrid-hub - \"Nightgrid Hub\" /var/lib/nightgrid-hub\n";

#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn system() -> Self {
        Self { root: PathBuf::from("/") }
    }
    pub fn at(&self, p: &str) -> PathBuf {
        self.root.join(p.trim_start_matches('/'))
    }
    pub fn bin(&self) -> PathBuf {
        self.at("/usr/local/bin/nightgrid-hub")
    }
    pub fn node_bin(&self) -> PathBuf {
        self.at("/usr/local/bin/nightgrid-node")
    }
    pub fn unit(&self, name: &str) -> PathBuf {
        self.at("/etc/systemd/system").join(format!("{name}.service"))
    }
    pub fn collect_dropin(&self) -> PathBuf {
        self.at("/etc/systemd/system/nightgrid-hub-collect.service.d/networks.conf")
    }
    pub fn sysusers(&self) -> PathBuf {
        self.at("/etc/sysusers.d/nightgrid-hub.conf")
    }
    pub fn config_dir(&self) -> PathBuf {
        self.at("/etc/nightgrid-hub")
    }
    pub fn config(&self) -> PathBuf {
        self.config_dir().join("config.toml")
    }
    pub fn data_dir(&self) -> PathBuf {
        self.at(crate::config::DEFAULT_DATA_DIR)
    }
    pub fn collect_dir(&self) -> PathBuf {
        self.at("/var/lib/nightgrid-hub-collect")
    }
    fn is_system(&self) -> bool {
        self.root == Path::new("/")
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub action: &'static str,
    pub version: &'static str,
    pub steps: Vec<String>,
    pub users: Vec<String>,
    pub warnings: Vec<String>,
    /// `install`, `allow collect`: the hub's id, for the members' pairing
    /// (`peers.set` and `hub.member.set`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hub_id: Option<String>,
    /// `allow collect`: the collector's certificate fingerprint, for the
    /// members' `peers.set`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// `allow collect`: where it answers members' pings (the address for
    /// their `peers.set`), once it does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listening: Option<String>,
}

impl Report {
    fn new(action: &'static str) -> Self {
        Self { action, version: env!("CARGO_PKG_VERSION"), ..Default::default() }
    }
    fn step(&mut self, s: impl Into<String>) {
        let s = s.into();
        nightgrid_node::info!("{s}");
        self.steps.push(s);
    }
}

fn preflight(layout: &Layout) -> Result<(), String> {
    if layout.is_system() {
        nightgrid_node::install::preflight(&nightgrid_node::install::Layout::system()).map_err(|e| {
            e.replace("nightgrid-node", "nightgrid-hub").replace("Nightgrid Node needs", "the hub needs")
        })?;
    }
    Ok(())
}

fn write_file(path: &Path, body: &str, mode: u32) -> Result<(), String> {
    let dir = path.parent().ok_or("no parent dir")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = path.with_extension("ngh-tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

fn copy_binary(from: &Path, to: &Path) -> Result<bool, String> {
    if let (Ok(a), Ok(b)) = (from.canonicalize(), to.canonicalize()) {
        if a == b {
            return Ok(false);
        }
    }
    let dir = to.parent().ok_or("no parent dir")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = to.with_extension("ngh-tmp");
    std::fs::copy(from, &tmp).map_err(|e| format!("copy to {}: {e}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, to).map_err(|e| format!("{}: {e}", to.display()))?;
    Ok(true)
}

fn remove(path: &Path, rep: &mut Report) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            rep.step(format!("removed {}", path.display()));
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn remove_dir(path: &Path, rep: &mut Report) -> Result<(), String> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {
            rep.step(format!("removed {}", path.display()));
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// `IPAddressAllow=` lines from `[collect] networks`.
fn collect_dropin(layout: &Layout) -> String {
    let nets = crate::config::Config::load(&layout.config()).map(|c| c.collect.networks).unwrap_or_default();
    let mut s = String::from(
        "# Written by `nightgrid-hub allow collect` from [collect] networks in /etc/nightgrid-hub/config.toml.\n\
         # Run that command again after changing the list.\n[Service]\n",
    );
    for n in nets.iter().filter(|n| nightgrid_node::firewall::parse_cidr(n).is_some()) {
        s.push_str(&format!("IPAddressAllow={}\n", n.trim()));
    }
    s
}

fn socket(layout: &Layout) -> PathBuf {
    crate::config::Config::load(&layout.config())
        .map(|c| c.server.socket)
        .unwrap_or_else(|_| PathBuf::from(nightgrid_node_proto::hub::DEFAULT_SOCKET))
}

fn wait_ready(layout: &Layout, rep: &mut Report) {
    let sock = socket(layout);
    for _ in 0..50 {
        if let Ok(c) = nightgrid_node::client::Client::connect(&sock, "nightgrid-hub install") {
            rep.hub_id = Some(c.welcome.node.node_id.clone());
            rep.step(format!("answering on {} (hub {})", sock.display(), c.welcome.node.node_id));
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    rep.warnings.push(format!("not answering on {} yet; see: journalctl -u {SERVICE}", sock.display()));
}

/// Is the state directory on an SD card? Constant small writes wear them out.
fn on_sd_card(sys: &mut dyn Sys) -> bool {
    sys.run("findmnt", &["-n", "-o", "SOURCE", "--target", "/var/lib"]).is_ok_and(|s| s.contains("mmcblk"))
}

pub fn install(
    layout: &Layout,
    sys: &mut dyn Sys,
    self_exe: &Path,
    users: &[String],
    no_start: bool,
) -> Result<Report, String> {
    preflight(layout)?;
    for u in users {
        if !valid_user(u) || u == USER {
            return Err(format!("not a usable login name: {u:?}"));
        }
    }
    if !layout.node_bin().exists() {
        return Err(format!(
            "{} is missing: install Nightgrid Node on this host first (the hub reads this host through it, \
             like any other member)",
            layout.node_bin().display()
        ));
    }
    let mut rep = Report::new("install");
    if copy_binary(self_exe, &layout.bin())? {
        rep.step(format!("installed {}", layout.bin().display()));
    } else {
        rep.step(format!("{} already in place", layout.bin().display()));
    }
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
    write_file(&layout.unit(SERVICE), UNIT, 0o644)?;
    rep.step(format!("wrote {}", layout.unit(SERVICE).display()));
    let collect = layout.unit(COLLECT_SERVICE).exists();
    if collect {
        write_file(&layout.unit(COLLECT_SERVICE), COLLECT_UNIT, 0o644)?;
        write_file(&layout.collect_dropin(), &collect_dropin(layout), 0o644)?;
        rep.step("updated the collector".to_string());
    }
    for u in users {
        sys.run("id", &["-u", u]).map_err(|_| format!("no such user: {u}"))?;
        sys.run("usermod", &["-a", "-G", USER, u])?;
        rep.users.push(u.clone());
        rep.step(format!("{u} may use the hub (from their next login)"));
    }
    if layout.is_system() && on_sd_card(sys) {
        rep.warnings.push("/var/lib is on an SD card. The hub writes every minute; an SSD lasts much longer.".into());
    }
    rep.warnings.push(
        "the hub keeps a copy of every member's monitoring data in /var/lib/nightgrid-hub; \
         full-disk encryption on this host is recommended."
            .into(),
    );
    sys.run("systemctl", &["daemon-reload"])?;
    sys.run("systemctl", &["enable", SERVICE])?;
    if no_start {
        rep.step("enabled at boot, not started".to_string());
    } else {
        sys.run("systemctl", &["restart", SERVICE])?;
        rep.step("service started".to_string());
        if layout.is_system() {
            wait_ready(layout, &mut rep);
        }
        if collect {
            match sys.run("systemctl", &["restart", COLLECT_SERVICE]) {
                Ok(_) => rep.step("collector restarted".to_string()),
                Err(e) => rep.warnings.push(e),
            }
        }
    }
    Ok(rep)
}

/// Reads the collector's fingerprint and address from the hub, waiting for
/// its first report.
fn collector_details(layout: &Layout, rep: &mut Report) {
    let sock = socket(layout);
    for _ in 0..75 {
        if let Ok(mut c) = nightgrid_node::client::Client::connect(&sock, "nightgrid-hub install") {
            rep.hub_id = Some(c.welcome.node.node_id.clone());
            if let Ok(v) = c.call(nightgrid_node_proto::hub::method::OVERVIEW, serde_json::Value::Null) {
                if let Some(fp) = v["fingerprint"].as_str() {
                    rep.fingerprint = Some(fp.to_owned());
                    rep.listening = v["listening"].as_str().map(str::to_owned);
                    rep.step(format!("collector fingerprint {fp}"));
                    return;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    rep.warnings.push(format!("the collector hasn't reported yet; see: journalctl -u {COLLECT_SERVICE}"));
}

/// Whether root started the collector (`allow collect`).
pub fn collect_allowed(layout: &Layout) -> bool {
    layout.unit(COLLECT_SERVICE).exists()
}

pub fn allow_collect(layout: &Layout, sys: &mut dyn Sys, no_start: bool) -> Result<Report, String> {
    preflight(layout)?;
    if !layout.unit(SERVICE).exists() {
        return Err("the hub isn't installed here (nightgrid-hub install first)".into());
    }
    let mut rep = Report::new("allow");
    rep.warnings.push(
        "the collector connects to the nodes the app makes members of this hub, over their node-to-node \
         link (mutual TLS, pinned certificates), and only to [collect] networks. It reads what each \
         member's root allows (`sync` in its [peers] serve) and can't change or run anything there."
            .into(),
    );
    write_file(&layout.unit(COLLECT_SERVICE), COLLECT_UNIT, 0o644)?;
    write_file(&layout.collect_dropin(), &collect_dropin(layout), 0o644)?;
    rep.step(format!("wrote {}", layout.unit(COLLECT_SERVICE).display()));
    sys.run("systemctl", &["daemon-reload"])?;
    if no_start {
        sys.run("systemctl", &["enable", COLLECT_SERVICE])?;
        rep.step("enabled at boot, not started".to_string());
    } else {
        sys.run("systemctl", &["enable", "--now", COLLECT_SERVICE])?;
        // Already running: pick up new networks.
        sys.run("systemctl", &["restart", COLLECT_SERVICE])?;
        rep.step("collector started".to_string());
        if layout.is_system() {
            collector_details(layout, &mut rep);
        }
    }
    Ok(rep)
}

pub fn deny_collect(layout: &Layout, sys: &mut dyn Sys) -> Result<Report, String> {
    preflight(layout)?;
    let mut rep = Report::new("deny");
    if layout.unit(COLLECT_SERVICE).exists() {
        if let Err(e) = sys.run("systemctl", &["disable", "--now", COLLECT_SERVICE]) {
            rep.warnings.push(e);
        }
        rep.step("collector stopped and disabled".to_string());
    }
    remove(&layout.unit(COLLECT_SERVICE), &mut rep)?;
    remove_dir(layout.collect_dropin().parent().unwrap_or(Path::new("/nonexistent")), &mut rep)?;
    sys.run("systemctl", &["daemon-reload"])?;
    Ok(rep)
}

pub fn uninstall(layout: &Layout, sys: &mut dyn Sys, purge: bool) -> Result<Report, String> {
    preflight(layout)?;
    let mut rep = deny_collect(layout, sys)?;
    rep.action = "uninstall";
    match sys.run("systemctl", &["disable", "--now", SERVICE]) {
        Ok(_) => rep.step("service stopped and disabled".to_string()),
        Err(e) => rep.warnings.push(e),
    }
    remove(&layout.unit(SERVICE), &mut rep)?;
    if let Err(e) = sys.run("systemctl", &["daemon-reload"]) {
        rep.warnings.push(e);
    }
    remove(&layout.bin(), &mut rep)?;
    if purge {
        remove_dir(&layout.data_dir(), &mut rep)?;
        remove_dir(&layout.collect_dir(), &mut rep)?;
        remove_dir(&layout.config_dir(), &mut rep)?;
        remove(&layout.sysusers(), &mut rep)?;
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
            "kept the store in {} and config in {} (uninstall --purge removes them)",
            layout.data_dir().display(),
            layout.config_dir().display()
        ));
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeSys(Vec<String>);

    impl Sys for FakeSys {
        fn run(&mut self, prog: &str, args: &[&str]) -> Result<String, String> {
            self.0.push(format!("{prog} {}", args.join(" ")));
            if prog == "id" && args.get(1) == Some(&"nobody-here") {
                return Err("no".into());
            }
            Ok(String::new())
        }
    }

    #[test]
    fn needs_the_node_then_installs_collects_and_removes() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout { root: d.path().to_path_buf() };
        let exe = d.path().join("nightgrid-hub-new");
        std::fs::write(&exe, b"bin").unwrap();
        let mut sys = FakeSys::default();
        let e = install(&l, &mut sys, &exe, &[], true).unwrap_err();
        assert!(e.contains("install Nightgrid Node"), "{e}");
        std::fs::create_dir_all(l.node_bin().parent().unwrap()).unwrap();
        std::fs::write(l.node_bin(), b"node").unwrap();
        let r = install(&l, &mut sys, &exe, &["alice".into()], true).unwrap();
        assert!(l.bin().exists() && l.config().exists() && l.unit(SERVICE).exists());
        assert_eq!(r.users, ["alice"]);
        assert!(sys.0.iter().any(|c| c == "usermod -a -G nightgrid-hub alice"));
        assert!(!l.unit(COLLECT_SERVICE).exists(), "the collector is a separate allow");
        assert!(install(&l, &mut sys, &exe, &["nightgrid-hub".into()], true).is_err());
        allow_collect(&l, &mut sys, true).unwrap();
        let drop = std::fs::read_to_string(l.collect_dropin()).unwrap();
        assert!(drop.contains("IPAddressAllow=100.64.0.0/10"), "{drop}");
        deny_collect(&l, &mut sys).unwrap();
        assert!(!l.unit(COLLECT_SERVICE).exists() && !l.collect_dropin().exists());
        uninstall(&l, &mut sys, true).unwrap();
        assert!(!l.bin().exists() && !l.config().exists() && l.node_bin().exists(), "the node stays");
    }
}
