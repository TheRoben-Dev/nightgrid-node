// Vendored from Nightgrid (https://github.com/TheRoben-Dev/nightgrid)
// core/src/win/disks.rs and core/src/win/updates.rs (DISKS, POOLS, CHECK and
// their parsers), MIT License, (c) 2026 TheRoben. Local changes are marked
// `NODE:` (the SSH plumbing is removed; the node's health helper service runs
// the same PowerShell as LocalSystem; the node's update report has no holds).
//! Health reads on Windows (W2c): physical disks from the Storage module
//! (`Get-PhysicalDisk`, `Get-StorageReliabilityCounter`), Storage Spaces
//! pools, and pending updates from the Windows Update Agent. The scripts and
//! parsers are the app's own, so the node and the app judge a disk the same
//! way.

use serde::Deserialize;

use super::packages::{Manager, PackageUpdate, UpdatesReport};
use super::pools::{Pool, PoolDevice, PoolKind, PoolsReport};
use super::smart::{Disk, DisksReport, Health};

pub const DISKS: &str = r#"$ErrorActionPreference = 'Stop'
$out = @(Get-PhysicalDisk | Sort-Object { [int]$_.DeviceId } | ForEach-Object {
  $r = $null; try { $r = $_ | Get-StorageReliabilityCounter -ErrorAction Stop } catch { }
  [ordered]@{
    id = [string]$_.DeviceId; model = [string]$_.FriendlyName; serial = ([string]$_.SerialNumber).Trim(); firmware = [string]$_.FirmwareVersion
    bus = [string]$_.BusType; media = [string]$_.MediaType; size = [int64]$_.Size; rpm = $_.SpindleSpeed
    health = [string]$_.HealthStatus; status = (@($_.OperationalStatus) -join ', ')
    temp = $(if ($r) { $r.Temperature } else { $null }); wear = $(if ($r) { $r.Wear } else { $null }); hours = $(if ($r) { $r.PowerOnHours } else { $null })
    readErr = $(if ($r) { $r.ReadErrorsUncorrected } else { $null }); writeErr = $(if ($r) { $r.WriteErrorsUncorrected } else { $null })
  }
})
ConvertTo-Json -InputObject $out -Depth 3 -Compress"#;

pub const POOLS: &str = r#"$ErrorActionPreference = 'Stop'
$out = @(Get-StoragePool -IsPrimordial $false | ForEach-Object {
  $p = $_
  [ordered]@{
    name = [string]$p.FriendlyName; health = [string]$p.HealthStatus; status = (@($p.OperationalStatus) -join ', ')
    size = [int64]$p.Size; used = [int64]$p.AllocatedSize
    disks = @($p | Get-PhysicalDisk | ForEach-Object { [ordered]@{ name = [string]$_.FriendlyName; health = [string]$_.HealthStatus; status = (@($_.OperationalStatus) -join ', ') } })
    vdisks = @($p | Get-VirtualDisk | ForEach-Object { [ordered]@{ name = [string]$_.FriendlyName; health = [string]$_.HealthStatus; status = (@($_.OperationalStatus) -join ', '); layout = [string]$_.ResiliencySettingName } })
    jobs = @(Get-StorageJob -ErrorAction SilentlyContinue | Where-Object { $_.JobState -eq 'Running' } | ForEach-Object { [ordered]@{ name = [string]$_.Name; pct = [int]$_.PercentComplete } })
  }
})
ConvertTo-Json -InputObject $out -Depth 4 -Compress"#;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDisk {
    id: String,
    model: Option<String>,
    serial: Option<String>,
    firmware: Option<String>,
    bus: String,
    media: String,
    size: Option<u64>,
    rpm: Option<u64>,
    health: String,
    status: String,
    temp: Option<i64>,
    wear: Option<i64>,
    hours: Option<u64>,
    read_err: Option<u64>,
    write_err: Option<u64>,
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

fn disk(r: RawDisk) -> Disk {
    let mut issues: Vec<(Health, String)> = Vec::new();
    match r.health.as_str() {
        "Healthy" => {}
        "Warning" => {
            issues.push((Health::Warn, format!("Windows reports this disk's health as Warning ({}).", r.status)))
        }
        "Unhealthy" => issues.push((
            Health::Bad,
            format!("Windows reports this disk as Unhealthy ({}). Back it up and replace it.", r.status),
        )),
        _ => issues.push((Health::Unknown, "Windows doesn't know this disk's health.".into())),
    }
    if let Some(w) = r.wear.filter(|w| *w >= 80) {
        issues.push((if w >= 100 { Health::Bad } else { Health::Warn }, format!("{w}% of its rated wear is used.")));
    }
    let errs = r.read_err.unwrap_or(0) + r.write_err.unwrap_or(0);
    if errs > 0 {
        issues.push((Health::Warn, format!("{errs} uncorrected read/write errors.")));
    }
    if let Some(t) = r.temp.filter(|t| *t >= 60) {
        issues.push((if t >= 70 { Health::Bad } else { Health::Warn }, format!("Running hot: {t} °C.")));
    }
    issues.sort_by_key(|i| std::cmp::Reverse(i.0));
    let health = issues.first().map_or(Health::Ok, |i| i.0);
    let rotation_rpm = match r.media.as_str() {
        "SSD" => Some(0),
        "HDD" => r.rpm.filter(|v| *v > 0 && *v < 100_000).map(|v| v as u32),
        _ => None,
    };
    Disk {
        name: format!("PhysicalDisk{}", r.id),
        protocol: if r.bus.is_empty() { "?".into() } else { r.bus },
        model: nonempty(r.model),
        serial: nonempty(r.serial),
        firmware: nonempty(r.firmware),
        capacity: r.size.filter(|s| *s > 0),
        rotation_rpm,
        passed: match r.health.as_str() {
            "Healthy" => Some(true),
            "Unhealthy" => Some(false),
            _ => None,
        },
        temp_c: r.temp.filter(|t| *t > 0).map(|t| t as i32),
        power_on_hours: r.hours.filter(|h| *h > 0),
        health,
        issues: issues.into_iter().map(|i| i.1).collect(),
        attributes: Vec::new(),
        nvme: None,
    }
}

/// Parses the [`DISKS`] JSON.
pub fn parse_disks(json: &str) -> Result<DisksReport, String> {
    let raw: Vec<RawDisk> =
        serde_json::from_str(json.trim()).map_err(|e| format!("couldn't read the disk list: {e}"))?;
    let disks: Vec<Disk> = raw.into_iter().map(disk).collect();
    let health = disks.iter().map(|d| d.health).max().unwrap_or(Health::Ok);
    Ok(DisksReport { disks, health })
}

#[derive(Deserialize)]
struct RawPool {
    name: String,
    health: String,
    status: String,
    size: Option<u64>,
    used: Option<u64>,
    #[serde(default)]
    disks: Vec<RawMember>,
    #[serde(default)]
    vdisks: Vec<RawMember>,
    #[serde(default)]
    jobs: Vec<RawJob>,
}

#[derive(Deserialize)]
struct RawMember {
    name: String,
    health: String,
    status: String,
    #[serde(default)]
    layout: Option<String>,
}

#[derive(Deserialize)]
struct RawJob {
    name: String,
    pct: f32,
}

fn word_health(h: &str) -> Health {
    match h {
        "Healthy" => Health::Ok,
        "Warning" => Health::Warn,
        "Unhealthy" => Health::Bad,
        _ => Health::Unknown,
    }
}

/// Parses the [`POOLS`] JSON.
pub fn parse_pools(json: &str) -> Result<PoolsReport, String> {
    let raw: Vec<RawPool> =
        serde_json::from_str(json.trim()).map_err(|e| format!("couldn't read the storage pools: {e}"))?;
    let pools: Vec<Pool> = raw
        .into_iter()
        .map(|p| {
            let mut issues: Vec<(Health, String)> = Vec::new();
            let ph = word_health(&p.health);
            if ph != Health::Ok {
                issues.push((ph, format!("The pool is {} ({}).", p.health, p.status)));
            }
            for v in &p.vdisks {
                let h = word_health(&v.health);
                if h != Health::Ok {
                    issues.push((h, format!("Virtual disk {} is {} ({}).", v.name, v.health, v.status)));
                }
            }
            for d in &p.disks {
                let h = word_health(&d.health);
                if h != Health::Ok {
                    issues.push((h, format!("Disk {} is {} ({}).", d.name, d.health, d.status)));
                }
            }
            issues.sort_by_key(|i| std::cmp::Reverse(i.0));
            let layouts: Vec<String> = p
                .vdisks
                .iter()
                .filter_map(|v| v.layout.clone().filter(|l| !l.is_empty()).map(|l| format!("{} ({l})", v.name)))
                .collect();
            let job = p.jobs.first();
            Pool {
                kind: PoolKind::StorageSpaces,
                name: p.name,
                detail: (!layouts.is_empty()).then(|| layouts.join(", ")),
                state: p.status,
                health: issues.first().map_or(ph, |i| i.0.max(ph)),
                size: p.size.filter(|s| *s > 0),
                used: p.used,
                devices: p
                    .disks
                    .into_iter()
                    .map(|d| PoolDevice { name: d.name, state: d.status, errors: None })
                    .collect(),
                issues: issues.into_iter().map(|i| i.1).collect(),
                activity: job.map(|j| format!("{} running", j.name)),
                progress: job.map(|j| j.pct),
            }
        })
        .collect();
    let health = pools.iter().map(|p| p.health).max().unwrap_or(Health::Ok);
    Ok(PoolsReport { pools, health })
}

pub const CHECK: &str = r#"$ErrorActionPreference = 'Stop'
$o = [ordered]@{ updates = @(); hidden = @(); error = $null; reboot = @() }
$rk = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired'
if (Test-Path $rk) { $o.reboot += 'Windows Update is waiting for a restart' }
if (Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending') { $o.reboot += 'a Windows component is waiting for a restart' }
try {
  $s = New-Object -ComObject Microsoft.Update.Session
  $s.ClientApplicationID = 'Nightgrid'
  $r = $s.CreateUpdateSearcher().Search("IsInstalled=0 and Type='Software'")
  $all = @(foreach ($u in $r.Updates) {
    [ordered]@{
      title = [string]$u.Title
      kb = (@($u.KBArticleIDs) | ForEach-Object { 'KB' + $_ }) -join ', '
      cat = (@($u.Categories) | Select-Object -First 1 | ForEach-Object { $_.Name }) -join ''
      severity = [string]$u.MsrcSeverity
      hidden = [bool]$u.IsHidden
    }
  })
  $o.updates = @($all | Where-Object { -not $_.hidden })
  $o.hidden = @($all | Where-Object { $_.hidden })
} catch {
  $o.error = $_.Exception.Message
}
ConvertTo-Json -InputObject $o -Depth 3 -Compress"#;

#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    updates: Vec<RawUpdate>,
    /// Updates hidden in Windows Update: held.
    #[serde(default)]
    hidden: Vec<RawUpdate>,
    error: Option<String>,
    #[serde(default)]
    reboot: Vec<String>,
}

#[derive(Deserialize)]
struct RawUpdate {
    title: String,
    #[serde(default)]
    kb: String,
    #[serde(default)]
    cat: String,
    #[serde(default)]
    severity: String,
}

/// Parses the [`CHECK`] JSON.
pub fn parse_updates(json: &str) -> Result<UpdatesReport, String> {
    let raw: Raw =
        serde_json::from_str(json.trim()).map_err(|e| format!("couldn't read Windows Update's answer: {e}"))?;
    let note = raw.error.map(|e| {
        if e.contains("0x80070005") || e.to_ascii_lowercase().contains("access is denied") {
            "Windows Update didn't answer this account. Log in as an administrator to check for updates.".to_owned()
        } else {
            format!("Windows Update couldn't search: {}", e.trim())
        }
    });
    // NODE: the node's report has no holds; hidden (held) updates are left out.
    let _ = raw.hidden;
    let updates = raw
        .updates
        .into_iter()
        .map(|u| {
            let repo = match (u.cat.trim(), u.severity.trim()) {
                ("", "") => None,
                (c, "") => Some(c.to_owned()),
                ("", s) => Some(format!("{s} security")),
                // "security" in the repo marks the row as a security update.
                (c, s) => Some(format!("{c} · {s} security")),
            };
            PackageUpdate {
                available: if u.kb.is_empty() { "update".into() } else { u.kb },
                name: u.title,
                current: None,
                repo,
            }
        })
        .collect();
    Ok(UpdatesReport {
        manager: Some(Manager::WindowsUpdate),
        updates,
        reboot_required: !raw.reboot.is_empty(),
        reboot_reasons: raw.reboot,
        note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates() {
        let r = parse_updates(
            r#"{"updates":[{"title":"2026-11 Cumulative Update","kb":"KB5099999","cat":"Security Updates","severity":"Critical","hidden":false}],
                "hidden":[{"title":"Held driver","kb":"","cat":"Drivers","severity":"","hidden":true}],
                "error":null,"reboot":["Windows Update is waiting for a restart"]}"#,
        )
        .unwrap();
        assert_eq!(r.manager, Some(Manager::WindowsUpdate));
        assert_eq!(r.updates.len(), 1);
        assert_eq!(r.updates[0].available, "KB5099999");
        assert_eq!(r.updates[0].repo.as_deref(), Some("Security Updates · Critical security"));
        assert!(r.reboot_required && r.note.is_none());
        let e =
            parse_updates(r#"{"updates":[],"hidden":[],"error":"0x80070005 Access is denied","reboot":[]}"#).unwrap();
        assert!(e.note.unwrap().contains("administrator"));
    }

    #[test]
    fn disks() {
        let r = parse_disks(
            r#"[{"id":"0","model":"Samsung SSD 980 PRO","serial":"S5GX ","firmware":"5B2QGXA7","bus":"NVMe","media":"SSD","size":1000204886016,"rpm":0,"health":"Healthy","status":"OK","temp":41,"wear":3,"hours":5120,"readErr":0,"writeErr":0},
                {"id":"1","model":"WDC WD80EFZX","serial":"","firmware":"","bus":"SATA","media":"HDD","size":8001563222016,"rpm":5400,"health":"Warning","status":"Predictive Failure","temp":null,"wear":null,"hours":null,"readErr":null,"writeErr":null}]"#,
        )
        .unwrap();
        assert_eq!(r.disks.len(), 2);
        let a = &r.disks[0];
        assert_eq!(
            (a.name.as_str(), a.protocol.as_str(), a.rotation_rpm, a.temp_c, a.health),
            ("PhysicalDisk0", "NVMe", Some(0), Some(41), Health::Ok)
        );
        assert_eq!(a.serial.as_deref(), Some("S5GX"));
        let b = &r.disks[1];
        assert_eq!((b.health, b.rotation_rpm, b.serial.as_deref()), (Health::Warn, Some(5400), None));
        assert!(b.issues[0].contains("Predictive Failure"));
        assert_eq!(r.health, Health::Warn);
        assert_eq!(parse_disks("[]").unwrap().disks.len(), 0);
    }

    #[test]
    fn pools() {
        let r = parse_pools(
            r#"[{"name":"Tank","health":"Warning","status":"Degraded","size":16000000000000,"used":9000000000000,
                 "disks":[{"name":"Disk A","health":"Healthy","status":"OK"},{"name":"Disk B","health":"Unhealthy","status":"Lost Communication"}],
                 "vdisks":[{"name":"Data","health":"Warning","status":"Degraded","layout":"Mirror"}],
                 "jobs":[{"name":"Repair","pct":42}]}]"#,
        )
        .unwrap();
        let p = &r.pools[0];
        assert_eq!(
            (p.kind, p.health, p.detail.as_deref()),
            (PoolKind::StorageSpaces, Health::Bad, Some("Data (Mirror)"))
        );
        assert_eq!(p.devices.len(), 2);
        assert!(p.issues[0].contains("Disk B"));
        assert_eq!(p.progress, Some(42.0));
    }
}

// NODE: the battery read is the node's own (W2d); the app has no Windows UPS read.

/// The UPS target that means "the UPS on this host's USB port": Windows
/// shows a USB HID UPS as a battery (`Win32_Battery`).
pub const BATTERY_TARGET: &str = "battery";

pub const BATTERY: &str = r#"$ErrorActionPreference = 'Stop'
$out = @(Get-CimInstance -ClassName Win32_Battery | ForEach-Object {
  [ordered]@{ name = [string]$_.Name; device = [string]$_.DeviceID; state = [int]$_.BatteryStatus; charge = $_.EstimatedChargeRemaining; minutes = $_.EstimatedRunTime; status = [string]$_.Status }
})
ConvertTo-Json -InputObject $out -Depth 2 -Compress"#;

#[derive(Deserialize)]
struct RawBattery {
    name: Option<String>,
    device: Option<String>,
    state: i64,
    charge: Option<i64>,
    minutes: Option<i64>,
    status: Option<String>,
}

/// Turns the battery read into `upsc`-style `key: value` lines, so the UPS
/// reading, its alerts and the shutdown guard are the same as with NUT.
/// `BatteryStatus`: 1 discharging, 2 on AC, 3 fully charged, 4 low,
/// 5 critical, 6–9 charging (8 and 9 also low or critical), 11 partly charged.
pub fn battery_text(json: &str) -> Result<String, String> {
    let all: Vec<RawBattery> = serde_json::from_str(json.trim()).map_err(|e| format!("battery read: {e}"))?;
    let Some(b) = all.first() else {
        return Ok("Error: Windows sees no UPS or battery on this host (connect the UPS's USB cable)\n".into());
    };
    let mut flags: Vec<&str> = match b.state {
        1 | 4 | 5 => vec!["OB"],
        _ => vec!["OL"],
    };
    if matches!(b.state, 4 | 5 | 8 | 9) {
        flags.push("LB");
    }
    if matches!(b.state, 6..=9) {
        flags.push("CHRG");
    }
    if matches!(b.status.as_deref(), Some("Degraded" | "Pred Fail")) {
        flags.push("RB");
    }
    let mut out = format!("ups.status: {}\n", flags.join(" "));
    if let Some(c) = b.charge.filter(|c| (0..=100).contains(c)) {
        out.push_str(&format!("battery.charge: {c}\n"));
    }
    // 71582788 minutes is Windows' "unknown" (on AC); only a real estimate counts.
    if let Some(m) = b.minutes.filter(|m| (0..=100_000).contains(m)) {
        out.push_str(&format!("battery.runtime: {}\n", m * 60));
    }
    let model = b.name.as_deref().filter(|n| !n.trim().is_empty()).or(b.device.as_deref());
    if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
        out.push_str(&format!("device.model: {m}\n"));
    }
    Ok(out)
}

#[cfg(test)]
mod battery_tests {
    use super::*;
    use crate::health::ups::parse;

    #[test]
    fn battery_reads_like_upsc() {
        let r = parse(
            &battery_text(
                r#"[{"name":"Back-UPS ES 700","device":"3B1234","state":1,"charge":64,"minutes":11,"status":"OK"}]"#,
            )
            .unwrap(),
        );
        assert!(r.on_battery && !r.low_battery && !r.replace_battery && r.error.is_none());
        assert_eq!((r.charge, r.runtime, r.model.as_deref()), (Some(64.0), Some(660), Some("Back-UPS ES 700")));

        let r = parse(
            &battery_text(r#"[{"name":"","device":"X","state":5,"charge":4,"minutes":null,"status":"Pred Fail"}]"#)
                .unwrap(),
        );
        assert!(r.on_battery && r.low_battery && r.replace_battery && r.runtime.is_none());

        let r = parse(
            &battery_text(r#"[{"name":"UPS","device":"X","state":2,"charge":100,"minutes":71582788,"status":"OK"}]"#)
                .unwrap(),
        );
        assert!(!r.on_battery && r.runtime.is_none() && r.status == ["OL"]);

        let r = parse(&battery_text("[]").unwrap());
        assert!(r.error.unwrap().contains("no UPS or battery"));
    }
}
