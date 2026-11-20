//! Text for people at a terminal: `status`, `events`, `audit`, completions and
//! the manual page. Everything here reads the node through the same methods
//! the app uses; nothing is a new interface (N-048).

use nightgrid_node_proto::{ActionRecord, ActionsLogResult, Alert, Event, NodeInfo, Sample};
use serde::Serialize;

/// `2026-11-17 21:04` in the host's time zone. Times a 32-bit `time_t`
/// can't hold (after 2038-01-19 on the 32-bit musl builds) are shown in UTC.
#[cfg(windows)]
pub fn local_time(t: i64) -> String {
    crate::win::sys::local_time(t).unwrap_or_else(|| format!("{} UTC", utc_time(t)))
}

#[cfg(unix)]
pub fn local_time(t: i64) -> String {
    // libc's musl time_t is 32-bit on 32-bit targets for now, i64 elsewhere.
    #[allow(deprecated, irrefutable_let_patterns, clippy::useless_conversion)]
    let Ok(tt) = libc::time_t::try_from(t) else {
        return format!("{} UTC", utc_time(t));
    };
    // SAFETY: localtime_r only writes the tm we pass.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&tt, &mut tm) }.is_null() {
        return t.to_string();
    }
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

/// `2026-11-17 21:04` in UTC, from i64 seconds, without libc (any year).
pub fn utc_time(t: i64) -> String {
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    // Days to a civil date (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

/// `3d 4h`, `5h 2m`, `40s`.
pub fn span(secs: i64) -> String {
    let s = secs.max(0);
    let (d, h, m) = (s / 86400, s % 86400 / 3600, s % 3600 / 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

pub fn bytes(b: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < units.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

/// An enum's wire name (`warn`, `dryRun`), for printing.
fn name(v: impl Serialize) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

fn pct(used: u64, total: u64) -> String {
    if total == 0 {
        "-".into()
    } else {
        format!("{:.0}%", used as f64 * 100.0 / total as f64)
    }
}

/// The `status` summary.
pub fn status(info: &NodeInfo, sample: Option<&Sample>, alerts: &[Alert], now: i64) -> String {
    let mut o = String::new();
    let n = &info.node;
    o += &format!("{} · nightgrid-node {} · {}\n", n.hostname, n.version, n.arch);
    o += &format!(
        "running for {} · {} resident · {} history\n",
        span(now - info.started),
        bytes(info.rss_bytes),
        bytes(info.db_bytes)
    );
    if let Some(s) = sample {
        o += "\n";
        let cpu = s.cpu.map_or("-".into(), |c| format!("{c:.0}%"));
        o += &format!("cpu     {cpu:<6} load {:.2} {:.2} {:.2}", s.load[0], s.load[1], s.load[2]);
        if let Some(t) = s.cpu_temp {
            o += &format!(" · {t:.0}°C");
        }
        o += "\n";
        o += &format!("memory  {:<6} {} of {}\n", pct(s.mem_used, s.mem_total), bytes(s.mem_used), bytes(s.mem_total));
        if s.swap_total > 0 {
            o += &format!(
                "swap    {:<6} {} of {}\n",
                pct(s.swap_used, s.swap_total),
                bytes(s.swap_used),
                bytes(s.swap_total)
            );
        }
        o += &format!("uptime  {}\n", span(s.uptime_secs as i64));
        for m in &s.mounts {
            o += &format!("disk    {:<6} {} free on {}\n", pct(m.used, m.total), bytes(m.avail), m.mount);
        }
    }
    o += "\n";
    if alerts.is_empty() {
        o += "no active alerts\n";
    } else {
        o += &format!("{} active alert{}\n", alerts.len(), if alerts.len() == 1 { "" } else { "s" });
        for a in alerts {
            o += &format!("  {:<8} {} ({}, since {})\n", name(a.severity), a.title, a.detail, local_time(a.since));
        }
    }
    let on: Vec<_> = info.access.iter().filter(|(_, v)| v.as_str() == "ok").map(|(k, _)| k.as_str()).collect();
    let other: Vec<_> = info
        .access
        .iter()
        .filter(|(_, v)| !matches!(v.as_str(), "ok" | "off" | "denied" | "absent"))
        .map(|(k, v)| format!("{k} ({v})"))
        .collect();
    o += &format!("\naccess  {}\n", if on.is_empty() { "none beyond the basics".into() } else { on.join(", ") });
    if !other.is_empty() {
        o += &format!("check   {}\n", other.join(", "));
    }
    o
}

/// One line per event, oldest first.
pub fn events(list: &[Event]) -> String {
    let mut o = String::new();
    for e in list {
        o += &format!("{}  {:<8} {:<10} {}", local_time(e.at), name(e.level), name(e.kind), e.title);
        if !e.detail.is_empty() {
            o += &format!(" · {}", e.detail);
        }
        o += "\n";
    }
    if list.is_empty() {
        o += "no events\n";
    }
    o
}

/// One line per action-log entry, oldest first.
pub fn audit(list: &[ActionRecord]) -> String {
    let mut o = String::new();
    for r in list {
        let target = r.target.as_deref().map(|t| format!(" {t}")).unwrap_or_default();
        o += &format!(
            "{:>5}  {}  {:<8} {}{} · {} · {}\n",
            r.seq,
            local_time(r.at),
            name(r.outcome),
            r.action,
            target,
            r.source,
            r.detail
        );
    }
    if list.is_empty() {
        o += "the action log is empty\n";
    }
    o
}

/// What `audit verify` found after walking the whole log.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verify {
    pub entries: u64,
    pub first: Option<i64>,
    pub last: Option<i64>,
    /// Ranges of `seq` whose page did not verify.
    pub broken: Vec<(i64, i64)>,
}

impl Verify {
    pub fn add(&mut self, page: &ActionsLogResult) {
        let (Some(a), Some(b)) = (page.records.first(), page.records.last()) else { return };
        self.entries += page.records.len() as u64;
        self.first.get_or_insert(a.seq);
        self.last = Some(b.seq);
        if !page.verified {
            self.broken.push((a.seq, b.seq));
        }
    }
    pub fn ok(&self) -> bool {
        self.broken.is_empty()
    }
    pub fn text(&self) -> String {
        match (self.first, self.last, self.ok()) {
            (None, _, _) => "the action log is empty; nothing to verify\n".into(),
            (Some(a), Some(b), true) => {
                format!("ok: {} entries ({a}–{b}), every hash checks out and links to the one before\n", self.entries)
            }
            _ => {
                let r: Vec<_> = self.broken.iter().map(|(a, b)| format!("{a}–{b}")).collect();
                format!(
                    "BROKEN: the chain does not hold in entries {} of {}. Someone edited or removed rows in the node's database.\n",
                    r.join(", "),
                    self.entries
                )
            }
        }
    }
}

/// The commands and options, for completions.
const COMMANDS: &[&str] = &[
    "fix",
    "run",
    "install",
    "upgrade",
    "uninstall",
    "allow",
    "deny",
    "serve",
    "status",
    "doctor",
    "events",
    "audit",
    "query",
    "watch",
    "bridge",
    "completions",
    "man",
    "version",
    "help",
];
const GRANTS: &[&str] =
    &["docker", "journal", "netwatch", "helper", "health", "power", "checks", "peers", "statuspage"];
const FIX_WORDS: &str = "list auto restart reset start journal logs packages prune ssh reboot run";

const OPTIONS: &[&str] = &[
    "--yes",
    "--config",
    "--socket",
    "--data-dir",
    "--user",
    "--json",
    "--purge",
    "--no-start",
    "--count",
    "--sha256",
    "--allow-downgrade",
    "--help",
    "--version",
];
const STREAMS: &[&str] = &["events", "metrics", "containers", "usage"];

/// A completion script for `bash`, `zsh` or `fish`.
pub fn completions(shell: &str) -> Option<String> {
    let cmds = COMMANDS.join(" ");
    let grants = GRANTS.join(" ");
    let opts = OPTIONS.join(" ");
    let streams = STREAMS.join(" ");
    let fix = FIX_WORDS;
    let auto = crate::menu::PRESETS.iter().map(|p| p.key).collect::<Vec<_>>().join(" ");
    match shell {
        "bash" => Some(format!(
            r#"# bash completion for nightgrid-node
# Install: nightgrid-node completions bash | sudo tee /etc/bash_completion.d/nightgrid-node
_nightgrid_node() {{
    local cur=${{COMP_WORDS[COMP_CWORD]}} cmd=${{COMP_WORDS[1]}}
    if [[ $cur == -* ]]; then COMPREPLY=($(compgen -W "{opts}" -- "$cur")); return; fi
    if [[ $COMP_CWORD -eq 1 ]]; then COMPREPLY=($(compgen -W "{cmds}" -- "$cur")); return; fi
    case $cmd in
        allow|deny) [[ $COMP_CWORD -eq 2 ]] && COMPREPLY=($(compgen -W "{grants}" -- "$cur")) ;;
        watch) COMPREPLY=($(compgen -W "{streams}" -- "$cur")) ;;
        fix) [[ $COMP_CWORD -eq 2 ]] && COMPREPLY=($(compgen -W "{fix}" -- "$cur")); [[ $COMP_CWORD -eq 3 && ${{COMP_WORDS[2]}} == auto ]] && COMPREPLY=($(compgen -W "{auto}" -- "$cur")); [[ $COMP_CWORD -eq 4 && ${{COMP_WORDS[2]}} == auto ]] && COMPREPLY=($(compgen -W "on preview off" -- "$cur")) ;;
        audit) [[ $COMP_CWORD -eq 2 ]] && COMPREPLY=($(compgen -W "verify" -- "$cur")) ;;
        serve) [[ $COMP_CWORD -eq 2 ]] && COMPREPLY=($(compgen -W "add remove" -- "$cur")); [[ $COMP_CWORD -eq 3 ]] && COMPREPLY=($(compgen -W "ping ups sync" -- "$cur")) ;;
        completions) [[ $COMP_CWORD -eq 2 ]] && COMPREPLY=($(compgen -W "bash zsh fish powershell" -- "$cur")) ;;
        upgrade) COMPREPLY=($(compgen -f -- "$cur")) ;;
    esac
}}
complete -F _nightgrid_node nightgrid-node
"#
        )),
        "zsh" => Some(format!(
            r#"#compdef nightgrid-node
# zsh completion for nightgrid-node
# Install: nightgrid-node completions zsh > "${{fpath[1]}}/_nightgrid-node"
_nightgrid-node() {{
    if (( CURRENT == 2 )); then compadd -- {cmds}; return; fi
    if [[ $words[CURRENT] == -* ]]; then compadd -- {opts}; return; fi
    case $words[2] in
        allow|deny) (( CURRENT == 3 )) && compadd -- {grants} ;;
        watch) compadd -- {streams} ;;
        fix) (( CURRENT == 3 )) && compadd -- {fix}; (( CURRENT == 4 )) && [[ $words[3] == auto ]] && compadd -- {auto}; (( CURRENT == 5 )) && [[ $words[3] == auto ]] && compadd -- on preview off ;;
        audit) (( CURRENT == 3 )) && compadd -- verify ;;
        serve) (( CURRENT == 3 )) && compadd -- add remove; (( CURRENT == 4 )) && compadd -- ping ups sync ;;
        completions) (( CURRENT == 3 )) && compadd -- bash zsh fish powershell ;;
        upgrade) _files ;;
    esac
}}
_nightgrid-node "$@"
"#
        )),
        "fish" => {
            let mut o = String::from(
                "# fish completion for nightgrid-node\n# Install: nightgrid-node completions fish > ~/.config/fish/completions/nightgrid-node.fish\ncomplete -c nightgrid-node -f\n",
            );
            o += &format!("complete -c nightgrid-node -n __fish_use_subcommand -a '{cmds}'\n");
            o += &format!("complete -c nightgrid-node -n '__fish_seen_subcommand_from allow deny' -a '{grants}'\n");
            o += &format!("complete -c nightgrid-node -n '__fish_seen_subcommand_from watch' -a '{streams}'\n");
            o += "complete -c nightgrid-node -n '__fish_seen_subcommand_from audit' -a verify\n";
            o += &format!("complete -c nightgrid-node -n '__fish_seen_subcommand_from fix; and not __fish_seen_subcommand_from auto' -a '{fix}'\n");
            o += &format!(
                "complete -c nightgrid-node -n '__fish_seen_subcommand_from auto' -a '{auto} on preview off'\n"
            );
            o += "complete -c nightgrid-node -n '__fish_seen_subcommand_from serve' -a 'add remove ping ups sync'\n";
            o += "complete -c nightgrid-node -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish powershell'\n";
            o += "complete -c nightgrid-node -n '__fish_seen_subcommand_from upgrade' -F\n";
            for opt in OPTIONS {
                o += &format!("complete -c nightgrid-node -l {}\n", opt.trim_start_matches("--"));
            }
            Some(o)
        }
        "powershell" | "pwsh" => {
            let q = |l: &str| l.split(' ').map(|w| format!("'{w}'")).collect::<Vec<_>>().join(",");
            Some(format!(
                r#"# PowerShell completion for nightgrid-node
# Install: nightgrid-node completions powershell | Out-File -Append $PROFILE
Register-ArgumentCompleter -Native -CommandName nightgrid-node,nightgrid-node.exe -ScriptBlock {{
    param($word, $ast, $cursor)
    $w = @($ast.CommandElements | ForEach-Object {{ $_.ToString() }})
    $n = $w.Count; if ($word -eq '') {{ $n++ }}
    $c = @()
    if ($word -like '-*') {{ $c = @({opts}) }}
    elseif ($n -le 2) {{ $c = @({cmds}) }}
    else {{
        switch ($w[1]) {{
            {{ $_ -in 'allow','deny' }} {{ if ($n -eq 3) {{ $c = @({grants}) }} }}
            'watch' {{ $c = @({streams}) }}
            'fix' {{ if ($n -eq 3) {{ $c = @({fix}) }} elseif ($w[2] -eq 'auto') {{ $c = @({auto},'on','preview','off') }} }}
            'audit' {{ if ($n -eq 3) {{ $c = @('verify') }} }}
            'serve' {{ $c = @('add','remove','ping','ups','sync') }}
            'completions' {{ if ($n -eq 3) {{ $c = @('bash','zsh','fish','powershell') }} }}
        }}
    }}
    $c | Where-Object {{ $_ -like "$word*" }} | ForEach-Object {{
        [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
    }}
}}
"#,
                opts = q(&opts),
                cmds = q(&cmds),
                grants = q(&grants),
                streams = q(&streams),
                fix = q(fix),
                auto = q(&auto),
            ))
        }
        _ => None,
    }
}

/// A roff manual page made from the help text (`nightgrid-node man | man -l -`).
pub fn man(help: &str, version: &str) -> String {
    let esc = |s: &str| {
        let s = s.replace('\\', "\\\\").replace('-', "\\-");
        // A line starting with . or ' would be read as a request.
        if s.starts_with(['.', '\'']) {
            format!("\\&{s}")
        } else {
            s
        }
    };
    let mut o = format!(
        ".TH NIGHTGRID-NODE 1 \"\" \"nightgrid-node {version}\" \"User Commands\"\n.SH NAME\nnightgrid\\-node \\- optional agent for the Nightgrid homelab app\n"
    );
    let mut section = None::<&str>;
    for line in help.lines().skip(1) {
        let t = line.trim_end();
        if t.is_empty() {
            continue;
        }
        if let Some(h) = t.strip_suffix(':').filter(|h| h.chars().all(|c| c.is_ascii_uppercase() || c == ' ')) {
            section = Some(h);
            o += &format!(".SH {h}\n");
            continue;
        }
        let indent = t.len() - t.trim_start().len();
        let body = t.trim_start();
        match section {
            Some("COMMANDS") | Some("OPTIONS") if indent <= 8 => {
                // `name args   description`: the name on the tag line.
                let (tag, rest) = match body.find("  ") {
                    Some(i) => (&body[..i], body[i..].trim_start()),
                    None => (body, ""),
                };
                o += &format!(".TP\n.B {}\n", esc(tag));
                if !rest.is_empty() {
                    o += &format!("{}\n", esc(rest));
                }
            }
            _ => o += &format!("{}\n", esc(body)),
        }
    }
    o += ".SH FILES\n.TP\n/etc/nightgrid\\-node/config.toml\nRoot's configuration; the service only reads it.\n.TP\n/var/lib/nightgrid\\-node\nHistory, alerts and the action log.\n.TP\n/run/nightgrid\\-node/node.sock\nThe socket the app and these commands talk to.\n";
    o += ".SH SEE ALSO\n.BR systemctl (1),\n.BR journalctl (1)\n";
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightgrid_node_proto::ActionOutcome;

    fn rec(seq: i64) -> ActionRecord {
        ActionRecord {
            seq,
            at: 0,
            source: "app (uid 1000)".into(),
            action: "unit.restart".into(),
            target: Some("nginx".into()),
            dry_run: false,
            outcome: ActionOutcome::Ok,
            detail: "restarted".into(),
            hash: String::new(),
        }
    }

    #[test]
    fn verify_walks_pages_and_names_the_broken_ones() {
        let mut v = Verify::default();
        assert!(v.text().contains("empty"));
        v.add(&ActionsLogResult { records: vec![rec(1), rec(2)], next: 2, more: true, verified: true });
        v.add(&ActionsLogResult { records: vec![rec(3), rec(4)], next: 4, more: true, verified: false });
        v.add(&ActionsLogResult { records: vec![rec(5)], next: 5, more: false, verified: true });
        assert_eq!((v.entries, v.first, v.last, v.ok()), (5, Some(1), Some(5), false));
        assert!(v.text().starts_with("BROKEN") && v.text().contains("3–4"), "{}", v.text());
        let mut good = Verify::default();
        good.add(&ActionsLogResult { records: vec![rec(7)], next: 7, more: false, verified: true });
        assert!(good.text().starts_with("ok: 1 entries (7–7)"));
        assert!(
            audit(&[rec(1)]).contains("ok       unit.restart nginx · app (uid 1000) · restarted"),
            "{}",
            audit(&[rec(1)])
        );
    }

    #[test]
    fn spans_and_sizes_read_well() {
        assert_eq!(
            [span(40), span(300), span(3 * 3600 + 120), span(2 * 86400 + 5 * 3600)],
            ["40s", "5m", "3h 2m", "2d 5h"]
        );
        assert_eq!([bytes(512), bytes(1536), bytes(9 * 1048576)], ["512 B", "1.5 KiB", "9.0 MiB"]);
        assert_eq!(pct(1, 4), "25%");
    }

    #[test]
    fn completions_cover_every_command_and_grant() {
        for sh in ["bash", "zsh", "fish", "powershell"] {
            let c = completions(sh).unwrap();
            for w in COMMANDS.iter().chain(GRANTS) {
                assert!(c.contains(w), "{sh} misses {w}");
            }
        }
        assert!(completions("tcsh").is_none());
        let main = include_str!("main.rs");
        for w in COMMANDS {
            assert!(main.contains(&format!("\"{w}\"")), "{w} is not a command");
        }
        for o in OPTIONS {
            assert!(main.contains(&format!("\"{o}\"")), "{o} is not an option");
        }
        for g in crate::install::Grant::ALL {
            assert!(GRANTS.contains(&g.name()), "{}", g.name());
        }
    }

    #[test]
    fn the_manual_page_has_every_command() {
        let help = "x: y\n\nUSAGE:\n    nightgrid-node <command>\n\nCOMMANDS:\n    run                     Run it\n    allow docker            Let it\n                            read more\n";
        let m = man(help, "1.0");
        assert!(m.starts_with(".TH NIGHTGRID-NODE 1"));
        assert!(m.contains(".SH COMMANDS\n.TP\n.B run\nRun it\n.TP\n.B allow docker\nLet it\nread more\n"), "{m}");
    }
}
