// Vendored from Nightgrid (https://github.com/TheRoben-Dev/nightgrid)
// core/src/system/pools.rs at commit 8eedcf2, MIT License, (c) 2026 TheRoben.
// Local changes are marked `NODE:` (the SSH and sudo plumbing is removed;
// the node's root helper runs the same scan commands).
//! Storage pools (M10m): ZFS pools, Linux software RAID (mdraid) and btrfs
//! filesystems, read without root from `zpool`, `/proc/mdstat` and
//! `/sys/fs/btrfs`.

use serde::{Deserialize, Serialize};

use super::smart::Health;
use super::{sections, sh, SECTION};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PoolKind {
    Zfs,
    Mdraid,
    Btrfs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolDevice {
    pub name: String,
    /// `ONLINE`, `DEGRADED`, `FAULTED`, `failed`, `spare`, `missing`, ...
    pub state: String,
    /// Read/write/checksum (ZFS) or btrfs error counters, when not all zero.
    pub errors: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pool {
    pub kind: PoolKind,
    pub name: String,
    /// Mount point (btrfs) or RAID level (mdraid), for context.
    pub detail: Option<String>,
    /// The tool's own word: `ONLINE`, `DEGRADED`, `active`, ...
    pub state: String,
    pub health: Health,
    pub size: Option<u64>,
    pub used: Option<u64>,
    pub devices: Vec<PoolDevice>,
    /// Plain-language problems, worst first.
    pub issues: Vec<String>,
    /// Scrub, resilver, resync or recovery: what and when.
    pub activity: Option<String>,
    /// Percent done of a running resilver, resync or recovery.
    pub progress: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolsReport {
    pub pools: Vec<Pool>,
    /// Worst health over all pools (`Ok` when there are none).
    pub health: Health,
}

/// One round trip, no root: ZFS, mdstat, btrfs from sysfs, degraded mounts.
pub fn scan_command() -> String {
    sh(&format!(
        "if command -v zpool >/dev/null 2>&1; then echo '@zlist'; zpool list -H -p -o name,size,alloc 2>&1; echo '@zstatus'; LANG=C zpool status 2>&1; fi; \
         echo {SECTION}; \
         [ -r /proc/mdstat ] && cat /proc/mdstat; \
         echo {SECTION}; \
         for d in /sys/fs/btrfs/*-*-*-*-*/; do [ -d \"$d\" ] || continue; u=$(basename \"$d\"); \
           echo \"@fs $u $(findmnt -n -o TARGET -S UUID=$u 2>/dev/null | head -n 1)\"; \
           echo \"@label $(cat \"$d/label\" 2>/dev/null)\"; \
           echo \"@devs $(ls \"$d/devices\" 2>/dev/null | tr '\\n' ' ')\"; \
           for i in \"$d\"devinfo/*/; do [ -d \"$i\" ] || continue; \
             echo \"@dev $(basename \"$i\") $(cat \"$i/missing\" 2>/dev/null || echo 0) $(cat \"$i/error_stats\" 2>/dev/null | tr '\\n' ' ')\"; \
           done; \
         done; \
         echo {SECTION}; \
         awk '$3 == \"btrfs\" && $4 ~ /degraded/ {{ print $2 }}' /proc/mounts 2>/dev/null; true"
    ))
}

/// Issues with their weight, before sorting.
type Issues = Vec<(Health, String)>;

fn worst(issues: &[(Health, String)]) -> Health {
    issues.iter().map(|(h, _)| *h).max().unwrap_or(Health::Ok)
}

fn finish(mut issues: Vec<(Health, String)>) -> (Health, Vec<String>) {
    issues.sort_by_key(|a| std::cmp::Reverse(a.0));
    (worst(&issues), issues.into_iter().map(|(_, s)| s).collect())
}

fn pct_in(s: &str) -> Option<f32> {
    let i = s.find('%')?;
    let start = s[..i].rfind(|c: char| !(c.is_ascii_digit() || c == '.')).map_or(0, |p| p + 1);
    s[start..i].parse().ok()
}

/// `zpool list -H -p` and `zpool status`.
pub fn parse_zfs(text: &str) -> Vec<Pool> {
    let (list, status) = match text.split_once("@zstatus") {
        Some((l, s)) => (l, s),
        None => return Vec::new(),
    };
    let sizes: Vec<(String, Option<u64>, Option<u64>)> = list
        .lines()
        .filter_map(|l| {
            let t: Vec<&str> = l.split('\t').collect();
            (t.len() >= 3).then(|| (t[0].to_owned(), t[1].trim().parse().ok(), t[2].trim().parse().ok()))
        })
        .collect();
    let mut pools = Vec::new();
    let mut cur: Option<(Pool, Issues)> = None;
    let mut in_config = false;
    let mut lines = status.lines().peekable();
    let flush = |cur: &mut Option<(Pool, Issues)>, pools: &mut Vec<Pool>| {
        if let Some((mut p, issues)) = cur.take() {
            let (h, list) = finish(issues);
            p.health = h;
            p.issues = list;
            pools.push(p);
        }
    };
    while let Some(raw) = lines.next() {
        let l = raw.trim();
        if let Some(name) = l.strip_prefix("pool:") {
            flush(&mut cur, &mut pools);
            in_config = false;
            let name = name.trim().to_owned();
            let (size, used) = sizes.iter().find(|s| s.0 == name).map_or((None, None), |s| (s.1, s.2));
            cur = Some((
                Pool {
                    kind: PoolKind::Zfs,
                    name,
                    detail: None,
                    state: String::new(),
                    health: Health::Ok,
                    size,
                    used,
                    devices: Vec::new(),
                    issues: Vec::new(),
                    activity: None,
                    progress: None,
                },
                Vec::new(),
            ));
            continue;
        }
        let Some((p, issues)) = cur.as_mut() else { continue };
        if let Some(s) = l.strip_prefix("state:") {
            p.state = s.trim().to_owned();
            match p.state.as_str() {
                "ONLINE" => {}
                "DEGRADED" => {
                    issues.push((Health::Bad, "degraded: a device is missing or failing, redundancy is reduced".into()))
                }
                other => issues.push((Health::Bad, format!("pool is {}", other.to_lowercase()))),
            }
        } else if let Some(s) = l.strip_prefix("scan:") {
            let mut text = s.trim().to_owned();
            // A running scan continues on the next indented lines with the percent.
            while let Some(n) = lines.peek() {
                let t = n.trim();
                if t.is_empty() || t.contains(':') && !t.contains("done") || t.starts_with("config") {
                    break;
                }
                if p.progress.is_none() {
                    p.progress = pct_in(t);
                }
                lines.next();
            }
            if text.len() > 160 {
                text.truncate(160);
            }
            let clean = text.contains("with 0 errors") || text.starts_with("none") || text.contains("in progress");
            if clean {
            } else if let Some(n) =
                text.split("with ").nth(1).and_then(|r| r.split_whitespace().next()).and_then(|n| n.parse::<u64>().ok())
            {
                if n > 0 {
                    issues.push((Health::Warn, format!("last scrub found {n} errors")));
                }
            }
            p.activity = Some(text);
        } else if l.starts_with("NAME") && l.contains("STATE") {
            in_config = true;
        } else if let Some(e) = l.strip_prefix("errors:") {
            in_config = false;
            let e = e.trim();
            if !e.starts_with("No known data errors") {
                issues.push((Health::Bad, format!("data errors: {e}")));
            }
        } else if in_config && !l.is_empty() {
            let t: Vec<&str> = l.split_whitespace().collect();
            if t.len() < 2 || t[0] == p.name {
                continue;
            }
            let counts: Vec<u64> = t.iter().skip(2).take(3).filter_map(|c| c.parse().ok()).collect();
            let errs = counts.iter().any(|c| *c > 0);
            let state = t[1].to_owned();
            if state != "ONLINE"
                && state != "AVAIL"
                && state != "INUSE"
                && !t[0].starts_with("spares")
                && !t[0].starts_with("logs")
                && !t[0].starts_with("cache")
                // A mirror/raidz vdev is degraded because of a device listed below it.
                && (!t[0].contains('-') || state != "DEGRADED")
            {
                issues.push((Health::Bad, format!("{} is {}", t[0], state.to_lowercase())));
            }
            if errs {
                issues.push((
                    Health::Warn,
                    format!(
                        "{} has read/write/checksum errors ({})",
                        t[0],
                        counts.iter().map(u64::to_string).collect::<Vec<_>>().join("/")
                    ),
                ));
            }
            p.devices.push(PoolDevice {
                name: t[0].to_owned(),
                state,
                errors: errs.then(|| counts.iter().map(u64::to_string).collect::<Vec<_>>().join("/")),
            });
        }
    }
    flush(&mut cur, &mut pools);
    pools
}

/// `/proc/mdstat`.
pub fn parse_mdstat(text: &str) -> Vec<Pool> {
    let mut pools = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        i += 1;
        let Some((name, rest)) = l.split_once(" : ") else { continue };
        let name = name.trim();
        if !name.starts_with("md") {
            continue;
        }
        let mut t = rest.split_whitespace();
        let state = t.next().unwrap_or("").to_owned();
        let mut toks: Vec<&str> = t.collect();
        if toks.first() == Some(&"(auto-read-only)") || toks.first() == Some(&"(read-only)") {
            toks.remove(0);
        }
        let level = toks.first().filter(|s| s.starts_with("raid") || **s == "linear").map(|s| s.to_string());
        let mut issues = Vec::new();
        let mut devices = Vec::new();
        for d in toks.iter().skip(usize::from(level.is_some())) {
            let Some(b) = d.find('[') else { continue };
            let dev = &d[..b];
            let flag = if d.ends_with("(F)") {
                issues.push((Health::Bad, format!("{dev} has failed")));
                "failed"
            } else if d.ends_with("(S)") {
                "spare"
            } else if d.ends_with("(W)") {
                "write-mostly"
            } else {
                "active"
            };
            devices.push(PoolDevice { name: dev.to_owned(), state: flag.into(), errors: None });
        }
        if state != "active" {
            issues.push((Health::Bad, format!("array is {state}")));
        }
        let mut size = None;
        let mut activity = None;
        let mut progress = None;
        while i < lines.len() && (lines[i].starts_with(' ') || lines[i].starts_with('\t')) {
            let d = lines[i].trim();
            i += 1;
            if let Some(b) =
                d.split_whitespace().next().and_then(|n| n.parse::<u64>().ok()).filter(|_| d.contains("blocks"))
            {
                size = Some(b * 1024);
            }
            // [2/1] [U_]
            if let Some(s) = d.split_whitespace().find(|w| w.starts_with('[') && w.contains('/') && w.ends_with(']')) {
                if let Some((want, have)) = s.trim_matches(|c| c == '[' || c == ']').split_once('/') {
                    if let (Ok(w), Ok(h)) = (want.parse::<u32>(), have.parse::<u32>()) {
                        if h < w {
                            issues.push((Health::Bad, format!("degraded: {h} of {w} devices working")));
                        }
                    }
                }
            }
            for word in ["recovery", "resync", "reshape", "check", "repair"] {
                if d.contains(&format!("{word} =")) || d.contains(&format!("{word}=")) {
                    progress = pct_in(d);
                    let finish = d.split("finish=").nth(1).and_then(|f| f.split_whitespace().next());
                    activity = Some(match finish {
                        Some(f) => format!("{word} running, about {f} left"),
                        None => format!("{word} running"),
                    });
                }
                if d.contains(&format!("{word}=DELAYED")) || d.contains(&format!("{word}=PENDING")) {
                    activity = Some(format!("{word} pending"));
                }
            }
        }
        let (health, issues) = finish(issues);
        pools.push(Pool {
            kind: PoolKind::Mdraid,
            name: name.to_owned(),
            detail: level,
            state,
            health,
            size,
            used: None,
            devices,
            issues,
            activity,
            progress,
        });
    }
    pools
}

const BTRFS_ERRS: [(&str, &str); 5] = [
    ("write_errs", "write"),
    ("read_errs", "read"),
    ("flush_errs", "flush"),
    ("corruption_errs", "corruption"),
    ("generation_errs", "generation"),
];

/// The sysfs dump and the degraded mounts.
pub fn parse_btrfs(text: &str, degraded: &[&str]) -> Vec<Pool> {
    let mut pools: Vec<(Pool, Issues, Vec<String>)> = Vec::new();
    for l in text.lines() {
        if let Some(rest) = l.strip_prefix("@fs ") {
            let mut t = rest.splitn(2, ' ');
            let uuid = t.next().unwrap_or("").to_owned();
            let mount = t.next().map(str::trim).filter(|m| !m.is_empty()).map(str::to_owned);
            let mut issues = Vec::new();
            if mount.as_deref().is_some_and(|m| degraded.contains(&m)) {
                issues.push((Health::Bad, "mounted degraded: a device is missing".to_owned()));
            }
            pools.push((
                Pool {
                    kind: PoolKind::Btrfs,
                    name: uuid[..uuid.len().min(8)].to_owned(),
                    detail: mount,
                    state: "mounted".into(),
                    health: Health::Ok,
                    size: None,
                    used: None,
                    devices: Vec::new(),
                    issues: Vec::new(),
                    activity: None,
                    progress: None,
                },
                issues,
                Vec::new(),
            ));
            continue;
        }
        let Some((p, issues, names)) = pools.last_mut() else { continue };
        if let Some(label) = l.strip_prefix("@label") {
            let label = label.trim();
            if !label.is_empty() {
                p.name = label.to_owned();
            }
        } else if let Some(d) = l.strip_prefix("@devs") {
            *names = d.split_whitespace().map(str::to_owned).collect();
        } else if let Some(d) = l.strip_prefix("@dev ") {
            let mut t = d.split_whitespace();
            let id = t.next().unwrap_or("?");
            let missing = t.next() == Some("1");
            let rest: Vec<&str> = t.collect();
            let mut errs = Vec::new();
            for pair in rest.chunks(2) {
                if let [k, v] = pair {
                    if let (Some((_, word)), Ok(n)) = (BTRFS_ERRS.iter().find(|(key, _)| key == k), v.parse::<u64>()) {
                        if n > 0 {
                            errs.push(format!("{n} {word}"));
                        }
                    }
                }
            }
            let idx: usize = id.parse::<usize>().unwrap_or(1).saturating_sub(1);
            let name = names.get(idx).cloned().unwrap_or_else(|| format!("devid {id}"));
            if missing {
                issues.push((Health::Bad, format!("{name} is missing")));
            }
            if !errs.is_empty() {
                let h = if errs.iter().any(|e| e.contains("corruption")) { Health::Bad } else { Health::Warn };
                issues.push((h, format!("{name}: {} errors", errs.join(", "))));
            }
            p.devices.push(PoolDevice {
                name,
                state: if missing { "missing".into() } else { "present".into() },
                errors: (!errs.is_empty()).then(|| errs.join(", ")),
            });
        }
    }
    pools
        .into_iter()
        .map(|(mut p, issues, names)| {
            if p.devices.is_empty() {
                p.devices =
                    names.into_iter().map(|n| PoolDevice { name: n, state: "present".into(), errors: None }).collect();
            }
            let (h, list) = finish(issues);
            p.health = h;
            p.issues = list;
            p
        })
        .collect()
}

pub fn parse_scan(out: &str) -> PoolsReport {
    let parts = sections(out);
    let part = |i: usize| parts.get(i).copied().unwrap_or("");
    let degraded: Vec<&str> = part(3).lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let mut pools = parse_zfs(part(0));
    pools.extend(parse_mdstat(part(1)));
    pools.extend(parse_btrfs(part(2), &degraded));
    let health = pools.iter().map(|p| p.health).max().unwrap_or(Health::Ok);
    PoolsReport { pools, health }
}

pub fn kind_word(k: PoolKind) -> &'static str {
    match k {
        PoolKind::Zfs => "ZFS pool",
        PoolKind::Mdraid => "RAID array",
        PoolKind::Btrfs => "btrfs",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZFS: &str =
        "@zlist\ntank\t3985729650688\t1992864825344\nfast\t500000000000\t100000000000\n@zstatus\n  pool: fast
 state: ONLINE
  scan: scrub repaired 0B in 00:01:02 with 0 errors on Sun Nov 10 00:24:03 2024
config:

\tNAME        STATE     READ WRITE CKSUM
\tfast        ONLINE       0     0     0
\t  nvme0n1   ONLINE       0     0     0

errors: No known data errors

  pool: tank
 state: DEGRADED
status: One or more devices could not be used because the label is missing or
\tinvalid.  Sufficient replicas exist for the pool to continue
\tfunctioning in a degraded state.
action: Replace the device using 'zpool replace'.
  scan: resilver in progress since Fri Nov 15 10:00:00 2024
\t1.20T scanned at 400M/s, 600G issued at 200M/s, 1.81T total
\t300G resilvered, 32.50% done, 01:40:00 to go
config:

\tNAME        STATE     READ WRITE CKSUM
\ttank        DEGRADED     0     0     0
\t  mirror-0  DEGRADED     0     0     0
\t    sda     ONLINE       0     0     2
\t    sdb     UNAVAIL      0     0     0  corrupted data

errors: No known data errors
";

    #[test]
    fn zfs() {
        let p = parse_zfs(ZFS);
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].name.as_str(), p[0].health, p[0].size), ("fast", Health::Ok, Some(500_000_000_000)));
        assert!(p[0].activity.as_deref().unwrap().starts_with("scrub repaired"));
        let t = &p[1];
        assert_eq!((t.state.as_str(), t.health), ("DEGRADED", Health::Bad));
        assert_eq!(t.progress, Some(32.5));
        assert_eq!(t.used, Some(1_992_864_825_344));
        assert_eq!(t.devices.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["mirror-0", "sda", "sdb"]);
        assert_eq!(t.devices[1].errors.as_deref(), Some("0/0/2"));
        assert_eq!(
            t.issues,
            [
                "degraded: a device is missing or failing, redundancy is reduced",
                "sdb is unavail",
                "sda has read/write/checksum errors (0/0/2)"
            ]
        );
    }

    #[test]
    fn mdstat() {
        let p = parse_mdstat(
            "Personalities : [raid1] [raid6]\n\
             md0 : active raid1 sdb1[1] sda1[0](F)\n      976630464 blocks super 1.2 [2/1] [U_]\n      \
             [=>...................]  recovery =  8.5% (83136/976630464) finish=81.2min speed=200000K/sec\n      bitmap: 1/8 pages [4KB], 65536KB chunk\n\n\
             md1 : active raid6 sdc[0] sdd[1] sde[2] sdf[3] sdg[4](S)\n      5860270080 blocks super 1.2 level 6, 512k chunk, algorithm 2 [4/4] [UUUU]\n\n\
             md2 : inactive sdh[0](S)\n      976630464 blocks super 1.2\n\nunused devices: <none>\n",
        );
        assert_eq!(p.len(), 3);
        assert_eq!((p[0].detail.as_deref(), p[0].health), (Some("raid1"), Health::Bad));
        assert_eq!(p[0].issues, ["sda1 has failed", "degraded: 1 of 2 devices working"]);
        assert_eq!(p[0].progress, Some(8.5));
        assert_eq!(p[0].activity.as_deref(), Some("recovery running, about 81.2min left"));
        assert_eq!(p[0].size, Some(976_630_464 * 1024));
        assert_eq!((p[1].health, p[1].devices.len()), (Health::Ok, 5));
        assert_eq!(p[1].devices[4].state, "spare");
        assert_eq!(p[2].health, Health::Bad);
        assert_eq!(p[2].issues, ["array is inactive"]);
    }

    #[test]
    fn btrfs_and_report() {
        let out = format!(
            "{SECTION}\n{SECTION}\n@fs 0f1e2d3c-1111-2222-3333-444455556666 /srv\n@label data\n@devs sdb sdc \n\
             @dev 1 0 write_errs 0 read_errs 0 flush_errs 0 corruption_errs 0 generation_errs 0 \n\
             @dev 2 0 write_errs 0 read_errs 3 flush_errs 0 corruption_errs 1 generation_errs 0 \n\
             @fs 99999999-aaaa-bbbb-cccc-dddddddddddd /\n@label \n@devs nvme0n1p2 \n\
             {SECTION}\n"
        );
        let r = parse_scan(&out);
        assert_eq!(r.pools.len(), 2);
        let d = &r.pools[0];
        assert_eq!((d.name.as_str(), d.detail.as_deref(), d.health), ("data", Some("/srv"), Health::Bad));
        assert_eq!(d.issues, ["sdc: 3 read, 1 corruption errors"]);
        assert_eq!(r.pools[1].name, "99999999");
        assert_eq!(r.pools[1].devices[0].name, "nvme0n1p2");
        assert_eq!(r.health, Health::Bad);
        // NODE: conditions come from the node's own `watch::pool_conditions`.
        let c = super::super::watch::pool_conditions("nas", &r);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].key, "pool:Btrfs:data");
        assert_eq!(c[0].detail, "btrfs data: sdc: 3 read, 1 corruption errors");
        let r = parse_scan(&format!(
            "{SECTION}\n{SECTION}\n@fs 99999999-aaaa-bbbb-cccc-dddddddddddd /\n@label root\n{SECTION}\n/\n"
        ));
        assert_eq!(r.pools[0].issues, ["mounted degraded: a device is missing"]);
        assert_eq!(parse_scan("").pools, vec![]);
    }
}

#[cfg(all(test, unix))]
mod shell {
    #[test]
    fn script_runs() {
        let out = std::process::Command::new("sh").arg("-c").arg(super::scan_command()).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let r = super::parse_scan(&String::from_utf8_lossy(&out.stdout));
        assert!(r.pools.iter().all(|p| !p.name.is_empty()));
    }
}
