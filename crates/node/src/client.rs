//! A small blocking client (for `status`, `query`, tests) and the stdio bridge.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use nightgrid_node_proto::{method, Hello, Request, Response, RpcError, Welcome, MAX_LINE};
use serde_json::Value;

pub struct Client {
    w: UnixStream,
    r: BufReader<UnixStream>,
    next: u64,
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
        let w = UnixStream::connect(path)?;
        let r = BufReader::new(w.try_clone()?);
        let mut c = Self { w, r, next: 1, welcome: placeholder() };
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
        let mut buf = Vec::new();
        (&mut self.r).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf)?;
        if buf.is_empty() {
            return Err(CallError::Protocol("connection closed".into()));
        }
        let resp: Response = serde_json::from_slice(&buf).map_err(|e| CallError::Protocol(e.to_string()))?;
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
pub fn bridge(path: &Path) -> std::io::Result<()> {
    let sock = UnixStream::connect(path)?;
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
