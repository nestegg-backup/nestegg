//! The `Status` object of the API (and `nestegg status`).

use std::process::{Command, Stdio};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::config::{Config, Paths};
use crate::state::{Health, Run, State, Store, health, next_due};

pub const TIMER_UNIT: &str = "nestegg-backup.timer";
/// The health-check oneshot, started right after a settings change so the
/// change is reported without waiting for the hourly timer (decision 0022).
pub const NOTIFY_UNIT: &str = "nestegg-notify.service";

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub targets: std::collections::BTreeMap<crate::state::Target, TargetStatus>,
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

#[derive(Debug, Clone, Serialize)]
pub struct TargetStatus {
    pub health: Health,
    pub state: String,
    pub last_run: Option<Run>,
    pub last_success: Option<Run>,
    pub current_run: Option<Run>,
    pub consecutive_failures: u32,
    pub overdue_after_hours: u32,
    pub next_run: Option<DateTime<Utc>>,
    pub available_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub space_checked_at: Option<DateTime<Utc>>,
    pub plugged_in: Option<bool>,
    pub plugged_in_uuids: Vec<String>,
    pub in_use: Option<crate::stick::target::Usage>,
    pub sticks: std::collections::BTreeMap<String, StickStatus>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub full_verified_at: Option<DateTime<Utc>>,
    pub server_verify_failed: Vec<String>,
    pub password_reminder: bool,
    pub password_reminders: std::collections::BTreeMap<String, PasswordReminder>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StickStatus {
    pub plugged_in: bool,
    pub in_use: Option<crate::stick::target::Usage>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PasswordReminder {
    KeyCopyMissing,
    PasswordCheckDue,
}

fn password_reminders(
    state: &State,
    config: &Config,
    now: DateTime<Utc>,
) -> std::collections::BTreeMap<String, PasswordReminder> {
    config
        .stick
        .iter()
        .flat_map(|s| &s.data_uuids)
        .filter_map(|uuid| {
            let reason = if !state.stick_key_exported(uuid) {
                PasswordReminder::KeyCopyMissing
            } else if state
                .stick_media
                .get(&uuid.to_ascii_lowercase())
                .and_then(|m| m.password_checked_at)
                .is_none_or(|last| {
                    now - last >= chrono::Duration::days(180)
                        || last > now + crate::state::future_tolerance()
                })
            {
                PasswordReminder::PasswordCheckDue
            } else {
                return None;
            };
            Some((uuid.to_ascii_lowercase(), reason))
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct Machine {
    pub hostname: String,
    pub backup_id: String,
    pub os: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct Schedule {
    pub enabled: bool,
    pub calendar: String,
    pub interval_hours: u32,
    #[serde(with = "crate::state::rfc3339_opt")]
    pub next_run: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Encryption {
    pub enabled: bool,
    pub key_fingerprint: Option<String>,
    pub key_exported: bool,
    /// An encryption key is stored (it may still be unconfirmed). `None`:
    /// this process may not look into the credentials directory (not root).
    pub key_present: Option<bool>,
}

/// Load the configuration for a status view. A missing file means "not
/// configured" (`Ok(None)`); a file that exists but cannot be read or parsed is
/// an error, so a caller who may not read it (for example a group member facing
/// a `0640 root:root` file) is not silently shown the built-in defaults as if
/// they were the configuration.
pub fn config_for_status(path: &std::path::Path) -> Result<Option<Config>> {
    if !path.exists() {
        return Ok(None);
    }
    Config::load(path).map(Some)
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
    let overdue_after_hours = config
        .map(|c| {
            if c.pbs.is_some() {
                c.overdue_after_hours
            } else {
                c.stick
                    .as_ref()
                    .map(|s| s.overdue_after_hours)
                    .unwrap_or(168)
            }
        })
        .unwrap_or(36);
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
    let mut targets = std::collections::BTreeMap::new();
    if let Some(config) = config {
        for target in [crate::state::Target::Pbs, crate::state::Target::Stick] {
            let threshold = match target {
                crate::state::Target::Pbs if config.pbs.is_some() => config.overdue_after_hours,
                crate::state::Target::Stick if config.stick.is_some() => {
                    config
                        .stick
                        .as_ref()
                        .expect("configured")
                        .overdue_after_hours
                }
                _ => continue,
            };
            let view = state.for_target(target);
            let running = current.as_ref().filter(|r| r.target == target).cloned();
            let health = health(true, &view, running.as_ref(), threshold, now);
            let mut condition = format!("{health:?}").to_ascii_lowercase();
            if target == crate::state::Target::Stick && running.is_none() {
                if view
                    .last_run
                    .as_ref()
                    .and_then(|r| r.error.as_ref())
                    .is_some_and(|e| e.kind == crate::state::ErrorKind::StickFull)
                {
                    condition = "stick_full".into();
                } else if view.stick_damaged || !view.server_verify_failed.is_empty() {
                    condition = "stick_damaged".into();
                } else if matches!(crate::stick::target::attached(config, None), Ok(None)) {
                    condition = "stick_not_plugged_in".into();
                }
            }
            let due = next_due(&view, interval, now);
            let retry = crate::state::retry_not_before(
                &view,
                interval,
                crate::state::config_stamp(&paths.config_file).as_deref(),
                now,
            );
            let password_reminders = if target == crate::state::Target::Stick {
                password_reminders(&state, config, now)
            } else {
                Default::default()
            };
            let plugged_in = (target == crate::state::Target::Stick).then(|| {
                config
                    .stick
                    .as_ref()
                    .expect("configured")
                    .data_uuids
                    .iter()
                    .any(|uuid| crate::stick::target::present(uuid))
            });
            let sticks: std::collections::BTreeMap<_, _> = config
                .stick
                .as_ref()
                .filter(|_| target == crate::state::Target::Stick)
                .map(|stick| {
                    stick
                        .data_uuids
                        .iter()
                        .map(|uuid| {
                            (
                                uuid.to_ascii_lowercase(),
                                StickStatus {
                                    plugged_in: crate::stick::target::present(uuid),
                                    in_use: crate::stick::target::in_use(paths, uuid),
                                },
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let plugged_in_uuids = sticks
                .iter()
                .filter(|(_, s)| s.plugged_in)
                .map(|(u, _)| u.clone())
                .collect();
            let in_use = sticks.values().find_map(|s| s.in_use);
            targets.insert(
                target,
                TargetStatus {
                    health,
                    state: condition,
                    last_run: view.last_run,
                    last_success: view.last_success,
                    current_run: running,
                    consecutive_failures: view.consecutive_failures,
                    overdue_after_hours: threshold,
                    next_run: timer.then_some(retry.map(|r| r.max(due)).unwrap_or(due)),
                    available_bytes: view.available_bytes,
                    total_bytes: view.total_bytes,
                    space_checked_at: view.space_checked_at,
                    plugged_in,
                    plugged_in_uuids,
                    in_use,
                    sticks,
                    last_checked_at: view.last_checked_at,
                    full_verified_at: view.full_verified_at,
                    server_verify_failed: view.server_verify_failed,
                    password_reminder: !password_reminders.is_empty(),
                    password_reminders,
                },
            );
        }
    }
    // Legacy fields continue to describe PBS when configured; stick-only
    // installations project their sole target into those fields.
    let state = if config.is_some_and(|c| c.pbs.is_none()) {
        state.for_target(crate::state::Target::Stick)
    } else {
        state
    };
    Ok(Status {
        targets,
        machine: Machine {
            hostname: crate::config::hostname(),
            backup_id: config
                .map(|c| c.effective_backup_id())
                .unwrap_or_else(crate::config::hostname),
            os: os_name(),
        },
        pbs: PbsStatus {
            server: config
                .and_then(|c| c.pbs.as_ref())
                .map(|p| format!("{}:{}", p.server, p.port))
                .unwrap_or_default(),
            datastore: config
                .and_then(|c| c.pbs.as_ref())
                .map(|p| p.datastore.clone())
                .unwrap_or_default(),
            namespace: config
                .and_then(|c| c.pbs.as_ref())
                .and_then(|p| p.namespace.clone()),
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
                let config_now = crate::state::config_stamp(&paths.config_file);
                match crate::state::retry_not_before(&state, interval, config_now.as_deref(), now) {
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
            key_exported: state.key_exported()
                || config.is_some_and(|c| {
                    c.pbs.is_none()
                        && c.stick.as_ref().is_some_and(|s| {
                            s.data_uuids.iter().any(|u| state.stick_key_exported(u))
                        })
                }),
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
        .filter(|key| match *key {
            "wake_on_lan" => wake_configured,
            "stick_overdue_after_hours" => config.stick.is_some(),
            "remove_target" => config.stick.is_some() && config.pbs.is_some(),
            _ => true,
        })
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
        "stick_overdue_after_hours": config.stick.as_ref().map(|s| s.overdue_after_hours),
        "stick_data_uuids": config.stick.as_ref().map(|s| s.data_uuids.clone()).unwrap_or_default(),
        "automatic_backups": timer_enabled,
        "wake_on_lan": config.active_wake().is_some(),
        "wake_on_lan_configured": wake_configured,
        "editable": editable,
        "limits": crate::settings::limits_json(config),
    })
}
