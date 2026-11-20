//! `nightgrid-node upgrade` (N8a): installs a new binary only if its SHA-256
//! matches what the app got from the release's `SHA256SUMS`, checks that the
//! new service comes up and stays up, and puts the previous binary back by
//! itself when it doesn't.
//!
//! The node has no network, so the app downloads the release, checks its
//! provenance, copies the binary over SSH and runs this with the expected
//! checksum (decision N-042). The new binary installs itself (its units and
//! drop-ins are the ones it ships), so a revert runs the previous binary's
//! own `install`, which restores its units too.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::install::{Layout, Sys, SERVICE};

/// Larger files are refused (the static binary is about 5 MB).
pub const MAX_SIZE: u64 = 64 * 1024 * 1024;
/// How long the new service must stay up without a restart.
pub const SETTLE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeReport {
    pub action: &'static str,
    pub from: String,
    pub to: String,
    pub ok: bool,
    /// The previous binary was put back.
    pub reverted: bool,
    pub steps: Vec<String>,
    pub error: Option<String>,
}

impl UpgradeReport {
    pub(crate) fn step(&mut self, s: impl Into<String>) {
        let s = s.into();
        crate::info!("{s}");
        self.steps.push(s);
    }
}

#[derive(Debug, Clone, Default)]
pub struct UpgradeOptions {
    /// Lowercase hex SHA-256 the file must have.
    pub sha256: String,
    pub allow_downgrade: bool,
}

/// Checks the running service after an install.
pub trait Probe {
    /// `Ok` once the service answers with `version` and stays up for the
    /// settle time without restarting.
    fn healthy(&mut self, version: &str) -> Result<(), String>;
}

/// "0.18.0" → (0, 18, 0).
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().split('.');
    let p = |s: Option<&str>| s.and_then(|x| x.split(['-', '+']).next()).and_then(|x| x.parse().ok());
    let r = (p(it.next())?, p(it.next())?, p(it.next())?);
    it.next().is_none().then_some(r)
}

/// The version from `nightgrid-node --version` output.
pub fn version_from_output(out: &str) -> Option<String> {
    let w: Vec<&str> = out.split_whitespace().collect();
    match w.as_slice() {
        ["nightgrid-node", v, ..] if parse_version(v).is_some() => Some((*v).to_owned()),
        _ => None,
    }
}

pub fn valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

pub fn sha256_file(p: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn is_elf(p: &Path) -> bool {
    let mut magic = [0u8; 4];
    std::fs::File::open(p).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && magic == *b"\x7fELF"
}

pub fn staged(layout: &Layout) -> PathBuf {
    layout.bin().with_file_name(".nightgrid-node.new")
}

pub fn previous(layout: &Layout) -> PathBuf {
    layout.bin().with_file_name("nightgrid-node.prev")
}

fn copy_exec(from: &Path, to: &Path) -> Result<(), String> {
    use crate::compat::fs::PermissionsExt;
    let tmp = to.with_extension("ngn-tmp");
    std::fs::copy(from, &tmp).map_err(|e| format!("copy to {}: {e}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, to).map_err(|e| format!("{}: {e}", to.display()))
}

/// Verifies, installs, checks, and reverts on failure. `Err` only before
/// anything was changed; later problems come back as `ok: false`.
pub fn upgrade(
    layout: &Layout,
    sys: &mut dyn Sys,
    probe: &mut dyn Probe,
    new_bin: &Path,
    opts: &UpgradeOptions,
) -> Result<UpgradeReport, String> {
    let current = env!("CARGO_PKG_VERSION");
    let mut rep = UpgradeReport { action: "upgrade", from: current.into(), ..Default::default() };
    if !valid_sha256(&opts.sha256) {
        return Err("--sha256 needs the 64 lowercase hex digits from the release's SHA256SUMS".into());
    }
    let meta = std::fs::metadata(new_bin).map_err(|e| format!("{}: {e}", new_bin.display()))?;
    if !meta.is_file() || meta.len() > MAX_SIZE {
        return Err(format!("{} isn't a binary of a plausible size", new_bin.display()));
    }
    if !layout.bin().exists() {
        return Err("the node isn't installed here; use install".into());
    }
    // Stage a copy next to the installed binary and check that copy, so the
    // file that runs is the file that was hashed.
    let stage = staged(layout);
    copy_exec(new_bin, &stage)?;
    let fail = |stage: &Path, e: String| {
        let _ = std::fs::remove_file(stage);
        Err(e)
    };
    let got = sha256_file(&stage)?;
    if got != opts.sha256 {
        return fail(&stage, format!("checksum mismatch: expected {}, got {got}; nothing was changed", opts.sha256));
    }
    rep.step(format!("checksum ok ({}…)", &got[..16]));
    if !is_elf(&stage) {
        return fail(&stage, "not an executable for Linux; nothing was changed".into());
    }
    let out = match sys.run(&stage.to_string_lossy(), &["--version"]) {
        Ok(o) => o,
        Err(e) => return fail(&stage, format!("the new binary doesn't run here ({e}); wrong architecture?")),
    };
    let Some(to) = version_from_output(&out) else {
        return fail(&stage, format!("not a nightgrid-node binary (it says {:?})", out.trim()));
    };
    rep.to = to.clone();
    if parse_version(&to) < parse_version(current) && !opts.allow_downgrade {
        return fail(&stage, format!("{to} is older than {current}; pass --allow-downgrade to go back"));
    }
    rep.step(format!("new binary runs: nightgrid-node {to}"));

    copy_exec(&layout.bin(), &previous(layout))?;
    rep.step(format!("kept {current} as {}", previous(layout).display()));

    let installed = sys.run(&stage.to_string_lossy(), &["install", "--json"]);
    let _ = std::fs::remove_file(&stage);
    let check = installed.and_then(|_| probe.healthy(&to));
    match check {
        Ok(()) => {
            rep.ok = true;
            rep.step(format!("{SERVICE} {to} is up and stayed up"));
        }
        Err(e) => {
            rep.error = Some(e.clone());
            rep.step(format!("{to} failed: {e}; putting {current} back"));
            let back = sys
                .run(&previous(layout).to_string_lossy(), &["install", "--json"])
                .and_then(|_| probe.healthy(current));
            match back {
                Ok(()) => {
                    rep.reverted = true;
                    rep.step(format!("reverted: {SERVICE} {current} is up"));
                }
                Err(e2) => {
                    rep.step(format!("revert failed too: {e2}"));
                    rep.error = Some(format!(
                        "{e}; the revert failed too ({e2}); run: sudo {} install",
                        previous(layout).display()
                    ));
                }
            }
        }
    }
    Ok(rep)
}

/// The real check: the socket answers with the version, then the service
/// stays active without a restart for [`SETTLE`].
pub struct SocketProbe {
    pub socket: PathBuf,
}

fn n_restarts(sys: &mut dyn Sys) -> Option<(String, u64)> {
    let out = sys.run("systemctl", &["show", "-p", "ActiveState,NRestarts", SERVICE]).ok()?;
    let get = |k: &str| out.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('=')).unwrap_or("").to_owned();
    Some((get("ActiveState"), get("NRestarts").parse().unwrap_or(0)))
}

pub(crate) fn answers(socket: &Path, version: &str, within: Duration) -> Result<(), String> {
    let start = Instant::now();
    let mut last = String::from("no answer");
    while start.elapsed() < within {
        match crate::client::Client::connect(socket, "nightgrid-node upgrade") {
            Ok(c) if c.welcome.node.version == version => return Ok(()),
            Ok(c) => last = format!("still answering as {}", c.welcome.node.version),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!("not answering as {version} on {} ({last})", socket.display()))
}

impl Probe for SocketProbe {
    fn healthy(&mut self, version: &str) -> Result<(), String> {
        let mut sys = crate::install::RealSys;
        answers(&self.socket, version, Duration::from_secs(20))?;
        let before = n_restarts(&mut sys).map_or(0, |r| r.1);
        std::thread::sleep(SETTLE);
        match n_restarts(&mut sys) {
            Some((state, n)) if state == "active" && n == before => {}
            Some((state, n)) => return Err(format!("service {state} after start, {} restarts", n - before.min(n))),
            None => return Err("systemctl show failed".into()),
        }
        answers(&self.socket, version, Duration::from_secs(5))
    }
}

/// Tells the running node what happened, for its timeline (root only).
pub fn note(socket: &Path, rep: &UpgradeReport) {
    let Ok(mut c) = crate::client::Client::connect(socket, "nightgrid-node upgrade") else { return };
    let v = serde_json::json!({
        "from": rep.from, "to": rep.to, "ok": rep.ok, "reverted": rep.reverted, "error": rep.error,
    });
    let _ = c.call(nightgrid_node_proto::method::UPGRADE_NOTE, v);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::Layout;

    #[test]
    fn versions() {
        assert_eq!(parse_version("0.18.0"), Some((0, 18, 0)));
        assert_eq!(parse_version("1.2.3-rc1"), Some((1, 2, 3)));
        assert!(parse_version("1.2").is_none() && parse_version("x.1.2").is_none());
        assert!(parse_version("0.9.0") < parse_version("0.18.0"));
        assert_eq!(version_from_output("nightgrid-node 0.19.0 (protocol 2)\n").as_deref(), Some("0.19.0"));
        assert!(version_from_output("bash 5.2").is_none());
        assert!(valid_sha256(&"a".repeat(64)) && !valid_sha256(&"A".repeat(64)) && !valid_sha256("ab"));
    }

    struct FakeSys {
        log: Vec<String>,
        version_out: String,
        fail_install_of_new: bool,
    }
    impl Sys for FakeSys {
        fn run(&mut self, prog: &str, args: &[&str]) -> Result<String, String> {
            self.log.push(format!("{} {}", Path::new(prog).file_name().unwrap().to_string_lossy(), args.join(" ")));
            if args == ["--version"] {
                return Ok(self.version_out.clone());
            }
            if prog.ends_with(".new") && self.fail_install_of_new {
                return Err("boom".into());
            }
            Ok(String::new())
        }
    }
    struct FakeProbe(Vec<String>, bool);
    impl Probe for FakeProbe {
        fn healthy(&mut self, v: &str) -> Result<(), String> {
            self.0.push(v.to_owned());
            if self.1 && v != env!("CARGO_PKG_VERSION") {
                Err("crashed".into())
            } else {
                Ok(())
            }
        }
    }

    fn setup() -> (tempfile::TempDir, Layout, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout { root: dir.path().join("root") };
        std::fs::create_dir_all(layout.bin().parent().unwrap()).unwrap();
        std::fs::write(layout.bin(), b"\x7fELFold").unwrap();
        let new = dir.path().join("upload");
        std::fs::write(&new, b"\x7fELFnew").unwrap();
        let sum = sha256_file(&new).unwrap();
        (dir, layout, new, sum)
    }

    #[test]
    fn verifies_before_touching_anything() {
        let (_d, layout, new, _sum) = setup();
        let mut sys = FakeSys { log: vec![], version_out: "nightgrid-node 99.0.0".into(), fail_install_of_new: false };
        let mut probe = FakeProbe(vec![], false);
        let opts = UpgradeOptions { sha256: "0".repeat(64), allow_downgrade: false };
        let e = upgrade(&layout, &mut sys, &mut probe, &new, &opts).unwrap_err();
        assert!(e.contains("checksum mismatch"), "{e}");
        assert!(sys.log.is_empty() && !staged(&layout).exists() && !previous(&layout).exists());
    }

    #[test]
    fn refuses_downgrades_and_strangers() {
        let (_d, layout, new, sum) = setup();
        let opts = UpgradeOptions { sha256: sum, allow_downgrade: false };
        let mut probe = FakeProbe(vec![], false);
        let mut sys = FakeSys { log: vec![], version_out: "nightgrid-node 0.0.1".into(), fail_install_of_new: false };
        assert!(upgrade(&layout, &mut sys, &mut probe, &new, &opts).unwrap_err().contains("older"));
        sys.version_out = "something else".into();
        assert!(upgrade(&layout, &mut sys, &mut probe, &new, &opts).unwrap_err().contains("not a nightgrid-node"));
        assert!(!previous(&layout).exists());
    }

    #[test]
    fn upgrades_and_reverts() {
        let (_d, layout, new, sum) = setup();
        let opts = UpgradeOptions { sha256: sum, allow_downgrade: false };
        let mut sys = FakeSys { log: vec![], version_out: "nightgrid-node 99.0.0".into(), fail_install_of_new: false };
        let mut probe = FakeProbe(vec![], false);
        let r = upgrade(&layout, &mut sys, &mut probe, &new, &opts).unwrap();
        assert!(r.ok && !r.reverted, "{r:?}");
        assert_eq!(r.to, "99.0.0");
        assert_eq!(sys.log, [".nightgrid-node.new --version", ".nightgrid-node.new install --json"]);
        assert_eq!(std::fs::read(previous(&layout)).unwrap(), b"\x7fELFold");
        assert!(!staged(&layout).exists());

        let mut sys = FakeSys { log: vec![], version_out: "nightgrid-node 99.0.0".into(), fail_install_of_new: false };
        let mut probe = FakeProbe(vec![], true);
        let r = upgrade(&layout, &mut sys, &mut probe, &new, &opts).unwrap();
        assert!(!r.ok && r.reverted, "{r:?}");
        assert_eq!(r.error.as_deref(), Some("crashed"));
        assert_eq!(sys.log.last().unwrap(), "nightgrid-node.prev install --json");
        assert_eq!(probe.0, ["99.0.0", env!("CARGO_PKG_VERSION")]);

        let mut sys = FakeSys { log: vec![], version_out: "nightgrid-node 99.0.0".into(), fail_install_of_new: true };
        let mut probe = FakeProbe(vec![], false);
        let r = upgrade(&layout, &mut sys, &mut probe, &new, &opts).unwrap();
        assert!(!r.ok && r.reverted && r.error.as_deref() == Some("boom"), "{r:?}");
    }
}
