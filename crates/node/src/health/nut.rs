//! A small NUT client (network UPS tools, `upsd` on TCP 3493) for Windows,
//! which has no `upsc` (decision N-092).
//!
//! The check runner, the only part of the Windows node with any network,
//! asks each `[health] ups` target that names a server (`ups@nas`,
//! `ups@192.168.1.5:3493`) with `LIST VAR <ups>` and hands the answer to the
//! node as `upsc` would print it, so [`super::ups::parse`], the alerts and
//! the shutdown guard work unchanged. Only reads: no login, no `SET`, no
//! instant commands. The server's address must be inside `[checks]
//! networks`, like any check.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// NUT's port.
pub const PORT: u16 = 3493;
/// The most lines an answer may have (a UPS lists ~100 variables).
const MAX_LINES: usize = 1000;

/// `ups@host[:port]` → (`ups`, `host`, `port`). Without a host, `upsc`
/// asks localhost. An IPv6 host goes in brackets: `ups@[fd00::5]:3493`.
pub fn split_target(t: &str) -> Option<(String, String, u16)> {
    if !super::ups::valid_target(t) {
        return None;
    }
    let (ups, rest) = t.split_once('@').unwrap_or((t, "localhost"));
    if ups.is_empty() || rest.is_empty() {
        return None;
    }
    let (host, port) = if let Some(r) = rest.strip_prefix('[') {
        let (h, after) = r.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if after.is_empty() => PORT,
            None => return None,
        };
        (h.to_owned(), port)
    } else {
        match rest.split_once(':') {
            Some((h, p)) => (h.to_owned(), p.parse().ok()?),
            None => (rest.to_owned(), PORT),
        }
    };
    if host.is_empty() || port == 0 {
        return None;
    }
    Some((ups.to_owned(), host, port))
}

/// True for a target the runner reads (a NUT server), not `"battery"`.
pub fn is_server(t: &str) -> bool {
    t != crate::health::windows::BATTERY_TARGET && split_target(t).is_some()
}

/// `upsc`'s wording for NUT's error codes.
fn error_text(code: &str) -> String {
    match code {
        "UNKNOWN-UPS" => "Unknown UPS".into(),
        "DATA-STALE" => "Data stale".into(),
        "DRIVER-NOT-CONNECTED" => "Driver not connected".into(),
        "ACCESS-DENIED" => "Access denied".into(),
        other => other.to_owned(),
    }
}

/// One `VAR <ups> <name> "<value>"` line → `name: value`.
fn var_line(line: &str, ups: &str) -> Option<String> {
    let rest = line.strip_prefix("VAR ")?.strip_prefix(ups)?.strip_prefix(' ')?;
    let (name, quoted) = rest.split_once(' ')?;
    let inner = quoted.strip_prefix('"')?.strip_suffix('"')?;
    let mut value = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => value.extend(chars.next()),
            c if c.is_control() => {}
            c => value.push(c),
        }
    }
    Some(format!("{name}: {value}"))
}

/// Turns the server's lines into `upsc` output.
pub fn answer_text(lines: &[String], ups: &str) -> String {
    let mut out = String::new();
    for l in lines {
        if let Some(code) = l.strip_prefix("ERR ") {
            let code = code.split_whitespace().next().unwrap_or("");
            return format!("Error: {}\n", error_text(code));
        }
        if let Some(v) = var_line(l, ups) {
            out.push_str(&v);
            out.push('\n');
        }
    }
    out
}

/// Reads one target. Never fails: problems come back as `Error: …`, as
/// `upsc` prints them.
pub async fn read(target: &str, networks: &[String], timeout: Duration) -> String {
    let Some((ups, host, port)) = split_target(target) else {
        return format!("Error: {target:?} is not a NUT target like ups@nas\n");
    };
    match tokio::time::timeout(timeout, ask(&ups, &host, port, networks)).await {
        Ok(Ok(lines)) => answer_text(&lines, &ups),
        Ok(Err(e)) => format!("Error: {e}\n"),
        Err(_) => format!("Error: no answer from {host}:{port} in {} s\n", timeout.as_secs()),
    }
}

async fn ask(ups: &str, host: &str, port: u16, networks: &[String]) -> Result<Vec<String>, String> {
    let addrs = crate::checks::resolve(host, port, networks).await?;
    let stream = crate::checks::connect(&addrs).await.map_err(|e| format!("{host}:{port}: {e}"))?;
    let (rd, mut wr) = stream.into_split();
    wr.write_all(format!("LIST VAR {ups}\n").as_bytes()).await.map_err(|e| e.to_string())?;
    let mut rd = BufReader::new(rd);
    let mut lines = Vec::new();
    let end = format!("END LIST VAR {ups}");
    loop {
        let mut l = String::new();
        // A line longer than 4 KiB isn't NUT.
        let n = (&mut rd).take(4096).read_line(&mut l).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let l = l.trim_end().to_owned();
        let done = l == end || l.starts_with("ERR ");
        lines.push(l);
        if done || lines.len() >= MAX_LINES {
            break;
        }
    }
    let _ = wr.write_all(b"LOGOUT\n").await;
    if lines.is_empty() {
        return Err(format!("{host}:{port} closed the connection without answering"));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_and_answers() {
        assert_eq!(split_target("ups"), Some(("ups".into(), "localhost".into(), 3493)));
        assert_eq!(split_target("ups@nas:3500"), Some(("ups".into(), "nas".into(), 3500)));
        assert_eq!(split_target("ups@[fd00::5]"), Some(("ups".into(), "fd00::5".into(), 3493)));
        assert_eq!(split_target("ups@[fd00::5]:1"), Some(("ups".into(), "fd00::5".into(), 1)));
        for bad in ["", "@nas", "ups@", "ups@nas:x", "ups@nas:0", "ups@[fd00::5", "ups@[::1]x", "-ups@a", "u p"] {
            assert_eq!(split_target(bad), None, "{bad}");
        }
        assert!(is_server("ups@nas") && !is_server("battery"));

        let lines: Vec<String> = [
            "BEGIN LIST VAR ups",
            r#"VAR ups ups.status "OB LB""#,
            r#"VAR ups battery.charge "9""#,
            r#"VAR ups device.model "Back \"UPS\" \\ 700""#,
            r#"VAR other ups.status "OL""#,
            "END LIST VAR ups",
        ]
        .map(String::from)
        .to_vec();
        let t = answer_text(&lines, "ups");
        assert_eq!(t, "ups.status: OB LB\nbattery.charge: 9\ndevice.model: Back \"UPS\" \\ 700\n");
        let r = super::super::ups::parse(&t);
        assert!(r.on_battery && r.low_battery && r.charge == Some(9.0));
        assert_eq!(answer_text(&["ERR UNKNOWN-UPS".into()], "ups"), "Error: Unknown UPS\n");
        assert_eq!(super::super::ups::parse("Error: Unknown UPS\n").error.as_deref(), Some("Unknown UPS"));
    }

    #[test]
    fn reads_a_server_inside_the_networks_only() {
        use std::io::{BufRead, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let srv = std::thread::spawn(move || {
            for _ in 0..2 {
                let (s, _) = l.accept().unwrap();
                let mut r = std::io::BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let mut w = s;
                if line.trim() == "LIST VAR ups" {
                    w.write_all(b"BEGIN LIST VAR ups\nVAR ups ups.status \"OL CHRG\"\nVAR ups battery.charge \"87\"\nEND LIST VAR ups\n").unwrap();
                } else {
                    w.write_all(b"ERR UNKNOWN-UPS\n").unwrap();
                }
            }
        });
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let nets = vec!["127.0.0.0/8".to_owned()];
        let t = rt.block_on(read(&format!("ups@127.0.0.1:{port}"), &nets, Duration::from_secs(5)));
        assert_eq!(t, "ups.status: OL CHRG\nbattery.charge: 87\n");
        let t = rt.block_on(read(&format!("nope@127.0.0.1:{port}"), &nets, Duration::from_secs(5)));
        assert_eq!(t, "Error: Unknown UPS\n");
        srv.join().unwrap();
        let t = rt.block_on(read("ups@127.0.0.1:1", &["10.0.0.0/8".to_owned()], Duration::from_secs(5)));
        assert!(t.starts_with("Error: 127.0.0.1 is outside [checks] networks"), "{t}");
    }
}
