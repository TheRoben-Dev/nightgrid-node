// Vendored from Nightgrid (https://github.com/TheRoben-Dev/nightgrid)
// core/src/system/smart.rs at commit 8eedcf2, MIT License, (c) 2026 TheRoben.
// Local changes are marked `NODE:` (the SSH and sudo plumbing is removed;
// the node's root helper runs the same scan commands).
//! SMART disk health from `smartctl -j` (smartmontools 7+), run as root.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{sections, SECTION};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Health {
    Ok,
    /// Couldn't read SMART (unsupported, USB bridge, ...).
    Unknown,
    /// Worth watching (reallocated sectors, hot, worn).
    Warn,
    /// Failing or failed.
    Bad,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmartAttr {
    pub id: u16,
    pub name: String,
    pub value: u16,
    pub worst: u16,
    pub thresh: u16,
    /// Raw value as smartctl prints it (vendor-specific format).
    pub raw: String,
    /// `now` / `past` when the attribute crossed its threshold.
    pub failed: Option<String>,
    pub health: Health,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NvmeHealth {
    pub critical_warning: u8,
    pub available_spare: u8,
    pub spare_threshold: u8,
    /// Wear estimate; may exceed 100.
    pub percentage_used: u16,
    pub media_errors: u64,
    pub error_log_entries: u64,
    pub unsafe_shutdowns: u64,
    pub bytes_written: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Disk {
    /// Device path, e.g. `/dev/sda`.
    pub name: String,
    /// `ATA`, `NVMe`, `SCSI`.
    pub protocol: String,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub capacity: Option<u64>,
    /// 0 for SSDs, `None` if unknown.
    pub rotation_rpm: Option<u32>,
    /// Overall self-assessment.
    pub passed: Option<bool>,
    pub temp_c: Option<i32>,
    pub power_on_hours: Option<u64>,
    pub health: Health,
    /// Plain-language reasons for a non-OK health.
    pub issues: Vec<String>,
    pub attributes: Vec<SmartAttr>,
    pub nvme: Option<NvmeHealth>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisksReport {
    pub disks: Vec<Disk>,
    /// Worst health over all disks (`Ok` when there are none).
    pub health: Health,
}

/// Scans and reads every device in one root session. Each device's JSON
/// follows a [`SECTION`] line.
pub fn scan_command() -> String {
    format!(
        "command -v smartctl >/dev/null 2>&1 || {{ echo '@nosmartctl'; exit 0; }}; \
         smartctl --scan-open 2>/dev/null | while read -r dev _d type _rest; do \
           case \"$dev\" in /dev/*) ;; *) continue ;; esac; \
           echo {SECTION}; \
           if [ \"$_d\" = \"-d\" ] && [ -n \"$type\" ]; then smartctl -a -j -d \"$type\" \"$dev\"; else smartctl -a -j \"$dev\"; fi; \
         done; true"
    )
}

/// Raw counters that should stay at zero: id, what a non-zero count means.
const WATCHED: [(u16, &str); 6] = [
    (5, "reallocated sectors"),
    (10, "spin-up retries"),
    (187, "uncorrectable errors"),
    (197, "sectors pending reallocation"),
    (198, "offline uncorrectable sectors"),
    (199, "interface CRC errors (check the cable)"),
];

fn u64_at(v: &Value, path: &[&str]) -> Option<u64> {
    path.iter().try_fold(v, |v, k| v.get(k))?.as_u64()
}

fn str_at(v: &Value, path: &[&str]) -> Option<String> {
    path.iter().try_fold(v, |v, k| v.get(k))?.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Parses one device's `smartctl -a -j` output and judges its health.
pub fn parse_disk(json: &str) -> Option<Disk> {
    let v: Value = serde_json::from_str(json.trim()).ok()?;
    let name = str_at(&v, &["device", "name"])?;
    let protocol = str_at(&v, &["device", "protocol"]).unwrap_or_else(|| "?".into());
    let mut issues: Vec<(Health, String)> = Vec::new();

    let exit = u64_at(&v, &["smartctl", "exit_status"]).unwrap_or(0);
    let messages: Vec<String> = v
        .pointer("/smartctl/messages")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|m| m.get("string").and_then(Value::as_str)).map(str::to_owned).collect())
        .unwrap_or_default();
    if exit & 0b11 != 0 {
        let why = messages.first().cloned().unwrap_or_else(|| "smartctl couldn't open the device".into());
        issues.push((Health::Unknown, why));
    }
    if v.pointer("/smart_support/available").and_then(Value::as_bool) == Some(false) {
        issues.push((Health::Unknown, "SMART isn't supported by this device".into()));
    }

    let passed = v.pointer("/smart_status/passed").and_then(Value::as_bool);
    if passed == Some(false) || exit & 0b1000 != 0 {
        issues.push((Health::Bad, "SMART self-assessment FAILED: back up this disk now".into()));
    }

    let mut attributes = Vec::new();
    if let Some(table) = v.pointer("/ata_smart_attributes/table").and_then(Value::as_array) {
        for a in table {
            let id = a.get("id").and_then(Value::as_u64).unwrap_or(0) as u16;
            let value = a.get("value").and_then(Value::as_u64).unwrap_or(0) as u16;
            let worst = a.get("worst").and_then(Value::as_u64).unwrap_or(0) as u16;
            let thresh = a.get("thresh").and_then(Value::as_u64).unwrap_or(0) as u16;
            let attr_name = a.get("name").and_then(Value::as_str).unwrap_or("Unknown_Attribute").to_owned();
            let raw_value = u64_at(a, &["raw", "value"]).unwrap_or(0);
            let raw = str_at(a, &["raw", "string"]).unwrap_or_else(|| raw_value.to_string());
            let failed =
                a.get("when_failed").and_then(Value::as_str).filter(|s| !s.is_empty() && *s != "-").map(str::to_owned);
            let mut health = Health::Ok;
            if failed.as_deref() == Some("now") || (thresh > 0 && value <= thresh) {
                health = Health::Bad;
                issues.push((Health::Bad, format!("{attr_name} is below its failure threshold ({value} ≤ {thresh})")));
            } else if failed.as_deref() == Some("past") {
                health = Health::Warn;
                issues.push((Health::Warn, format!("{attr_name} was below its threshold in the past")));
            }
            if let Some((_, what)) = WATCHED.iter().find(|(w, _)| *w == id) {
                // Some vendors pack extra data in the high bytes.
                let count = raw_value & 0xFFFF_FFFF;
                if count > 0 {
                    health = health.max(Health::Warn);
                    issues.push((Health::Warn, format!("{count} {what}")));
                }
            }
            attributes.push(SmartAttr { id, name: attr_name, value, worst, thresh, raw, failed, health });
        }
    }

    let nvme = v.get("nvme_smart_health_information_log").map(|n| {
        let g = |k: &str| n.get(k).and_then(Value::as_u64).unwrap_or(0);
        NvmeHealth {
            critical_warning: g("critical_warning") as u8,
            available_spare: g("available_spare") as u8,
            spare_threshold: g("available_spare_threshold") as u8,
            percentage_used: g("percentage_used").min(u64::from(u16::MAX)) as u16,
            media_errors: g("media_errors"),
            error_log_entries: g("num_err_log_entries"),
            unsafe_shutdowns: g("unsafe_shutdowns"),
            bytes_written: g("data_units_written").saturating_mul(512_000),
        }
    });
    if let Some(n) = &nvme {
        if n.critical_warning != 0 {
            issues.push((Health::Bad, format!("NVMe critical warning flags set (0x{:02x})", n.critical_warning)));
        }
        if n.spare_threshold > 0 && n.available_spare < n.spare_threshold {
            issues.push((
                Health::Bad,
                format!("spare blocks below threshold ({}% < {}%)", n.available_spare, n.spare_threshold),
            ));
        }
        if n.percentage_used >= 100 {
            issues.push((Health::Bad, format!("rated endurance used up ({}%)", n.percentage_used)));
        } else if n.percentage_used >= 90 {
            issues.push((Health::Warn, format!("{}% of rated endurance used", n.percentage_used)));
        }
        if n.media_errors > 0 {
            issues.push((Health::Warn, format!("{} media errors", n.media_errors)));
        }
    }

    let rotation_rpm = u64_at(&v, &["rotation_rate"]).map(|r| r as u32);
    let temp_c = v.pointer("/temperature/current").and_then(Value::as_i64).map(|t| t as i32);
    if let Some(t) = temp_c {
        let limit = if protocol == "NVMe" {
            70
        } else if rotation_rpm.unwrap_or(0) > 0 {
            55
        } else {
            65
        };
        if t > limit {
            issues.push((Health::Warn, format!("running hot ({t} °C)")));
        }
    }

    let health =
        issues.iter().map(|(h, _)| *h).max().unwrap_or(if passed.is_some() { Health::Ok } else { Health::Unknown });
    issues.sort_by_key(|i| std::cmp::Reverse(i.0));
    Some(Disk {
        name,
        protocol,
        model: str_at(&v, &["model_name"]).or_else(|| str_at(&v, &["scsi_model_name"])),
        serial: str_at(&v, &["serial_number"]),
        firmware: str_at(&v, &["firmware_version"]),
        capacity: u64_at(&v, &["user_capacity", "bytes"]).or_else(|| u64_at(&v, &["nvme_total_capacity"])),
        rotation_rpm,
        passed,
        temp_c,
        power_on_hours: u64_at(&v, &["power_on_time", "hours"]),
        health,
        issues: issues.into_iter().map(|(_, s)| s).collect(),
        attributes,
        nvme,
    })
}

// NODE: the error is a message, not the app's SystemError.
pub fn parse_scan(out: &str) -> Result<DisksReport, String> {
    if out.lines().any(|l| l.trim() == "@nosmartctl") {
        return Err("smartctl isn't installed (install smartmontools)".into());
    }
    let disks: Vec<Disk> = sections(out).into_iter().skip(1).filter_map(parse_disk).collect();
    let health = disks.iter().map(|d| d.health).max().unwrap_or(Health::Ok);
    Ok(DisksReport { disks, health })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SATA_SSD: &str = include_str!("fixtures/smart_sata_ssd.json");
    const HDD_WORN: &str = include_str!("fixtures/smart_hdd_worn.json");
    const NVME: &str = include_str!("fixtures/smart_nvme.json");
    const USB: &str = include_str!("fixtures/smart_usb_unknown.json");

    #[test]
    fn healthy_ssd() {
        let d = parse_disk(SATA_SSD).unwrap();
        assert_eq!(d.name, "/dev/sda");
        assert_eq!(d.protocol, "ATA");
        assert_eq!(d.model.as_deref(), Some("Example SSD 500GB"));
        assert_eq!(d.capacity, Some(500_107_862_016));
        assert_eq!(d.rotation_rpm, Some(0));
        assert_eq!((d.passed, d.temp_c, d.power_on_hours), (Some(true), Some(33), Some(15_310)));
        assert_eq!(d.health, Health::Ok, "{:?}", d.issues);
        assert!(d.issues.is_empty());
        assert_eq!(d.attributes.len(), 6);
    }

    #[test]
    fn worn_hdd_warns() {
        let d = parse_disk(HDD_WORN).unwrap();
        assert_eq!(d.health, Health::Warn);
        assert_eq!(d.issues, ["8 reallocated sectors", "2 sectors pending reallocation"]);
        let a = d.attributes.iter().find(|a| a.id == 5).unwrap();
        assert_eq!((a.health, a.raw.as_str()), (Health::Warn, "8"));
        // Packed raw value (high bytes = vendor data) counts only the low part.
        assert_eq!(d.attributes.iter().find(|a| a.id == 199).unwrap().health, Health::Ok);
    }

    #[test]
    fn nvme_wear_and_errors() {
        let d = parse_disk(NVME).unwrap();
        assert_eq!(d.protocol, "NVMe");
        let n = d.nvme.as_ref().unwrap();
        assert_eq!((n.percentage_used, n.media_errors, n.unsafe_shutdowns), (92, 3, 41));
        assert_eq!(n.bytes_written, 123_456 * 512_000);
        assert_eq!(d.health, Health::Warn);
        assert_eq!(d.issues, ["92% of rated endurance used", "3 media errors", "running hot (74 °C)"]);
    }

    #[test]
    fn failing_disk_is_bad() {
        let json = HDD_WORN.replace(r#""passed": true"#, r#""passed": false"#);
        let d = parse_disk(&json).unwrap();
        assert_eq!(d.health, Health::Bad);
        assert!(d.issues[0].contains("FAILED"));
    }

    #[test]
    fn unreadable_usb_bridge() {
        let d = parse_disk(USB).unwrap();
        assert_eq!(d.health, Health::Unknown);
        assert!(d.issues[0].contains("Unknown USB bridge"));
    }

    #[test]
    fn scan_output() {
        let out = format!("{SECTION}\n{SATA_SSD}\n{SECTION}\n{NVME}\n{SECTION}\nnot json\n");
        let r = parse_scan(&out).unwrap();
        assert_eq!(r.disks.len(), 2);
        assert_eq!(r.health, Health::Warn);
        assert!(parse_scan("@nosmartctl\n").is_err());
        assert_eq!(parse_scan("").unwrap().health, Health::Ok);
    }
}
