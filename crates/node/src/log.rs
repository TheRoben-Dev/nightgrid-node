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
static NAME: OnceLock<&'static str> = OnceLock::new();

/// The program name on lines outside the journal (default `nightgrid-node`).
pub fn set_name(name: &'static str) {
    let _ = NAME.set(name);
}

pub fn set_max(level: u8) {
    MAX.store(level, Ordering::Relaxed);
}

fn journal() -> bool {
    static J: OnceLock<bool> = OnceLock::new();
    *J.get_or_init(|| std::env::var_os("JOURNAL_STREAM").is_some())
}

/// A log file instead of stderr (a Windows service has no stderr), kept
/// under [`FILE_MAX`] by moving it to `.1` when full.
struct Sink {
    path: std::path::PathBuf,
    file: std::fs::File,
    size: u64,
}

static FILE: std::sync::Mutex<Option<Sink>> = std::sync::Mutex::new(None);
/// Largest log file before it is rotated.
pub const FILE_MAX: u64 = 1 << 20;

/// Sends log lines to `path` from now on.
pub fn to_file(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    *FILE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Sink { path: path.to_owned(), file, size });
    Ok(())
}

fn to_sink(line: &str) -> bool {
    let mut g = FILE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = g.as_mut() else { return false };
    if s.size + line.len() as u64 > FILE_MAX {
        let old = s.path.with_extension("log.1");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&s.path, &old);
        if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&s.path) {
            s.file = f;
            s.size = 0;
        }
    }
    if s.file.write_all(line.as_bytes()).is_ok() {
        s.size += line.len() as u64;
    }
    true
}

pub fn write(level: u8, args: std::fmt::Arguments<'_>) {
    if level > MAX.load(Ordering::Relaxed) {
        return;
    }
    if FILE.lock().map(|g| g.is_some()).unwrap_or(false) {
        let w = match level {
            0..=ERR => "error",
            WARNING => "warn",
            INFO => "info",
            _ => "debug",
        };
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let line = format!("{} {w}: {args}\n", crate::cli::utc_time(now));
        if to_sink(&line) {
            return;
        }
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
        writeln!(e, "{} {w}: {args}", NAME.get().copied().unwrap_or("nightgrid-node"))
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
