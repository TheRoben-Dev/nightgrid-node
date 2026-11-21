//! The node's named pipe: the Windows form of the unix socket.
//!
//! - **Server:** `\\.\pipe\nightgrid-node`, created as the first instance
//!   (so a process that squatted the name makes the service fail loudly
//!   instead of serving next to it), remote clients rejected, and a DACL
//!   for SYSTEM, Administrators, the service's own SID and the local group
//!   `Nightgrid Node Users` ([`super::acl::pipe_sddl`]). Each caller is
//!   identified from its token: the account SID is the session's "uid", and
//!   an elevated administrator or SYSTEM counts as root.
//! - **Clients** (`bridge`, `status`, `query`, the node's own helpers) open
//!   it with identification-only quality of service, so whatever holds the
//!   name can learn who connected but never act as them, and check that the
//!   pipe belongs to the service, SYSTEM or Administrators before talking.

use std::future::Future;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};
use tokio::sync::Semaphore;
use windows_sys::Win32::Foundation::{ERROR_PIPE_BUSY, HANDLE};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{
    CheckTokenMembership, GetTokenInformation, RevertToSelf, TokenElevation, TokenUser, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT};
use windows_sys::Win32::System::Pipes::{ImpersonateNamedPipeClient, WaitNamedPipeW};
use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

use super::acl::{self, Sddl};
use super::{Handle, Local};
use crate::daemon::Shared;

/// Who is on the other end of a pipe.
#[derive(Debug, Clone, Default)]
pub struct Caller {
    /// Account SID (`S-1-5-…`).
    pub sid: String,
    /// `DOMAIN\name`, for the log.
    pub name: String,
    /// SYSTEM, or a member of Administrators with an elevated token.
    pub root: bool,
}

fn options() -> ServerOptions {
    let mut o = ServerOptions::new();
    o.reject_remote_clients(true).in_buffer_size(64 * 1024).out_buffer_size(64 * 1024);
    o
}

/// The pipe's DACL for this process: its own SID and the users' group.
fn sddl() -> io::Result<Sddl> {
    let me = super::sys::my_sid();
    let group = sid_of(super::GROUP);
    Sddl::new(&acl::pipe_sddl(Some(&me), group.as_deref(), &acl::unit_sids()))
}

/// The SID of a local account or group, if it exists.
pub fn sid_of(name: &str) -> Option<String> {
    use windows_sys::Win32::Security::{LookupAccountNameW, SID_NAME_USE};
    let w = super::wide(name);
    let mut sid = vec![0u8; 256];
    let mut sl = sid.len() as u32;
    let mut dom = vec![0u16; 256];
    let mut dl = dom.len() as u32;
    let mut use_: SID_NAME_USE = 0;
    // SAFETY: buffers and their lengths match.
    let ok = unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            w.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sl,
            dom.as_mut_ptr(),
            &mut dl,
            &mut use_,
        )
    };
    if ok == 0 {
        return None;
    }
    // SAFETY: LookupAccountNameW wrote a SID into `sid`.
    unsafe { acl::sid_string(sid.as_mut_ptr().cast()) }
}

fn create(name: &str, first: bool) -> io::Result<NamedPipeServer> {
    create_with(name, sddl()?, first)
}

/// A pipe instance with the given DACL (remote clients rejected).
pub fn create_with(name: &str, mut sd: Sddl, first: bool) -> io::Result<NamedPipeServer> {
    let mut o = options();
    o.first_pipe_instance(first);
    // SAFETY: `sd.attrs` is a valid SECURITY_ATTRIBUTES for this call.
    unsafe { o.create_with_security_attributes_raw(name, (&mut sd.attrs as *mut SECURITY_ATTRIBUTES).cast()) }
}

/// Creates the first instance of the pipe, failing if anything else holds
/// the name.
pub fn first_instance(name: &str) -> Result<NamedPipeServer, String> {
    create(name, true).map_err(|e| {
        if e.raw_os_error() == Some(5) {
            format!("{name}: another process already holds this pipe name ({e})")
        } else {
            format!("{name}: {e}")
        }
    })
}

/// Serves connections on `name` until `shutdown` completes, like
/// `server::serve` on Unix.
pub async fn serve(first: NamedPipeServer, name: &str, shared: Arc<Shared>, shutdown: impl Future<Output = ()>) {
    let slots = Arc::new(Semaphore::new(shared.config.server.max_connections as usize));
    let mut server = first;
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            r = server.connect() => {
                if let Err(e) = r {
                    crate::debug!("pipe connect: {e}");
                }
                let next = match create(name, false) {
                    Ok(s) => s,
                    Err(e) => {
                        crate::error!("{name}: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        continue;
                    }
                };
                let client = std::mem::replace(&mut server, next);
                let Ok(permit) = slots.clone().try_acquire_owned() else {
                    crate::warn!("connection refused: {} connections already open", shared.config.server.max_connections);
                    continue;
                };
                let shared = shared.clone();
                tokio::spawn(async move {
                    let who = tokio::time::timeout(std::time::Duration::from_secs(30), identify(client)).await;
                    let (stream, caller) = match who.unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into())) {
                        Ok(v) => v,
                        Err(e) => {
                            crate::debug!("pipe client: {e}");
                            return;
                        }
                    };
                    crate::debug!("client connected ({} {})", caller.name, caller.sid);
                    if let Err(e) = crate::server::connection(stream, &shared, caller.sid.clone(), caller.root).await {
                        crate::debug!("client ({}): {e}", caller.name);
                    }
                    crate::debug!("client disconnected ({})", caller.name);
                    drop(permit);
                });
            }
        }
    }
}

/// Reads the client's first bytes (Windows lets a pipe server look at the
/// caller's token only after it has written something), identifies it, and
/// hands back a stream that replays those bytes.
pub(super) async fn identify(pipe: NamedPipeServer) -> io::Result<(Prefixed<NamedPipeServer>, Caller)> {
    let mut first = vec![0u8; 4096];
    let n = loop {
        pipe.readable().await?;
        match pipe.try_read(&mut first) {
            Ok(n) => break n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    };
    first.truncate(n);
    let caller = caller(pipe.as_raw_handle() as HANDLE)?;
    Ok((Prefixed { prefix: first, at: 0, inner: pipe }, caller))
}

/// The pipe client's identity, read while impersonating it.
fn caller(pipe: HANDLE) -> io::Result<Caller> {
    // SAFETY: impersonation is undone below on every path before returning.
    if unsafe { ImpersonateNamedPipeClient(pipe) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut t: HANDLE = std::ptr::null_mut();
    // SAFETY: the thread's impersonation token, opened as ourselves.
    let ok = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut t) };
    let err = io::Error::last_os_error();
    // SAFETY: drops the impersonation.
    unsafe { RevertToSelf() };
    if ok == 0 {
        return Err(err);
    }
    let token = Handle(t);
    let sid = token_user(token.0).unwrap_or_default();
    let name = token_user_name(token.0).unwrap_or_else(|| sid.clone());
    let root = sid == acl::SYSTEM || (is_elevated(token.0) && is_admin(token.0));
    Ok(Caller { sid, name, root })
}

fn info(token: HANDLE, class: i32) -> Option<Vec<u8>> {
    let mut len = 0u32;
    // SAFETY: size query.
    unsafe { GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut len) };
    let mut buf = vec![0u8; len.max(1) as usize];
    // SAFETY: buffer of the requested size.
    (unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) } != 0).then_some(buf)
}

fn token_user(token: HANDLE) -> Option<String> {
    let b = info(token, TokenUser)?;
    // SAFETY: TokenUser puts a TOKEN_USER first; its SID lives in `b`.
    unsafe { acl::sid_string((*(b.as_ptr() as *const TOKEN_USER)).User.Sid) }
}

fn token_user_name(token: HANDLE) -> Option<String> {
    let b = info(token, TokenUser)?;
    // SAFETY: as above.
    unsafe { acl::account_name((*(b.as_ptr() as *const TOKEN_USER)).User.Sid) }
}

fn is_elevated(token: HANDLE) -> bool {
    let Some(b) = info(token, TokenElevation) else { return false };
    // SAFETY: TokenElevation fills a TOKEN_ELEVATION.
    unsafe { &*(b.as_ptr() as *const TOKEN_ELEVATION) }.TokenIsElevated != 0
}

fn is_admin(token: HANDLE) -> bool {
    use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
    let w = super::wide(acl::ADMINISTRATORS);
    let mut sid: PSID = std::ptr::null_mut();
    // SAFETY: Windows allocates the SID; freed by `Local`.
    if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut sid) } == 0 {
        return false;
    }
    let _free = Local(sid);
    // CheckTokenMembership needs an impersonation token; the one opened
    // from the impersonating thread is one.
    let mut member = 0;
    // SAFETY: valid token and SID.
    (unsafe { CheckTokenMembership(token, sid, &mut member) } != 0) && member != 0
}

/// A stream that first yields bytes already read from `inner`.
pub struct Prefixed<S> {
    prefix: Vec<u8>,
    at: usize,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if self.at < self.prefix.len() {
            let n = (self.prefix.len() - self.at).min(buf.remaining());
            let at = self.at;
            buf.put_slice(&self.prefix[at..at + n]);
            self.at += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Owners a real node pipe can have: the service account (whose SID is
/// fixed by its name), SYSTEM and Administrators. Tests run the server as
/// the same user, which is also accepted.
fn trusted_owner(owner: &str) -> bool {
    owner == acl::SYSTEM
        || owner == acl::ADMINISTRATORS
        || owner == super::sys::my_sid()
        || sid_of(&format!("NT SERVICE\\{}", super::SERVICE)).is_some_and(|s| s == owner)
}

/// The SID that owns a kernel object.
fn owner_of(h: HANDLE) -> Option<String> {
    let mut owner: PSID = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: Windows allocates `sd`; `owner` points into it.
    let r = unsafe {
        GetSecurityInfo(
            h,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if r != 0 {
        return None;
    }
    let _free = Local(sd);
    // SAFETY: `owner` points into `sd`, still alive.
    unsafe { acl::sid_string(owner) }
}

fn check_owner(h: HANDLE, name: &str, trusted: fn(&str) -> bool, strict: bool) -> io::Result<()> {
    match owner_of(h) {
        Some(o) if trusted(&o) => Ok(()),
        Some(o) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{name} belongs to {o}, not to the Nightgrid Node service; not talking to it"),
        )),
        // No READ_CONTROL on the pipe: trust the DACL that let us in,
        // unless only SYSTEM's pipe will do.
        None if strict => Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{name}: owner unknown"))),
        None => Ok(()),
    }
}

/// Opens the pipe for blocking, one-request-at-a-time use (the CLI client).
pub fn open_sync(name: &str) -> io::Result<std::fs::File> {
    open_sync_owned(name, trusted_owner, false)
}

/// Opens a pipe only SYSTEM or Administrators can have created: the
/// helper's, which runs as LocalSystem.
pub fn open_system(name: &str) -> io::Result<std::fs::File> {
    open_sync_owned(name, |o| o == acl::SYSTEM || o == acl::ADMINISTRATORS, true)
}

fn open_sync_owned(name: &str, trusted: fn(&str) -> bool, strict: bool) -> io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    for _ in 0..50 {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .security_qos_flags(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION)
            .open(name)
        {
            Ok(f) => {
                check_owner(f.as_raw_handle() as HANDLE, name, trusted, strict)?;
                return Ok(f);
            }
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                let w = super::wide(name);
                // SAFETY: valid string; waits up to 100 ms for a free instance.
                unsafe { WaitNamedPipeW(w.as_ptr(), 100) };
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, format!("{name}: busy")))
}

/// Opens the pipe for async use (the bridge).
pub async fn open_async(name: &str) -> io::Result<NamedPipeClient> {
    for _ in 0..50 {
        match ClientOptions::new().security_qos_flags(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION).open(name) {
            Ok(c) => {
                check_owner(c.as_raw_handle() as HANDLE, name, trusted_owner, false)?;
                return Ok(c);
            }
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, format!("{name}: busy")))
}

/// Copies stdin to the pipe and the pipe to stdout (`nightgrid-node bridge`).
/// Pipes have no half-close, so the bridge ends when either side ends.
pub fn bridge(name: &str) -> io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let pipe = match open_async(name).await {
            Ok(p) => p,
            Err(e) => {
                println!("{}", crate::client::bridge_error(std::path::Path::new(name), &e));
                return Err(e);
            }
        };
        let (mut r, mut w) = tokio::io::split(pipe);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        // Console and pipe stdin are blocking handles: read them on a thread.
        std::thread::spawn(move || {
            use std::io::Read;
            let mut stdin = std::io::stdin().lock();
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let up = async {
            while let Some(b) = rx.recv().await {
                w.write_all(&b).await?;
            }
            // stdin closed: give the node a moment to answer what it got.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            Ok::<(), io::Error>(())
        };
        let down = async {
            use std::io::Write;
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let n = r.read(&mut buf).await?;
                if n == 0 {
                    return Ok::<(), io::Error>(());
                }
                let mut out = std::io::stdout().lock();
                out.write_all(&buf[..n])?;
                out.flush()?;
            }
        };
        tokio::select! {
            r = down => r,
            r = up => r,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_round_trip_identifies_the_caller() {
        let name = format!(r"\\.\pipe\nightgrid-node-test-{}", std::process::id());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let server = first_instance(&name).unwrap();
            // A second "first" instance must fail: no squatting next to us.
            assert!(first_instance(&name).is_err());
            let n2 = name.clone();
            let client = tokio::task::spawn_blocking(move || {
                use std::io::{BufRead, Write};
                let mut f = open_sync(&n2).unwrap();
                f.write_all(b"ping\n").unwrap();
                let mut line = String::new();
                std::io::BufReader::new(f).read_line(&mut line).unwrap();
                line
            });
            server.connect().await.unwrap();
            let (mut s, who) = identify(server).await.unwrap();
            assert_eq!(who.sid, super::super::sys::my_sid());
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let mut line = String::new();
            let mut r = tokio::io::BufReader::new(&mut s);
            r.read_line(&mut line).await.unwrap();
            assert_eq!(line, "ping\n");
            s.write_all(b"pong\n").await.unwrap();
            assert_eq!(client.await.unwrap(), "pong\n");
        });
    }
}
