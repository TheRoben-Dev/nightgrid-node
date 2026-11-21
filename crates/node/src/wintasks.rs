//! Jobs on Windows (decision N-095): Task Scheduler instead of systemd.
//!
//! A backup job whose source is "unit" names a scheduled task by its path
//! (`\Backups\Nightly`, or Windows Server Backup's own
//! `\Microsoft\Windows\Backup\Microsoft-Windows-WindowsBackup`), as the app
//! does for Windows hosts. Scheduled jobs are the tasks in `\Nightgrid\`,
//! the folder the app keeps its own tasks in. The node reads both with one
//! PowerShell script per round (`Get-ScheduledTask`, `Get-ScheduledTaskInfo`),
//! read-only, as its own account. Everything here but running the script is
//! plain parsing, so it is tested on every system.
//!
//! The status rules are the app's (`core/src/win/backups.rs`): result 0 is
//! a good run, `0x41301` is running, `0x41303` never ran, anything else is
//! a failure whose code is the program's exit code or a Task Scheduler
//! HRESULT.

use nightgrid_node_proto::{BackupResult, BackupStatus, JobRun};
use serde::{Deserialize, Deserializer};

/// The folder whose tasks count as the app's scheduled jobs.
pub const FOLDER: &str = "\\Nightgrid\\";
/// "Running" and "has not run yet" results.
pub const RUNNING: i64 = 0x41301;
pub const NOT_YET: i64 = 0x41303;
/// The most tasks read from [`FOLDER`].
const MAX_RUNS: usize = 100;

/// A scheduled task path: `\Folder\Name`, no wildcard or quote characters.
pub fn valid_task(p: &str) -> bool {
    p.len() <= 200
        && p.starts_with('\\')
        && !p.starts_with("\\\\")
        && !p.ends_with('\\')
        && !p.contains("\\\\")
        && p.len() > 1
        && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | ':' | '-' | '\\' | ' '))
}

/// An absolute Windows path: `D:\Backups` or `\\nas\share` (the app's rule).
pub fn absolute_path(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with("\\\\")
        || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The script for one round: the listed tasks and everything in [`FOLDER`].
pub fn script(tasks: &[String]) -> String {
    let list = tasks.iter().filter(|t| valid_task(t)).map(|t| quote(t)).collect::<Vec<_>>().join(", ");
    format!(
        r#"$ErrorActionPreference = 'SilentlyContinue'
function U($d) {{ if ($d -and $d.Year -gt 2000) {{ [int64](($d.ToUniversalTime() - [datetime]'1970-01-01').TotalSeconds) }} else {{ $null }} }}
function T($k) {{
  $n = $k | Get-ScheduledTaskInfo
  @{{ path = $k.TaskPath + $k.TaskName; found = $true; state = [string]$k.State; description = [string]$k.Description; last = (U $n.LastRunTime); result = $(if ($n) {{ [int64]$n.LastTaskResult }} else {{ $null }}) }}
}}
$r = @{{ jobs = @(); runs = @() }}
foreach ($p in @({list})) {{
  $i = $p.LastIndexOf('\')
  $k = Get-ScheduledTask -TaskPath $p.Substring(0, $i + 1) -TaskName $p.Substring($i + 1)
  if ($k) {{ $r.jobs += T $k }} else {{ $r.jobs += @{{ path = $p; found = $false }} }}
}}
foreach ($k in @(Get-ScheduledTask -TaskPath '{FOLDER}' | Select-Object -First {MAX_RUNS})) {{ $r.runs += T $k }}
[Console]::Out.Write((ConvertTo-Json -InputObject $r -Depth 4 -Compress))
"#
    )
}

/// One task as the script read it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Task {
    pub path: String,
    #[serde(default)]
    pub found: bool,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub description: String,
    pub last: Option<f64>,
    pub result: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Doc {
    #[serde(default, deserialize_with = "one_or_many")]
    pub jobs: Vec<Task>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub runs: Vec<Task>,
}

/// PowerShell writes a one-element array as the element.
fn one_or_many<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum M<T> {
        Many(Vec<T>),
        One(T),
        None(()),
    }
    Ok(match M::deserialize(d)? {
        M::Many(v) => v,
        M::One(t) => vec![t],
        M::None(()) => Vec::new(),
    })
}

pub fn parse(out: &str) -> Result<Doc, String> {
    let start = out.find('{').ok_or("Task Scheduler gave no answer")?;
    let mut d: Doc = serde_json::from_str(&out[start..]).map_err(|e| format!("unreadable answer: {e}"))?;
    d.runs.retain(|t| t.path.starts_with(FOLDER) && valid_task(&t.path));
    d.runs.truncate(MAX_RUNS);
    Ok(d)
}

fn running(t: &Task) -> bool {
    t.state == "Running" || t.result == Some(RUNNING)
}

/// A finished run, identified so it is reported once.
pub fn run_key(t: &Task) -> Option<String> {
    let last = t.last?;
    let code = t.result.unwrap_or(NOT_YET);
    (!running(t) && code != NOT_YET).then(|| format!("{last}:{code}"))
}

/// A backup job's status from its task (the app's rules).
pub fn task_status(id: i32, t: Option<&Task>) -> BackupStatus {
    let mut st = crate::backups::empty(id);
    let Some(t) = t.filter(|t| t.found) else {
        st.error = Some("The scheduled task wasn't found on this host, or this node may not read it.".into());
        return st;
    };
    let code = t.result.unwrap_or(NOT_YET);
    st.last_run = t.last;
    if running(t) {
        st.result = BackupResult::Running;
        st.summary = Some("Running now".into());
    } else if t.last.is_none() || code == NOT_YET {
        st.summary = Some("The task hasn't run yet".into());
        st.last_run = None;
    } else if code == 0 {
        st.result = BackupResult::Ok;
        st.last_ok = t.last;
    } else {
        st.result = BackupResult::Failed;
        st.exit_code = i32::try_from(code).ok();
        st.summary = Some(format!("Last result 0x{code:X}"));
    }
    if t.state == "Disabled" {
        st.summary = Some(format!("{} (task disabled)", st.summary.as_deref().unwrap_or("Not running")));
    }
    st
}

/// A task in [`FOLDER`] as a scheduled job's last run.
pub fn task_run(t: &Task) -> JobRun {
    let st = task_status(0, Some(t));
    let name = t.path.rsplit('\\').next().unwrap_or(&t.path);
    let d = t.description.trim();
    JobRun {
        unit: t.path.clone(),
        description: if d.is_empty() { name.to_owned() } else { d.chars().take(120).collect() },
        result: st.result,
        started: st.last_run,
        finished: if st.result == BackupResult::Running { None } else { st.last_run },
        exit_code: st.exit_code,
        summary: st.summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        for ok in [
            r"\Backups\Nightly",
            r"\Nightgrid\nightly sync",
            r"\Microsoft\Windows\Backup\Microsoft-Windows-WindowsBackup",
            r"\x",
        ] {
            assert!(valid_task(ok), "{ok}");
        }
        for bad in ["", r"\", r"Backups\N", r"\\nas\x", r"\a\", r"\a\\b", r"\it's", r"\a*", r"\a[1]"] {
            assert!(!valid_task(bad), "{bad}");
        }
        assert!(absolute_path(r"D:\Backups") && absolute_path(r"\\nas\share") && absolute_path("c:/x"));
        assert!(!absolute_path("/srv") && !absolute_path(r"Backups") && !absolute_path("C:"));
        let s = script(&[r"\Backups\Night ly".into(), r"\it's".into()]);
        assert!(s.contains(r"foreach ($p in @('\Backups\Night ly'))"), "{s}");
        assert!(s.contains(r"Get-ScheduledTask -TaskPath '\Nightgrid\'"), "{s}");
        assert!(script(&[]).contains("foreach ($p in @())"));
    }

    #[test]
    fn statuses_and_runs() {
        let out = r#"{"jobs":{"path":"\\Backups\\N","found":true,"state":"Ready","description":"","last":1700000000,"result":0},"runs":[{"path":"\\Nightgrid\\sync","found":true,"state":"Ready","description":"nightly sync","last":1700000100,"result":2},{"path":"\\Other\\x","found":true,"state":"Ready","last":1,"result":0}]}"#;
        let d = parse(out).unwrap();
        assert_eq!(d.jobs.len(), 1);
        assert_eq!(d.runs.len(), 1, "only tasks in the folder");
        let st = task_status(7, d.jobs.first());
        assert_eq!((st.result, st.last_ok), (BackupResult::Ok, Some(1_700_000_000.0)));
        let r = task_run(&d.runs[0]);
        assert_eq!((r.result, r.exit_code, r.description.as_str()), (BackupResult::Failed, Some(2), "nightly sync"));
        assert_eq!(r.summary.as_deref(), Some("Last result 0x2"));
        assert_eq!(run_key(&d.runs[0]).as_deref(), Some("1700000100:2"));

        let running = Task {
            path: r"\a\b".into(),
            found: true,
            state: "Running".into(),
            last: Some(5.0),
            result: Some(RUNNING),
            ..Default::default()
        };
        assert_eq!(task_status(1, Some(&running)).result, BackupResult::Running);
        assert_eq!(run_key(&running), None);
        let never = Task {
            path: r"\a\b".into(),
            found: true,
            state: "Disabled".into(),
            last: None,
            result: Some(NOT_YET),
            ..Default::default()
        };
        let st = task_status(1, Some(&never));
        assert_eq!(st.summary.as_deref(), Some("The task hasn't run yet (task disabled)"));
        assert_eq!(run_key(&never), None);
        assert!(task_status(1, None).error.is_some());
        assert!(task_status(1, Some(&Task { path: r"\x".into(), ..Default::default() })).error.is_some());
        assert!(parse(r#"{"jobs":[],"runs":null}"#).unwrap().runs.is_empty());
        assert!(parse("").is_err());
    }
}
