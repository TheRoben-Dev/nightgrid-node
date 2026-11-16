//! Minimal logging to stderr. Under systemd (`JOURNAL_STREAM` set) each line
//! gets an `sd-daemon(3)` priority prefix so journald files it at the right
//! level; otherwise a short level word. No logging framework: keeps the binary
//! and its memory small.

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

/// syslog priorities.
pub const ERR: u8 = 3;
pub const WARNING: u8 = 4;
pub const INFO: u8 = 6;
pub const DEBUG: u8 = 7;

static MAX: AtomicU8 = AtomicU8::new(INFO);

pub fn set_max(level: u8) {
    MAX.store(level, Ordering::Relaxed);
}

fn journal() -> bool {
    static J: OnceLock<bool> = OnceLock::new();
    *J.get_or_init(|| std::env::var_os("JOURNAL_STREAM").is_some())
}

pub fn write(level: u8, args: std::fmt::Arguments<'_>) {
    if level > MAX.load(Ordering::Relaxed) {
        return;
    }
    let mut e = std::io::stderr().lock();
    let _ = if journal() {
        writeln!(e, "<{level}>{args}")
    } else {
        let w = match level {
            0..=ERR => "error",
            WARNING => "warn",
            INFO => "info",
            _ => "debug",
        };
        writeln!(e, "nightgrid-node {w}: {args}")
    };
}

#[macro_export]
macro_rules! error { ($($t:tt)*) => { $crate::log::write($crate::log::ERR, format_args!($($t)*)) }; }
#[macro_export]
macro_rules! warn { ($($t:tt)*) => { $crate::log::write($crate::log::WARNING, format_args!($($t)*)) }; }
#[macro_export]
macro_rules! info { ($($t:tt)*) => { $crate::log::write($crate::log::INFO, format_args!($($t)*)) }; }
#[macro_export]
macro_rules! debug { ($($t:tt)*) => { $crate::log::write($crate::log::DEBUG, format_args!($($t)*)) }; }
