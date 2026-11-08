//! nestegg – unattended, encrypted backups of Linux desktops to a Proxmox
//! Backup Server. See `docs/architecture.md`.

pub mod client;
pub mod config;
pub mod daemon;
pub mod inventory;
pub mod mounts;
pub mod run;
pub mod secrets;
pub mod state;
pub mod status;
pub mod ui;
pub mod wake;
