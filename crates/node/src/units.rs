//! Fits the shipped systemd units to an older systemd.
//!
//! The units use settings from systemd 235-247 (`StateDirectory=`,
//! `IPAddressDeny=`, `ProtectSystem=strict`, `@system-service`...). An older
//! systemd, like the 215 in ReadyNAS OS 6 (Debian 8), logs unknown settings
//! and ignores them, which is mostly harmless, but some go wrong: a syscall
//! filter naming a group it doesn't know can leave an empty allow-list, and
//! without `StateDirectory=` nothing creates the data directory. So install
//! comments out what the running systemd lacks, swaps in the older spelling
//! where one exists (`MemoryLimit=` for `MemoryMax=`), creates the state
//! directories itself, and `doctor` lists what was left out.

/// The first systemd version that knows a setting (or value), for those
/// newer than 209. Settings not listed are older.
fn needs(key: &str, value: &str) -> u32 {
    match key {
        "ProtectProc" => 247,
        "ProtectClock" => 245,
        "ProtectKernelLogs" => 244,
        "ProtectHostname" | "RestrictSUIDSGID" => 242,
        "SystemCallFilter" if value.contains("@system-service") => 239,
        "CollectMode" => 236,
        "StateDirectory" | "StateDirectoryMode" | "IPAddressDeny" | "IPAddressAllow" | "LockPersonality" => 235,
        "RestrictNamespaces" => 233,
        "ProtectSystem" if value == "strict" => 232,
        "ProtectKernelModules" | "ProtectKernelTunables" | "ProtectControlGroups" | "RemoveIPC" => 232,
        "SystemCallFilter" if value.contains('@') => 231,
        "MemoryMax"
        | "MemoryHigh"
        | "CPUWeight"
        | "IOWeight"
        | "MemoryDenyWriteExecute"
        | "RestrictRealtime"
        | "InaccessiblePaths"
        | "ReadWritePaths" => 231,
        "AmbientCapabilities" | "RuntimeMaxSec" => 229,
        "TasksMax" => 227,
        "ProtectSystem" | "ProtectHome" | "SocketUser" | "SocketGroup" | "RemoveOnStop" => 214,
        "CPUQuota" => 213,
        "RuntimeDirectory" | "RuntimeDirectoryMode" | "RestrictAddressFamilies" => 211,
        _ => 0,
    }
}

/// A unit fitted to one systemd version.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Adapted {
    pub text: String,
    /// Settings left out, as `Key=value (systemd N)`.
    pub dropped: Vec<String>,
    /// State directories install must create itself: (path, mode, user).
    pub state_dirs: Vec<(String, u32, String)>,
}

/// Fits `unit` to systemd `version`; `None` (unknown) keeps it as it is.
pub fn adapt(unit: &str, version: Option<u32>) -> Adapted {
    let Some(v) = version else {
        return Adapted { text: unit.to_owned(), ..Default::default() };
    };
    let mut out = Adapted::default();
    let (mut user, mut dirs, mut mode) = (String::from("root"), Vec::new(), 0o755);
    for line in unit.lines() {
        let t = line.trim();
        let Some((key, value)) = t.split_once('=').filter(|_| !t.starts_with('#') && !t.starts_with('[')) else {
            out.text.push_str(line);
            out.text.push('\n');
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "User" => user = value.to_owned(),
            "StateDirectory" => dirs.extend(value.split_whitespace().map(|d| format!("/var/lib/{d}"))),
            "StateDirectoryMode" => mode = u32::from_str_radix(value, 8).unwrap_or(0o755),
            _ => {}
        }
        let min = needs(key, value);
        if min <= v {
            out.text.push_str(line);
            out.text.push('\n');
            continue;
        }
        // The older spelling, where there is one.
        let older = match key {
            "MemoryMax" => Some(format!("MemoryLimit={value}")),
            "ProtectSystem" if v >= 214 => Some("ProtectSystem=full".to_owned()),
            _ => None,
        };
        out.text.push_str(&format!("# needs systemd {min}: {t}\n"));
        if let Some(o) = older {
            out.text.push_str(&o);
            out.text.push('\n');
        }
        out.dropped.push(format!("{t} (systemd {min})"));
    }
    if v < 235 {
        out.state_dirs = dirs.into_iter().map(|d| (d, mode, user.clone())).collect();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNIT: &str = include_str!("../../../dist/systemd/nightgrid-node.service");

    #[test]
    fn a_current_systemd_gets_the_unit_as_it_is() {
        assert_eq!(adapt(UNIT, Some(255)).text, UNIT);
        assert!(adapt(UNIT, Some(255)).dropped.is_empty());
        assert_eq!(adapt(UNIT, None).text, UNIT);
    }

    #[test]
    fn systemd_215_from_readynas_os_6() {
        let a = adapt(UNIT, Some(215));
        // Nothing newer than 215 is left active.
        for line in a.text.lines().filter(|l| !l.starts_with('#') && l.contains('=')) {
            let (k, v) = line.split_once('=').unwrap();
            assert!(needs(k, v) <= 215, "{line}");
        }
        assert!(a.text.contains("\nMemoryLimit=64M\n") && a.text.contains("\nProtectSystem=full\n"), "{}", a.text);
        assert!(a.text.contains("# needs systemd 239: SystemCallFilter=@system-service"));
        assert!(a.text.contains("\nNoNewPrivileges=yes\n") && a.text.contains("\nRuntimeDirectory=nightgrid-node\n"));
        assert!(a.dropped.iter().any(|d| d.starts_with("IPAddressDeny=any")), "{:?}", a.dropped);
        assert_eq!(a.state_dirs, [("/var/lib/nightgrid-node".to_owned(), 0o700, "nightgrid-node".to_owned())]);
    }

    #[test]
    fn every_shipped_unit_fits_systemd_215() {
        for unit in [
            crate::install::NETWATCH_UNIT,
            crate::install::CHECKS_UNIT,
            crate::install::PEER_UNIT,
            crate::install::STATUS_UNIT,
            crate::install::HELPER_SOCKET_UNIT,
            crate::install::HELPER_SERVICE_UNIT,
            crate::install::HEALTH_SOCKET_UNIT,
            crate::install::HEALTH_SERVICE_UNIT,
        ] {
            let a = adapt(unit, Some(215));
            assert!(a.text.contains("[Service]") || a.text.contains("[Socket]"));
            assert!(!a.text.lines().any(|l| l.starts_with("SystemCallFilter=@")), "{}", a.text);
        }
    }
}
