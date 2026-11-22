//! Nightgrid Hub (H1): an optional, heavier companion to Nightgrid Node for
//! one always-on host per network. It pulls what the nodes on its network
//! record, over the nodes' own link and read-only, keeps a copy with the
//! nodes' own cursors, and serves it to the app from one place. It notices
//! a node that stops answering while the app is closed. See `docs/HUB.md`.
//!
//! Two units: `nightgrid-hub run` holds the store and the socket and has no
//! network at all; `nightgrid-hub collect` holds the link key and is the only
//! part that talks to the nodes.

pub mod collect;
pub mod config;
pub mod doctor;
pub mod fleet;
pub mod install;
pub mod profile;
pub mod server;
pub mod store;

pub use nightgrid_node::{debug, info, warn};

pub fn now_secs() -> i64 {
    nightgrid_node::daemon::now_secs()
}
