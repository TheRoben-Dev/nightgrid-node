//! Windows Event Log records, from the XML `EvtRender` gives (W1).
//!
//! Pure parsing, so it is tested on every platform; reading the log is in
//! `win::eventlog`. Only the fields the node uses are read: the record id
//! (the cursor), the time, the provider, the event id, the level and the
//! event's data values.

/// One event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Record {
    /// `EventRecordID`: increases by one per record in a channel.
    pub id: u64,
    /// Unix seconds.
    pub at: i64,
    pub provider: String,
    pub event_id: u32,
    /// 1 critical, 2 error, 3 warning, 4 information, 5 verbose (0 is
    /// "log always", which classic providers use for information).
    pub level: u8,
    /// `<Data Name='x'>v</Data>` values in order; unnamed ones have "".
    pub data: Vec<(String, String)>,
}

impl Record {
    /// A named data value.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.data.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// The journal priority this level maps to (`0..=7`, lower is worse).
    pub fn priority(&self) -> u8 {
        match self.level {
            1 => 2,
            2 => 3,
            3 => 4,
            5 => 7,
            _ => 6,
        }
    }

    /// The data values joined, for an event whose message can't be formatted.
    pub fn data_text(&self) -> String {
        self.data.iter().map(|(_, v)| v.as_str()).filter(|v| !v.is_empty()).collect::<Vec<_>>().join("; ")
    }
}

/// Parses one rendered event.
pub fn parse(xml: &str) -> Option<Record> {
    let system = between(xml, "<System>", "</System>")?;
    let mut r = Record {
        id: text(system, "EventRecordID")?.trim().parse().ok()?,
        at: attr(system, "TimeCreated", "SystemTime").and_then(|t| unix_time(&t)).unwrap_or(0),
        provider: attr(system, "Provider", "Name").unwrap_or_default(),
        event_id: text(system, "EventID").and_then(|t| t.trim().parse().ok()).unwrap_or(0),
        level: text(system, "Level").and_then(|t| t.trim().parse().ok()).unwrap_or(4),
        data: Vec::new(),
    };
    if let Some(ed) = between(xml, "<EventData", "</EventData>") {
        let mut rest = ed;
        while let Some(i) = rest.find("<Data") {
            rest = &rest[i + 5..];
            // `<DataItem`, say: not a Data element.
            if !rest.starts_with([' ', '>', '/']) {
                continue;
            }
            let Some(end) = rest.find('>') else { break };
            let head = &rest[..end];
            let name = attr_in(head, "Name").unwrap_or_default();
            if head.ends_with('/') {
                r.data.push((name, String::new()));
                rest = &rest[end + 1..];
                continue;
            }
            let body = &rest[end + 1..];
            let Some(close) = body.find("</Data>") else { break };
            r.data.push((name, unescape(&body[..close])));
            rest = &body[close + 7..];
            if r.data.len() >= 64 {
                break;
            }
        }
    }
    Some(r)
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let i = s.find(open)? + open.len();
    let j = s[i..].find(close)?;
    Some(&s[i..i + j])
}

/// The text of `<tag ...>text</tag>`.
fn text(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let mut from = 0;
    loop {
        let i = from + s[from..].find(&open)?;
        let after = &s[i + open.len()..];
        if after.starts_with([' ', '>']) {
            let gt = after.find('>')?;
            if after[..gt].ends_with('/') {
                return Some(String::new());
            }
            let body = &after[gt + 1..];
            let end = body.find(&format!("</{tag}>"))?;
            return Some(unescape(&body[..end]));
        }
        from = i + open.len();
    }
}

/// An attribute of the first `<tag ...>`.
fn attr(s: &str, tag: &str, name: &str) -> Option<String> {
    let open = format!("<{tag}");
    let mut from = 0;
    loop {
        let i = from + s[from..].find(&open)?;
        let after = &s[i + open.len()..];
        if after.starts_with([' ', '/', '>']) {
            let gt = after.find('>')?;
            return attr_in(&after[..gt], name);
        }
        from = i + open.len();
    }
}

/// `name='v'` or `name="v"` in a start tag's inside.
fn attr_in(head: &str, name: &str) -> Option<String> {
    let mut rest = head;
    loop {
        let i = rest.find(name)?;
        let before_ok = i == 0 || rest.as_bytes()[i - 1].is_ascii_whitespace();
        let after = rest[i + name.len()..].trim_start();
        if before_ok {
            if let Some(v) = after.strip_prefix('=') {
                let v = v.trim_start();
                let q = v.chars().next()?;
                if q == '\'' || q == '"' {
                    let end = v[1..].find(q)?;
                    return Some(unescape(&v[1..1 + end]));
                }
            }
        }
        rest = &rest[i + name.len()..];
    }
}

/// XML entities and character references.
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.find(';').filter(|n| *n <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let c = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .or_else(|| ent.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `2026-11-20T10:00:00.1234567Z` to unix seconds.
fn unix_time(t: &str) -> Option<i64> {
    let b = t.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| t.get(r).and_then(|s| s.parse::<i64>().ok());
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SSHD: &str = "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System><Provider Name='OpenSSH' Guid='{c4b57d35-0636-4bc3-a262-370f249f9802}'/><EventID>4</EventID><Version>0</Version><Level>4</Level><Task>0</Task><Opcode>0</Opcode><Keywords>0x4000000000000000</Keywords><TimeCreated SystemTime='2026-11-20T10:00:05.1234567Z'/><EventRecordID>1042</EventRecordID><Correlation/><Execution ProcessID='3100' ThreadID='4204'/><Channel>OpenSSH/Operational</Channel><Computer>srv01</Computer><Security UserID='S-1-5-18'/></System><EventData><Data Name='process'>sshd.exe</Data><Data Name='payload'>Accepted publickey for alice from 10.0.0.5 port 50522 ssh2: ED25519 SHA256:abc</Data></EventData></Event>";

    #[test]
    fn reads_an_openssh_event() {
        let r = parse(SSHD).unwrap();
        assert_eq!((r.id, r.event_id, r.level, r.provider.as_str()), (1042, 4, 4, "OpenSSH"));
        assert_eq!(r.at, 1_795_168_805);
        assert_eq!(r.get("process"), Some("sshd.exe"));
        let line = crate::logins::parse(r.get("payload").unwrap()).unwrap();
        assert_eq!(
            line,
            crate::logins::Line::Accepted { user: "alice".into(), from: "10.0.0.5".into(), method: "publickey".into() }
        );
    }

    #[test]
    fn reads_a_classic_event_with_qualifiers_and_unnamed_data() {
        let x = r#"<Event xmlns="http://schemas.microsoft.com/win/2004/08/events/event"><System><Provider Name="Service Control Manager" Guid="{555908d1}" EventSourceName="Service Control Manager"/><EventID Qualifiers="49152">7031</EventID><Level>2</Level><TimeCreated SystemTime="2026-01-01T00:00:00.000Z"/><EventRecordID>7</EventRecordID></System><EventData><Data Name="param1">Print &amp; Spool &lt;x&gt;</Data><Data Name="param2">1</Data><Data/><Data>tail&#xD;&#10;two</Data><Binary>00</Binary></EventData></Event>"#;
        let r = parse(x).unwrap();
        assert_eq!((r.id, r.event_id, r.level, r.priority()), (7, 7031, 2, 3));
        assert_eq!(r.provider, "Service Control Manager");
        assert_eq!(r.at, 1_767_225_600);
        assert_eq!(r.get("param1"), Some("Print & Spool <x>"));
        assert_eq!(r.data.len(), 4);
        assert_eq!(r.data[3], (String::new(), "tail\r\ntwo".into()));
        assert_eq!(r.data_text(), "Print & Spool <x>; 1; tail\r\ntwo");
    }

    #[test]
    fn security_logon_fields() {
        let x = "<Event><System><Provider Name='Microsoft-Windows-Security-Auditing'/><EventID>4625</EventID><Level>0</Level><TimeCreated SystemTime='2026-11-20T10:00:00Z'/><EventRecordID>99</EventRecordID></System><EventData><Data Name='TargetUserName'>administrator</Data><Data Name='LogonType'>10</Data><Data Name='IpAddress'>203.0.113.9</Data><Data Name='DataItemish'>x</Data></EventData></Event>";
        let r = parse(x).unwrap();
        assert_eq!((r.event_id, r.priority()), (4625, 6));
        assert_eq!((r.get("LogonType"), r.get("IpAddress")), (Some("10"), Some("203.0.113.9")));
    }

    #[test]
    fn rejects_what_is_not_an_event() {
        assert!(parse("").is_none());
        assert!(parse("<Event><System><EventID>1</EventID></System></Event>").is_none());
        assert_eq!(unescape("a &bogus; & b &#65;"), "a &bogus; & b A");
    }
}
