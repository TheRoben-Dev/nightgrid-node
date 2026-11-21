//! Host fixes (N11): the actions behind `nightgrid-node fix`, root's
//! menu-managed `fixes.toml`, and the root helper's side of them.
//!
//! These are node-local action types (`fix.*`). They are not part of the
//! app protocol's `action::ALL` (which is frozen); they go through the same
//! path as every other action all the same: root's allow list, the global
//! rate limit, the hash-chained audit log, an `action` event, and the root
//! helper for anything privileged. The helper re-checks root's files itself.
//!
//! `fixes.toml` sits next to `config.toml` and is written only by root,
//! normally through the menu (`sudo nightgrid-node fix`). It adds to
//! `[actions] allow` and `[actions] units`, sets the journal size to keep,
//! defines runbooks, and holds the opt-in automation rules. Nothing in it
//! is on by default and the menu never rewrites `config.toml`.
//!
//! Work that needs to write outside the helper's own sandbox (the journal,
//! logs, package caches, home directories, runbooks) runs as a short
//! transient unit through `systemd-run --wait`, each with its own time
//! limit and, where it fits, its own read-only-except-this sandbox.

use std::path::{Path, PathBuf};

use nightgrid_node_proto::action;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::helper::{Reply, Runner};

pub const UNIT_RESET: &str = "fix.unit-reset";
pub const JOURNAL_VACUUM: &str = "fix.journal-vacuum";
pub const LOGROTATE: &str = "fix.logrotate";
pub const PACKAGE_CACHE: &str = "fix.package-cache";
pub const SSH_PERMISSIONS: &str = "fix.ssh-permissions";
pub const RUNBOOK: &str = "fix.runbook";
/// The node-local action types. All of them go through the root helper.
pub const LOCAL: [&str; 6] = [UNIT_RESET, JOURNAL_VACUUM, LOGROTATE, PACKAGE_CACHE, SSH_PERMISSIONS, RUNBOOK];

pub const FILE_NAME: &str = "fixes.toml";
const MAX_RUNBOOKS: usize = 32;
const MAX_RULES: usize = 32;
/// The longest a fix may run; the helper unit's RuntimeMaxSec and the
/// service's wait for the helper are a little longer.
pub const MAX_SECS: u32 = 600;

/// Whether `a` is any action the node knows (the protocol's or a fix).
pub fn known(a: &str) -> bool {
    action::ALL.contains(&a) || LOCAL.contains(&a)
}

pub fn is_local(a: &str) -> bool {
    LOCAL.contains(&a)
}

/// Whether the action needs the root helper (protocol or local).
pub fn needs_helper(a: &str) -> bool {
    action::needs_helper(a) || is_local(a)
}

/// Whether the action's target is a systemd unit.
pub fn takes_unit(a: &str) -> bool {
    action::takes_unit(a) || a == UNIT_RESET
}

/// Whether the action has a target at all.
pub fn takes_target(a: &str) -> bool {
    action::takes_container(a) || takes_unit(a) || action::takes_address(a) || matches!(a, SSH_PERMISSIONS | RUNBOOK)
}

/// `fixes.toml` next to the given `config.toml`.
pub fn path_for(config: &Path) -> PathBuf {
    config.parent().unwrap_or(Path::new("/etc/nightgrid-node")).join(FILE_NAME)
}

/// What the automation rule waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum When {
    /// A container's health check says unhealthy.
    ContainerUnhealthy,
    /// It was running and stopped (the container-down alert).
    ContainerDown,
    /// The disk-full alert is active for a mount.
    DiskFull,
    /// A unit root listed is in the failed state.
    UnitFailed,
}

impl When {
    pub fn name(self) -> &'static str {
        match self {
            When::ContainerUnhealthy => "container-unhealthy",
            When::ContainerDown => "container-down",
            When::DiskFull => "disk-full",
            When::UnitFailed => "unit-failed",
        }
    }

    /// The actions that make sense for the condition.
    pub fn fits(self, a: &str) -> bool {
        match self {
            When::ContainerUnhealthy | When::ContainerDown => action::takes_container(a),
            When::DiskFull => matches!(a, action::IMAGES_PRUNE | JOURNAL_VACUUM | PACKAGE_CACHE | LOGROTATE),
            When::UnitFailed => a == action::UNIT_RESTART,
        }
    }
}

impl From<nightgrid_node_proto::HealWhen> for When {
    fn from(w: nightgrid_node_proto::HealWhen) -> Self {
        use nightgrid_node_proto::HealWhen as H;
        match w {
            H::ContainerUnhealthy => When::ContainerUnhealthy,
            H::ContainerDown => When::ContainerDown,
            H::DiskFull => When::DiskFull,
        }
    }
}

/// One automation rule: root's local twin of the app's self-healing rule,
/// evaluated by the same engine with the same guard rails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutoRule {
    pub id: String,
    pub enabled: bool,
    /// Only record what it would do (a preview in the audit log).
    pub dry_run: bool,
    pub when: When,
    /// Container, mount or unit; `*` for any.
    pub target: String,
    pub action: String,
    /// How long the condition must hold first, and the wait between runs (1-1440).
    pub for_minutes: u32,
    /// Most runs per target per hour (1-60) and per day (1-500).
    pub max_per_hour: u32,
    pub max_per_day: u32,
    /// Runs without the condition clearing before it gives up (1-20).
    pub give_up_after: u32,
}

impl Default for AutoRule {
    fn default() -> Self {
        Self {
            id: String::new(),
            enabled: false,
            dry_run: true,
            when: When::UnitFailed,
            target: "*".into(),
            action: action::UNIT_RESTART.into(),
            for_minutes: 5,
            max_per_hour: 2,
            max_per_day: 6,
            give_up_after: 3,
        }
    }
}

impl AutoRule {
    pub fn from_heal(r: &nightgrid_node_proto::HealRule) -> Self {
        Self {
            id: r.id.clone(),
            enabled: r.enabled,
            dry_run: r.dry_run,
            when: r.when.into(),
            target: r.target.clone(),
            action: r.action.clone(),
            for_minutes: r.for_minutes,
            max_per_hour: r.max_per_hour,
            max_per_day: r.max_per_day,
            give_up_after: r.give_up_after,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let id_ok = !self.id.is_empty()
            && self.id.len() <= 40
            && self.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !id_ok {
            return Err(format!("autofix id {:?}: 1-40 letters, digits, - or _", self.id));
        }
        if !self.when.fits(&self.action) {
            return Err(format!("autofix {}: {} can't run on {}", self.id, self.action, self.when.name()));
        }
        let target_ok = self.target == "*"
            || match self.when {
                When::UnitFailed => valid_unit(&self.target),
                When::DiskFull => valid_mount(&self.target),
                _ => crate::actions::valid_container(&self.target),
            };
        if !target_ok {
            return Err(format!("autofix {}: target {:?} doesn't fit {}", self.id, self.target, self.when.name()));
        }
        for (name, v, lo, hi) in [
            ("for_minutes", self.for_minutes, 1, 1440),
            ("max_per_hour", self.max_per_hour, 1, 60),
            ("max_per_day", self.max_per_day, 1, 500),
            ("give_up_after", self.give_up_after, 1, 20),
        ] {
            if !(lo..=hi).contains(&v) {
                return Err(format!("autofix {}: {name} must be {lo}-{hi}", self.id));
            }
        }
        Ok(())
    }
}

/// A command root defined, run as-is (no shell) in a transient unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Runbook {
    pub name: String,
    pub description: String,
    /// Absolute program path, then its arguments.
    pub run: Vec<String>,
    /// Time limit (1-600 s).
    pub timeout_secs: u32,
}

impl Default for Runbook {
    fn default() -> Self {
        Self { name: String::new(), description: String::new(), run: Vec::new(), timeout_secs: 300 }
    }
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 40
        && n.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Root's `fixes.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalFixes {
    /// Added to `[actions] allow`.
    pub allow: Vec<String>,
    /// Added to `[actions] units`.
    pub units: Vec<String>,
    /// What `fix.journal-vacuum` keeps, like `500M` or `2G`.
    pub journal_keep: String,
    #[serde(rename = "runbook", skip_serializing_if = "Vec::is_empty")]
    pub runbooks: Vec<Runbook>,
    #[serde(rename = "autofix", skip_serializing_if = "Vec::is_empty")]
    pub autofix: Vec<AutoRule>,
}

impl Default for LocalFixes {
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            units: Vec::new(),
            journal_keep: "500M".into(),
            runbooks: Vec::new(),
            autofix: Vec::new(),
        }
    }
}

fn valid_size(s: &str) -> bool {
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    !n.is_empty() && n.len() <= 6 && n.bytes().all(|b| b.is_ascii_digit()) && matches!(unit, "K" | "M" | "G")
}

impl LocalFixes {
    /// Reads the file; a missing file means nothing added.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::parse(&s).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        let f: Self = toml::from_str(s).map_err(|e| e.to_string())?;
        f.validate()?;
        Ok(f)
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(a) = self.allow.iter().find(|a| !known(a)) {
            return Err(format!("allow: unknown action {a:?}"));
        }
        if self.units.len() > 64 {
            return Err("units takes at most 64 units".into());
        }
        if let Some(u) = self.units.iter().find(|u| !valid_unit(u)) {
            return Err(format!("units: {u:?} is not a full unit name like nginx.service"));
        }
        if !valid_size(&self.journal_keep) {
            return Err("journal_keep must be a size like 500M or 2G".into());
        }
        if self.runbooks.len() > MAX_RUNBOOKS {
            return Err(format!("at most {MAX_RUNBOOKS} runbooks"));
        }
        for (i, r) in self.runbooks.iter().enumerate() {
            if !valid_name(&r.name) {
                return Err(format!("runbook {:?}: name takes 1-40 letters, digits, - or _", r.name));
            }
            if self.runbooks[..i].iter().any(|o| o.name == r.name) {
                return Err(format!("runbook {}: name used twice", r.name));
            }
            if r.run.is_empty() || !r.run[0].starts_with('/') || r.run.len() > 32 {
                return Err(format!(
                    "runbook {}: run must start with an absolute program path (at most 32 parts)",
                    r.name
                ));
            }
            if r.run.iter().any(|a| a.contains('\0') || a.len() > 4096) || r.description.len() > 200 {
                return Err(format!("runbook {}: arguments or description too long", r.name));
            }
            if !(1..=MAX_SECS).contains(&r.timeout_secs) {
                return Err(format!("runbook {}: timeout_secs must be 1-{MAX_SECS}", r.name));
            }
        }
        if self.autofix.len() > MAX_RULES {
            return Err(format!("at most {MAX_RULES} autofix rules"));
        }
        for (i, r) in self.autofix.iter().enumerate() {
            r.validate()?;
            if self.autofix[..i].iter().any(|o| o.id == r.id) {
                return Err(format!("autofix {}: id used twice", r.id));
            }
        }
        Ok(())
    }

    /// The file's text, with a header saying who writes it.
    pub fn to_text(&self) -> String {
        let body = toml::to_string(self).unwrap_or_default();
        format!(
            "# Nightgrid Node host fixes. Written by `sudo nightgrid-node fix`; safe to edit by hand\n\
             # (root only). Adds to [actions] allow/units in config.toml, never removes from them.\n\
             # See `nightgrid-node man` (FIXES) for runbooks and autofix rules.\n\n{body}"
        )
    }

    /// Writes it (root), mode 0644, replacing the old file in one step.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        use crate::compat::fs::OpenOptionsExt;
        use std::io::Write;
        self.validate()?;
        let tmp = path.with_extension("toml.new");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(&tmp)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(self.to_text().as_bytes()).and_then(|()| f.sync_all()).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn runbook(&self, name: &str) -> Option<&Runbook> {
        self.runbooks.iter().find(|r| r.name == name)
    }
}

/// Whether root allows `a`, in `config.toml` or `fixes.toml`.
pub fn allowed(config: &Config, local: &LocalFixes, a: &str) -> bool {
    config.actions.allow.iter().chain(&local.allow).any(|x| x == a)
}

/// A unit the helper may act on: a systemd unit name on Linux, a service
/// name on Windows (`W32Time`), never one of the node's own.
pub fn valid_unit(name: &str) -> bool {
    if cfg!(windows) {
        action::valid_service(name)
    } else {
        action::valid_unit(name)
    }
}

/// A mount a disk rule may name: an absolute path on Linux, a volume root
/// such as `C:\` on Windows (how the disk-full alert names it).
pub fn valid_mount(m: &str) -> bool {
    if m.len() > 255 {
        return false;
    }
    if cfg!(windows) {
        let b = m.as_bytes();
        b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\'
    } else {
        m.starts_with('/')
    }
}

/// Whether root listed the unit, in `config.toml` or `fixes.toml`.
pub fn unit_listed(config: &Config, local: &LocalFixes, unit: &str) -> bool {
    valid_unit(unit) && config.actions.units.iter().chain(&local.units).any(|u| u == unit)
}

/// Every unit root listed, without repeats.
pub fn units(config: &Config, local: &LocalFixes) -> Vec<String> {
    let mut v: Vec<String> = config.actions.units.iter().chain(&local.units).cloned().collect();
    v.sort();
    v.dedup();
    v
}

/// Everything root allowed, without repeats.
pub fn allow_list(config: &Config, local: &LocalFixes) -> Vec<String> {
    let mut v: Vec<String> = config.actions.allow.iter().chain(&local.allow).cloned().collect();
    v.sort();
    v.dedup();
    v
}

/// The daemon's view of `fixes.toml`, re-read when it changes.
#[derive(Debug, Default)]
pub struct FixesState {
    pub local: LocalFixes,
    stamp: Option<(std::time::SystemTime, u64)>,
    warned: bool,
}

impl FixesState {
    /// Re-reads the file if it changed. `Some(rules)` when the automation
    /// rules should be handed to the engine again.
    pub fn refresh(&mut self, path: &Path) -> Option<Vec<AutoRule>> {
        let stamp = std::fs::metadata(path).ok().map(|m| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len()));
        if stamp == self.stamp {
            return None;
        }
        self.stamp = stamp;
        match LocalFixes::load(path) {
            Ok(l) => {
                if l != self.local {
                    let on = l.autofix.iter().filter(|r| r.enabled).count();
                    crate::info!(
                        "{}: {} allowed, {} units, {} runbooks, {on} autofix rules on",
                        path.display(),
                        l.allow.len(),
                        l.units.len(),
                        l.runbooks.len()
                    );
                }
                self.local = l;
                self.warned = false;
                Some(self.local.autofix.clone())
            }
            Err(e) => {
                // Keep what was there: a half-edited file shouldn't switch
                // anything on or off.
                if !self.warned {
                    crate::warn!("{e} (keeping the previous host fixes)");
                    self.warned = true;
                }
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The root helper's side.

/// `systemd-run` arguments for one transient unit that waits, pipes its
/// output back and is collected afterwards.
fn transient(name: &str, timeout: u32, props: &[String], argv: &[&str]) -> Vec<String> {
    let mut a: Vec<String> =
        ["--wait", "--collect", "--quiet", "--pipe", "--service-type=exec"].iter().map(|s| (*s).to_owned()).collect();
    a.push(format!("--unit=nightgrid-node-fix-{name}-{}", std::process::id()));
    a.push(format!("--description=Nightgrid Node fix: {name}"));
    for p in [format!("RuntimeMaxSec={timeout}"), "NoNewPrivileges=yes".to_owned()].iter().chain(props) {
        a.push("-p".into());
        a.push(p.clone());
    }
    a.push("--".into());
    a.extend(argv.iter().map(|s| (*s).to_owned()));
    a
}

fn run_transient(
    run: &mut dyn Runner,
    name: &str,
    timeout: u32,
    props: &[String],
    argv: &[&str],
) -> Result<String, String> {
    let args = transient(name, timeout, props, argv);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run.output("systemd-run", &refs)
}

fn first_existing<'a>(paths: &[&'a str]) -> Option<&'a str> {
    paths.iter().copied().find(|p| Path::new(p).exists())
}

/// The package manager's clean command on this host.
pub fn package_clean() -> Option<(&'static str, Vec<&'static str>)> {
    if let Some(p) = first_existing(&["/usr/bin/apt-get"]) {
        return Some(("apt", vec![p, "clean"]));
    }
    if let Some(p) = first_existing(&["/usr/bin/dnf", "/usr/bin/dnf5"]) {
        return Some(("dnf", vec![p, "clean", "packages"]));
    }
    if let Some(p) = first_existing(&["/usr/bin/zypper"]) {
        return Some(("zypper", vec![p, "--non-interactive", "clean"]));
    }
    if let Some(p) = first_existing(&["/usr/bin/pacman"]) {
        return Some(("pacman", vec![p, "-Sc", "--noconfirm"]));
    }
    first_existing(&["/sbin/apk", "/usr/sbin/apk"]).map(|p| ("apk", vec![p, "cache", "clean"]))
}

pub fn logrotate_bin() -> Option<&'static str> {
    first_existing(&["/usr/sbin/logrotate", "/sbin/logrotate", "/usr/bin/logrotate"])
}

/// No password database on Windows (W2 reads profiles instead).
#[cfg(windows)]
pub fn passwd(_user: &str) -> Option<(u32, u32, PathBuf)> {
    None
}

/// A login user's name, uid, gid and home, from the password database.
#[cfg(unix)]
pub fn passwd(user: &str) -> Option<(u32, u32, PathBuf)> {
    if !crate::install::valid_user(user) {
        return None;
    }
    let name = std::ffi::CString::new(user).ok()?;
    // SAFETY: getpwnam takes a valid C string; the fields are copied at once.
    unsafe {
        let pw = libc::getpwnam(name.as_ptr());
        if pw.is_null() || (*pw).pw_dir.is_null() {
            return None;
        }
        let home = std::ffi::CStr::from_ptr((*pw).pw_dir).to_string_lossy().into_owned();
        Some(((*pw).pw_uid, (*pw).pw_gid, PathBuf::from(home)))
    }
}

/// A home directory safe to hand to systemd as a property value.
fn plain_home(home: &Path) -> Option<&str> {
    let s = home.to_str()?;
    (s.starts_with('/')
        && s.len() > 1
        && !s.contains("..")
        && s.bytes().all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\' && b != b'\''))
    .then_some(s)
}

/// The last line that says something, for the reply.
fn last_line(out: &str, fallback: &str) -> String {
    out.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map_or_else(|| fallback.to_owned(), |l| crate::events::tidy(l, 240))
}

/// Carries out a `fix.*` request in the helper. The caller already checked
/// root allowed the action.
pub fn helper_run(config: &Config, local: &LocalFixes, a: &str, target: Option<&str>, run: &mut dyn Runner) -> Reply {
    let target = target.unwrap_or("");
    let r = match a {
        UNIT_RESET => {
            if !unit_listed(config, local, target) {
                return Reply::no(format!("{target:?} is not in [actions] units"));
            }
            run.run("systemctl", &["reset-failed", "--", target])
                .map(|()| format!("cleared the failed state of {target}"))
        }
        JOURNAL_VACUUM => {
            let keep = format!("--vacuum-size={}", local.journal_keep);
            let props = [
                "ProtectSystem=strict".to_owned(),
                "ReadWritePaths=-/var/log/journal -/run/log/journal".to_owned(),
                "PrivateNetwork=yes".to_owned(),
            ];
            let bin = first_existing(&["/usr/bin/journalctl", "/bin/journalctl"]).unwrap_or("/usr/bin/journalctl");
            run_transient(run, "journal", 300, &props, &[bin, &keep]).map(|out| {
                let freed = out.lines().rev().find(|l| l.contains("freed")).map(|l| crate::events::tidy(l.trim(), 200));
                freed.unwrap_or_else(|| format!("journal vacuumed to {}", local.journal_keep))
            })
        }
        LOGROTATE => match logrotate_bin() {
            None => Err("logrotate isn't installed".into()),
            Some(_) if !Path::new("/etc/logrotate.conf").exists() => Err("/etc/logrotate.conf is missing".into()),
            Some(bin) => run_transient(run, "logrotate", 600, &[], &[bin, "--force", "/etc/logrotate.conf"])
                .map(|_| "rotated all logs (logrotate --force)".to_owned()),
        },
        PACKAGE_CACHE => match package_clean() {
            None => Err("no known package manager (apt, dnf, zypper, pacman, apk)".into()),
            Some((name, argv)) => {
                run_transient(run, "packages", 600, &[], &argv).map(|_| format!("cleaned the {name} package cache"))
            }
        },
        SSH_PERMISSIONS => {
            let Some((_, _, home)) = passwd(target) else {
                return Reply::no(format!("no login user {target:?}"));
            };
            let Some(h) = plain_home(&home) else {
                return Reply::no(format!("{target}'s home {} isn't a plain path", home.display()));
            };
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/usr/local/bin/nightgrid-node".into());
            let props = [
                "ProtectSystem=strict".to_owned(),
                format!("ReadWritePaths={h}"),
                "PrivateNetwork=yes".to_owned(),
                "CapabilityBoundingSet=CAP_CHOWN CAP_FOWNER CAP_DAC_OVERRIDE CAP_DAC_READ_SEARCH".to_owned(),
            ];
            run_transient(run, "ssh", 60, &props, &[&exe, "fix-apply", "ssh-permissions", target])
                .map(|out| last_line(&out, "ssh permissions checked"))
        }
        RUNBOOK => {
            let Some(rb) = local.runbook(target) else {
                return Reply::no(format!("no runbook named {target:?} in {FILE_NAME}"));
            };
            let argv: Vec<&str> = rb.run.iter().map(String::as_str).collect();
            run_transient(run, &format!("runbook-{}", rb.name), rb.timeout_secs, &[], &argv)
                .map(|out| format!("runbook {} done: {}", rb.name, last_line(&out, "no output")))
        }
        _ => return Reply::no(format!("the helper doesn't do {a:?}")),
    };
    match r {
        Ok(detail) => Reply { ok: true, detail },
        Err(e) => Reply::no(e),
    }
}

// ---------------------------------------------------------------------------
// `nightgrid-node fix-apply ssh-permissions USER` (runs in the transient
// unit as root): tightens ~/.ssh the way sshd's StrictModes wants it,
// through file descriptors opened without following links, so a user
// can't point it at anything outside their own .ssh.

#[cfg(unix)]
fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap_or_default()
}

#[cfg(unix)]
fn open_at(dir: i32, name: &str, flags: i32) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let c = cstr(name);
    // SAFETY: valid C string; the returned fd is owned below.
    let fd = unsafe { libc::openat(dir, c.as_ptr(), flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: openat just returned this fd.
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
fn fstat(fd: i32) -> std::io::Result<libc::stat> {
    // SAFETY: an all-zero stat is a valid out-buffer.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fd is open; st is writable.
    if unsafe { libc::fstat(fd, &mut st) } < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(st)
    }
}

#[cfg(unix)]
fn tighten(
    fd: i32,
    what: &str,
    uid: u32,
    gid: u32,
    mode: u32,
    chown: bool,
    done: &mut Vec<String>,
) -> Result<(), String> {
    let st = fstat(fd).map_err(|e| format!("{what}: {e}"))?;
    if chown && (st.st_uid != uid || st.st_gid != gid) {
        // SAFETY: fd is open.
        if unsafe { libc::fchown(fd, uid, gid) } < 0 {
            return Err(format!("{what}: chown: {}", std::io::Error::last_os_error()));
        }
        done.push(format!("{what} owner"));
    }
    let cur = st.st_mode & 0o7777;
    if cur != mode {
        // SAFETY: fd is open.
        if unsafe { libc::fchmod(fd, mode as libc::mode_t) } < 0 {
            return Err(format!("{what}: chmod: {}", std::io::Error::last_os_error()));
        }
        done.push(format!("{what} {cur:o}->{mode:o}"));
    }
    Ok(())
}

/// The fix itself on Windows: comes with the helper service (W2).
#[cfg(windows)]
pub fn apply_ssh_permissions(_user: &str) -> Result<String, String> {
    Err("not available on Windows yet".into())
}

/// The fix itself; returns what it changed.
#[cfg(unix)]
pub fn apply_ssh_permissions(user: &str) -> Result<String, String> {
    use std::os::fd::AsRawFd;
    let (uid, gid, home) = passwd(user).ok_or_else(|| format!("no login user {user:?}"))?;
    let home_s = home.to_str().ok_or("home isn't UTF-8")?;
    let mut done = Vec::new();
    // The home itself comes from root's password database.
    let h =
        open_at(libc::AT_FDCWD, home_s, libc::O_RDONLY | libc::O_DIRECTORY).map_err(|e| format!("{home_s}: {e}"))?;
    let st = fstat(h.as_raw_fd()).map_err(|e| e.to_string())?;
    if st.st_uid == uid || st.st_uid == 0 {
        // sshd refuses keys when the home is group- or world-writable.
        tighten(h.as_raw_fd(), "home", uid, gid, (st.st_mode & 0o7777) & !0o022, false, &mut done)?;
    }
    let ssh = match open_at(h.as_raw_fd(), ".ssh", libc::O_RDONLY | libc::O_DIRECTORY) {
        Ok(fd) => fd,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(format!("{user} has no ~/.ssh; nothing to fix")),
        Err(e) => return Err(format!("~/.ssh: {e} (a link or not a directory?)")),
    };
    tighten(ssh.as_raw_fd(), "~/.ssh", uid, gid, 0o700, true, &mut done)?;
    for name in ["authorized_keys", "authorized_keys2"] {
        match open_at(ssh.as_raw_fd(), name, libc::O_RDONLY | libc::O_NONBLOCK) {
            Ok(fd) => {
                let st = fstat(fd.as_raw_fd()).map_err(|e| e.to_string())?;
                if st.st_mode & libc::S_IFMT != libc::S_IFREG {
                    return Err(format!("~/.ssh/{name} isn't a regular file"));
                }
                tighten(fd.as_raw_fd(), &format!("~/.ssh/{name}"), uid, gid, 0o600, true, &mut done)?;
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
            Err(e) => return Err(format!("~/.ssh/{name}: {e} (a link?)")),
        }
    }
    Ok(if done.is_empty() {
        format!("{user}'s ssh permissions were already fine")
    } else {
        format!("fixed {user}'s ssh permissions: {}", done.join(", "))
    })
}

/// Read-only: what `apply_ssh_permissions` would change, for detection.
pub fn ssh_permission_problems(home: &Path, uid: u32) -> Vec<String> {
    use crate::compat::fs::MetadataExt;
    let mut out = Vec::new();
    if let Ok(m) = std::fs::metadata(home) {
        if m.mode() & 0o022 != 0 {
            out.push(format!("{} is group- or world-writable", home.display()));
        }
    }
    let ssh = home.join(".ssh");
    if let Ok(m) = std::fs::symlink_metadata(&ssh) {
        if m.is_dir() && (m.mode() & 0o777 != 0o700 || m.uid() != uid) {
            out.push(format!("~/.ssh is {:o}, should be 700 and yours", m.mode() & 0o777));
        }
        for name in ["authorized_keys", "authorized_keys2"] {
            if let Ok(m) = std::fs::symlink_metadata(ssh.join(name)) {
                if m.is_file() && (m.mode() & 0o077 != 0 || m.uid() != uid) {
                    out.push(format!("~/.ssh/{name} is {:o}, should be 600 and yours", m.mode() & 0o777));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        log: Vec<String>,
        out: String,
    }

    impl Runner for Fake {
        fn run(&mut self, prog: &str, args: &[&str]) -> Result<(), String> {
            self.log.push(format!("{prog} {}", args.join(" ")));
            Ok(())
        }
        fn output(&mut self, prog: &str, args: &[&str]) -> Result<String, String> {
            self.run(prog, args)?;
            Ok(self.out.clone())
        }
    }

    #[test]
    fn parses_validates_and_round_trips() {
        let text = r#"
allow = ["fix.journal-vacuum", "unit.restart"]
units = ["nginx.service"]
journal_keep = "1G"

[[runbook]]
name = "renew-certs"
description = "Renew TLS certificates"
run = ["/usr/bin/certbot", "renew", "--quiet"]

[[autofix]]
id = "disk-journal"
enabled = true
dry_run = false
when = "disk-full"
action = "fix.journal-vacuum"
"#;
        let f = LocalFixes::parse(text).unwrap();
        assert_eq!(f.runbooks[0].timeout_secs, 300);
        assert_eq!((f.autofix[0].when, f.autofix[0].target.as_str()), (When::DiskFull, "*"));
        let again = LocalFixes::parse(&f.to_text()).unwrap();
        assert_eq!(again, f);
        assert_eq!(LocalFixes::parse("").unwrap(), LocalFixes::default());

        for bad in [
            "allow = [\"rm -rf\"]",
            "units = [\"nightgrid-node.service\"]",
            "journal_keep = \"lots\"",
            "[[runbook]]\nname = \"x\"\nrun = [\"certbot\"]",
            "[[runbook]]\nname = \"../x\"\nrun = [\"/bin/true\"]",
            "[[autofix]]\nid = \"a\"\nwhen = \"unit-failed\"\naction = \"power.reboot\"",
            "[[autofix]]\nid = \"a\"\nwhen = \"disk-full\"\naction = \"container.restart\"",
            "[[autofix]]\nid = \"a\"\nwhen = \"unit-failed\"\ntarget = \"ssh d\"",
            "[[autofix]]\nid = \"a\"\n[[autofix]]\nid = \"a\"",
            "[[autofix]]\nid = \"a\"\nmax_per_hour = 0",
            "surprise = 1",
        ] {
            assert!(LocalFixes::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rules_name_mounts_and_units_the_platform_way() {
        let rule = |when: &str, target: &str| {
            LocalFixes::parse(&format!(
                "[[autofix]]\nid = \"a\"\nwhen = \"{when}\"\ntarget = '{target}'\naction = \"{}\"",
                if when == "unit-failed" { "unit.restart" } else { "fix.journal-vacuum" }
            ))
        };
        assert_eq!(rule("disk-full", "/srv").is_ok(), !cfg!(windows));
        assert_eq!(rule("disk-full", r"D:\").is_ok(), cfg!(windows));
        assert!(cfg!(windows) || rule("unit-failed", "nginx.service").is_ok());
        assert_eq!(rule("unit-failed", "W32Time").is_ok(), cfg!(windows));
    }

    #[test]
    fn allow_and_units_add_to_the_config() {
        let mut c = Config::default();
        c.actions.allow = vec!["unit.restart".into()];
        c.actions.units = vec!["a.service".into()];
        let l =
            LocalFixes { allow: vec![JOURNAL_VACUUM.into()], units: vec!["b.service".into()], ..Default::default() };
        assert!(allowed(&c, &l, "unit.restart") && allowed(&c, &l, JOURNAL_VACUUM) && !allowed(&c, &l, LOGROTATE));
        assert!(
            unit_listed(&c, &l, "a.service") && unit_listed(&c, &l, "b.service") && !unit_listed(&c, &l, "c.service")
        );
        assert_eq!(units(&c, &l), ["a.service", "b.service"]);
    }

    #[test]
    fn helper_runs_fixed_commands_in_transient_units() {
        let c = Config::default();
        let mut l = LocalFixes { units: vec!["web.service".into()], ..Default::default() };
        l.runbooks.push(Runbook {
            name: "hello".into(),
            run: vec!["/bin/echo".into(), "hi there".into()],
            ..Default::default()
        });
        let mut f = Fake {
            out:
                "Deleted archived journal ...\nVacuuming done, freed 1.2G of archived journals from /var/log/journal.\n"
                    .into(),
            ..Default::default()
        };
        let r = helper_run(&c, &l, JOURNAL_VACUUM, None, &mut f);
        assert!(r.ok && r.detail.starts_with("Vacuuming done, freed 1.2G"), "{r:?}");
        let cmd = &f.log[0];
        assert!(cmd.starts_with("systemd-run --wait --collect --quiet --pipe"), "{cmd}");
        assert!(
            cmd.contains("-p ReadWritePaths=-/var/log/journal -/run/log/journal")
                && cmd.ends_with("-- /usr/bin/journalctl --vacuum-size=500M"),
            "{cmd}"
        );

        let r = helper_run(&c, &l, UNIT_RESET, Some("web.service"), &mut f);
        assert!(r.ok, "{r:?}");
        assert_eq!(f.log[1], "systemctl reset-failed -- web.service");
        assert!(!helper_run(&c, &l, UNIT_RESET, Some("sshd.service"), &mut f).ok);

        f.out = "hi there\n".into();
        let r = helper_run(&c, &l, RUNBOOK, Some("hello"), &mut f);
        assert_eq!(r.detail, "runbook hello done: hi there");
        assert!(f.log[2].contains("RuntimeMaxSec=300") && f.log[2].ends_with("-- /bin/echo hi there"));
        assert!(!helper_run(&c, &l, RUNBOOK, Some("nope"), &mut f).ok);
        assert!(!helper_run(&c, &l, SSH_PERMISSIONS, Some("no-such-user-xyz"), &mut f).ok);
        assert!(!helper_run(&c, &l, "unit.restart", Some("web.service"), &mut f).ok);
        assert_eq!(f.log.len(), 3, "refusals ran nothing");
    }

    #[cfg(unix)]
    #[test]
    fn ssh_detection_reads_modes() {
        use crate::compat::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        assert!(ssh_permission_problems(home, uid).is_empty());
        std::fs::create_dir(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/authorized_keys"), "ssh-ed25519 AAAA x\n").unwrap();
        std::fs::set_permissions(home.join(".ssh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(home.join(".ssh/authorized_keys"), std::fs::Permissions::from_mode(0o664)).unwrap();
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o777)).unwrap();
        let p = ssh_permission_problems(home, uid);
        assert_eq!(p.len(), 3, "{p:?}");
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn plain_homes_only() {
        assert_eq!(plain_home(Path::new("/home/ann")), Some("/home/ann"));
        for bad in ["/", "home", "/home/a b", "/home/../etc", "/home/\"x"] {
            assert!(plain_home(Path::new(bad)).is_none(), "{bad}");
        }
    }

    #[test]
    fn conditions_fit_actions() {
        assert!(When::UnitFailed.fits("unit.restart") && !When::UnitFailed.fits("power.reboot"));
        assert!(
            When::DiskFull.fits(JOURNAL_VACUUM) && When::DiskFull.fits("images.prune") && !When::DiskFull.fits(RUNBOOK)
        );
        assert!(When::ContainerDown.fits("container.start") && !When::ContainerDown.fits(UNIT_RESET));
        assert!(LOCAL.iter().all(|a| known(a) && needs_helper(a) && !action::ALL.contains(a)));
    }
}
