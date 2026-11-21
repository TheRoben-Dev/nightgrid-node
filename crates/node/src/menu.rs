//! `nightgrid-node fix` and the node's other menus (N11), on [`crate::tui`].
//!
//! `fix` finds what looks wrong on this host (read-only), lists the fixes
//! root allows, and runs the one the user picks after saying exactly what
//! it will do and asking. Runs go to the running node as `actions.run`, so
//! root's allow list, the rate limit, the audit log, events and the root
//! helper all apply; nothing here runs privileged work itself. With sudo,
//! the menu can also allow a fix or switch an automation preset on, which
//! it saves in `fixes.toml` (never `config.toml`).

use std::path::{Path, PathBuf};

use nightgrid_node_proto::{action, method, ActionOutcome, ActionRecord, ActionRequest, ContainersResult};
use serde::Serialize;

use crate::client::Client;
use crate::config::Config;
use crate::fixes::{self, AutoRule, LocalFixes, When};
use crate::tui::{self, Item, Menu, Tone};

/// What the menu calls itself in `hello` (the audit log says `menu (uid N)`).
pub const CLIENT: &str = "nightgrid-node fix";

pub struct Ctx {
    pub config_path: PathBuf,
    pub socket: PathBuf,
    /// `-y`: don't ask.
    pub yes: bool,
    pub json: bool,
}

/// How the menu tells a user to repeat a command with privileges.
#[cfg(unix)]
const ELEVATE: &str = "sudo ";
#[cfg(windows)]
const ELEVATE: &str = "(from an elevated prompt) ";

fn is_root() -> bool {
    crate::compat::am_root()
}

fn hostname() -> String {
    crate::compat::hostname()
}

#[cfg(unix)]
fn who() -> String {
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        "root".into()
    } else {
        std::env::var("USER").unwrap_or_else(|_| format!("uid {euid}"))
    }
}

#[cfg(windows)]
fn who() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "?".into());
    if is_root() {
        format!("{user} (Administrator)")
    } else {
        user
    }
}

/// The login user a fix for "me" means: under sudo, the one who ran it;
/// otherwise whoever we run as (from the password database, not $USER,
/// which an environment can carry over).
#[cfg(windows)]
fn me() -> Option<String> {
    std::env::var("USERNAME").ok().filter(|u| !u.is_empty())
}

#[cfg(unix)]
fn me() -> Option<String> {
    if is_root() {
        return std::env::var("SUDO_USER").ok().filter(|u| !u.is_empty() && u != "root");
    }
    // SAFETY: getuid has no preconditions; getpwuid's result is read at once.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_name.is_null() {
            return None;
        }
        Some(std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned())
    }
}

/// What a fix needs as its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(windows, allow(dead_code))] // no containers there yet
enum Needs {
    Nothing,
    /// A unit root listed, or a container.
    Service,
    Unit,
    Container,
    User,
    Runbook,
}

struct Recipe {
    key: &'static str,
    group: &'static str,
    label: &'static str,
    about: &'static str,
    needs: Needs,
}

#[cfg(unix)]
const RECIPES: &[Recipe] = &[
    Recipe {
        key: "restart",
        group: "Services",
        label: "Restart a service or container",
        about: "systemctl restart on a unit root listed, or a container restart through the engine.",
        needs: Needs::Service,
    },
    Recipe {
        key: "reset",
        group: "Services",
        label: "Clear a unit's failed state",
        about: "systemctl reset-failed, so the unit can start again and stops showing as failed.",
        needs: Needs::Unit,
    },
    Recipe {
        key: "start",
        group: "Services",
        label: "Start a stopped container",
        about: "Starts a container that exited, through the engine socket.",
        needs: Needs::Container,
    },
    Recipe {
        key: "journal",
        group: "Disk & logs",
        label: "Shrink the systemd journal",
        about: "journalctl --vacuum-size (journal_keep in fixes.toml, 500M by default), in a sandboxed unit.",
        needs: Needs::Nothing,
    },
    Recipe {
        key: "logs",
        group: "Disk & logs",
        label: "Rotate all logs now",
        about: "logrotate --force /etc/logrotate.conf: starts fresh log files and compresses the old ones.",
        needs: Needs::Nothing,
    },
    Recipe {
        key: "packages",
        group: "Disk & logs",
        label: "Clean the package cache",
        about: "Removes downloaded package files (apt-get clean, dnf clean packages, zypper clean, ...).",
        needs: Needs::Nothing,
    },
    Recipe {
        key: "prune",
        group: "Disk & logs",
        label: "Remove unused container images",
        about: "Deletes dangling image layers through the engine socket (images in use stay).",
        needs: Needs::Nothing,
    },
    Recipe {
        key: "ssh",
        group: "Access",
        label: "Fix SSH key permissions",
        about: "Makes ~/.ssh 700 and authorized_keys 600, owned by the user, and the home not group-writable \
                (sshd ignores keys otherwise). Never follows links.",
        needs: Needs::User,
    },
    Recipe {
        key: "reboot",
        group: "Power",
        label: "Reboot the host",
        about: "Reboots one minute from now; root can cancel with shutdown -c.",
        needs: Needs::Nothing,
    },
    Recipe {
        key: "run",
        group: "Runbooks",
        label: "Run a runbook",
        about: "A command root defined in fixes.toml, run as-is (no shell) in a time-limited unit.",
        needs: Needs::Runbook,
    },
];

/// The Windows menu: the same names where Windows has a counterpart.
#[cfg(windows)]
const RECIPES: &[Recipe] = &[
    Recipe {
        key: "restart",
        group: "Services",
        label: "Restart a service",
        about: "Restarts a service an administrator listed in [actions] units, through the admin helper.",
        needs: Needs::Unit,
    },
    Recipe {
        key: "packages",
        group: "Disk & logs",
        label: "Clean the Windows Update cache",
        about: "Empties SoftwareDistribution\\Download (wuauserv and BITS stop meanwhile, then start again).",
        needs: Needs::Nothing,
    },
    Recipe {
        key: "ssh",
        group: "Access",
        label: "Fix SSH key permissions",
        about: "Resets the ACLs on administrators_authorized_keys and the user's .ssh\\authorized_keys to \
                SYSTEM, Administrators and the user (OpenSSH ignores keys otherwise). Never follows links.",
        needs: Needs::User,
    },
    Recipe {
        key: "reboot",
        group: "Power",
        label: "Reboot the host",
        about: "Reboots one minute from now; an administrator can cancel with shutdown /a.",
        needs: Needs::Nothing,
    },
    Recipe {
        key: "run",
        group: "Runbooks",
        label: "Run a runbook",
        about: "A command an administrator defined in fixes.toml, run as-is (no shell) with a time limit.",
        needs: Needs::Runbook,
    },
];

fn recipe(key: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|r| r.key == key)
}

/// The action a recipe runs for a target.
fn action_for(key: &str, target: Option<&str>) -> Option<&'static str> {
    Some(match key {
        "restart" if cfg!(windows) || target.is_some_and(fixes::valid_unit) => action::UNIT_RESTART,
        "restart" => action::CONTAINER_RESTART,
        "reset" => fixes::UNIT_RESET,
        "start" => action::CONTAINER_START,
        "journal" => fixes::JOURNAL_VACUUM,
        "logs" => fixes::LOGROTATE,
        "packages" => fixes::PACKAGE_CACHE,
        "prune" => action::IMAGES_PRUNE,
        "ssh" => fixes::SSH_PERMISSIONS,
        "reboot" => action::POWER_REBOOT,
        "run" => fixes::RUNBOOK,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Automation presets: the rules `fix auto` switches on and off.

pub struct Preset {
    pub key: &'static str,
    pub label: &'static str,
    pub when: When,
    pub action: &'static str,
    pub for_minutes: u32,
    pub max_per_hour: u32,
    pub max_per_day: u32,
    pub give_up_after: u32,
}

#[cfg(unix)]
pub const PRESETS: &[Preset] = &[
    Preset {
        key: "services",
        label: "Restart failed services",
        when: When::UnitFailed,
        action: action::UNIT_RESTART,
        for_minutes: 5,
        max_per_hour: 2,
        max_per_day: 6,
        give_up_after: 3,
    },
    Preset {
        key: "containers",
        label: "Restart unhealthy containers",
        when: When::ContainerUnhealthy,
        action: action::CONTAINER_RESTART,
        for_minutes: 5,
        max_per_hour: 2,
        max_per_day: 6,
        give_up_after: 3,
    },
    Preset {
        key: "stopped",
        label: "Start containers that stopped",
        when: When::ContainerDown,
        action: action::CONTAINER_START,
        for_minutes: 10,
        max_per_hour: 1,
        max_per_day: 4,
        give_up_after: 2,
    },
    Preset {
        key: "journal",
        label: "Shrink the journal when a disk fills",
        when: When::DiskFull,
        action: fixes::JOURNAL_VACUUM,
        for_minutes: 10,
        max_per_hour: 1,
        max_per_day: 4,
        give_up_after: 2,
    },
    Preset {
        key: "packages",
        label: "Clean the package cache when a disk fills",
        when: When::DiskFull,
        action: fixes::PACKAGE_CACHE,
        for_minutes: 10,
        max_per_hour: 1,
        max_per_day: 2,
        give_up_after: 2,
    },
    Preset {
        key: "images",
        label: "Remove unused images when a disk fills",
        when: When::DiskFull,
        action: action::IMAGES_PRUNE,
        for_minutes: 10,
        max_per_hour: 1,
        max_per_day: 4,
        give_up_after: 2,
    },
];

/// The Windows presets: services, and the update cache when a disk fills.
#[cfg(windows)]
pub const PRESETS: &[Preset] = &[
    Preset {
        key: "services",
        label: "Restart failed services",
        when: When::UnitFailed,
        action: action::UNIT_RESTART,
        for_minutes: 5,
        max_per_hour: 2,
        max_per_day: 6,
        give_up_after: 3,
    },
    Preset {
        key: "packages",
        label: "Clean the Windows Update cache when a disk fills",
        when: When::DiskFull,
        action: fixes::PACKAGE_CACHE,
        for_minutes: 10,
        max_per_hour: 1,
        max_per_day: 2,
        give_up_after: 2,
    },
];

impl Preset {
    fn rule(&self, live: bool) -> AutoRule {
        AutoRule {
            id: self.key.into(),
            enabled: true,
            dry_run: !live,
            when: self.when,
            target: "*".into(),
            action: self.action.into(),
            for_minutes: self.for_minutes,
            max_per_hour: self.max_per_hour,
            max_per_day: self.max_per_day,
            give_up_after: self.give_up_after,
        }
    }

    fn about(&self) -> String {
        format!(
            "When {} holds for {} min: {}. At most {}/hour and {}/day per target; stops after {} tries until it clears.",
            self.when.name(),
            self.for_minutes,
            self.action,
            self.max_per_hour,
            self.max_per_day,
            self.give_up_after
        )
    }
}

/// `off`, `preview` (dry runs, logged) or `on`.
pub fn preset_state(local: &LocalFixes, key: &str) -> &'static str {
    match local.autofix.iter().find(|r| r.id == key) {
        Some(r) if r.enabled && !r.dry_run => "on",
        Some(r) if r.enabled => "preview",
        _ => "off",
    }
}

fn state_tone(s: &str) -> Tone {
    match s {
        "on" => Tone::Ok,
        "preview" => Tone::Cyan,
        _ => Tone::Dim,
    }
}

/// Sets a preset in the file's rules (and allows its action when turning
/// it on). Returns what changed, for the message.
pub fn set_preset(local: &mut LocalFixes, key: &str, state: &str) -> Result<String, String> {
    let p = PRESETS.iter().find(|p| p.key == key).ok_or_else(|| {
        format!("no preset {key:?} (one of {})", PRESETS.iter().map(|p| p.key).collect::<Vec<_>>().join(", "))
    })?;
    local.autofix.retain(|r| r.id != key);
    match state {
        "off" => return Ok(format!("{} is off", p.label)),
        "on" | "preview" => local.autofix.push(p.rule(state == "on")),
        other => return Err(format!("{other:?}: say on, preview or off")),
    }
    let mut note = String::new();
    if !local.allow.iter().any(|a| a == p.action) {
        local.allow.push(p.action.into());
        note = format!(" (and allowed {})", p.action);
    }
    Ok(format!("{} is {state}{note}", p.label))
}

// ---------------------------------------------------------------------------
// What looks wrong (read-only).

#[derive(Debug, Clone, Serialize)]
pub struct Issue {
    pub fix: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub title: String,
    pub detail: String,
    #[serde(skip)]
    pub tone: Tone,
}

/// Failed units: every failed systemd unit (the menu may list one).
#[cfg(unix)]
fn failed_units(_listed: &[String]) -> Vec<String> {
    let out = std::process::Command::new("systemctl")
        .args(["list-units", "--state=failed", "--plain", "--no-legend", "--no-pager", "--full"])
        .env("SYSTEMD_PAGER", "")
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter(|u| action::valid_unit(u))
            .map(str::to_owned)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Fixed volumes and how full they are (percent).
/// Failed services: Windows keeps no failed state, so only the listed
/// services are checked, the way `heal` does.
#[cfg(windows)]
fn failed_units(listed: &[String]) -> Vec<String> {
    crate::heal::failed_units(listed)
}

#[cfg(windows)]
fn full_disks(threshold: u64) -> Vec<(String, u64)> {
    crate::metrics::collect::volumes()
        .into_iter()
        .filter_map(|v| {
            let (total, used, avail) = crate::compat::disk_usage(std::path::Path::new(&v.root))?;
            let _ = total;
            let pct = (used * 100).checked_div(used + avail)?;
            (pct >= threshold).then_some((v.root, pct))
        })
        .collect()
}

/// Mounts of real file systems and how full they are (percent).
#[cfg(unix)]
fn full_disks(threshold: u64) -> Vec<(String, u64)> {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let mut out: Vec<(String, u64)> = Vec::new();
    for l in mounts.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 3
            || !matches!(f[2], "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "zfs" | "f2fs" | "bcachefs" | "jfs")
        {
            continue;
        }
        let mnt = f[1].replace("\\040", " ");
        if out.iter().any(|(m, _)| *m == mnt) {
            continue;
        }
        let Ok(c) = std::ffi::CString::new(mnt.as_str()) else { continue };
        // SAFETY: an all-zero statvfs is a valid out-buffer.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: valid C string and out-buffer.
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 || s.f_blocks == 0 {
            continue;
        }
        let total = s.f_blocks as u64;
        let used = total.saturating_sub(s.f_bfree as u64);
        let avail = s.f_bavail as u64;
        let pct = (used * 100).checked_div(used + avail).unwrap_or(0);
        if pct >= threshold {
            out.push((mnt, pct));
        }
    }
    out
}

/// `journalctl --disk-usage`, in bytes.
#[cfg(unix)]
fn journal_bytes() -> Option<u64> {
    let o = std::process::Command::new("journalctl").args(["--disk-usage", "--no-pager"]).output().ok()?;
    let text = String::from_utf8_lossy(&o.stdout);
    parse_journal_usage(&text)
}

#[cfg(windows)]
fn journal_bytes() -> Option<u64> {
    None
}

/// Whether Windows has servicing work that waits for a restart.
#[cfg(windows)]
fn reboot_pending() -> bool {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    Path::new(&root).join(r"WinSxS\pending.xml").exists()
}

#[cfg(unix)]
fn reboot_pending() -> bool {
    Path::new("/run/reboot-required").exists() || Path::new("/var/run/reboot-required").exists()
}

pub fn parse_journal_usage(text: &str) -> Option<u64> {
    // "Archived and active journals take up 1.2G in the file system."
    let word = text.split_whitespace().skip_while(|w| *w != "up").nth(1)?;
    let (num, unit) = word.split_at(word.find(|c: char| c.is_ascii_alphabetic())?);
    let n: f64 = num.parse().ok()?;
    let mult = match unit.chars().next()? {
        'B' => 1.0,
        'K' => 1024.0,
        'M' => 1024.0 * 1024.0,
        'G' => 1024.0 * 1024.0 * 1024.0,
        'T' => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((n * mult) as u64)
}

/// Looks around the host. `listed` are the units root listed (Windows
/// checks only those); `containers` comes from the node when reachable.
pub fn detect(listed: &[String], containers: Option<&ContainersResult>) -> Vec<Issue> {
    let mut out = Vec::new();
    for u in failed_units(listed) {
        out.push(Issue {
            fix: "restart",
            target: Some(u.clone()),
            title: format!("{u} has failed"),
            detail: if cfg!(windows) {
                format!("Restart it. The System event log says why it stopped (nightgrid-node logs {u}).")
            } else {
                format!("Restart it (or `reset` to just clear the failed state). See: journalctl -u {u} -n 50")
            },
            tone: Tone::Danger,
        });
    }
    if let Some(c) = containers {
        for k in &c.containers {
            if k.state == "running" && k.health.as_deref() == Some("unhealthy") {
                out.push(Issue {
                    fix: "restart",
                    target: Some(k.name.clone()),
                    title: format!("container {} is unhealthy", k.name),
                    detail: format!("Its health check fails ({}). Restart it.", k.status),
                    tone: Tone::Warn,
                });
            } else if k.state == "exited" || k.state == "dead" {
                out.push(Issue {
                    fix: "start",
                    target: Some(k.name.clone()),
                    title: format!("container {} is stopped", k.name),
                    detail: format!("{} ({}). Start it again.", k.status, k.image),
                    tone: Tone::Warn,
                });
            }
        }
    }
    for (m, pct) in full_disks(90) {
        out.push(Issue {
            fix: if cfg!(windows) { "packages" } else { "journal" },
            target: None,
            title: format!("disk {m} is {pct}% full"),
            detail: if cfg!(windows) {
                "Free space: clean the Windows Update download cache.".into()
            } else {
                "Free space: shrink the journal, clean the package cache, or remove unused images.".into()
            },
            tone: if pct >= 95 { Tone::Danger } else { Tone::Warn },
        });
    }
    if let Some(b) = journal_bytes().filter(|b| *b >= 2 << 30) {
        out.push(Issue {
            fix: "journal",
            target: None,
            title: format!("the journal takes {}", crate::cli::bytes(b)),
            detail: "Shrink it to journal_keep (500M unless fixes.toml says otherwise).".into(),
            tone: Tone::Warn,
        });
    }
    if reboot_pending() {
        out.push(Issue {
            fix: "reboot",
            target: None,
            title: "a reboot is pending".into(),
            detail: if cfg!(windows) {
                "Installed updates finish when Windows restarts.".into()
            } else {
                "Updates installed a new kernel or libraries that need a reboot.".into()
            },
            tone: Tone::Warn,
        });
    }
    if let Some(user) = me() {
        if let Some((uid, _, home)) = fixes::passwd(&user) {
            let p = fixes::ssh_permission_problems(&home, uid);
            if !p.is_empty() {
                out.push(Issue {
                    fix: "ssh",
                    target: Some(user.clone()),
                    title: format!("{user}'s SSH key permissions are too open"),
                    detail: format!("{}. sshd may ignore the keys.", p.join("; ")),
                    tone: Tone::Warn,
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Root's view: what's allowed.

struct Policy {
    config: Config,
    local: LocalFixes,
    fixes_path: PathBuf,
    helper: bool,
}

impl Policy {
    fn load(ctx: &Ctx) -> Self {
        let config = Config::load(&ctx.config_path).unwrap_or_default();
        let fixes_path = fixes::path_for(&ctx.config_path);
        let local = LocalFixes::load(&fixes_path).unwrap_or_else(|e| {
            eprintln!("{} {e}", tui::paint("warning:", Tone::Warn, true));
            LocalFixes::default()
        });
        Self { config, local, fixes_path, helper: crate::helper::installed(Path::new(crate::helper::SOCKET)) }
    }

    fn units(&self) -> Vec<String> {
        fixes::units(&self.config, &self.local)
    }

    fn allowed(&self, a: &str) -> bool {
        fixes::allowed(&self.config, &self.local, a)
    }

    fn state(&self, key: &str) -> (Tone, String) {
        let a = action_for(key, None).unwrap_or("");
        let (a, alt) = if key == "restart" && cfg!(unix) {
            (action::UNIT_RESTART, Some(action::CONTAINER_RESTART))
        } else {
            (a, None)
        };
        let any = self.allowed(a) || alt.is_some_and(|x| self.allowed(x));
        if !any {
            (Tone::Dim, "not allowed".into())
        } else if fixes::needs_helper(a) && !alt.is_some_and(|x| self.allowed(x)) && !self.helper {
            (Tone::Warn, "needs helper".into())
        } else {
            (Tone::Ok, "allowed".into())
        }
    }
}

// ---------------------------------------------------------------------------
// Running one fix.

fn connect(ctx: &Ctx) -> Result<Client, String> {
    Client::connect(&ctx.socket, CLIENT).map_err(|e| {
        if cfg!(windows) {
            return format!(
                "can't reach the node at {} ({e}). Is the service running (sc query nightgrid-node), and are you an \
                 Administrator or in the \"Nightgrid Node Users\" group?",
                ctx.socket.display()
            );
        }
        format!(
            "can't reach the node at {} ({e}). Is it running (systemctl status nightgrid-node), and are you in the \
             nightgrid-node group (or use sudo)?",
            ctx.socket.display()
        )
    })
}

fn containers(ctx: &Ctx) -> Option<ContainersResult> {
    let mut c = connect(ctx).ok()?;
    serde_json::from_value(c.call(method::CONTAINERS_LIST, serde_json::Value::Null).ok()?).ok()
}

/// Candidate targets for a recipe, for the picker.
fn candidates(ctx: &Ctx, pol: &Policy, needs: Needs) -> Vec<(String, String)> {
    let mut v = Vec::new();
    let failed = if matches!(needs, Needs::Service | Needs::Unit) { failed_units(&pol.units()) } else { Vec::new() };
    if matches!(needs, Needs::Service | Needs::Unit) {
        for u in fixes::units(&pol.config, &pol.local) {
            let f = failed.contains(&u);
            v.push((u.clone(), if f { "failed".to_owned() } else { "unit".to_owned() }));
        }
        let unlisted: Vec<String> = failed.iter().filter(|u| !v.iter().any(|(x, _)| x == *u)).cloned().collect();
        for u in unlisted {
            v.push((u, "failed, not listed".into()));
        }
    }
    if matches!(needs, Needs::Service | Needs::Container) {
        if let Some(c) = containers(ctx) {
            for k in c.containers {
                let s = k.health.clone().filter(|_| k.state == "running").unwrap_or(k.state.clone());
                v.push((k.name, format!("container, {s}")));
            }
        }
    }
    if needs == Needs::User {
        if let Some(u) = me() {
            v.push((u, "you".into()));
        }
    }
    if needs == Needs::Runbook {
        for r in &pol.local.runbooks {
            v.push((r.name.clone(), r.description.clone()));
        }
    }
    v
}

fn pick_target(ctx: &Ctx, pol: &Policy, r: &Recipe) -> Result<Option<String>, String> {
    if r.needs == Needs::Nothing {
        return Ok(None);
    }
    let list = candidates(ctx, pol, r.needs);
    let what = match r.needs {
        Needs::Service => "a unit like nginx.service or a container name",
        Needs::Unit if cfg!(windows) => "a service like W32Time",
        Needs::Unit => "a unit like nginx.service",
        Needs::Container => "a container name",
        Needs::User => "a user name",
        Needs::Runbook => "a runbook name from fixes.toml",
        Needs::Nothing => "",
    };
    if !tui::interactive() {
        return Err(format!("`fix {}` needs a target: {what}", r.key));
    }
    let mut m = Menu::new("node", &format!("fix · {}", r.key));
    m.subtitle = format!("Pick {what}");
    m.section("Targets");
    if list.is_empty() {
        m.note("Nothing to suggest here.");
    }
    for (name, note) in &list {
        let tone = if note.starts_with("failed") || note.contains("unhealthy") || note.contains("exited") {
            Tone::Warn
        } else {
            Tone::Dim
        };
        m.item(Item::new(name.clone(), name.clone(), r.about).state(tone, note.clone()));
    }
    m.section("Other");
    m.item(Item::new("…", "Type a name", format!("Enter {what} yourself.")));
    match tui::pick(&m) {
        None => Ok(None),
        Some(i) if i < list.len() => Ok(Some(list[i].0.clone())),
        Some(_) => {
            let t = tui::read_line(&format!("{}:", capitalize(what)));
            Ok((!t.is_empty()).then_some(t))
        }
    }
}

fn say(tone: Tone, word: &str, text: &str) {
    println!("{} {text}", tui::pill(tone, word));
}

/// Offers root the missing permission; `Ok(true)` when it may go on.
fn ensure_allowed(ctx: &Ctx, pol: &mut Policy, a: &str, target: Option<&str>) -> Result<bool, String> {
    let need_unit = fixes::takes_unit(a) && !target.is_some_and(|t| fixes::unit_listed(&pol.config, &pol.local, t));
    let need_allow = !pol.allowed(a);
    if !need_allow && !need_unit {
        return Ok(true);
    }
    let mut what = Vec::new();
    if need_allow {
        what.push(format!("allow {a}"));
    }
    if need_unit {
        what.push(format!("list {} for unit actions", target.unwrap_or("?")));
    }
    let what = what.join(" and ");
    if !is_root() {
        say(Tone::Dim, "not allowed", &format!("Root hasn't let the node {what} on this host."));
        println!("  To allow it: {}nightgrid-node fix {}", ELEVATE, ctx_hint(a, target));
        return Err(String::new());
    }
    println!("This host doesn't allow it yet. As root you can {what} now;");
    println!("it's saved in {} and the node picks it up at once.", pol.fixes_path.display());
    if !(ctx.yes || tui::confirm(&format!("{}?", capitalize(&what)), false)) {
        return Ok(false);
    }
    if need_allow {
        pol.local.allow.push(a.into());
    }
    if need_unit {
        pol.local.units.push(target.unwrap_or_default().into());
    }
    pol.local.save(&pol.fixes_path)?;
    say(Tone::Ok, "saved", &format!("{what} ({})", pol.fixes_path.display()));
    Ok(true)
}

fn ctx_hint(a: &str, target: Option<&str>) -> String {
    let key = RECIPES.iter().find(|r| action_for(r.key, target) == Some(a)).map_or("", |r| r.key);
    match target {
        Some(t) => format!("{key} {t}"),
        None => key.to_owned(),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

fn ensure_helper(ctx: &Ctx, pol: &mut Policy, a: &str) -> Result<bool, String> {
    if !fixes::needs_helper(a) || pol.helper {
        return Ok(true);
    }
    if !is_root() {
        say(Tone::Warn, "needs helper", "This fix runs through the root helper, which isn't installed.");
        println!("  Install it: {ELEVATE}nightgrid-node allow helper");
        return Err(String::new());
    }
    if !(ctx.yes || tui::confirm("It runs through the root helper, which isn't installed. Install it now?", true)) {
        return Ok(false);
    }
    install_helper(ctx)?;
    pol.helper = true;
    say(Tone::Ok, "installed", "the root helper");
    Ok(true)
}

#[cfg(windows)]
fn install_helper(_ctx: &Ctx) -> Result<(), String> {
    crate::win::helper::allow(&crate::win::helper::ADMIN, true).map(|_| ())
}

#[cfg(unix)]
fn install_helper(ctx: &Ctx) -> Result<(), String> {
    let config = Config::load(&ctx.config_path)?;
    crate::install::allow(
        &crate::install::Layout::system(),
        &mut crate::install::RealSys,
        crate::install::Grant::Helper,
        &config.docker.socket,
        false,
    )
    .map(|_| ())
}

fn explain(pol: &Policy, key: &str, a: &str, target: Option<&str>) {
    let r = recipe(key);
    println!();
    println!("{}", tui::banner("node", &format!("fix · {key}")));
    let line = |k: &str, v: &str| println!("  {:<7} {v}", tui::paint(k, Tone::Dim, false));
    line("what", r.map_or("", |r| r.label));
    if let Some(t) = target {
        line("on", t);
    }
    let how = match a {
        fixes::JOURNAL_VACUUM => {
            format!("journalctl --vacuum-size={} in a sandboxed transient unit", pol.local.journal_keep)
        }
        fixes::RUNBOOK => pol
            .local
            .runbook(target.unwrap_or(""))
            .map_or_else(String::new, |rb| format!("{} (no shell, {} s limit)", rb.run.join(" "), rb.timeout_secs)),
        fixes::PACKAGE_CACHE if cfg!(windows) => {
            "empties %SystemRoot%\\SoftwareDistribution\\Download with wuauserv and BITS stopped".into()
        }
        fixes::PACKAGE_CACHE => {
            fixes::package_clean().map_or("no package manager found".into(), |(_, argv)| argv.join(" "))
        }
        _ => r.map_or(String::new(), |r| r.about.to_owned()),
    };
    line("how", &how);
    line(
        "via",
        if fixes::needs_helper(a) { "the node, then the root helper" } else { "the node, through the engine socket" },
    );
    line("logged", &format!("{a} in the audit log (nightgrid-node audit)"));
    println!();
}

/// `fix KEY [TARGET]`: explain, ask, run, report. Errors are for the caller.
pub fn run_fix(ctx: &Ctx, key: &str, target: Option<String>) -> Result<(), String> {
    let r = recipe(key).ok_or_else(|| format!("no fix {key:?} (try: nightgrid-node fix list)"))?;
    let mut pol = Policy::load(ctx);
    let target = match target {
        Some(t) => Some(t),
        None if r.needs == Needs::User && !tui::interactive() => me(),
        None => match pick_target(ctx, &pol, r)? {
            Some(t) => Some(t),
            None if r.needs == Needs::Nothing => None,
            None => return Ok(()),
        },
    };
    let a = action_for(key, target.as_deref()).unwrap_or_default();
    explain(&pol, key, a, target.as_deref());
    if !ensure_allowed(ctx, &mut pol, a, target.as_deref())? || !ensure_helper(ctx, &mut pol, a)? {
        return Ok(());
    }
    let dry = if ctx.yes {
        false
    } else if a == action::POWER_REBOOT {
        if !tui::type_to_confirm("reboot") {
            say(Tone::Dim, "cancelled", "nothing ran");
            return Ok(());
        }
        false
    } else {
        match tui::ask("Run it?", "[y]es · [p]review · [N]o", 'n') {
            'y' => false,
            'p' => true,
            _ => {
                say(Tone::Dim, "cancelled", "nothing ran");
                return Ok(());
            }
        }
    };
    let mut c = connect(ctx)?;
    let req = ActionRequest { action: a.into(), target, dry_run: dry };
    let v = c.call(method::ACTIONS_RUN, serde_json::to_value(&req).unwrap_or_default()).map_err(|e| e.to_string())?;
    let rec: ActionRecord = serde_json::from_value(v).map_err(|e| e.to_string())?;
    let seq = if rec.seq > 0 { format!("  (audit #{})", rec.seq) } else { String::new() };
    match rec.outcome {
        ActionOutcome::Ok => say(Tone::Ok, "done", &format!("{}{seq}", rec.detail)),
        ActionOutcome::DryRun => say(Tone::Cyan, "preview", &format!("{}{seq}", rec.detail)),
        ActionOutcome::Failed => say(Tone::Danger, "failed", &format!("{}{seq}", rec.detail)),
        ActionOutcome::Refused => say(Tone::Warn, "refused", &format!("{}{seq}", rec.detail)),
    }
    match rec.outcome {
        ActionOutcome::Failed | ActionOutcome::Refused => Err(String::new()),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// `fix auto`.

pub fn auto(ctx: &Ctx, args: &[String]) -> Result<(), String> {
    let mut pol = Policy::load(ctx);
    let (key, state) = match args {
        [] => {
            if !tui::interactive() {
                print!("{}", auto_menu(&pol).plain_with_title());
                println!("\n\nChange one (root): nightgrid-node fix auto NAME on|preview|off");
                return Ok(());
            }
            loop {
                let m = auto_menu(&pol);
                let Some(i) = tui::pick(&m) else { return Ok(()) };
                let p = &PRESETS[i];
                let next = match preset_state(&pol.local, p.key) {
                    "off" => "preview",
                    "preview" => "on",
                    _ => "off",
                };
                println!("\n{}", tui::banner("node", &format!("auto · {}", p.key)));
                println!("  {}", p.about());
                if let Err(e) = change_preset(ctx, &mut pol, p.key, next) {
                    say(Tone::Danger, "error", &e);
                }
                tui::pause();
            }
        }
        [k, s] => (k.as_str(), s.as_str()),
        _ => return Err("usage: fix auto [NAME on|preview|off]".into()),
    };
    change_preset(ctx, &mut pol, key, state)
}

fn auto_menu(pol: &Policy) -> Menu {
    let mut m = Menu::new("node", "fix · auto");
    m.subtitle = format!(
        "{} as {} · Node acts on its own only for what's on. preview logs what it would do.",
        hostname(),
        who()
    );
    m.section("Automation");
    for p in PRESETS {
        let s = preset_state(&pol.local, p.key);
        m.item(Item::new(p.key, p.label, p.about()).state(state_tone(s), s));
    }
    m
}

fn change_preset(ctx: &Ctx, pol: &mut Policy, key: &str, state: &str) -> Result<(), String> {
    if !is_root() {
        say(Tone::Dim, "root only", "Only root decides what the node does by itself.");
        println!("  Run: {ELEVATE}nightgrid-node fix auto {key} {state}");
        return Ok(());
    }
    let p = PRESETS.iter().find(|p| p.key == key).ok_or_else(|| format!("no preset {key:?}"))?;
    let question = match state {
        "on" => format!("Let the node {} by itself from now on?", p.label.to_lowercase()),
        "preview" => format!("Preview \"{}\" (log what it would do, run nothing)?", p.label),
        _ => format!("Turn \"{}\" off?", p.label),
    };
    if !(ctx.yes || tui::confirm(&question, state != "on")) {
        say(Tone::Dim, "unchanged", "");
        return Ok(());
    }
    let msg = set_preset(&mut pol.local, key, state)?;
    pol.local.save(&pol.fixes_path)?;
    say(state_tone(state), state, &format!("{msg} ({})", pol.fixes_path.display()));
    if state != "off" && p.when == When::UnitFailed && fixes::units(&pol.config, &pol.local).is_empty() {
        println!("  It only watches units root listed; add some with: {ELEVATE}nightgrid-node fix restart UNIT");
    }
    if state != "off" && fixes::needs_helper(p.action) && !pol.helper {
        println!("  It needs the root helper: {ELEVATE}nightgrid-node allow helper");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `fix` and `fix list`.

#[derive(Serialize)]
struct ListJson {
    issues: Vec<Issue>,
    fixes: Vec<FixJson>,
    auto: Vec<AutoJson>,
}

#[derive(Serialize)]
struct FixJson {
    name: &'static str,
    group: &'static str,
    label: &'static str,
    state: String,
}

#[derive(Serialize)]
struct AutoJson {
    name: &'static str,
    label: &'static str,
    state: &'static str,
}

enum Entry {
    Issue(usize),
    Fix(&'static str),
    Auto,
}

fn fix_menu(pol: &Policy, issues: &[Issue]) -> (Menu, Vec<Entry>) {
    let mut m = Menu::new("node", "fix");
    m.subtitle = format!("{} as {} · nothing runs until you say so", hostname(), who());
    let mut entries = Vec::new();
    m.section("Detected");
    if issues.is_empty() {
        m.note("Nothing looks wrong right now.");
    }
    for (i, is) in issues.iter().enumerate() {
        let key = match &is.target {
            Some(t) => format!("{} {t}", is.fix),
            None => is.fix.to_owned(),
        };
        m.item(Item::new(key, is.title.clone(), is.detail.clone()).state(is.tone, "fix"));
        entries.push(Entry::Issue(i));
    }
    let mut group = "";
    for r in RECIPES {
        if r.group != group {
            m.section(r.group);
            group = r.group;
        }
        if r.key == "run" && pol.local.runbooks.is_empty() {
            m.note("No runbooks yet: root adds [[runbook]] entries to fixes.toml.");
            continue;
        }
        let (t, w) = pol.state(r.key);
        m.item(Item::new(r.key, r.label, r.about).state(t, w));
        entries.push(Entry::Fix(r.key));
    }
    m.section("Automation");
    let on = pol.local.autofix.iter().filter(|r| r.enabled && !r.dry_run).count();
    let pv = pol.local.autofix.iter().filter(|r| r.enabled && r.dry_run).count();
    let st = match (on, pv) {
        (0, 0) => (Tone::Dim, "all off".to_owned()),
        (on, 0) => (Tone::Ok, format!("{on} on")),
        (on, pv) => (Tone::Cyan, format!("{on} on, {pv} preview")),
    };
    m.item(
        Item::new(
            "auto",
            "Let the node fix things by itself",
            "Opt-in presets with waits, caps and give-up limits; every run audited.",
        )
        .state(st.0, st.1),
    );
    entries.push(Entry::Auto);
    (m, entries)
}

/// `nightgrid-node fix [list | auto ... | NAME [TARGET]]`.
pub fn fix(ctx: &Ctx, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("list") => list(ctx),
        Some("auto") => auto(ctx, &args[1..]),
        Some(key) => run_fix(ctx, key, args.get(1).cloned()),
        None if !tui::interactive() => list(ctx),
        None => loop {
            let pol = Policy::load(ctx);
            let issues = detect(&pol.units(), containers(ctx).as_ref());
            let (m, entries) = fix_menu(&pol, &issues);
            let Some(i) = tui::pick(&m) else { return Ok(()) };
            let r = match &entries[i] {
                Entry::Issue(n) => run_fix(ctx, issues[*n].fix, issues[*n].target.clone()),
                Entry::Fix(k) => run_fix(ctx, k, None),
                Entry::Auto => auto(ctx, &[]),
            };
            if let Err(e) = r {
                if !e.is_empty() {
                    say(Tone::Danger, "error", &e);
                }
            }
            if !matches!(entries[i], Entry::Auto) {
                tui::pause();
            }
        },
    }
}

fn list(ctx: &Ctx) -> Result<(), String> {
    let pol = Policy::load(ctx);
    let issues = detect(&pol.units(), containers(ctx).as_ref());
    if ctx.json {
        let j = ListJson {
            fixes: RECIPES
                .iter()
                .map(|r| FixJson { name: r.key, group: r.group, label: r.label, state: pol.state(r.key).1 })
                .collect(),
            auto: PRESETS
                .iter()
                .map(|p| AutoJson { name: p.key, label: p.label, state: preset_state(&pol.local, p.key) })
                .collect(),
            issues,
        };
        println!("{}", serde_json::to_string_pretty(&j).unwrap_or_default());
        return Ok(());
    }
    let (m, _) = fix_menu(&pol, &issues);
    println!("{}", m.plain_with_title());
    println!("\nRun one: nightgrid-node fix NAME [TARGET]   (-y: don't ask)");
    Ok(())
}

// ---------------------------------------------------------------------------
// The node's other menus.

/// `nightgrid-node allow` / `deny` with no name: every grant, on or off.
pub fn access(config_path: &Path) -> Result<(), String> {
    use crate::install::{Grant, Layout, RealSys};
    let layout = Layout::system();
    let label = |g: Grant| match g {
        Grant::Docker => "Containers",
        Grant::Journal => "SSH logins and logs",
        Grant::Netwatch => "LAN watch",
        Grant::Helper => "Root helper (actions and fixes)",
        Grant::Health => "Disk, pool, update and UPS health",
        Grant::Power => "Power draw",
        Grant::Checks => "Service checks",
        Grant::Peers => "Node-to-node link",
        Grant::StatusPage => "Status page",
    };
    let about = |g: Grant| match g {
        Grant::Docker => "Read containers through the engine socket's group (root-equivalent access).",
        Grant::Journal => "Read the journal for SSH logins and log searches.",
        Grant::Netwatch => "A listen-only helper for ARP and DHCP (CAP_NET_RAW).",
        Grant::Helper => "The root helper for unit, power, firewall and fix actions.",
        Grant::Health => "A read-only helper for SMART, pools, updates and UPS status.",
        Grant::Power => "Read the CPU energy counters (RAPL) for power draw.",
        Grant::Checks => "The service check runner, with network access to [checks] networks only.",
        Grant::Peers => "The node-to-node link (mutual TLS, tailnet address only).",
        Grant::StatusPage => "A read-only status page on the tailnet address.",
    };
    loop {
        let mut m = Menu::new("node", "access");
        m.subtitle = format!("{} as {} · what root lets the node reach", hostname(), who());
        m.section("Access");
        for g in Grant::ALL {
            let on = g.is_allowed(&layout);
            m.item(
                Item::new(g.name(), label(g), about(g))
                    .state(if on { Tone::Ok } else { Tone::Dim }, if on { "on" } else { "off" }),
            );
        }
        if !tui::interactive() {
            println!("{}", m.plain_with_title());
            println!("\nChange one (root): nightgrid-node allow NAME | deny NAME");
            return Ok(());
        }
        let Some(i) = tui::pick(&m) else { return Ok(()) };
        let g = Grant::ALL[i];
        let on = g.is_allowed(&layout);
        println!("\n{}", tui::banner("node", &format!("access · {}", g.name())));
        println!("  {}", about(g));
        if !is_root() {
            say(
                Tone::Dim,
                "root only",
                &format!("Run: sudo nightgrid-node {} {}", if on { "deny" } else { "allow" }, g.name()),
            );
        } else if tui::confirm(&format!("{} {}?", if on { "Take away" } else { "Allow" }, g.name()), !on) {
            let r = if on {
                crate::install::deny(&layout, &mut RealSys, g, false)
            } else {
                let config = Config::load(config_path)?;
                crate::install::allow(&layout, &mut RealSys, g, &config.docker.socket, false)
            };
            match r {
                Ok(rep) => {
                    for w in &rep.warnings {
                        say(Tone::Warn, "note", w);
                    }
                    if let Some(u) = &rep.url {
                        println!("  status page: {u}");
                    }
                    say(Tone::Ok, "done", &format!("{} is {}", g.name(), if on { "off" } else { "on" }));
                }
                Err(e) => say(Tone::Danger, "error", &e),
            }
        }
        tui::pause();
    }
}

/// `nightgrid-node serve` with nothing after it: what peers may ask.
pub fn serve(config_path: &Path) -> Result<(), String> {
    use crate::install::{Layout, RealSys};
    let about = |s: &str| match s {
        "ping" => "Paired nodes may ask whether this host is up (node-to-node checks).",
        "ups" => "Paired nodes may follow this host's UPS for their own shutdown.",
        _ => "Paired nodes (and a hub) may pull this host's history, read-only.",
    };
    loop {
        let config = Config::load(config_path)?;
        let mut m = Menu::new("node", "serve");
        m.subtitle = format!("{} as {} · what paired peers may ask this node", hostname(), who());
        m.section("Peers may ask");
        for s in crate::config::PEER_SERVES {
            let on = config.peers.serve.iter().any(|x| x == s);
            let label = match *s {
                "ping" => "Up checks",
                "ups" => "UPS status",
                _ => "History sync (and the hub)",
            };
            m.item(
                Item::new(*s, label, about(s))
                    .state(if on { Tone::Ok } else { Tone::Dim }, if on { "on" } else { "off" }),
            );
        }
        if !tui::interactive() {
            println!("{}", m.plain_with_title());
            println!("\nChange one (root): nightgrid-node serve add|remove NAME");
            return Ok(());
        }
        let Some(i) = tui::pick(&m) else { return Ok(()) };
        let s = crate::config::PEER_SERVES[i];
        let on = config.peers.serve.iter().any(|x| x == s);
        if !is_root() {
            say(
                Tone::Dim,
                "root only",
                &format!("Run: sudo nightgrid-node serve {} {s}", if on { "remove" } else { "add" }),
            );
        } else if tui::confirm(&format!("{} {s}?", if on { "Stop serving" } else { "Serve" }), !on) {
            match crate::install::serve(&Layout::system(), &mut RealSys, s, !on) {
                Ok(_) => say(Tone::Ok, "done", &format!("{s} is {}", if on { "off" } else { "on" })),
                Err(e) => say(Tone::Danger, "error", &e),
            }
        }
        tui::pause();
    }
}

/// `nightgrid-node` with no command on a terminal: the home menu. Returns
/// the command to run (the caller runs it like any other).
pub fn home() -> Option<&'static str> {
    const ENTRIES: &[(&str, &str, &str, &str)] = &[
        ("Look", "status", "What the node reports", "The host, active alerts and access."),
        ("Look", "events", "Latest events", "The last 30 events, oldest first."),
        ("Look", "audit", "Action log", "Every action, dry run and refusal, hash-chained."),
        ("Look", "doctor", "Check the install", "Config, service, socket, memory and each access (read-only)."),
        ("Act", "fix", "Fix something", "Detected issues and every fix root allows, plus automation."),
        ("Settings", "access", "Access", "What root lets the node reach (allow/deny)."),
        ("Settings", "serve", "Peers", "What paired peers may ask this node."),
        ("Help", "help", "All commands", "The full command list."),
    ];
    let mut m = Menu::new("node", "");
    m.subtitle = format!("{} as {} · v{}", hostname(), who(), env!("CARGO_PKG_VERSION"));
    let mut group = "";
    for (g, key, label, about) in ENTRIES {
        if *g != group {
            m.section(g);
            group = g;
        }
        m.item(Item::new(*key, *label, *about));
    }
    tui::pick(&m).map(|i| ENTRIES[i].1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recipe_maps_to_a_known_action() {
        for r in RECIPES {
            let a = action_for(r.key, Some("x.service")).unwrap();
            assert!(fixes::known(a), "{}", r.key);
        }
        #[cfg(unix)]
        assert_eq!(action_for("restart", Some("web")), Some(action::CONTAINER_RESTART));
        #[cfg(windows)]
        assert_eq!(action_for("restart", Some("W32Time")), Some(action::UNIT_RESTART));
        assert_eq!(action_for("restart", Some("nginx.service")), Some(action::UNIT_RESTART));
        assert_eq!(action_for("nope", None), None);
    }

    #[test]
    #[cfg(unix)]
    fn presets_make_valid_rules_and_switch() {
        let mut l = LocalFixes::default();
        for p in PRESETS {
            p.rule(true).validate().unwrap();
            assert_eq!(preset_state(&l, p.key), "off");
        }
        assert!(set_preset(&mut l, "journal", "preview").unwrap().contains("allowed fix.journal-vacuum"));
        assert_eq!(preset_state(&l, "journal"), "preview");
        set_preset(&mut l, "journal", "on").unwrap();
        assert_eq!((preset_state(&l, "journal"), l.autofix.len(), l.allow.len()), ("on", 1, 1));
        set_preset(&mut l, "journal", "off").unwrap();
        assert!(l.autofix.is_empty());
        assert_eq!(l.allow, [fixes::JOURNAL_VACUUM], "turning off keeps the manual fix allowed");
        assert!(set_preset(&mut l, "nope", "on").is_err() && set_preset(&mut l, "journal", "maybe").is_err());
        l.validate().unwrap();
    }

    #[test]
    fn windows_has_the_same_names_where_it_has_a_counterpart() {
        let win = ["restart", "packages", "ssh", "reboot", "run"];
        for k in win {
            assert!(recipe(k).is_some(), "{k}");
        }
        if cfg!(windows) {
            assert_eq!(RECIPES.len(), win.len());
            assert!(RECIPES.iter().all(|r| r.needs != Needs::Container));
            assert_eq!(PRESETS.iter().map(|p| p.key).collect::<Vec<_>>(), ["services", "packages"]);
            for r in RECIPES {
                let a = action_for(r.key, Some("W32Time")).unwrap();
                assert!(crate::helper::supported(a), "{a}");
            }
        }
        for p in PRESETS {
            p.rule(true).validate().unwrap();
        }
    }

    #[test]
    fn journal_usage_parses() {
        let t = "Archived and active journals take up 1.5G in the file system.\n";
        assert_eq!(parse_journal_usage(t), Some((1.5 * 1024.0 * 1024.0 * 1024.0) as u64));
        assert_eq!(parse_journal_usage("take up 24.0M in"), Some(24 * 1024 * 1024));
        assert_eq!(parse_journal_usage("nothing"), None);
    }

    #[test]
    fn the_fix_menu_lists_issues_first_and_automation_last() {
        let pol =
            Policy { config: Config::default(), local: LocalFixes::default(), fixes_path: "/x".into(), helper: false };
        let issues = vec![Issue {
            fix: "restart",
            target: Some("nginx.service".into()),
            title: "nginx.service has failed".into(),
            detail: String::new(),
            tone: Tone::Danger,
        }];
        let (m, e) = fix_menu(&pol, &issues);
        let t = m.plain();
        assert!(t.find("DETECTED").unwrap() < t.find("SERVICES").unwrap());
        assert!(t.contains(" 1  restart nginx.service"), "{t}");
        assert!(t.contains("not allowed") && t.contains("No runbooks yet"), "{t}");
        assert!(matches!(e.first(), Some(Entry::Issue(0))) && matches!(e.last(), Some(Entry::Auto)));
        assert_eq!(m.items().len(), e.len());
    }
}
