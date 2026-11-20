//! What kind of host this is: machine, C library, init system, kernel and
//! the features the node can use on it. `install --json` and `doctor --json`
//! report it, so the app can say why something is limited on an older or
//! smaller box (a 32-bit NAS, BusyBox userland, no systemd).
//!
//! Everything is read from files under `root` ("/" for real) and from
//! `uname(2)`, never by running programs, so it is cheap and works on any
//! userland.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// The release asset arch for a `uname -m` machine, or `None` when there is
/// no build for it. Release files are `nightgrid-node-<tag>-<arch>-linux`.
pub fn asset_arch(machine: &str) -> Option<&'static str> {
    Some(match machine {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        // armv8l is a 32-bit userland on an ARMv8 CPU: aarch64 may not run.
        "armv7l" | "armv7" | "armv7hl" | "armv8l" => "armv7",
        "armv6l" | "armv6" => "armv6",
        "i386" | "i486" | "i586" | "i686" | "x86" => "i686",
        _ => return None,
    })
}

/// The init system that starts services here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Init {
    Systemd,
    Openrc,
    Sysvinit,
    /// BusyBox's own init, or anything else without a service manager the
    /// node can install into.
    Other,
}

impl Init {
    pub fn name(self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::Openrc => "openrc",
            Self::Sysvinit => "sysvinit",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostFacts {
    /// `uname -m`, e.g. `armv7l`.
    pub arch: String,
    /// The release asset arch for it (`armv7`), absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_arch: Option<&'static str>,
    /// The host's C library: `glibc`, `musl`, `uclibc` or `unknown`. The
    /// node itself is static and doesn't use it; it tells the app what
    /// userland to expect.
    pub libc: &'static str,
    /// `systemd`, `openrc`, `sysvinit` or `other`.
    pub init: &'static str,
    /// systemd's version (`215`), when systemd runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub init_version: Option<u32>,
    /// `uname -r`.
    pub kernel: String,
    /// `/bin/sh` and the tools are BusyBox.
    pub busybox: bool,
    /// What the node can't do here, one short sentence each (empty on a
    /// modern systemd host).
    pub limits: Vec<String>,
}

/// Reads the facts of the host the node runs on.
pub fn facts() -> HostFacts {
    let (machine, kernel) = uname();
    facts_at(Path::new("/"), &machine, &kernel, systemd_version())
}

/// The same, from files under `root`; for tests.
pub fn facts_at(root: &Path, machine: &str, kernel: &str, systemd: Option<u32>) -> HostFacts {
    let at = |p: &str| -> PathBuf { root.join(p.trim_start_matches('/')) };
    let init = init_at(root);
    let libc = if any_exists(root, "lib", "ld-musl-") {
        "musl"
    } else if any_exists(root, "lib", "ld-uClibc") {
        "uclibc"
    } else if ["lib", "lib64", "usr/lib", "lib/x86_64-linux-gnu", "lib/arm-linux-gnueabihf", "lib/aarch64-linux-gnu"]
        .iter()
        .any(|d| at(d).join("libc.so.6").exists())
        || any_exists(root, "lib", "ld-linux")
    {
        "glibc"
    } else {
        "unknown"
    };
    let busybox = std::fs::read_link(at("/bin/sh")).map(|t| t.to_string_lossy().contains("busybox")).unwrap_or(false)
        || (at("/bin/busybox").exists() && !at("/bin/bash").exists() && !at("/usr/bin/bash").exists());
    let mut limits = Vec::new();
    match init {
        Init::Systemd => {}
        Init::Openrc | Init::Sysvinit => limits.push(format!(
            "no systemd ({}): the service runs without systemd's sandbox and memory cap, and the extra \
             access (allow ...) and the hub need systemd",
            init.name()
        )),
        Init::Other => limits.push("no service manager the node can install into (systemd, OpenRC or sysvinit)".into()),
    }
    let features = Features::at(root);
    if !features.cgroup2 {
        limits.push("no cgroup v2: no per-service and per-container usage".into());
    }
    if !features.rapl {
        limits.push("no CPU energy counters (RAPL): no power draw".into());
    }
    HostFacts {
        arch: machine.to_owned(),
        asset_arch: asset_arch(machine),
        libc,
        init: init.name(),
        init_version: if init == Init::Systemd { systemd } else { None },
        kernel: kernel.to_owned(),
        busybox,
        limits,
    }
}

/// Kernel features the node uses when they are there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Features {
    pub cgroup2: bool,
    pub psi: bool,
    pub rapl: bool,
}

impl Features {
    pub fn at(root: &Path) -> Self {
        let at = |p: &str| root.join(p);
        Self {
            cgroup2: at("sys/fs/cgroup/cgroup.controllers").exists(),
            psi: at("proc/pressure/cpu").exists(),
            rapl: std::fs::read_dir(at("sys/class/powercap"))
                .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with("intel-rapl")))
                .unwrap_or(false),
        }
    }
}

fn any_exists(root: &Path, dir: &str, prefix: &str) -> bool {
    std::fs::read_dir(root.join(dir))
        .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with(prefix)))
        .unwrap_or(false)
}

/// Which init system runs, by what it leaves behind at run time.
pub fn init_at(root: &Path) -> Init {
    let at = |p: &str| root.join(p);
    if at("run/systemd/system").is_dir() {
        return Init::Systemd;
    }
    if at("run/openrc").is_dir() || (at("sbin/openrc-run").exists() && at("etc/init.d").is_dir()) {
        return Init::Openrc;
    }
    // sysvinit proper (Debian's, or a NAS firmware's): /etc/init.d plus a
    // way to start a daemon as another user and to enable it at boot.
    let ssd = ["sbin/start-stop-daemon", "usr/sbin/start-stop-daemon", "bin/start-stop-daemon"]
        .iter()
        .any(|p| at(p).exists());
    let rc = at("etc/rc2.d").is_dir()
        || at("etc/rc.d/rc2.d").is_dir()
        || ["usr/sbin/update-rc.d", "sbin/chkconfig", "usr/sbin/chkconfig"].iter().any(|p| at(p).exists());
    if at("etc/init.d").is_dir() && ssd && rc {
        return Init::Sysvinit;
    }
    Init::Other
}

/// `uname -m` and `uname -r` (Windows: the arch and `10.0.BUILD`).
#[cfg(windows)]
pub fn uname() -> (String, String) {
    (std::env::consts::ARCH.to_owned(), crate::win::sys::os_version())
}

/// `uname -m` and `uname -r`.
#[cfg(unix)]
pub fn uname() -> (String, String) {
    // SAFETY: uname fills the struct we pass and nothing else.
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return (std::env::consts::ARCH.to_owned(), String::new());
    }
    let s = |f: &[libc::c_char]| {
        // c_char is i8 on x86 and u8 on ARM.
        #[allow(clippy::unnecessary_cast)]
        let b: Vec<u8> = f.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
        String::from_utf8_lossy(&b).into_owned()
    };
    (s(&u.machine), s(&u.release))
}

/// The running systemd's version, from the manager itself (no fork).
pub fn systemd_version() -> Option<u32> {
    if !Path::new("/run/systemd/system").is_dir() {
        return None;
    }
    let out = std::process::Command::new("systemctl").arg("--version").output().ok()?;
    parse_systemd_version(&String::from_utf8_lossy(&out.stdout))
}

/// `systemd 215` / `systemd 255 (255.4-1ubuntu8)` -> 215 / 255.
pub fn parse_systemd_version(text: &str) -> Option<u32> {
    let first = text.lines().next()?;
    let v = first.strip_prefix("systemd ")?;
    v.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, p: &str) {
        let f = root.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, "").unwrap();
    }

    #[test]
    fn asset_arches_cover_old_and_small_boxes() {
        for (m, a) in [
            ("x86_64", Some("x86_64")),
            ("aarch64", Some("aarch64")),
            ("armv7l", Some("armv7")),
            ("armv6l", Some("armv6")),
            ("i386", Some("i686")),
            ("i586", Some("i686")),
            ("i686", Some("i686")),
            ("mips", None),
        ] {
            assert_eq!(asset_arch(m), a, "{m}");
        }
    }

    #[test]
    fn a_readynas_like_debian_with_old_systemd() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join("run/systemd/system")).unwrap();
        touch(r, "lib/arm-linux-gnueabihf/libc.so.6");
        let f = facts_at(r, "armv7l", "4.4.218.armada.1", Some(215));
        assert_eq!((f.arch.as_str(), f.asset_arch, f.libc, f.init), ("armv7l", Some("armv7"), "glibc", "systemd"));
        assert_eq!(f.init_version, Some(215));
        // An old kernel: no cgroup v2 or RAPL, each named.
        assert_eq!(f.limits.len(), 2, "{:?}", f.limits);
    }

    #[test]
    fn busybox_openrc_and_sysvinit_hosts() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(r, "lib/ld-musl-armhf.so.1");
        touch(r, "bin/busybox");
        std::fs::create_dir_all(r.join("run/openrc")).unwrap();
        let f = facts_at(r, "armv6l", "5.10.0", None);
        assert_eq!((f.libc, f.init, f.busybox, f.init_version), ("musl", "openrc", true, None));
        assert!(f.limits[0].starts_with("no systemd (openrc)"), "{:?}", f.limits);

        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join("etc/init.d")).unwrap();
        std::fs::create_dir_all(r.join("etc/rc2.d")).unwrap();
        assert_eq!(init_at(r), Init::Other, "no start-stop-daemon");
        touch(r, "sbin/start-stop-daemon");
        assert_eq!(init_at(r), Init::Sysvinit);
    }

    #[test]
    fn systemd_versions() {
        assert_eq!(parse_systemd_version("systemd 215\n+PAM +AUDIT"), Some(215));
        assert_eq!(parse_systemd_version("systemd 255 (255.4-1ubuntu8.4)\n"), Some(255));
        assert_eq!(parse_systemd_version("nope"), None);
    }
}
