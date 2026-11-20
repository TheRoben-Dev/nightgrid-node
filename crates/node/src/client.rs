//! A small blocking client (for `status`, `query`, tests) and the stdio bridge.

use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
/// The node's socket (Windows: its named pipe, opened for blocking use).
#[cfg(windows)]
type UnixStream = std::fs::File;
use std::path::Path;

use nightgrid_node_proto::{method, Frame, Hello, Request, Response, RpcError, Welcome, MAX_LINE};
use serde_json::Value;

pub struct Client {
    w: UnixStream,
    r: BufReader<UnixStream>,
    next: u64,
    /// Frames that arrived while waiting for a response.
    frames: std::collections::VecDeque<Frame>,
    pub welcome: Welcome,
}

#[derive(Debug)]
pub enum CallError {
    Io(std::io::Error),
    Rpc(RpcError),
    Protocol(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Io(e) => write!(f, "{e}"),
            CallError::Rpc(e) => write!(f, "{:?}: {}", e.code, e.message),
            CallError::Protocol(e) => write!(f, "protocol: {e}"),
        }
    }
}

impl From<std::io::Error> for CallError {
    fn from(e: std::io::Error) -> Self {
        CallError::Io(e)
    }
}

impl Client {
    /// Connects and says hello.
    pub fn connect(path: &Path, client: &str) -> Result<Self, CallError> {
        #[cfg(unix)]
        let w = UnixStream::connect(path)?;
        #[cfg(windows)]
        let w = crate::win::pipe::open_sync(&path.to_string_lossy())?;
        let r = BufReader::new(w.try_clone()?);
        let mut c = Self { w, r, next: 1, frames: Default::default(), welcome: placeholder() };
        let v = c.call(method::HELLO, serde_json::to_value(Hello::this_client(client)).unwrap_or_default())?;
        c.welcome = serde_json::from_value(v).map_err(|e| CallError::Protocol(e.to_string()))?;
        Ok(c)
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        let id = self.next;
        self.next += 1;
        let mut line = serde_json::to_vec(&Request { id, method: method.into(), params })
            .map_err(|e| CallError::Protocol(e.to_string()))?;
        line.push(b'\n');
        self.w.write_all(&line)?;
        let resp = loop {
            match self.read()? {
                Incoming::Frame(f) => self.frames.push_back(f),
                Incoming::Response(r) => break r,
            }
        };
        if resp.id != id {
            return Err(CallError::Protocol(format!("answer for {} while waiting for {id}", resp.id)));
        }
        match (resp.result, resp.error) {
            (_, Some(e)) => Err(CallError::Rpc(e)),
            (Some(v), None) => Ok(v),
            (None, None) => Ok(Value::Null),
        }
    }
}

enum Incoming {
    Response(Response),
    Frame(Frame),
}

impl Client {
    fn read(&mut self) -> Result<Incoming, CallError> {
        let mut buf = Vec::new();
        (&mut self.r).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf)?;
        if buf.is_empty() {
            return Err(CallError::Protocol("connection closed".into()));
        }
        let v: Value = serde_json::from_slice(&buf).map_err(|e| CallError::Protocol(e.to_string()))?;
        let bad = |e: serde_json::Error| CallError::Protocol(e.to_string());
        if v.get("stream").is_some() && v.get("id").is_none() {
            Ok(Incoming::Frame(serde_json::from_value(v).map_err(bad)?))
        } else {
            Ok(Incoming::Response(serde_json::from_value(v).map_err(bad)?))
        }
    }

    /// The next pushed frame (after `subscribe`), waiting for it.
    pub fn next_frame(&mut self) -> Result<Frame, CallError> {
        if let Some(f) = self.frames.pop_front() {
            return Ok(f);
        }
        match self.read()? {
            Incoming::Frame(f) => Ok(f),
            Incoming::Response(r) => Err(CallError::Protocol(format!("unexpected answer {}", r.id))),
        }
    }
}

/// The line `bridge` prints when it can't connect: `no_access` when the
/// socket is there but this user may not use it, `not_running` otherwise.
pub fn bridge_error(path: &Path, e: &std::io::Error) -> String {
    let code = match e.kind() {
        std::io::ErrorKind::PermissionDenied => "no_access",
        _ => "not_running",
    };
    let message = match code {
        "no_access" => {
            format!("{}: {e} (the login user is not in the socket's group yet, or needs a new login)", path.display())
        }
        _ => format!("{}: {e} (the service is not running)", path.display()),
    };
    serde_json::json!({ "error": { "code": code, "message": message } }).to_string()
}

fn placeholder() -> Welcome {
    Welcome {
        proto: 0,
        node: nightgrid_node_proto::NodeIdentity {
            node_id: String::new(),
            version: String::new(),
            hostname: String::new(),
            arch: String::new(),
        },
        caps: Default::default(),
    }
}

/// Copies stdin to the socket and the socket to stdout until either side
/// closes. This is what the app runs over an SSH exec channel
/// (`nightgrid-node bridge`) when streamlocal forwarding isn't available.
///
/// When the socket can't be reached it prints one JSON line on stdout before
/// failing, `{"error":{"code":"no_access"|"not_running","message":...}}`, so
/// the app can try the bridge first and probe only when it fails.
#[cfg(windows)]
pub fn bridge(path: &Path) -> std::io::Result<()> {
    crate::win::pipe::bridge(&path.to_string_lossy())
}

/// See above: the Unix version.
#[cfg(unix)]
pub fn bridge(path: &Path) -> std::io::Result<()> {
    let sock = match UnixStream::connect(path) {
        Ok(s) => s,
        Err(e) => {
            println!("{}", bridge_error(path, &e));
            return Err(e);
        }
    };
    let mut to_sock = sock.try_clone()?;
    let mut from_sock = sock;
    let up = std::thread::spawn(move || {
        let r = std::io::copy(&mut std::io::stdin().lock(), &mut to_sock);
        let _ = to_sock.shutdown(std::net::Shutdown::Write);
        r
    });
    let mut out = std::io::stdout().lock();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = from_sock.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        out.flush()?;
    }
    // The node closed: don't wait for stdin (it may never close).
    drop(up);
    Ok(())
}
