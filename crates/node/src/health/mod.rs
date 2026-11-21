//! Host health that needs root (N5a): SMART, storage pools, package
//! updates and a pending reboot, and UPS status.
//!
//! The reads run in a separate, read-only root helper
//! (`nightgrid-node-health.socket`, installed by `allow health`) with the
//! app's own scan scripts (vendored in [`smart`], [`pools`], [`packages`]),
//! and the service parses them with the app's own parsers, so the node and
//! the app judge a disk or a pool the same way. Results are kept in memory
//! for `health.latest`, raise alerts through the engine, and are read again
//! on a schedule: pools every 5 minutes, SMART hourly, updates every 6 hours,
//! UPS every 30 seconds.

pub mod nut;
pub mod packages;
pub mod pools;
pub mod smart;
pub mod ups;
pub mod watch;
pub mod windows;

/// Separates the parts of a scan's output (same marker as the app's).
pub const SECTION: &str = "@@NIGHTGRID@@";

/// Splits `text` at [`SECTION`] lines.
pub fn sections(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(SECTION) {
        out.push(&rest[..i]);
        rest = &rest[i + SECTION.len()..];
        rest = rest.strip_prefix('\n').unwrap_or(rest);
    }
    out.push(rest);
    out
}

/// The app wraps its scripts for the login shell; the helper runs them with
/// `/bin/sh -c` itself.
pub fn sh(inner: &str) -> String {
    inner.to_owned()
}
