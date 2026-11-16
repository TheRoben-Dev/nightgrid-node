//! A tiny client for the Docker Engine API (also Podman's compatible
//! socket): HTTP/1.0 over the unix socket, so the engine closes the
//! connection after one response. Reads use `/containers/json` and
//! `/events` (with `until`, so it returns). Writes are only the typed
//! actions root allowed (N4): start, stop, restart a container, and prune
//! dangling images.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use nightgrid_node_proto::{ContainerAccess, ContainerInfo};
use serde::Deserialize;

const TIMEOUT: Duration = Duration::from_secs(10);
/// Largest response read (a host with hundreds of containers is ~1 MB).
const MAX_BODY: u64 = 16 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerError {
    pub access: ContainerAccess,
    pub message: String,
}

impl DockerError {
    fn error(message: impl Into<String>) -> Self {
        Self { access: ContainerAccess::Error, message: message.into() }
    }
}

impl std::fmt::Display for DockerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// `GET path` and the body of a 200 response.
pub fn get(socket: &Path, path: &str) -> Result<Vec<u8>, DockerError> {
    request(socket, "GET", path)
}

/// `POST path` with no body. A 2xx or 304 ("already in that state") is success.
pub fn post(socket: &Path, path: &str) -> Result<Vec<u8>, DockerError> {
    request(socket, "POST", path)
}

fn request(socket: &Path, method: &str, path: &str) -> Result<Vec<u8>, DockerError> {
    // Actions such as a restart wait for the container to stop (up to 10 s).
    let timeout = if method == "GET" { TIMEOUT } else { TIMEOUT * 3 };
    let mut s = UnixStream::connect(socket).map_err(|e| {
        let access = match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => ContainerAccess::Absent,
            std::io::ErrorKind::PermissionDenied => ContainerAccess::Denied,
            _ => ContainerAccess::Error,
        };
        DockerError { access, message: format!("{}: {e}", socket.display()) }
    })?;
    s.set_read_timeout(Some(timeout)).ok();
    s.set_write_timeout(Some(TIMEOUT)).ok();
    let len = if method == "GET" { "" } else { "Content-Length: 0\r\n" };
    write!(s, "{method} {path} HTTP/1.0\r\nHost: docker\r\nUser-Agent: nightgrid-node\r\n{len}\r\n")
        .map_err(|e| DockerError::error(format!("write: {e}")))?;
    let mut buf = Vec::new();
    s.take(MAX_BODY).read_to_end(&mut buf).map_err(|e| DockerError::error(format!("read: {e}")))?;
    parse_response(&buf)
}

/// Splits an HTTP/1.x response; decodes a chunked body defensively.
pub fn parse_response(buf: &[u8]) -> Result<Vec<u8>, DockerError> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| DockerError::error("no HTTP header"))?;
    let head = String::from_utf8_lossy(&buf[..end]);
    let body = &buf[end + 4..];
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| DockerError::error("bad status line"))?;
    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = if chunked { dechunk(body)? } else { body.to_vec() };
    if !(200..300).contains(&status) && status != 304 {
        let msg = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["message"].as_str().map(str::to_owned))
            .unwrap_or_default();
        return Err(DockerError::error(format!("engine answered {status} {msg}").trim_end().to_owned()));
    }
    Ok(body)
}

fn dechunk(mut b: &[u8]) -> Result<Vec<u8>, DockerError> {
    let mut out = Vec::new();
    loop {
        let nl = b.windows(2).position(|w| w == b"\r\n").ok_or_else(|| DockerError::error("bad chunk"))?;
        let size = std::str::from_utf8(&b[..nl])
            .ok()
            .and_then(|s| usize::from_str_radix(s.split(';').next().unwrap_or("").trim(), 16).ok())
            .ok_or_else(|| DockerError::error("bad chunk size"))?;
        b = &b[nl + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size {
            return Err(DockerError::error("short chunk"));
        }
        out.extend_from_slice(&b[..size]);
        b = b.get(size + 2..).unwrap_or(&[]);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Summary {
    #[serde(default)]
    id: String,
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    image: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    labels: Option<std::collections::HashMap<String, String>>,
}

fn health_from_status(status: &str) -> Option<String> {
    ["unhealthy", "healthy", "health: starting"]
        .into_iter()
        .find(|h| status.contains(&format!("({h})")))
        .map(|h| h.trim_start_matches("health: ").to_owned())
}

/// Every container, running or not.
pub fn containers(socket: &Path) -> Result<Vec<ContainerInfo>, DockerError> {
    let body = get(socket, "/containers/json?all=1")?;
    parse_containers(&body)
}

pub fn parse_containers(body: &[u8]) -> Result<Vec<ContainerInfo>, DockerError> {
    let list: Vec<Summary> =
        serde_json::from_slice(body).map_err(|e| DockerError::error(format!("container list: {e}")))?;
    let mut out: Vec<ContainerInfo> = list
        .into_iter()
        .map(|c| ContainerInfo {
            id: c.id.chars().take(12).collect(),
            name: c.names.first().map(|n| n.trim_start_matches('/').to_owned()).unwrap_or_default(),
            image: c.image,
            health: health_from_status(&c.status),
            project: c.labels.and_then(|l| l.get("com.docker.compose.project").cloned()),
            state: c.state,
            status: c.status,
        })
        .filter(|c| !c.name.is_empty())
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// One container event as the engine reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEvent {
    pub at: i64,
    /// `die`, `start`, `restart`, `oom`, `health_status: unhealthy`, …
    pub action: String,
    pub name: String,
    pub exit_code: Option<i32>,
}

#[derive(Deserialize)]
struct ApiEvent {
    #[serde(rename = "Type", default)]
    kind: String,
    #[serde(rename = "Action", default)]
    action: String,
    #[serde(rename = "Actor", default)]
    actor: Actor,
    #[serde(default)]
    time: i64,
}

#[derive(Deserialize, Default)]
struct Actor {
    #[serde(rename = "Attributes", default)]
    attributes: std::collections::HashMap<String, String>,
}

/// Container events in `[since, until]` (unix seconds).
pub fn events(socket: &Path, since: i64, until: i64) -> Result<Vec<RawEvent>, DockerError> {
    // filters={"type":["container"]}
    let path = format!("/events?since={since}&until={until}&filters=%7B%22type%22%3A%5B%22container%22%5D%7D");
    Ok(parse_events(&get(socket, &path)?))
}

/// The body is a stream of JSON objects, one per line.
pub fn parse_events(body: &[u8]) -> Vec<RawEvent> {
    serde_json::Deserializer::from_slice(body)
        .into_iter::<ApiEvent>()
        .map_while(Result::ok)
        .filter(|e| e.kind == "container" && !e.action.is_empty())
        .filter_map(|e| {
            let name = e.actor.attributes.get("name")?.clone();
            Some(RawEvent {
                at: e.time,
                exit_code: e.actor.attributes.get("exitCode").and_then(|c| c.parse().ok()),
                action: e.action,
                name,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_responses_lists_and_events() {
        let r = b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n[]";
        assert_eq!(parse_response(r).unwrap(), b"[]");
        let r = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n[1,\r\n2\r\n2]\r\n0\r\n\r\n";
        assert_eq!(parse_response(r).unwrap(), b"[1,2]");
        let r = b"HTTP/1.0 404 Not Found\r\n\r\n{\"message\":\"page not found\"}";
        assert_eq!(parse_response(r).unwrap_err().message, "engine answered 404 page not found");

        let list = br#"[{"Id":"0123456789abcdef","Names":["/web"],"Image":"nginx","State":"running",
            "Status":"Up 2 hours (unhealthy)","Labels":{"com.docker.compose.project":"site"}},
            {"Id":"ff","Names":["/db"],"Image":"pg","State":"exited","Status":"Exited (137) 5 minutes ago"}]"#;
        let c = parse_containers(list).unwrap();
        assert_eq!((c[0].name.as_str(), c[0].state.as_str()), ("db", "exited"));
        assert_eq!(c[1].id, "0123456789ab");
        assert_eq!((c[1].health.as_deref(), c[1].project.as_deref()), (Some("unhealthy"), Some("site")));

        let ev = br#"{"Type":"container","Action":"die","Actor":{"ID":"x","Attributes":{"name":"web","exitCode":"137"}},"time":100}
{"Type":"network","Action":"connect","Actor":{"Attributes":{"name":"bridge"}},"time":101}
{"Type":"container","Action":"start","Actor":{"Attributes":{"name":"web"}},"time":102}
"#;
        let e = parse_events(ev);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0], RawEvent { at: 100, action: "die".into(), name: "web".into(), exit_code: Some(137) });
    }

    #[test]
    fn missing_socket_is_absent() {
        let e = get(Path::new("/nonexistent/docker.sock"), "/_ping").unwrap_err();
        assert_eq!(e.access, ContainerAccess::Absent);
    }

    #[test]
    fn talks_http_over_a_unix_socket() {
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("d.sock");
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let t = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut req = [0u8; 512];
            let n = s.read(&mut req).unwrap();
            assert!(String::from_utf8_lossy(&req[..n]).starts_with("GET /containers/json?all=1 HTTP/1.0\r\n"));
            s.write_all(b"HTTP/1.0 200 OK\r\n\r\n[{\"Names\":[\"/a\"],\"State\":\"running\"}]").unwrap();
        });
        let c = containers(&sock).unwrap();
        t.join().unwrap();
        assert_eq!(c[0].name, "a");
    }
}
