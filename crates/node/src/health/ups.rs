// Vendored from Nightgrid (https://github.com/TheRoben-Dev/nightgrid)
// core/src/ups.rs at commit 8eedcf2 (UpsReading, valid_target, parse, words),
// MIT License, (c) 2026 TheRoben. Local changes are marked `NODE:`.

//! UPS status from NUT's `upsc`.

use serde::{Deserialize, Serialize};

/// `upsname[@hostname[:port]]`, as `upsc` takes it.
pub fn valid_target(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 120
        && !t.starts_with('-')
        && t.chars().all(|c| c.is_ascii_alphanumeric() || "-_.@:[]".contains(c))
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpsReading {
    /// `ups.status` flags: OL (on line), OB (on battery), LB (low battery),
    /// CHRG, RB (replace battery) …
    pub status: Vec<String>,
    pub on_battery: bool,
    pub low_battery: bool,
    pub replace_battery: bool,
    /// Percent.
    pub charge: Option<f32>,
    /// Seconds of runtime left.
    pub runtime: Option<u32>,
    /// Percent.
    pub load: Option<f32>,
    pub input_voltage: Option<f32>,
    pub model: Option<String>,
    /// Why there is no reading (no upsc, driver not connected, unknown UPS).
    pub error: Option<String>,
}

/// Parses `upsc` output (`key: value` lines).
pub fn parse(text: &str) -> UpsReading {
    let mut r = UpsReading::default();
    let mut errors = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line == "NIGHTGRID: no upsc" {
            r.error = Some("upsc isn't installed on this host (install nut-client, or nut)".into());
            return r;
        }
        if line.starts_with("Init SSL") || line.is_empty() {
            continue;
        }
        if let Some(e) = line.strip_prefix("Error: ") {
            errors.push(e.to_owned());
            continue;
        }
        let Some((k, v)) = line.split_once(": ") else { continue };
        let num = || v.trim().parse::<f32>().ok();
        match k.trim() {
            "ups.status" => r.status = v.split_whitespace().map(str::to_owned).collect(),
            "battery.charge" => r.charge = num(),
            "battery.runtime" => r.runtime = num().map(|s| s.max(0.0) as u32),
            "ups.load" => r.load = num(),
            "input.voltage" => r.input_voltage = num(),
            "device.model" | "ups.model" if r.model.is_none() => r.model = Some(v.trim().to_owned()),
            _ => {}
        }
    }
    r.on_battery = r.status.iter().any(|s| s == "OB");
    r.low_battery = r.status.iter().any(|s| s == "LB");
    r.replace_battery = r.status.iter().any(|s| s == "RB");
    if r.status.is_empty() {
        r.error = Some(errors.first().cloned().unwrap_or_else(|| "no ups.status in upsc's answer".into()));
    }
    r
}

/// "4 min", "1 h 5 min", "45 s".
pub fn words(secs: u32) -> String {
    match secs {
        s if s < 90 => format!("{s} s"),
        s if s < 3600 => format!("{} min", (s + 30) / 60),
        s => {
            let m = (s % 3600 + 30) / 60;
            if m == 0 {
                format!("{} h", s / 3600)
            } else {
                format!("{} h {m} min", s / 3600)
            }
        }
    }
}
