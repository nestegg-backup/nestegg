//! nestegg – unattended, encrypted backups of Linux desktops to a Proxmox
//! Backup Server. See `docs/architecture.md`.

pub mod cancel;
pub mod catalog;
pub mod check;
pub mod client;
pub mod config;
pub mod daemon;
pub mod identity;
pub mod inventory;
pub mod keyproof;
pub mod layout;
pub mod metadata;
pub mod mounts;
pub mod notify;
pub mod rescue;
pub mod restore;
pub mod run;
pub mod secrets;
pub mod settings;
pub mod staging;
pub mod state;
pub mod status;
pub mod stick;
pub mod ui;
pub mod verify;
pub mod wake;
