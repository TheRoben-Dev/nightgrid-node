//! Reading the Windows Event Log (wevtapi), the counterpart of journalctl.
//!
//! The node polls instead of subscribing: each read asks a channel for the
//! records after the last record id it saw, so the cursor is one number per
//! channel and nothing is missed while the service was stopped (as long as
//! the log hasn't wrapped past it).

use std::collections::HashMap;

use windows_sys::Win32::System::EventLog::{
    EvtClose, EvtFormatMessage, EvtFormatMessageEvent, EvtNext, EvtOpenPublisherMetadata, EvtQuery,
    EvtQueryChannelPath, EvtQueryForwardDirection, EvtQueryReverseDirection, EvtRender, EvtRenderEventXml, EVT_HANDLE,
};

use super::{from_wide, wide};
use crate::evtx::{self, Record};

const ERROR_NO_MORE_ITEMS: i32 = 259;
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
/// The channel doesn't exist (OpenSSH not installed, say).
pub const ERROR_EVT_CHANNEL_NOT_FOUND: i32 = 15007;
/// Records asked for per EvtNext call.
const BATCH: usize = 64;

struct H(EVT_HANDLE);

impl Drop for H {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: a handle wevtapi gave us, closed once.
            unsafe { EvtClose(self.0) };
        }
    }
}

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

/// One event, with its formatted message when asked for.
pub struct Event {
    pub record: Record,
    pub message: Option<String>,
}

fn query(channel: &str, xpath: &str, reverse: bool) -> std::io::Result<H> {
    let (c, q) = (wide(channel), wide(xpath));
    let dir = if reverse { EvtQueryReverseDirection } else { EvtQueryForwardDirection };
    // SAFETY: valid strings; a null session is the local machine.
    let h = unsafe { EvtQuery(0, c.as_ptr(), q.as_ptr(), EvtQueryChannelPath | dir) };
    if h == 0 {
        return Err(last_error());
    }
    Ok(H(h))
}

fn render(event: EVT_HANDLE) -> Option<String> {
    let (mut used, mut props) = (0u32, 0u32);
    // SAFETY: a size query with an empty buffer.
    let ok = unsafe { EvtRender(0, event, EvtRenderEventXml, 0, std::ptr::null_mut(), &mut used, &mut props) };
    if ok != 0 || last_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER) || used == 0 {
        return None;
    }
    let mut buf = vec![0u16; (used as usize).div_ceil(2)];
    // SAFETY: a buffer of the size wevtapi asked for, in bytes.
    let ok = unsafe { EvtRender(0, event, EvtRenderEventXml, used, buf.as_mut_ptr().cast(), &mut used, &mut props) };
    (ok != 0).then(|| from_wide(&buf))
}

/// The event's message in the system's language, from its provider's
/// message table. `None` when the provider has no message for it.
fn message(publisher: EVT_HANDLE, event: EVT_HANDLE) -> Option<String> {
    let mut used = 0u32;
    // SAFETY: a size query with an empty buffer.
    let ok = unsafe {
        EvtFormatMessage(
            publisher,
            event,
            0,
            0,
            std::ptr::null(),
            EvtFormatMessageEvent,
            0,
            std::ptr::null_mut(),
            &mut used,
        )
    };
    if ok != 0 || last_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER) || used == 0 {
        return None;
    }
    let mut buf = vec![0u16; used as usize];
    // SAFETY: a buffer of `used` characters.
    let ok = unsafe {
        EvtFormatMessage(
            publisher,
            event,
            0,
            0,
            std::ptr::null(),
            EvtFormatMessageEvent,
            used,
            buf.as_mut_ptr(),
            &mut used,
        )
    };
    (ok != 0).then(|| from_wide(&buf).trim_end().to_owned())
}

/// Publisher metadata handles, opened once per provider per read.
#[derive(Default)]
struct Publishers(HashMap<String, H>);

impl Publishers {
    fn get(&mut self, provider: &str) -> EVT_HANDLE {
        if let Some(h) = self.0.get(provider) {
            return h.0;
        }
        let w = wide(provider);
        // SAFETY: valid string; null log path, default locale.
        let h = unsafe { EvtOpenPublisherMetadata(0, w.as_ptr(), std::ptr::null(), 0, 0) };
        if self.0.len() < 256 {
            self.0.insert(provider.to_owned(), H(h));
        }
        h
    }
}

/// Records of `channel` after record id `after` that match `filter` (an
/// XPath condition inside `System[...]`, such as `EventID=4625`, or empty),
/// oldest first, at most `max`. With `messages`, each comes with its
/// formatted message (slower: the provider's message table is loaded).
pub fn read(channel: &str, filter: &str, after: u64, max: usize, messages: bool) -> std::io::Result<Vec<Event>> {
    let cond = if filter.is_empty() {
        format!("EventRecordID > {after}")
    } else {
        format!("(EventRecordID > {after}) and ({filter})")
    };
    let q = query(channel, &format!("*[System[{cond}]]"), false)?;
    let mut out = Vec::new();
    let mut pubs = Publishers::default();
    while out.len() < max {
        let mut handles = [0isize; BATCH];
        let mut n = 0u32;
        let want = BATCH.min(max - out.len()) as u32;
        // SAFETY: room for `want` handles.
        if unsafe { EvtNext(q.0, want, handles.as_mut_ptr(), 0xFFFF_FFFF, 0, &mut n) } == 0 {
            let e = last_error();
            if e.raw_os_error() == Some(ERROR_NO_MORE_ITEMS) {
                break;
            }
            return Err(e);
        }
        for &h in &handles[..n as usize] {
            let h = H(h);
            let Some(record) = render(h.0).as_deref().and_then(evtx::parse) else { continue };
            let message = if messages {
                let p = pubs.get(&record.provider);
                if p != 0 {
                    message(p, h.0)
                } else {
                    None
                }
            } else {
                None
            };
            out.push(Event { record, message });
        }
        if n == 0 {
            break;
        }
    }
    Ok(out)
}

/// The newest record id in `channel` (0 when it's empty), so a first start
/// begins at the end instead of reading years of history.
pub fn last_id(channel: &str) -> std::io::Result<u64> {
    let q = query(channel, "*", true)?;
    let mut h = 0isize;
    let mut n = 0u32;
    // SAFETY: room for one handle.
    if unsafe { EvtNext(q.0, 1, &mut h, 0xFFFF_FFFF, 0, &mut n) } == 0 || n == 0 {
        let e = last_error();
        return if e.raw_os_error() == Some(ERROR_NO_MORE_ITEMS) || n == 0 { Ok(0) } else { Err(e) };
    }
    let h = H(h);
    Ok(render(h.0).as_deref().and_then(evtx::parse).map_or(0, |r| r.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_system_log() {
        // CI runs as an administrator, who may read the System log.
        let last = last_id("System").unwrap();
        assert!(last > 0);
        let from = last.saturating_sub(5);
        let ev = read("System", "", from, 10, true).unwrap();
        assert!(!ev.is_empty() && ev.iter().all(|e| e.record.id > from), "{}", ev.len());
        assert!(ev.iter().any(|e| e.message.is_some()));
        let missing = read("Nightgrid/NoSuchChannel", "", 0, 1, false).err().and_then(|e| e.raw_os_error());
        assert_eq!(missing, Some(ERROR_EVT_CHANNEL_NOT_FOUND));
    }
}
