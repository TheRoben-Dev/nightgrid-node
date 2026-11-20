//! Nightgrid Node: an optional, lightweight agent that keeps collecting on a
//! Linux server while the Nightgrid app is closed, and hands the history to
//! the app when it reconnects. See `docs/DESIGN.md`.

pub mod log;

pub mod compat;

pub mod actions;
pub mod alerts;
pub mod backups;
pub mod bus;
pub mod checks;
pub mod cli;
pub mod client;
pub mod config;
pub mod containers;
pub mod daemon;
pub mod dhcp;
pub mod diskusage;
pub mod docker;
pub mod doctor;
pub mod engine;
pub mod events;
pub mod files;
pub mod firewall;
pub mod fixes;
pub mod heal;
pub mod health;
pub mod helper;
pub mod host;
pub mod install;
pub mod jobs;
pub mod logins;
pub mod logs;
pub mod logwatch;
pub mod menu;
pub mod metrics;
pub mod netwatch;
pub mod peers;
pub mod ports;
pub mod redact;
pub mod rollback;
pub mod server;
#[cfg(unix)]
pub mod sniff;
pub mod statuspage;
pub mod store;
pub mod summary;
pub mod tui;
pub mod units;
pub mod upgrade;
pub mod upsguard;
pub mod uptime;
pub mod usage;
#[cfg(windows)]
pub mod win;
