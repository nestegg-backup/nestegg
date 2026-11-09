//! The `Status` object of the API (and `nestegg status`).

use std::process::{Command, Stdio};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::config::{Config, Paths};
use crate::state::{Health, Run, State, Store, health, next_due};

pub const TIMER_UNIT: &str = "nestegg-backup.timer";

#[derive(Debug, Serialize)]
pub struct Status {
    pub machine: Machine,
    pub pbs: PbsStatus,
    pub schedule: Schedule,
    pub current_run: Option<Run>,
    pub last_run: Option<Run>,
    pub last_success: Option<Run>,
    pub health: Health,
    pub overdue_after_hours: u32,
    pub encryption: Encryption,
    pub server_verify_failed: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Machine {
    pub hostname: String,
    pub backup_id: String,
    pub os: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PbsStatus {
    pub server: String,
    pub datastore: String,
    pub namespace: Option<String>,
    pub reachable: Option<bool>,
    #[serde(with = "crate::state::rfc3339_opt")]
    pub checked_at: Option<DateTime<Utc>>,
    pub wake_on_lan: bool,
    /// The token secret (or password) is stored. `None`: this process may
    /// not look into the credentials directory (not root).
    pub credentials_stored: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct Schedule {
    pub enabled: bool,
    pub calendar: String,
    pub interval_hours: u32,
    #[serde(with = "crate::state::rfc3339_opt")]
    pub next_run: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct Encryption {
    pub enabled: bool,
    pub key_fingerprint: Option<String>,
    pub key_exported: bool,
    /// An encryption key is stored (it may still be unconfirmed). `None`:
    /// this process may not look into the credentials directory (not root).
    pub key_present: Option<bool>,
}

pub fn timer_enabled(paths: &Paths) -> bool {
    systemctl_is_enabled(&paths.systemctl)
}

/// `systemctl is-enabled` of the backup timer; false if systemctl is missing.
pub fn systemctl_is_enabled(systemctl: &std::path::Path) -> bool {
    Command::new(systemctl)
        .args(["is-enabled", "--quiet", TIMER_UNIT])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn os_name() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
}

pub fn build(paths: &Paths, config: Option<&Config>, timer: bool) -> Result<Status> {
    let store = Store::new(paths);
    let state: State = store.load_state()?;
    let current = crate::run::active_run(paths)?;
    let now = Utc::now();
    let overdue_after_hours = config.map(|c| c.overdue_after_hours).unwrap_or(36);
    // Only root can list the credentials directory; anyone else gets "unknown"
    // instead of a wrong "no".
    let stored = |name: &str| -> Option<bool> {
        let secrets = crate::secrets::SecretStore::new(&paths.credentials_dir);
        if secrets.exists(name) {
            return Some(true);
        }
        match std::fs::read_dir(&paths.credentials_dir) {
            Ok(_) => Some(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
            Err(_) => None,
        }
    };
    let interval = config.map(|c| c.schedule.interval_hours).unwrap_or(24);
    Ok(Status {
        machine: Machine {
            hostname: crate::config::hostname(),
            backup_id: config
                .map(|c| c.effective_backup_id())
                .unwrap_or_else(crate::config::hostname),
            os: os_name(),
        },
        pbs: PbsStatus {
            server: config
                .map(|c| format!("{}:{}", c.pbs.server, c.pbs.port))
                .unwrap_or_default(),
            datastore: config.map(|c| c.pbs.datastore.clone()).unwrap_or_default(),
            namespace: config.and_then(|c| c.pbs.namespace.clone()),
            reachable: state.pbs_reachable,
            checked_at: state.pbs_checked_at,
            wake_on_lan: config.is_some_and(|c| c.active_wake().is_some()),
            credentials_stored: stored(crate::secrets::PASSWORD),
        },
        schedule: Schedule {
            enabled: timer,
            calendar: format!("every {interval} h (checked hourly)"),
            interval_hours: interval,
            next_run: (timer && current.is_none()).then(|| {
                let due = next_due(&state, interval, now);
                // After failures in a row the timer pauses before it retries.
                let config_changed = std::fs::metadata(&paths.config_file)
                    .and_then(|m| m.modified())
                    .ok()
                    .map(DateTime::<Utc>::from);
                match crate::state::retry_not_before(&state, interval, config_changed) {
                    Some(not_before) => due.max(not_before),
                    None => due,
                }
            }),
        },
        health: health(
            config.is_some(),
            &state,
            current.as_ref(),
            overdue_after_hours,
            now,
        ),
        current_run: current,
        last_run: state.last_run.clone(),
        last_success: state.last_success.clone(),
        overdue_after_hours,
        encryption: Encryption {
            enabled: config.is_none_or(|c| c.backup.encryption),
            key_fingerprint: state.key_fingerprint.clone(),
            key_exported: state.key_exported(),
            key_present: stored(crate::secrets::ENCRYPTION_KEY),
        },
        server_verify_failed: state.server_verify_failed.clone(),
    })
}

/// Configuration without secrets (the config file never contains any),
/// plus what the settings form of the web UI needs (decision 0013).
pub fn config_view(config: &Config, timer_enabled: bool) -> serde_json::Value {
    let wake_configured = config.wake.is_some();
    let editable: Vec<&str> = crate::settings::EDITABLE
        .into_iter()
        .filter(|key| *key != "wake_on_lan" || wake_configured)
        .collect();
    serde_json::json!({
        "backup_id": config.effective_backup_id(),
        "sources": config.backup.sources.iter().map(|s| serde_json::json!({
            "archive": format!("{}.pxar", s.archive),
            "path": s.path,
        })).collect::<Vec<_>>(),
        "excludes": config.backup.excludes,
        "schedule": format!("every {} h", config.schedule.interval_hours),
        "interval_hours": config.schedule.interval_hours,
        "encryption": config.backup.encryption,
        "overdue_after_hours": config.overdue_after_hours,
        "automatic_backups": timer_enabled,
        "wake_on_lan": config.active_wake().is_some(),
        "wake_on_lan_configured": wake_configured,
        "editable": editable,
        "limits": crate::settings::limits_json(),
    })
}
