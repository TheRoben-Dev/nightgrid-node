// Vendored from Nightgrid (https://github.com/TheRoben-Dev/nightgrid),
// core/src/logwatch.rs at commit 8eedcf2, MIT License, (c) 2026 TheRoben.
// The node finds the same bursts and new patterns as the app. Local changes
// are marked `NODE:`.

//! Log anomaly detection (M10g): error bursts and new error patterns in
//! container logs and the journal.
//!
//! Each round, the app hands the error lines of one source (a container, or a
//! journal identifier like `sshd`) since the last round to [`Detector`]. A
//! line becomes a *pattern* by replacing what changes between occurrences
//! (numbers, ids, addresses, quoted values), so "timeout after 3012 ms" and
//! "timeout after 87 ms" count as the same error. The first look at a source
//! only learns; after that a pattern never seen before is reported once, and a
//! round with several times the usual number of error lines is a burst.
//! Everything here is pure; the app crate fetches the lines and raises alerts.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::redact::line_has_secret;

/// Fewest error lines in one round that can count as a burst.
pub const BURST_MIN: usize = 20;
/// A burst is this many times the usual count per round.
pub const BURST_FACTOR: f64 = 4.0;
/// Rounds remembered for the usual count.
const HISTORY: usize = 12;
/// Patterns remembered per source.
// NODE: 500 instead of 2000, so 64 sources stay within a few MB.
const KNOWN_MAX: usize = 500;
/// Sample lines kept per source for "what broke".
pub const SAMPLES: usize = 40;
pub const PATTERN_MAX: usize = 160;

/// Does this log line report an error?
pub fn is_error_line(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    // Common "all good" lines that still contain the word.
    if l.contains("0 errors") || l.contains("no error") || l.contains("errors=0") || l.contains("error_log") {
        return false;
    }
    [
        "error",
        "fatal",
        "panic",
        "exception",
        "traceback",
        "critical",
        "segfault",
        "level=err",
        "[err]",
        "[crit]",
        "out of memory",
        "oom-kill",
        "failed to",
        "permission denied",
        "connection refused",
    ]
    .iter()
    .any(|w| l.contains(w))
}

/// The shape of a line: what stays the same between occurrences.
pub fn pattern(line: &str) -> String {
    let body = strip_timestamp(line);
    let mut out = String::with_capacity(body.len().min(PATTERN_MAX + 8));
    let mut chars = body.chars().peekable();
    let mut last_placeholder = false;
    while let Some(c) = chars.next() {
        if out.len() >= PATTERN_MAX {
            break;
        }
        if c == '"' || c == '\'' {
            // Quoted values vary: keep the quotes, drop the content.
            let q = c;
            let mut closed = false;
            for n in chars.by_ref() {
                if n == q {
                    closed = true;
                    break;
                }
            }
            out.push(q);
            out.push('…');
            if closed {
                out.push(q);
            }
            last_placeholder = false;
            continue;
        }
        if c.is_ascii_alphanumeric() {
            let mut word = String::from(c);
            while let Some(&n) = chars.peek() {
                if n.is_ascii_alphanumeric() || matches!(n, '.' | ':' | '-' | '_' | '/') {
                    word.push(n);
                    chars.next();
                } else {
                    break;
                }
            }
            let word = word.trim_end_matches(['.', ':', '-', '/']);
            if varies(word) {
                if !last_placeholder {
                    out.push('#');
                }
                last_placeholder = true;
            } else {
                out.push_str(word);
                last_placeholder = false;
            }
            continue;
        }
        out.push(c);
        last_placeholder = false;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Numbers, hex ids, UUIDs, addresses and other tokens with digits in them.
fn varies(w: &str) -> bool {
    let digits = w.chars().filter(char::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    // Words like "http2", "ipv4", "utf8", "h264" stay.
    !(digits <= 2
        && w.len() >= 4
        && w.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && !w.contains(['.', ':', '-', '/']))
}

/// Drops a leading ISO or syslog timestamp and log level brackets' times.
fn strip_timestamp(line: &str) -> &str {
    let t = line.trim_start();
    // 2026-11-15T10:00:00.123Z / 2026-11-15 10:00:00,123
    let b = t.as_bytes();
    if b.len() > 19 && b[4] == b'-' && b[7] == b'-' && (b[10] == b'T' || b[10] == b' ') {
        let rest = &t[10..];
        let end = rest.find(|c: char| c.is_whitespace()).map(|i| i + 10);
        if let Some(e) = end {
            // "2026-11-15 10:00:00" has a space at index 10: skip the time part too.
            if b[10] == b' ' {
                let after = &t[11..];
                return after.split_once(' ').map(|(_, r)| r).unwrap_or(after);
            }
            return &t[e..];
        }
    }
    t
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewPattern {
    pub pattern: String,
    /// One line with this pattern (`None` if every one looked like it held a secret).
    pub sample: Option<String>,
    pub count: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Finding {
    /// Error lines this round and the usual count, when it's a burst.
    pub burst: Option<(usize, f64)>,
    pub new_patterns: Vec<NewPattern>,
}

#[derive(Debug, Default)]
struct SourceState {
    known: HashSet<String>,
    history: VecDeque<usize>,
    learned: bool,
    samples: VecDeque<String>,
}

#[derive(Debug, Default)]
pub struct Detector {
    sources: HashMap<String, SourceState>,
}

impl Detector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one round of `source`'s error lines.
    pub fn observe(&mut self, source: &str, lines: &[String]) -> Finding {
        let st = self.sources.entry(source.to_owned()).or_default();
        let mut finding = Finding::default();
        let count = lines.len();
        let mut fresh: Vec<NewPattern> = Vec::new();
        for l in lines {
            let p = pattern(l);
            if p.is_empty() {
                continue;
            }
            let safe = (!line_has_secret(l)).then(|| l.chars().take(400).collect::<String>());
            if let Some(s) = &safe {
                st.samples.push_back(s.clone());
                if st.samples.len() > SAMPLES {
                    st.samples.pop_front();
                }
            }
            if st.known.contains(&p) {
                continue;
            }
            match fresh.iter_mut().find(|n| n.pattern == p) {
                Some(n) => {
                    n.count += 1;
                    if n.sample.is_none() {
                        n.sample = safe;
                    }
                }
                None => fresh.push(NewPattern { pattern: p, sample: safe, count: 1 }),
            }
        }
        if st.learned {
            if st.history.len() >= 3 {
                let usual = st.history.iter().sum::<usize>() as f64 / st.history.len() as f64;
                if count >= BURST_MIN && count as f64 >= BURST_FACTOR * usual.max(1.0) {
                    finding.burst = Some((count, usual));
                }
            }
            finding.new_patterns = fresh.clone();
        }
        if st.known.len() < KNOWN_MAX {
            st.known.extend(fresh.into_iter().map(|n| n.pattern));
        }
        st.history.push_back(count);
        if st.history.len() > HISTORY {
            st.history.pop_front();
        }
        st.learned = true;
        finding
    }

    /// Recent error lines of a source (secret-looking ones left out).
    pub fn samples(&self, source: &str) -> Vec<String> {
        self.sources.get(source).map(|s| s.samples.iter().cloned().collect()).unwrap_or_default()
    }

    /// Sources seen before whose key starts with `prefix`.
    pub fn sources_with_prefix(&self, prefix: &str) -> Vec<String> {
        self.sources.keys().filter(|k| k.starts_with(prefix)).cloned().collect()
    }

    /// Forgets sources whose key starts with `prefix` (a removed host).
    pub fn forget(&mut self, prefix: &str) {
        self.sources.retain(|k, _| !k.starts_with(prefix));
    }
}

/// Error lines from the journal since `since` (unix seconds), priority err and worse.
pub fn journal_command(since: i64) -> String {
    format!("journalctl -p 3 --since @{since} -o short-unix --no-pager -q 2>/dev/null | tail -n 1000")
}

/// `1731664800.123456 host sshd[123]: message` → (identifier, message).
pub fn parse_journal(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, ' ');
            let ts = it.next()?;
            ts.parse::<f64>().ok()?;
            let _host = it.next()?;
            let rest = it.next()?;
            let (ident, msg) = rest.split_once(": ")?;
            let ident = ident.split('[').next().unwrap_or(ident);
            Some((ident.to_owned(), msg.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn error_lines() {
        assert!(is_error_line("level=error msg=\"db down\""));
        assert!(is_error_line("Traceback (most recent call last):"));
        assert!(is_error_line("connect: Connection refused"));
        assert!(!is_error_line("compiled with 0 errors"));
        assert!(!is_error_line("GET /health 200"));
    }

    #[test]
    fn patterns_fold_variable_parts() {
        assert_eq!(pattern("timeout after 3012 ms to 10.0.0.5:5432"), pattern("timeout after 87 ms to 10.0.0.9:5432"));
        assert_eq!(
            pattern("2026-11-15T10:00:00.123Z ERROR request 6f1c2a9e-1b2c-4d5e-8f90-123456789abc failed"),
            "ERROR request # failed"
        );
        assert_eq!(pattern("user \"bob\" not found"), pattern("user \"alice\" not found"));
        assert_eq!(pattern("http2 stream error"), "http2 stream error");
        assert_ne!(pattern("disk full on /data"), pattern("certificate expired"));
    }

    #[test]
    fn learns_then_reports_new_patterns_once() {
        let mut d = Detector::new();
        let f = d.observe("c/web", &v(&["ERROR db timeout after 30s", "ERROR db timeout after 31s"]));
        assert_eq!(f, Finding::default(), "first look only learns");
        let f = d.observe("c/web", &v(&["ERROR db timeout after 12s", "FATAL out of memory"]));
        assert_eq!(f.new_patterns.len(), 1);
        assert_eq!(f.new_patterns[0].sample.as_deref(), Some("FATAL out of memory"));
        let f = d.observe("c/web", &v(&["FATAL out of memory"]));
        assert!(f.new_patterns.is_empty());
    }

    #[test]
    fn bursts_against_the_usual_count() {
        let mut d = Detector::new();
        for _ in 0..4 {
            d.observe("j/sshd", &v(&["error: kex failed", "error: kex failed"]));
        }
        let many: Vec<String> = (0..30).map(|i| format!("error: kex failed {i}")).collect();
        let f = d.observe("j/sshd", &many);
        assert_eq!(f.burst.map(|b| b.0), Some(30));
        // 10 lines is under BURST_MIN.
        let f = d.observe("j/sshd", &many[..10]);
        assert!(f.burst.is_none());
    }

    #[test]
    fn secrets_stay_out_of_samples() {
        let mut d = Detector::new();
        d.observe("c/x", &v(&["error: login failed"]));
        let f = d.observe("c/x", &v(&["error: bad password=hunter2222"]));
        assert_eq!(f.new_patterns.len(), 1);
        assert!(f.new_patterns[0].sample.is_none());
        assert_eq!(d.samples("c/x"), vec!["error: login failed"]);
    }

    #[test]
    fn journal_lines() {
        let out = "1731664800.123456 nas sshd[123]: error: kex_exchange_identification: Connection closed\n\
                   1731664801.000000 nas kernel: Out of memory: Killed process 42\n-- No entries --\n";
        let j = parse_journal(out);
        assert_eq!(j.len(), 2);
        assert_eq!(j[0].0, "sshd");
        assert_eq!(j[1], ("kernel".into(), "Out of memory: Killed process 42".into()));
    }
}
