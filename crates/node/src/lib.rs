//! Nightgrid Node: an optional, lightweight agent that keeps collecting on a
//! Linux server while the Nightgrid app is closed, and hands the history to
//! the app when it reconnects. See `docs/DESIGN.md`.

pub mod log;

pub mod client;
pub mod config;
pub mod daemon;
pub mod install;
pub mod metrics;
pub mod server;
pub mod store;
