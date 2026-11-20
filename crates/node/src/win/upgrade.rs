//! `nightgrid-node upgrade FILE --sha256 HEX` on Windows (W2b): the same
//! checks and self-revert as on Linux (decision N-042), with the Service
//! Control Manager in place of systemd.
//!
//! The new exe is copied next to the installed one and that copy is hashed,
//! so the file that runs is the file that was checked. It must be a PE file
//! that runs and says it is `nightgrid-node`. The installed exe is kept as
//! `nightgrid-node.prev.exe`; the new one installs itself (`install`
//! stops the service, replaces the binary and starts it). The service must
//! then answer on its pipe with the new version and keep the same process
//! for the settle time; otherwise the previous exe installs itself again.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::upgrade::{
    answers, parse_version, sha256_file, valid_sha256, version_from_output, UpgradeOptions, UpgradeReport, MAX_SIZE,
    SETTLE,
};

use super::{scm, SERVICE};

pub fn staged() -> PathBuf {
    super::bin_dir().join(".nightgrid-node.new.exe")
}

pub fn previous() -> PathBuf {
    super::bin_dir().join("nightgrid-node.prev.exe")
}

/// `MZ` at the start: a Windows executable.
fn is_pe(p: &Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 2];
    std::fs::File::open(p).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && magic == *b"MZ"
}

fn run(exe: &Path, args: &[&str]) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let out = Command::new(exe)
        .args(args)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        Ok(text)
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = err.lines().chain(text.lines()).map(str::trim).find(|l| !l.is_empty()).unwrap_or("failed");
        Err(crate::events::tidy(msg, 300))
    }
}

/// The service answers as `version`, then keeps running in the same
/// process for [`SETTLE`] (a crash would bring a new pid or a stop).
fn healthy(pipe: &Path, version: &str) -> Result<(), String> {
    answers(pipe, version, Duration::from_secs(30))?;
    let before = scm::status(SERVICE)?.ok_or("the service is gone")?;
    std::thread::sleep(SETTLE);
    let after = scm::status(SERVICE)?.ok_or("the service is gone")?;
    if !after.running() || after.pid != before.pid {
        return Err(format!("the service {} after starting (pid {} → {})", after.word(), before.pid, after.pid));
    }
    answers(pipe, version, Duration::from_secs(5))
}

/// Verifies, installs, checks, and reverts on failure. `Err` only before
/// anything was changed; later problems come back as `ok: false`.
pub fn upgrade(new_bin: &Path, opts: &UpgradeOptions, pipe: &Path) -> Result<UpgradeReport, String> {
    super::install::preflight()?;
    let current = env!("CARGO_PKG_VERSION");
    let mut rep = UpgradeReport { action: "upgrade", from: current.into(), ..Default::default() };
    if !valid_sha256(&opts.sha256) {
        return Err("--sha256 needs the 64 lowercase hex digits from the release's SHA256SUMS".into());
    }
    let meta = std::fs::metadata(new_bin).map_err(|e| format!("{}: {e}", new_bin.display()))?;
    if !meta.is_file() || meta.len() > MAX_SIZE {
        return Err(format!("{} isn't a binary of a plausible size", new_bin.display()));
    }
    let bin = super::installed_binary();
    if !bin.exists() {
        return Err("the node isn't installed here; use install".into());
    }
    let stage = staged();
    std::fs::copy(new_bin, &stage).map_err(|e| format!("copy to {}: {e}", stage.display()))?;
    let fail = |e: String| {
        let _ = std::fs::remove_file(staged());
        Err(e)
    };
    let got = sha256_file(&stage)?;
    if got != opts.sha256 {
        return fail(format!("checksum mismatch: expected {}, got {got}; nothing was changed", opts.sha256));
    }
    rep.step(format!("checksum ok ({}…)", &got[..16]));
    if !is_pe(&stage) {
        return fail("not an executable for Windows; nothing was changed".into());
    }
    let out = match run(&stage, &["--version"]) {
        Ok(o) => o,
        Err(e) => return fail(format!("the new binary doesn't run here ({e}); wrong architecture?")),
    };
    let Some(to) = version_from_output(&out) else {
        return fail(format!("not a nightgrid-node binary (it says {:?})", out.trim()));
    };
    rep.to = to.clone();
    if parse_version(&to) < parse_version(current) && !opts.allow_downgrade {
        return fail(format!("{to} is older than {current}; pass --allow-downgrade to go back"));
    }
    rep.step(format!("new binary runs: nightgrid-node {to}"));

    std::fs::copy(&bin, previous()).map_err(|e| format!("{}: {e}", previous().display()))?;
    rep.step(format!("kept {current} as {}", previous().display()));

    let installed = run(&stage, &["install", "--json"]);
    let _ = std::fs::remove_file(&stage);
    match installed.and_then(|_| healthy(pipe, &to)) {
        Ok(()) => {
            rep.ok = true;
            rep.step(format!("{SERVICE} {to} is up and stayed up"));
        }
        Err(e) => {
            rep.error = Some(e.clone());
            rep.step(format!("{to} failed: {e}; putting {current} back"));
            match run(&previous(), &["install", "--json"]).and_then(|_| healthy(pipe, current)) {
                Ok(()) => {
                    rep.reverted = true;
                    rep.step(format!("reverted: {SERVICE} {current} is up"));
                }
                Err(e2) => {
                    rep.step(format!("revert failed too: {e2}"));
                    rep.error = Some(format!(
                        "{e}; the revert failed too ({e2}); run as an administrator: \"{}\" install",
                        previous().display()
                    ));
                }
            }
        }
    }
    Ok(rep)
}
