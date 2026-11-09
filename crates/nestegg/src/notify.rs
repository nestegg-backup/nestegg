//! `nestegg notify`: tell people when backups fail or have stopped
//! (decision 0017).
//!
//! An hourly check, independent of the backup unit, looks at the same data as
//! `nestegg status` and reports problems through two channels: a desktop
//! notification for every logged-in user, and an optional command (hook)
//! that runs as root. What was reported to whom is kept in `notify.json` in
//! the state directory, so a problem is announced when it appears, repeated
//! at a calm pace while it lasts and closed with one "works again" message.
//!
//! The policy ([`apply`]) is a plain function over a clock value and a
//! [`Channels`] implementation, so it is tested without processes or time.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration as StdDuration, Instant};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{Config, NotifyConfig, Paths};
use crate::identity::UserIdentity;
use crate::state::{Health, RunState, format_time, read_json, write_json_atomic};
use crate::status::{self, Status};

/// How long the hook may run.
pub const COMMAND_TIMEOUT: StdDuration = StdDuration::from_secs(60);
/// How long `notify-send` may take for one user.
pub const DESKTOP_TIMEOUT: StdDuration = StdDuration::from_secs(15);
/// A "works again" message that could not be delivered is dropped after this.
const RECOVERY_KEEP: Duration = Duration::days(3);
/// The check runs hourly (and after each backup); a repeat that is due
/// within this margin counts as due, so the cycle does not slip by an hour
/// every day.
const REPEAT_MARGIN: Duration = Duration::minutes(30);
/// Longest piece of an error text or reason that goes into a message.
const MAX_DETAIL: usize = 300;
/// Regular (non-system) accounts: from `UID_MIN` of `/etc/login.defs`
/// (default 1000) up to the end of the systemd-homed range. Above that come
/// container, dynamic-service and `nobody` ids.
const DEFAULT_UID_MIN: u32 = 1000;
const REGULAR_UID_MAX: u32 = 60513;

/// `UID_MIN` from the text of `/etc/login.defs`.
pub fn parse_uid_min(login_defs: &str) -> Option<u32> {
    login_defs.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()? == "UID_MIN").then(|| words.next()?.parse().ok())?
    })
}

pub fn regular_uids() -> std::ops::RangeInclusive<u32> {
    let min = fs::read_to_string("/etc/login.defs")
        .ok()
        .and_then(|text| parse_uid_min(&text))
        .unwrap_or(DEFAULT_UID_MIN);
    min..=REGULAR_UID_MAX
}

// ---------------------------------------------------------------------------
// Events and conditions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Event {
    Failing,
    Overdue,
    Incomplete,
    Damaged,
    Recovered,
    Test,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Event::Failing => "failing",
            Event::Overdue => "overdue",
            Event::Incomplete => "incomplete",
            Event::Damaged => "damaged",
            Event::Recovered => "recovered",
            Event::Test => "test",
        }
    }

    /// Critical desktop notifications stay on screen until dismissed.
    pub fn urgent(self) -> bool {
        matches!(self, Event::Failing | Event::Overdue | Event::Damaged)
    }
}

/// A problem that exists right now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    pub kind: Event,
    /// Same key = same problem. A different key (another error, other
    /// reasons) is announced again at once.
    pub key: String,
    /// `failing`: the error text.
    pub error: Option<String>,
    /// `incomplete`: what the last backup left out.
    pub reasons: Vec<String>,
    /// `damaged`: number of damaged snapshots on the server.
    pub damaged: usize,
    /// `overdue`: hours since the last success (`None`: never succeeded).
    pub age_hours: Option<i64>,
    /// `overdue`: the backup timer is switched off.
    pub timer_off: bool,
}

impl Condition {
    fn new(kind: Event, key: String) -> Condition {
        Condition {
            kind,
            key,
            error: None,
            reasons: Vec::new(),
            damaged: 0,
            age_hours: None,
            timer_off: false,
        }
    }

    /// Minimum time between two announcements of an unchanged problem.
    fn repeat_after(&self) -> Duration {
        match self.kind {
            Event::Incomplete => Duration::days(7),
            Event::Overdue if self.timer_off => Duration::days(7),
            _ => Duration::hours(24),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Evaluation {
    /// Nothing to judge this round (not configured, backup in progress).
    Skip(&'static str),
    Checked {
        conditions: Vec<Condition>,
        /// The last run succeeded (or there is none). Only then does an
        /// empty list mean "everything is fine": after a run that ended as
        /// `pbs_unreachable` or `cancelled` a failure is neither confirmed
        /// nor gone.
        settled: bool,
    },
}

/// Derive the current problems from the status.
///
/// `unbacked_since`: since when this computer is known to be set up without
/// a successful backup (the earlier of the configuration file's date and the
/// first check that saw it; a later change of the settings must not restart
/// the wait).
pub fn evaluate(
    status: &Status,
    timer_enabled: bool,
    unbacked_since: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Evaluation {
    match status.health {
        Health::Unconfigured => return Evaluation::Skip("nestegg is not configured"),
        Health::Running => return Evaluation::Skip("a backup is running"),
        _ => {}
    }
    let mut conditions = Vec::new();
    let last_success_at = status
        .last_success
        .as_ref()
        .map(|run| run.finished_at.unwrap_or(run.started_at));
    let age_hours = last_success_at.map(|t| (now - t).num_hours().max(0));
    let failed = status
        .last_run
        .as_ref()
        .filter(|run| run.state == RunState::Failed);

    if let Some(run) = failed {
        let error = run
            .error
            .as_ref()
            .map(|e| e.message.trim().to_string())
            .filter(|m| !m.is_empty());
        let kind = run
            .error
            .as_ref()
            .and_then(|e| serde_json::to_value(e.kind).ok())
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        // Digits are levelled out: an error text with a counter, a size or
        // a time in it is the same problem at every retry, not a new one.
        let mut stable = String::new();
        for ch in error.as_deref().unwrap_or("").chars() {
            if !ch.is_ascii_digit() {
                stable.push(ch);
            } else if !stable.ends_with('#') {
                stable.push('#');
            }
        }
        let mut c = Condition::new(Event::Failing, format!("{kind}|{stable}"));
        c.error = error;
        c.age_hours = age_hours;
        conditions.push(c);
    } else {
        let overdue_limit = Duration::hours(i64::from(status.overdue_after_hours));
        let never_and_old = status.health == Health::Never
            && unbacked_since.is_some_and(|since| now - since > overdue_limit);
        if status.health == Health::Overdue || never_and_old {
            let mut c = Condition::new(
                Event::Overdue,
                if timer_enabled {
                    "timer on"
                } else {
                    "timer off"
                }
                .to_string(),
            );
            c.age_hours = age_hours;
            c.timer_off = !timer_enabled;
            conditions.push(c);
        } else if status.health == Health::Incomplete {
            let reasons = status
                .last_success
                .as_ref()
                .map(|run| run.incomplete.clone())
                .unwrap_or_default();
            let mut c = Condition::new(Event::Incomplete, reasons.join("|"));
            c.reasons = reasons;
            conditions.push(c);
        }
    }

    if !status.server_verify_failed.is_empty() {
        let mut snapshots = status.server_verify_failed.clone();
        snapshots.sort();
        let mut c = Condition::new(Event::Damaged, snapshots.join("|"));
        c.damaged = snapshots.len();
        conditions.push(c);
    }

    let settled = status
        .last_run
        .as_ref()
        .is_none_or(|run| run.state == RunState::Succeeded || run.state == RunState::Failed);
    Evaluation::Checked {
        conditions,
        settled,
    }
}

/// The configuration file exists but cannot be used: backups cannot run at
/// all, and the dashboard shows "not configured", so this is reported as a
/// failure with the parser's message.
pub fn config_problem(error: &str) -> Condition {
    let mut c = Condition::new(Event::Failing, format!("config|{error}"));
    c.error = Some(error.to_string());
    c
}

/// The state of nestegg cannot be read (damaged `state.json`, or one written
/// by a newer version): backups fail on it too, so nobody would hear of it.
pub fn state_problem(error: &str) -> Condition {
    let mut c = Condition::new(Event::Failing, format!("state|{error}"));
    c.error = Some(format!("the backup status cannot be read ({error})"));
    c
}

/// The `[notify]` section of a configuration file that is otherwise invalid,
/// so that a broken file does not silence the hook that would report it.
pub fn lenient_notify_config(text: &str) -> Option<NotifyConfig> {
    let table: toml::Table = toml::from_str(text).ok()?;
    table.get("notify")?.clone().try_into().ok()
}

// ---------------------------------------------------------------------------
// Texts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    De,
}

/// `de`, `de_DE.UTF-8`, `de_AT` ... mean German, everything else English.
pub fn lang_from_value(value: &str) -> Lang {
    if value.trim().to_ascii_lowercase().starts_with("de") {
        Lang::De
    } else {
        Lang::En
    }
}

/// `LC_MESSAGES` or `LANG` from the text of `/etc/default/locale` or
/// `/etc/locale.conf` (`KEY=value`, optionally quoted).
pub fn parse_locale_file(text: &str) -> Option<String> {
    let find = |key: &str| {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches(['"', '\'']).to_string())
            .filter(|v| !v.is_empty())
    };
    find("LC_MESSAGES").or_else(|| find("LANG"))
}

/// The system language: the locale files first (a systemd service has no
/// user environment), then `LANG`.
pub fn system_lang() -> Lang {
    for file in ["/etc/default/locale", "/etc/locale.conf"] {
        if let Some(value) = fs::read_to_string(file)
            .ok()
            .and_then(|t| parse_locale_file(&t))
        {
            return lang_from_value(&value);
        }
    }
    std::env::var("LANG")
        .map(|v| lang_from_value(&v))
        .unwrap_or(Lang::En)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub event: Event,
    pub title: String,
    pub body: String,
}

fn tr(lang: Lang, en: &str, de: &str) -> String {
    match lang {
        Lang::En => en,
        Lang::De => de,
    }
    .to_string()
}

fn shorten(text: &str) -> String {
    let first: String = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    if first.chars().count() > MAX_DETAIL {
        let cut: String = first.chars().take(MAX_DETAIL).collect();
        format!("{}...", cut.trim_end())
    } else {
        first
    }
}

/// "5 hours", "3 days" / "5 Stunden", "3 Tage".
fn span(lang: Lang, hours: i64) -> String {
    let (n, en, en_many, de, de_many) = if hours < 48 {
        (hours.max(1), "hour", "hours", "Stunde", "Stunden")
    } else {
        (hours / 24, "day", "days", "Tag", "Tage")
    };
    match lang {
        Lang::En => format!("{n} {}", if n == 1 { en } else { en_many }),
        Lang::De => format!("{n} {}", if n == 1 { de } else { de_many }),
    }
}

/// Words for a message. `condition` is needed for the problem events.
pub fn compose(lang: Lang, event: Event, condition: Option<&Condition>) -> Message {
    let fallback = Condition::new(event, String::new());
    let c = condition.unwrap_or(&fallback);
    let (title, body) = match event {
        Event::Failing => {
            let error = c
                .error
                .as_deref()
                .map(|e| shorten(e).trim_end_matches('.').to_string())
                .unwrap_or_else(|| tr(lang, "no reason was recorded", "ohne genannten Grund"));
            (
                tr(lang, "Backup failed", "Datensicherung fehlgeschlagen"),
                match lang {
                    Lang::En => format!(
                        "The last backup of this computer did not work: {error}. \
                         Details: run \"nestegg ui\"."
                    ),
                    Lang::De => format!(
                        "Die letzte Datensicherung dieses Computers hat nicht geklappt: {error}. \
                         Mehr dazu: \"nestegg ui\" ausführen."
                    ),
                },
            )
        }
        Event::Overdue => {
            let age = c.age_hours.map(|h| span(lang, h));
            let (title, first, last) = match (c.timer_off, &age) {
                (true, Some(age)) => (
                    tr(
                        lang,
                        "Automatic backups are off",
                        "Automatische Sicherungen sind aus",
                    ),
                    match lang {
                        Lang::En => format!(
                            "Automatic backups are switched off, and the last successful \
                             backup was {age} ago."
                        ),
                        Lang::De => format!(
                            "Automatische Sicherungen sind ausgeschaltet; die letzte \
                             erfolgreiche Sicherung ist {age} her."
                        ),
                    },
                    tr(
                        lang,
                        "Switch them on again: \"nestegg ui\".",
                        "Wieder einschalten: \"nestegg ui\".",
                    ),
                ),
                (true, None) => (
                    tr(
                        lang,
                        "Automatic backups are off",
                        "Automatische Sicherungen sind aus",
                    ),
                    tr(
                        lang,
                        "Automatic backups are switched off, and this computer has not been \
                         backed up yet.",
                        "Automatische Sicherungen sind ausgeschaltet, und dieser Computer \
                         wurde noch nie gesichert.",
                    ),
                    tr(
                        lang,
                        "Switch them on again: \"nestegg ui\".",
                        "Wieder einschalten: \"nestegg ui\".",
                    ),
                ),
                (false, Some(age)) => (
                    tr(lang, "Backups are overdue", "Datensicherung überfällig"),
                    match lang {
                        Lang::En => format!(
                            "The last successful backup of this computer was {age} ago. \
                             Please check that the backup server is switched on and reachable."
                        ),
                        Lang::De => format!(
                            "Die letzte erfolgreiche Sicherung dieses Computers ist {age} her. \
                             Bitte prüfen, ob der Backup-Server eingeschaltet und erreichbar ist."
                        ),
                    },
                    tr(
                        lang,
                        "Details: \"nestegg ui\".",
                        "Mehr dazu: \"nestegg ui\".",
                    ),
                ),
                (false, None) => (
                    tr(lang, "No backup yet", "Noch keine Datensicherung"),
                    tr(
                        lang,
                        "This computer has not been backed up successfully yet. Please check \
                         that the backup server is switched on and reachable.",
                        "Dieser Computer wurde noch nicht erfolgreich gesichert. Bitte prüfen, \
                         ob der Backup-Server eingeschaltet und erreichbar ist.",
                    ),
                    tr(
                        lang,
                        "Details: \"nestegg ui\".",
                        "Mehr dazu: \"nestegg ui\".",
                    ),
                ),
            };
            (title, format!("{first} {last}"))
        }
        Event::Incomplete => {
            let reasons = Some(
                c.reasons
                    .iter()
                    .map(|r| shorten(r))
                    .collect::<Vec<_>>()
                    .join("; "),
            )
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| tr(lang, "see the details", "siehe Einzelheiten"));
            (
                tr(lang, "Backup is incomplete", "Datensicherung unvollständig"),
                match lang {
                    Lang::En => format!(
                        "The last backup worked, but it left something out: {reasons}. \
                         Details: \"nestegg ui\"."
                    ),
                    Lang::De => format!(
                        "Die letzte Datensicherung hat geklappt, aber es fehlt etwas darin: \
                         {reasons}. Mehr dazu: \"nestegg ui\"."
                    ),
                },
            )
        }
        Event::Damaged => {
            let n = c.damaged.max(1);
            (
                tr(
                    lang,
                    "Damaged backups on the server",
                    "Beschädigte Sicherungen auf dem Server",
                ),
                match lang {
                    Lang::En => format!(
                        "The backup server reports that {n} saved {} of this computer {} \
                         damaged. Older files may not be restorable from {}. \
                         Details: \"nestegg ui\".",
                        if n == 1 { "backup" } else { "backups" },
                        if n == 1 { "is" } else { "are" },
                        if n == 1 { "it" } else { "them" },
                    ),
                    Lang::De => format!(
                        "Der Backup-Server meldet, dass {n} gespeicherte {} dieses Computers \
                         beschädigt {}. Ältere Dateien lassen sich daraus vielleicht nicht \
                         wiederherstellen. Mehr dazu: \"nestegg ui\".",
                        if n == 1 { "Sicherung" } else { "Sicherungen" },
                        if n == 1 { "ist" } else { "sind" },
                    ),
                },
            )
        }
        Event::Recovered => (
            tr(
                lang,
                "Backups work again",
                "Datensicherung funktioniert wieder",
            ),
            tr(
                lang,
                "Everything is fine again: this computer is being backed up as planned. \
                 Overview: \"nestegg ui\".",
                "Alles in Ordnung: Dieser Computer wird wieder wie geplant gesichert. \
                 Übersicht: \"nestegg ui\".",
            ),
        ),
        Event::Test => (
            tr(lang, "nestegg test message", "nestegg Testnachricht"),
            tr(
                lang,
                "If you can read this, notifications from nestegg reach you. \
                 Your backups: \"nestegg ui\".",
                "Wenn du das liest, erreichen dich die Meldungen von nestegg. \
                 Deine Sicherungen: \"nestegg ui\".",
            ),
        ),
    };
    Message { event, title, body }
}

// ---------------------------------------------------------------------------
// Persistent bookkeeping
// ---------------------------------------------------------------------------

/// A problem that was announced and still lasts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Active {
    /// The problem as last seen; it supplies the words for late deliveries.
    pub condition: Condition,
    #[serde(with = "crate::state::rfc3339")]
    pub since: DateTime<Utc>,
    #[serde(with = "crate::state::rfc3339")]
    pub announced_at: DateTime<Utc>,
    /// Number of the current announcement; grows with every repeat.
    pub seq: u64,
    /// Announcement the hook has received (0: none).
    #[serde(default)]
    pub command_seq: u64,
    /// Announcement each user (by uid) has received.
    #[serde(default)]
    pub users: BTreeMap<u32, u64>,
}

/// A "works again" message that still has to reach some of its audience.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingRecovery {
    #[serde(with = "crate::state::rfc3339")]
    pub since: DateTime<Utc>,
    /// Users who were told about the problem and should hear the end of it.
    #[serde(default)]
    pub users: BTreeSet<u32>,
    #[serde(default)]
    pub users_done: BTreeSet<u32>,
    /// The hook was told about the problem and has not heard the end yet.
    #[serde(default)]
    pub command: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NotifyState {
    #[serde(default)]
    pub active: BTreeMap<Event, Active>,
    #[serde(default)]
    pub recovery: Option<PendingRecovery>,
    /// First check that found nestegg set up without any successful backup.
    #[serde(default, with = "crate::state::rfc3339_opt")]
    pub unbacked_since: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

/// A logged-in user with a session bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub uid: u32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    Sent,
    /// Not an error: no `notify-send`, no notification service answering.
    Unavailable(String),
}

pub trait Channels {
    /// Users to notify; empty when the desktop channel is off.
    fn sessions(&mut self) -> Vec<Session>;
    fn command_enabled(&self) -> bool;
    fn send_desktop(&mut self, session: &Session, message: &Message) -> Result<Delivery>;
    fn send_command(&mut self, message: &Message) -> Result<()>;
}

/// Run the policy for the current problems.
///
/// `carry_failing`: the last run was neither a success nor a failure
/// (server unreachable, cancelled), so an announced `failing` problem stays
/// open instead of being closed.
///
/// Returns a line per action for the journal / `--dry-run`.
pub fn apply(
    state: &mut NotifyState,
    conditions: &[Condition],
    carry_failing: bool,
    now: DateTime<Utc>,
    lang: Lang,
    channels: &mut dyn Channels,
) -> Vec<String> {
    let mut log = Vec::new();
    let sessions = channels.sessions();
    let command = channels.command_enabled();

    // Problems that are gone.
    let mut told_users = BTreeSet::new();
    let mut told_command = false;
    let gone: Vec<Event> = state
        .active
        .keys()
        .copied()
        .filter(|kind| !conditions.iter().any(|c| c.kind == *kind))
        .filter(|kind| !(carry_failing && *kind == Event::Failing))
        .collect();
    for kind in gone {
        if let Some(entry) = state.active.remove(&kind) {
            log.push(format!("{}: no longer the case", kind.as_str()));
            told_users.extend(entry.users.keys().copied());
            told_command |= entry.command_seq > 0;
        }
    }

    // Problems that exist: new, changed, due for a repeat or unchanged.
    for c in conditions {
        let announce = match state.active.get_mut(&c.kind) {
            Some(entry) if entry.condition.key == c.key => {
                entry.condition = c.clone();
                // A clock that was ahead and has been corrected must not
                // silence the repeats until it catches up.
                if entry.announced_at > now {
                    entry.announced_at = now;
                }
                if now - entry.announced_at >= c.repeat_after() - REPEAT_MARGIN {
                    entry.seq += 1;
                    entry.announced_at = now;
                    log.push(format!("{}: still the case, repeating", c.kind.as_str()));
                }
                false
            }
            Some(_) => true,
            None => true,
        };
        if announce {
            let replaced = state.active.contains_key(&c.kind);
            log.push(format!(
                "{}: {}",
                c.kind.as_str(),
                if replaced {
                    "changed, announcing again"
                } else {
                    "new, announcing"
                }
            ));
            state.active.insert(
                c.kind,
                Active {
                    condition: c.clone(),
                    since: now,
                    announced_at: now,
                    seq: 1,
                    command_seq: 0,
                    users: BTreeMap::new(),
                },
            );
        }
    }
    if !conditions.is_empty() {
        state.recovery = None;
    }

    // Deliver what has not reached its audience yet (a user who logs in
    // later, a hook that failed).
    for (kind, entry) in state.active.iter_mut() {
        let message = compose(lang, *kind, Some(&entry.condition));
        for session in &sessions {
            if entry.users.get(&session.uid).copied().unwrap_or(0) >= entry.seq {
                continue;
            }
            match channels.send_desktop(session, &message) {
                Ok(Delivery::Sent) => {
                    entry.users.insert(session.uid, entry.seq);
                    log.push(format!(
                        "{}: desktop notification to {}",
                        kind.as_str(),
                        session.name
                    ));
                }
                Ok(Delivery::Unavailable(why)) => log.push(format!(
                    "{}: no desktop notification to {} ({why})",
                    kind.as_str(),
                    session.name
                )),
                Err(e) => log.push(format!(
                    "{}: desktop notification to {} failed: {e:#}",
                    kind.as_str(),
                    session.name
                )),
            }
        }
        if command && entry.command_seq < entry.seq {
            match channels.send_command(&message) {
                Ok(()) => {
                    entry.command_seq = entry.seq;
                    log.push(format!("{}: command ran", kind.as_str()));
                }
                Err(e) => log.push(format!(
                    "{}: command failed, trying again at the next check: {e:#}",
                    kind.as_str()
                )),
            }
        }
    }

    // Everything fine again.
    if state.active.is_empty() && conditions.is_empty() {
        if !told_users.is_empty() || told_command {
            let pending = state.recovery.get_or_insert(PendingRecovery {
                since: now,
                users: BTreeSet::new(),
                users_done: BTreeSet::new(),
                command: false,
            });
            pending.users.extend(told_users);
            pending.command |= told_command;
        }
        if let Some(pending) = &mut state.recovery {
            let message = compose(lang, Event::Recovered, None);
            for session in &sessions {
                if !pending.users.contains(&session.uid)
                    || pending.users_done.contains(&session.uid)
                {
                    continue;
                }
                match channels.send_desktop(session, &message) {
                    Ok(Delivery::Sent) => {
                        pending.users_done.insert(session.uid);
                        log.push(format!(
                            "recovered: desktop notification to {}",
                            session.name
                        ));
                    }
                    Ok(Delivery::Unavailable(why)) => log.push(format!(
                        "recovered: no desktop notification to {} ({why})",
                        session.name
                    )),
                    Err(e) => log.push(format!(
                        "recovered: desktop notification to {} failed: {e:#}",
                        session.name
                    )),
                }
            }
            if pending.command && command {
                match channels.send_command(&message) {
                    Ok(()) => {
                        pending.command = false;
                        log.push("recovered: command ran".to_string());
                    }
                    Err(e) => log.push(format!(
                        "recovered: command failed, trying again at the next check: {e:#}"
                    )),
                }
            }
            let command_open = pending.command && command;
            let users_open = pending
                .users
                .iter()
                .any(|u| !pending.users_done.contains(u));
            if (!command_open && !users_open) || now - pending.since > RECOVERY_KEEP {
                state.recovery = None;
            }
        }
    }
    log
}

// ---------------------------------------------------------------------------
// The real channels
// ---------------------------------------------------------------------------

/// What the hook and the messages know about the computer.
pub struct Context {
    pub lang: Lang,
    pub hostname: String,
    pub last_success: Option<DateTime<Utc>>,
}

pub struct SystemChannels<'a> {
    pub config: &'a NotifyConfig,
    pub context: &'a Context,
    /// `/run/user`
    pub runtime_root: PathBuf,
}

/// `/run/user`, or `NESTEGG_USER_RUNTIME_ROOT` (tests).
pub fn runtime_root() -> PathBuf {
    std::env::var_os("NESTEGG_USER_RUNTIME_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/user"))
}

/// Users in the uid range `regular` that have a session bus below `root`
/// (`<root>/<uid>/bus`).
/// `user_name` maps a uid to an account name (`None`: no such account).
pub fn find_sessions(
    root: &Path,
    regular: std::ops::RangeInclusive<u32>,
    user_name: impl Fn(u32) -> Option<String>,
) -> Vec<Session> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut sessions: Vec<Session> = entries
        .flatten()
        .filter_map(|entry| {
            let uid: u32 = entry.file_name().to_str()?.parse().ok()?;
            if !regular.contains(&uid) {
                return None;
            }
            let bus = fs::symlink_metadata(entry.path().join("bus")).ok()?;
            if !bus.file_type().is_socket() {
                return None;
            }
            Some(Session {
                uid,
                name: user_name(uid)?,
            })
        })
        .collect();
    sessions.sort_by_key(|s| s.uid);
    sessions
}

fn system_user_name(uid: u32) -> Option<String> {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
}

fn default_path() -> String {
    std::env::var("PATH")
        .unwrap_or_else(|_| "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into())
}

fn find_in_path(program: &str, path: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|candidate| {
            fs::metadata(candidate)
                .map(|m| m.is_file() && m.mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

struct Outcome {
    /// `None`: still running at the deadline, killed.
    status: Option<ExitStatus>,
    output: String,
}

/// Run `cmd` with `input` on stdin; stdout and stderr are collected (only the
/// start is kept). After `timeout` the whole process group is killed. Output
/// is read without blocking, so a program that leaves a background process
/// holding the pipe cannot hang the check.
fn run_with_timeout(mut cmd: Command, input: &[u8], timeout: StdDuration) -> Result<Outcome> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use std::io::Write;

    let (read_end, write_end) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
    fcntl(&read_end, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::from(write_end.try_clone()?))
        .stderr(Stdio::from(write_end))
        .process_group(0);
    let mut child = cmd.spawn()?;
    // `cmd` holds the write ends until it is dropped.
    drop(cmd);
    if let Some(mut stdin) = child.stdin.take() {
        // The messages are far smaller than the pipe buffer. A program that
        // ignores its input and exits early is fine.
        let _ = stdin.write_all(input);
    }
    let mut reader = fs::File::from(read_end);
    let mut output = Vec::new();
    let mut chunk = [0u8; 2048];
    let mut drain = |output: &mut Vec<u8>| {
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            if output.len() < 4096 {
                output.extend_from_slice(&chunk[..n.min(4096 - output.len())]);
            }
        }
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        drain(&mut output);
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(StdDuration::from_millis(50));
    };
    drain(&mut output);
    Ok(Outcome {
        status,
        output: String::from_utf8_lossy(&output).trim().to_string(),
    })
}

/// Notification servers render the body as markup (bold, clickable links).
/// Error texts come from the backup server and from file names, so they are
/// shown literally. The hook gets the plain text.
pub fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

impl SystemChannels<'_> {
    /// `notify-send` for `session`, run with that user's identity and session
    /// bus.
    fn desktop_command(
        &self,
        program: &Path,
        session: &Session,
        message: &Message,
    ) -> Result<Command> {
        let identity = UserIdentity::from_uid(session.uid)?;
        let user_dir = self.runtime_root.join(session.uid.to_string());
        let mut cmd = Command::new(program);
        cmd.env_clear()
            .env("PATH", default_path())
            .env("XDG_RUNTIME_DIR", &user_dir)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", user_dir.join("bus").display()),
            )
            .arg("--app-name=nestegg")
            .arg(format!(
                "--urgency={}",
                if message.event.urgent() {
                    "critical"
                } else {
                    "normal"
                }
            ))
            .arg(if message.event.urgent() {
                "--icon=dialog-warning"
            } else {
                "--icon=dialog-information"
            })
            .arg("--")
            .arg(&message.title)
            .arg(escape_markup(&message.body));
        identity.apply(&mut cmd);
        Ok(cmd)
    }
}

impl Channels for SystemChannels<'_> {
    fn sessions(&mut self) -> Vec<Session> {
        if !self.config.desktop {
            return Vec::new();
        }
        find_sessions(&self.runtime_root, regular_uids(), system_user_name)
    }

    fn command_enabled(&self) -> bool {
        self.config.command.is_some()
    }

    fn send_desktop(&mut self, session: &Session, message: &Message) -> Result<Delivery> {
        let Some(program) = find_in_path("notify-send", &default_path()) else {
            return Ok(Delivery::Unavailable("notify-send is not installed".into()));
        };
        let cmd = self.desktop_command(&program, session, message)?;
        let outcome = run_with_timeout(cmd, b"", DESKTOP_TIMEOUT)
            .with_context(|| format!("cannot run notify-send as {}", session.name))?;
        match outcome.status {
            Some(status) if status.success() => Ok(Delivery::Sent),
            Some(status) => Ok(Delivery::Unavailable(format!(
                "notify-send {status}: {}",
                shorten(&outcome.output)
            ))),
            None => Ok(Delivery::Unavailable(
                "notify-send did not answer in time".into(),
            )),
        }
    }

    fn send_command(&mut self, message: &Message) -> Result<()> {
        let Some(argv) = &self.config.command else {
            return Ok(());
        };
        let (program, args) = argv.split_first().context("notify.command is empty")?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir("/")
            .env("NESTEGG_EVENT", message.event.as_str())
            .env("NESTEGG_TITLE", &message.title)
            .env("NESTEGG_MESSAGE", &message.body)
            .env("NESTEGG_HOSTNAME", &self.context.hostname)
            .env(
                "NESTEGG_LAST_SUCCESS",
                self.context
                    .last_success
                    .map(format_time)
                    .unwrap_or_default(),
            );
        let outcome = run_with_timeout(
            cmd,
            format!("{}\n", message.body).as_bytes(),
            COMMAND_TIMEOUT,
        )
        .with_context(|| format!("cannot run {program}"))?;
        match outcome.status {
            Some(status) if status.success() => Ok(()),
            Some(status) => anyhow::bail!("{program} {status}: {}", shorten(&outcome.output)),
            None => anyhow::bail!(
                "{program} did not finish within {} s",
                COMMAND_TIMEOUT.as_secs()
            ),
        }
    }
}

/// Prints instead of sending (`--dry-run`).
struct DryChannels<'a> {
    inner: SystemChannels<'a>,
    log: Vec<String>,
}

impl Channels for DryChannels<'_> {
    fn sessions(&mut self) -> Vec<Session> {
        self.inner.sessions()
    }
    fn command_enabled(&self) -> bool {
        self.inner.command_enabled()
    }
    fn send_desktop(&mut self, session: &Session, message: &Message) -> Result<Delivery> {
        self.log.push(format!(
            "  would notify {} ({}): {} | {}",
            session.name,
            if message.event.urgent() {
                "critical"
            } else {
                "normal"
            },
            message.title,
            message.body
        ));
        Ok(Delivery::Sent)
    }
    fn send_command(&mut self, message: &Message) -> Result<()> {
        self.log.push(format!(
            "  would run {:?} with NESTEGG_EVENT={}: {} | {}",
            self.inner.config.command.as_deref().unwrap_or_default(),
            message.event.as_str(),
            message.title,
            message.body
        ));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The command
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Send a test message through all configured channels.
    pub test: bool,
    /// Print what would be sent; change nothing.
    pub dry_run: bool,
}

/// Serialises concurrent checks (timer, `OnSuccess=`, by hand).
fn lock(paths: &Paths) -> Result<nix::fcntl::Flock<fs::File>> {
    fs::create_dir_all(&paths.state_dir)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.state_dir.join(".notify.lock"))?;
    nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, e)| e)
        .context("cannot lock the notification state")
}

fn load_state(paths: &Paths) -> NotifyState {
    let file = paths.notify_state_file();
    if !file.exists() {
        return NotifyState::default();
    }
    match read_json(&file) {
        Ok(state) => state,
        Err(e) => {
            eprintln!("warning: starting the notification record anew: {e:#}");
            NotifyState::default()
        }
    }
}

pub fn run(paths: &Paths, options: &Options) -> Result<()> {
    let mut config_error = None;
    let config = match Config::load(&paths.config_file) {
        Ok(config) => Some(config),
        Err(e) => {
            if paths.config_file.exists() {
                eprintln!("warning: {e:#}");
                config_error = Some(format!("{e:#}"));
            }
            None
        }
    };
    let notify_config = match &config {
        Some(c) => c.notify.clone(),
        None => fs::read_to_string(&paths.config_file)
            .ok()
            .and_then(|text| lenient_notify_config(&text))
            .unwrap_or_default(),
    };
    // The lock comes first: a check that waits for another one (a slow hook)
    // must judge the situation as it is when its turn comes.
    let _lock = if options.dry_run || options.test {
        None
    } else {
        Some(lock(paths)?)
    };
    let timer = status::timer_enabled(paths);
    let now = Utc::now();
    // An unreadable state must not silence the check: backups fail on the
    // same file, and this is the only place that would tell anyone.
    let (status, state_error) = match status::build(paths, config.as_ref(), timer) {
        Ok(status) => (Some(status), None),
        Err(e) => {
            eprintln!("warning: {e:#}");
            (None, Some(format!("{e:#}")))
        }
    };
    let context = Context {
        lang: system_lang(),
        hostname: crate::config::hostname(),
        last_success: status
            .as_ref()
            .and_then(|s| s.last_success.as_ref())
            .map(|run| run.finished_at.unwrap_or(run.started_at)),
    };
    let channels = SystemChannels {
        config: &notify_config,
        context: &context,
        runtime_root: runtime_root(),
    };

    if options.test {
        return send_test(channels, options.dry_run);
    }

    let mut state = load_state(paths);
    // Since when the computer has been set up without a successful backup.
    let never = status.as_ref().is_some_and(|s| s.health == Health::Never);
    if never {
        state.unbacked_since.get_or_insert(now);
    } else if status.as_ref().is_some_and(|s| s.last_success.is_some()) {
        state.unbacked_since = None;
    }
    let modified = fs::metadata(&paths.config_file)
        .and_then(|m| m.modified())
        .ok()
        .map(DateTime::<Utc>::from);
    let unbacked_since = match (modified, state.unbacked_since) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let save = |state: &NotifyState| {
        if options.dry_run {
            return;
        }
        // A full disk must not turn every check into a repeat of everything
        // or stop the messages; the record is written again next time.
        if let Err(e) = write_json_atomic(&paths.notify_state_file(), state) {
            eprintln!("warning: cannot save the notification record: {e:#}");
        }
    };
    let (conditions, settled) = if let Some(error) = &config_error {
        (vec![config_problem(error)], true)
    } else if let Some(error) = &state_error {
        (vec![state_problem(error)], true)
    } else {
        let status = status.as_ref().expect("status or state_error is set");
        match evaluate(status, timer, unbacked_since, now) {
            Evaluation::Skip(why) => {
                println!("nothing to check: {why}");
                save(&state);
                return Ok(());
            }
            Evaluation::Checked {
                conditions,
                settled,
            } => (conditions, settled),
        }
    };
    // Last run neither success nor failure and no problem found: nothing is
    // confirmed either way, so the record stays as it is.
    if conditions.is_empty() && !settled {
        println!("nothing new: the last run ended without a result (server unreachable?)");
        save(&state);
        return Ok(());
    }
    // A failure that is not the last run any more (the last run was
    // cancelled, for example) stays open.
    let carry_failing = !settled;

    let lines = if options.dry_run {
        let mut dry = DryChannels {
            inner: channels,
            log: Vec::new(),
        };
        let mut lines = apply(
            &mut state,
            &conditions,
            carry_failing,
            now,
            context.lang,
            &mut dry,
        );
        lines.extend(dry.log);
        lines
    } else {
        let mut channels = channels;
        let lines = apply(
            &mut state,
            &conditions,
            carry_failing,
            now,
            context.lang,
            &mut channels,
        );
        save(&state);
        lines
    };
    if lines.is_empty() {
        println!(
            "{}",
            if conditions.is_empty() {
                "everything is fine, nothing to report"
            } else {
                "known problems, nothing new to report"
            }
        );
    }
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

fn send_test(mut channels: SystemChannels<'_>, dry_run: bool) -> Result<()> {
    let message = compose(channels.context.lang, Event::Test, None);
    println!("{} | {}", message.title, message.body);
    let sessions = channels.sessions();
    if !channels.config.desktop {
        println!("desktop: switched off ([notify] desktop = false)");
    } else if sessions.is_empty() {
        println!("desktop: nobody is logged in with a session bus");
    }
    for session in &sessions {
        if dry_run {
            println!("desktop: would notify {}", session.name);
            continue;
        }
        match channels.send_desktop(session, &message) {
            Ok(Delivery::Sent) => println!("desktop: sent to {}", session.name),
            Ok(Delivery::Unavailable(why)) => {
                println!("desktop: not delivered to {} ({why})", session.name)
            }
            Err(e) => println!("desktop: failed for {}: {e:#}", session.name),
        }
    }
    if !channels.command_enabled() {
        println!("command: not configured ([notify] command)");
    } else if dry_run {
        println!(
            "command: would run {:?}",
            channels.config.command.as_deref().unwrap_or_default()
        );
    } else {
        match channels.send_command(&message) {
            Ok(()) => println!("command: ran successfully"),
            Err(e) => println!("command: failed: {e:#}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ErrorKind, Run, RunError, Trigger};
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 9, 8, 0, 0).unwrap()
    }

    fn hours(h: i64) -> DateTime<Utc> {
        t0() + Duration::hours(h)
    }

    // --- policy -----------------------------------------------------------

    struct Fake {
        sessions: Vec<Session>,
        command: bool,
        command_fails: bool,
        desktop_unavailable: bool,
        desktop: Vec<(u32, Event)>,
        hook: Vec<Event>,
        bodies: Vec<String>,
    }

    impl Fake {
        fn new(users: &[(u32, &str)], command: bool) -> Fake {
            Fake {
                sessions: users
                    .iter()
                    .map(|(uid, name)| Session {
                        uid: *uid,
                        name: name.to_string(),
                    })
                    .collect(),
                command,
                command_fails: false,
                desktop_unavailable: false,
                desktop: Vec::new(),
                hook: Vec::new(),
                bodies: Vec::new(),
            }
        }

        fn login(&mut self, uid: u32, name: &str) {
            self.sessions.push(Session {
                uid,
                name: name.into(),
            });
        }

        fn clear(&mut self) {
            self.desktop.clear();
            self.hook.clear();
            self.bodies.clear();
        }
    }

    impl Channels for Fake {
        fn sessions(&mut self) -> Vec<Session> {
            self.sessions.clone()
        }
        fn command_enabled(&self) -> bool {
            self.command
        }
        fn send_desktop(&mut self, session: &Session, message: &Message) -> Result<Delivery> {
            if self.desktop_unavailable {
                return Ok(Delivery::Unavailable("no notify-send".into()));
            }
            self.desktop.push((session.uid, message.event));
            self.bodies.push(message.body.clone());
            Ok(Delivery::Sent)
        }
        fn send_command(&mut self, message: &Message) -> Result<()> {
            if self.command_fails {
                anyhow::bail!("hook exited with status 1");
            }
            self.hook.push(message.event);
            Ok(())
        }
    }

    fn failing(error: &str) -> Condition {
        let mut c = Condition::new(Event::Failing, format!("client_failed|{error}"));
        c.error = Some(error.to_string());
        c
    }

    fn overdue(timer_off: bool, age: Option<i64>) -> Condition {
        let mut c = Condition::new(
            Event::Overdue,
            if timer_off { "timer off" } else { "timer on" }.into(),
        );
        c.timer_off = timer_off;
        c.age_hours = age;
        c
    }

    fn incomplete(reason: &str) -> Condition {
        let mut c = Condition::new(Event::Incomplete, reason.to_string());
        c.reasons = vec![reason.to_string()];
        c
    }

    fn damaged(n: usize) -> Condition {
        let mut c = Condition::new(Event::Damaged, format!("{n} snapshots"));
        c.damaged = n;
        c
    }

    fn step(
        state: &mut NotifyState,
        conditions: &[Condition],
        at: DateTime<Utc>,
        ch: &mut Fake,
    ) -> Vec<String> {
        ch.clear();
        apply(state, conditions, false, at, Lang::En, ch)
    }

    #[test]
    fn a_new_problem_is_announced_once_and_repeated_after_a_day() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], true);
        let problem = [failing("disk full")];

        step(&mut state, &problem, hours(0), &mut ch);
        assert_eq!(ch.desktop, [(1000, Event::Failing)]);
        assert_eq!(ch.hook, [Event::Failing]);
        assert!(ch.bodies[0].contains("disk full"));

        // The hourly checks that follow stay quiet.
        for h in 1..=22 {
            step(&mut state, &problem, hours(h), &mut ch);
            assert!(ch.desktop.is_empty() && ch.hook.is_empty(), "hour {h}");
        }
        // 24 hours after the announcement (the check a few minutes early
        // still counts, so the cycle does not slip).
        step(&mut state, &problem, hours(24), &mut ch);
        assert_eq!(ch.desktop.len(), 1);
        assert_eq!(ch.hook.len(), 1);
        step(&mut state, &problem, hours(25), &mut ch);
        assert!(ch.desktop.is_empty());
        let early = hours(48) - Duration::minutes(10);
        step(&mut state, &problem, early, &mut ch);
        assert_eq!(ch.desktop.len(), 1, "due within the margin");
    }

    #[test]
    fn a_changed_error_is_announced_at_once() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], true);
        step(&mut state, &[failing("disk full")], hours(0), &mut ch);
        step(&mut state, &[failing("disk full")], hours(1), &mut ch);
        assert!(ch.desktop.is_empty());
        step(&mut state, &[failing("wrong password")], hours(2), &mut ch);
        assert_eq!(ch.desktop, [(1000, Event::Failing)]);
        assert_eq!(ch.hook, [Event::Failing]);
        assert!(ch.bodies[0].contains("wrong password"));
        // The 24 hours now count from the new announcement.
        step(&mut state, &[failing("wrong password")], hours(24), &mut ch);
        assert!(ch.desktop.is_empty());
        step(&mut state, &[failing("wrong password")], hours(26), &mut ch);
        assert_eq!(ch.desktop.len(), 1);
    }

    #[test]
    fn a_different_kind_is_announced_at_once_and_alongside() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], false);
        step(&mut state, &[failing("x")], hours(0), &mut ch);
        step(&mut state, &[failing("x"), damaged(2)], hours(1), &mut ch);
        assert_eq!(ch.desktop, [(1000, Event::Damaged)]);
        assert_eq!(state.active.len(), 2);
    }

    #[test]
    fn recovery_is_announced_once_after_a_notification() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], true);
        step(&mut state, &[failing("x")], hours(0), &mut ch);
        step(&mut state, &[], hours(3), &mut ch);
        assert_eq!(ch.desktop, [(1000, Event::Recovered)]);
        assert_eq!(ch.hook, [Event::Recovered]);
        assert!(state.active.is_empty() && state.recovery.is_none());
        step(&mut state, &[], hours(4), &mut ch);
        assert!(ch.desktop.is_empty() && ch.hook.is_empty());
    }

    #[test]
    fn no_recovery_message_when_nobody_was_told() {
        let mut state = NotifyState::default();
        // Desktop channel off, no command: the problem is recorded only.
        let mut ch = Fake::new(&[], false);
        step(&mut state, &[failing("x")], hours(0), &mut ch);
        assert_eq!(state.active.len(), 1);
        step(&mut state, &[], hours(2), &mut ch);
        assert!(ch.desktop.is_empty() && ch.hook.is_empty());
        assert!(state.active.is_empty() && state.recovery.is_none());
        // Nothing was ever recorded: fine from the start stays silent.
        step(&mut state, &[], hours(3), &mut ch);
        assert!(state.recovery.is_none());
    }

    #[test]
    fn recovery_waits_for_a_user_who_is_not_logged_in() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], false);
        step(&mut state, &[failing("x")], hours(0), &mut ch);
        ch.sessions.clear();
        step(&mut state, &[], hours(2), &mut ch);
        assert!(ch.desktop.is_empty());
        assert!(state.recovery.is_some());
        // Bob was never told, alice comes back.
        ch.login(1001, "bob");
        step(&mut state, &[], hours(10), &mut ch);
        assert!(ch.desktop.is_empty());
        ch.login(1000, "alice");
        step(&mut state, &[], hours(11), &mut ch);
        assert_eq!(ch.desktop, [(1000, Event::Recovered)]);
        assert!(state.recovery.is_none());
    }

    #[test]
    fn an_undelivered_recovery_is_dropped_after_three_days() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], false);
        step(&mut state, &[failing("x")], hours(0), &mut ch);
        ch.sessions.clear();
        step(&mut state, &[], hours(1), &mut ch);
        step(&mut state, &[], hours(1 + 24 * 3 + 1), &mut ch);
        assert!(state.recovery.is_none());
    }

    #[test]
    fn a_new_problem_cancels_the_pending_recovery() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], false);
        step(&mut state, &[failing("x")], hours(0), &mut ch);
        ch.sessions.clear();
        step(&mut state, &[], hours(1), &mut ch);
        assert!(state.recovery.is_some());
        step(&mut state, &[failing("y")], hours(2), &mut ch);
        assert!(state.recovery.is_none());
    }

    #[test]
    fn incomplete_repeats_weekly() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], false);
        let problem = [incomplete("full system backup skipped")];
        step(&mut state, &problem, hours(0), &mut ch);
        assert_eq!(ch.desktop, [(1000, Event::Incomplete)]);
        assert!(ch.bodies[0].contains("full system backup skipped"));
        step(&mut state, &problem, hours(24), &mut ch);
        step(&mut state, &problem, hours(24 * 6), &mut ch);
        assert!(ch.desktop.is_empty());
        step(&mut state, &problem, hours(24 * 7), &mut ch);
        assert_eq!(ch.desktop.len(), 1);
    }

    #[test]
    fn overdue_repeats_daily_but_weekly_while_automatic_backups_are_off() {
        let mut ch = Fake::new(&[(1000, "alice")], false);
        let mut on = NotifyState::default();
        step(&mut on, &[overdue(false, Some(40))], hours(0), &mut ch);
        assert_eq!(ch.desktop.len(), 1);
        step(&mut on, &[overdue(false, Some(64))], hours(24), &mut ch);
        assert_eq!(ch.desktop.len(), 1);
        assert!(ch.bodies[0].contains("2 days"), "{:?}", ch.bodies);

        let mut off = NotifyState::default();
        step(&mut off, &[overdue(true, Some(300))], hours(0), &mut ch);
        assert_eq!(ch.desktop.len(), 1);
        assert!(ch.bodies[0].contains("switched off"));
        step(&mut off, &[overdue(true, Some(324))], hours(24), &mut ch);
        step(
            &mut off,
            &[overdue(true, Some(460))],
            hours(24 * 6),
            &mut ch,
        );
        assert!(ch.desktop.is_empty());
        step(
            &mut off,
            &[overdue(true, Some(468))],
            hours(24 * 7),
            &mut ch,
        );
        assert_eq!(ch.desktop.len(), 1);

        // Switching the timer off (or on) is a changed condition.
        let mut switched = NotifyState::default();
        step(
            &mut switched,
            &[overdue(false, Some(40))],
            hours(0),
            &mut ch,
        );
        step(&mut switched, &[overdue(true, Some(41))], hours(1), &mut ch);
        assert_eq!(ch.desktop.len(), 1);
    }

    #[test]
    fn every_user_gets_the_message_once_even_when_logging_in_later() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], false);
        step(&mut state, &[failing("x")], hours(0), &mut ch);
        assert_eq!(ch.desktop, [(1000, Event::Failing)]);
        // Bob logs in the next morning: he is told, alice is not told again.
        ch.login(1001, "bob");
        step(&mut state, &[failing("x")], hours(5), &mut ch);
        assert_eq!(ch.desktop, [(1001, Event::Failing)]);
        step(&mut state, &[failing("x")], hours(6), &mut ch);
        assert!(ch.desktop.is_empty());
        // The repeat reaches both.
        step(&mut state, &[failing("x")], hours(24), &mut ch);
        assert_eq!(ch.desktop.len(), 2);
    }

    #[test]
    fn a_missing_desktop_is_not_an_error_and_is_tried_again() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], true);
        ch.desktop_unavailable = true;
        let log = step(&mut state, &[failing("x")], hours(0), &mut ch);
        assert!(log.iter().any(|l| l.contains("no desktop notification")));
        // The hook is independent of the desktop.
        assert_eq!(ch.hook, [Event::Failing]);
        ch.desktop_unavailable = false;
        step(&mut state, &[failing("x")], hours(1), &mut ch);
        assert_eq!(ch.desktop, [(1000, Event::Failing)]);
        assert!(ch.hook.is_empty(), "the hook already has this one");
    }

    #[test]
    fn a_failing_hook_is_retried_and_does_not_stop_the_desktop() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], true);
        ch.command_fails = true;
        let log = step(&mut state, &[failing("x")], hours(0), &mut ch);
        assert_eq!(ch.desktop, [(1000, Event::Failing)]);
        assert!(ch.hook.is_empty());
        assert!(log.iter().any(|l| l.contains("command failed")));
        // Still failing at the next check; the desktop is not told twice.
        step(&mut state, &[failing("x")], hours(1), &mut ch);
        assert!(ch.desktop.is_empty() && ch.hook.is_empty());
        ch.command_fails = false;
        step(&mut state, &[failing("x")], hours(2), &mut ch);
        assert_eq!(ch.hook, [Event::Failing]);
        assert!(ch.desktop.is_empty());
        step(&mut state, &[failing("x")], hours(3), &mut ch);
        assert!(ch.hook.is_empty());
    }

    #[test]
    fn a_failure_stays_open_while_the_last_run_gave_no_result() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], false);
        step(&mut state, &[failing("x")], hours(0), &mut ch);
        // The last run ended with "server unreachable": another problem is
        // known, the failure is neither confirmed nor over.
        ch.clear();
        apply(&mut state, &[damaged(1)], true, hours(1), Lang::En, &mut ch);
        assert_eq!(ch.desktop, [(1000, Event::Damaged)]);
        assert!(state.active.contains_key(&Event::Failing));
        assert!(state.recovery.is_none());
        // Back to normal runs: the failure is gone for real.
        ch.clear();
        apply(&mut state, &[], false, hours(2), Lang::En, &mut ch);
        assert_eq!(ch.desktop, [(1000, Event::Recovered)]);
    }

    #[test]
    fn the_record_survives_a_restart() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], true);
        step(&mut state, &[failing("x"), damaged(3)], hours(0), &mut ch);
        let text = serde_json::to_string(&state).unwrap();
        let mut again: NotifyState = serde_json::from_str(&text).unwrap();
        assert_eq!(again, state);
        step(&mut again, &[failing("x"), damaged(3)], hours(1), &mut ch);
        assert!(ch.desktop.is_empty() && ch.hook.is_empty());
        // Old or partial files still load.
        let empty: NotifyState = serde_json::from_str("{}").unwrap();
        assert!(empty.active.is_empty());
    }

    // --- what counts as a problem -------------------------------------------

    fn run_with(state: RunState, at: DateTime<Utc>) -> Run {
        let mut run = Run::new(Trigger::Timer, at - Duration::minutes(5));
        run.finish(state, at);
        run
    }

    fn status(
        health: Health,
        last_run: Option<Run>,
        last_success: Option<Run>,
        damaged: &[&str],
    ) -> Status {
        Status {
            machine: status::Machine {
                hostname: "desktop".into(),
                backup_id: "desktop".into(),
                os: None,
            },
            pbs: status::PbsStatus {
                server: "pbs.example.com:8007".into(),
                datastore: "backups".into(),
                namespace: None,
                reachable: Some(true),
                checked_at: None,
                wake_on_lan: false,
                credentials_stored: Some(true),
            },
            schedule: status::Schedule {
                enabled: true,
                calendar: String::new(),
                interval_hours: 24,
                next_run: None,
            },
            current_run: None,
            last_run,
            last_success,
            health,
            overdue_after_hours: 36,
            encryption: status::Encryption {
                enabled: true,
                key_fingerprint: None,
                key_exported: true,
                key_present: Some(true),
            },
            server_verify_failed: damaged.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn checked(e: Evaluation) -> (Vec<Condition>, bool) {
        match e {
            Evaluation::Checked {
                conditions,
                settled,
            } => (conditions, settled),
            Evaluation::Skip(why) => panic!("skipped: {why}"),
        }
    }

    #[test]
    fn not_configured_and_running_report_nothing() {
        let s = status(Health::Unconfigured, None, None, &[]);
        assert!(matches!(
            evaluate(&s, true, None, t0()),
            Evaluation::Skip(_)
        ));
        let ok = run_with(RunState::Succeeded, hours(-2));
        let s = status(Health::Running, Some(ok.clone()), Some(ok), &["host/a/1"]);
        assert!(matches!(
            evaluate(&s, true, None, t0()),
            Evaluation::Skip(_)
        ));
    }

    #[test]
    fn a_healthy_computer_has_no_problems() {
        let ok = run_with(RunState::Succeeded, hours(-2));
        let s = status(Health::Ok, Some(ok.clone()), Some(ok), &[]);
        assert_eq!(checked(evaluate(&s, true, None, t0())), (vec![], true));
    }

    #[test]
    fn a_failed_run_is_a_problem_with_its_error() {
        let ok = run_with(RunState::Succeeded, hours(-30));
        let mut bad = run_with(RunState::Failed, hours(-1));
        bad.error = Some(RunError {
            kind: ErrorKind::CredentialsUnreadable,
            message: "stored secrets unreadable".into(),
        });
        let s = status(Health::Failing, Some(bad.clone()), Some(ok), &[]);
        let (list, settled) = checked(evaluate(&s, true, None, t0()));
        assert!(settled);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, Event::Failing);
        assert_eq!(list[0].error.as_deref(), Some("stored secrets unreadable"));
        // Another error text is another problem.
        bad.error.as_mut().unwrap().message = "no space".into();
        let s2 = status(Health::Failing, Some(bad), None, &[]);
        // Also when there was never a success (the dashboard says "never").
        let (list2, _) = checked(evaluate(&s2, true, None, t0()));
        assert_eq!(list2[0].kind, Event::Failing);
        assert_ne!(list2[0].key, list[0].key);
    }

    #[test]
    fn overdue_covers_old_successes_and_a_setup_that_never_succeeded() {
        let old = run_with(RunState::Succeeded, hours(-50));
        let unreachable = run_with(RunState::PbsUnreachable, hours(-1));
        let s = status(Health::Overdue, Some(unreachable), Some(old), &[]);
        let (list, settled) = checked(evaluate(&s, true, None, t0()));
        assert!(!settled, "the last run ended without a result");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, Event::Overdue);
        assert_eq!(list[0].age_hours, Some(50));
        assert!(!list[0].timer_off);
        // The timer is off: another key and a note in the condition.
        let (off, _) = checked(evaluate(&s, false, None, t0()));
        assert!(off[0].timer_off);
        assert_ne!(off[0].key, list[0].key);

        // Never succeeded: a young setup gets time, an old one is overdue.
        let never = status(Health::Never, None, None, &[]);
        let young = Some(hours(-10));
        let old_setup = Some(hours(-40));
        assert!(checked(evaluate(&never, true, young, t0())).0.is_empty());
        assert!(checked(evaluate(&never, true, None, t0())).0.is_empty());
        let (list, _) = checked(evaluate(&never, true, old_setup, t0()));
        assert_eq!(list[0].kind, Event::Overdue);
        assert_eq!(list[0].age_hours, None);
    }

    #[test]
    fn incomplete_and_damaged_come_from_the_status() {
        let mut partial = run_with(RunState::Succeeded, hours(-2));
        partial.incomplete = vec!["optional source /root missing".into()];
        let s = status(
            Health::Incomplete,
            Some(partial.clone()),
            Some(partial),
            &[
                "host/desktop/2026-11-01T10:00:00Z",
                "host/desktop/2026-10-01T10:00:00Z",
            ],
        );
        let (list, settled) = checked(evaluate(&s, true, None, t0()));
        assert!(settled);
        assert_eq!(
            list.iter().map(|c| c.kind).collect::<Vec<_>>(),
            [Event::Incomplete, Event::Damaged]
        );
        assert_eq!(list[0].reasons, ["optional source /root missing"]);
        assert_eq!(list[1].damaged, 2);
        // The order in which the server lists the snapshots does not matter.
        let mut reversed = s;
        reversed.server_verify_failed.reverse();
        let (list2, _) = checked(evaluate(&reversed, true, None, t0()));
        assert_eq!(list2[1].key, list[1].key);
    }

    // --- words ------------------------------------------------------------------

    #[test]
    fn a_broken_configuration_is_a_failure_and_keeps_the_hook() {
        let c = config_problem("invalid configuration /etc/nestegg/config.toml: unknown field");
        assert_eq!(c.kind, Event::Failing);
        let m = compose(Lang::En, c.kind, Some(&c));
        assert!(m.body.contains("unknown field"), "{}", m.body);
        // The error text is part of the key: fixing it, or breaking it
        // differently, is a change.
        assert_ne!(c.key, config_problem("another error").key);

        let broken = "bogus = 1\n[pbs]\n[notify]\ndesktop = false\ncommand = [\"/bin/hook\"]\n";
        assert!(Config::parse(broken).is_err());
        let notify = lenient_notify_config(broken).unwrap();
        assert!(!notify.desktop);
        assert_eq!(notify.command, Some(vec!["/bin/hook".to_string()]));
        assert_eq!(lenient_notify_config("bogus = 1\n"), None);
        assert_eq!(lenient_notify_config("this is not toml"), None);
        assert_eq!(lenient_notify_config("[notify]\nmail = 1\n"), None);
    }

    #[test]
    fn the_language_follows_the_system_locale() {
        assert_eq!(lang_from_value("de_DE.UTF-8"), Lang::De);
        assert_eq!(lang_from_value("de"), Lang::De);
        assert_eq!(lang_from_value("de_AT"), Lang::De);
        assert_eq!(lang_from_value("en_US.UTF-8"), Lang::En);
        assert_eq!(lang_from_value("C.UTF-8"), Lang::En);
        assert_eq!(lang_from_value("fr_FR"), Lang::En);
        assert_eq!(lang_from_value(""), Lang::En);

        assert_eq!(
            parse_locale_file("LANG=de_DE.UTF-8\n").as_deref(),
            Some("de_DE.UTF-8")
        );
        assert_eq!(
            parse_locale_file("# comment\nLANG=\"de_DE.UTF-8\"\nLANGUAGE=de\n").as_deref(),
            Some("de_DE.UTF-8")
        );
        assert_eq!(
            parse_locale_file("LANG=en_US.UTF-8\nLC_MESSAGES=de_DE.UTF-8\n").as_deref(),
            Some("de_DE.UTF-8")
        );
        assert_eq!(parse_locale_file("LANGUAGE=de\n"), None);
        assert_eq!(parse_locale_file("LANG=\n"), None);
        assert_eq!(parse_locale_file("#LANG=de_DE\n"), None);
    }

    #[test]
    fn every_problem_message_points_to_the_ui_in_both_languages() {
        let conditions = [
            failing("The server said no"),
            overdue(false, Some(100)),
            overdue(false, None),
            overdue(true, Some(300)),
            overdue(true, None),
            incomplete("a source is missing"),
            damaged(1),
            damaged(3),
        ];
        for lang in [Lang::En, Lang::De] {
            for c in &conditions {
                let m = compose(lang, c.kind, Some(c));
                assert!(!m.title.is_empty() && !m.body.is_empty());
                assert!(m.body.contains("nestegg ui"), "{lang:?} {c:?}: {}", m.body);
                assert!(m.body.ends_with('.'), "{}", m.body);
            }
            for event in [Event::Recovered, Event::Test] {
                let m = compose(lang, event, None);
                assert!(m.body.contains("nestegg ui"), "{}", m.body);
            }
        }
    }

    #[test]
    fn the_texts_say_what_happened() {
        let en = compose(Lang::En, Event::Failing, Some(&failing("disk full")));
        assert_eq!(en.title, "Backup failed");
        assert!(en.body.contains("disk full"));
        let de = compose(Lang::De, Event::Failing, Some(&failing("disk full")));
        assert_eq!(de.title, "Datensicherung fehlgeschlagen");
        assert!(de.body.contains("disk full"));

        let m = compose(Lang::De, Event::Overdue, Some(&overdue(false, Some(100))));
        assert!(m.body.contains("4 Tage"), "{}", m.body);
        let m = compose(Lang::De, Event::Overdue, Some(&overdue(false, Some(40))));
        assert!(m.body.contains("40 Stunden"), "{}", m.body);
        let m = compose(Lang::En, Event::Overdue, Some(&overdue(false, Some(100))));
        assert!(m.body.contains("4 days"), "{}", m.body);
        let m = compose(Lang::En, Event::Overdue, Some(&overdue(true, Some(300))));
        assert_eq!(m.title, "Automatic backups are off");
        assert!(m.body.contains("12 days"));
        let m = compose(Lang::De, Event::Overdue, Some(&overdue(true, None)));
        assert!(m.body.contains("ausgeschaltet"));
        let m = compose(Lang::En, Event::Overdue, Some(&overdue(false, None)));
        assert_eq!(m.title, "No backup yet");

        let one = compose(Lang::En, Event::Damaged, Some(&damaged(1)));
        assert!(
            one.body
                .contains("1 saved backup of this computer is damaged")
        );
        let many = compose(Lang::En, Event::Damaged, Some(&damaged(3)));
        assert!(
            many.body
                .contains("3 saved backups of this computer are damaged")
        );
        let de = compose(Lang::De, Event::Damaged, Some(&damaged(2)));
        assert!(de.body.contains("2 gespeicherte Sicherungen") && de.body.contains("sind"));

        // A long, multi-line error is cut to its first line.
        let long = format!("first line\nsecond line\n{}", "x".repeat(1000));
        let m = compose(Lang::En, Event::Failing, Some(&failing(&long)));
        assert!(m.body.contains("first line") && !m.body.contains("second line"));
        let m = compose(Lang::En, Event::Failing, Some(&failing(&"y".repeat(1000))));
        assert!(m.body.len() < 500);
    }

    #[test]
    fn only_serious_events_are_urgent() {
        for (event, urgent) in [
            (Event::Failing, true),
            (Event::Overdue, true),
            (Event::Damaged, true),
            (Event::Incomplete, false),
            (Event::Recovered, false),
            (Event::Test, false),
        ] {
            assert_eq!(event.urgent(), urgent, "{event:?}");
        }
        // The names the hook sees.
        assert_eq!(Event::Failing.as_str(), "failing");
        assert_eq!(
            serde_json::to_string(&Event::Recovered).unwrap(),
            "\"recovered\""
        );
    }

    // --- fixes after review ---------------------------------------------------

    #[test]
    fn an_error_text_with_changing_numbers_is_the_same_problem() {
        let mut bad = run_with(RunState::Failed, hours(-1));
        bad.error = Some(RunError {
            kind: ErrorKind::ClientFailed,
            message: "upload failed after 132 s at 4711 MiB".into(),
        });
        let first = status(Health::Failing, Some(bad.clone()), None, &[]);
        bad.error.as_mut().unwrap().message = "upload failed after 97 s at 12 MiB".into();
        let second = status(Health::Failing, Some(bad.clone()), None, &[]);
        let key = |s: &Status| checked(evaluate(s, true, None, t0())).0[0].key.clone();
        assert_eq!(key(&first), key(&second));
        // The message itself keeps the numbers.
        let (list, _) = checked(evaluate(&second, true, None, t0()));
        assert_eq!(
            list[0].error.as_deref(),
            Some("upload failed after 97 s at 12 MiB")
        );
        bad.error.as_mut().unwrap().message = "authentication failed".into();
        let other = status(Health::Failing, Some(bad), None, &[]);
        assert_ne!(key(&first), key(&other));
    }

    #[test]
    fn a_clock_that_was_ahead_does_not_silence_the_repeats() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], true);
        let problem = [failing("disk full")];
        // Announced while the clock was a year ahead.
        step(&mut state, &problem, hours(24 * 365), &mut ch);
        assert_eq!(ch.hook, [Event::Failing]);
        // Clock corrected: nothing now, but a day later the repeat comes.
        step(&mut state, &problem, hours(0), &mut ch);
        assert!(ch.hook.is_empty());
        step(&mut state, &problem, hours(25), &mut ch);
        assert_eq!(ch.hook, [Event::Failing]);
    }

    #[test]
    fn the_desktop_body_is_shown_literally() {
        assert_eq!(
            escape_markup(r#"upload <a href="http://x.example">click</a> & <b>x</b>"#),
            "upload &lt;a href=\"http://x.example\"&gt;click&lt;/a&gt; &amp; &lt;b&gt;x&lt;/b&gt;"
        );
        assert_eq!(escape_markup("plain text"), "plain text");
    }

    #[test]
    fn an_error_that_ends_with_a_full_stop_gets_no_second_one() {
        let message = compose(Lang::En, Event::Failing, Some(&failing("no space left.")));
        assert!(
            message.body.contains("no space left. Details"),
            "{}",
            message.body
        );
        assert!(!message.body.contains(".."), "{}", message.body);
    }

    #[test]
    fn uid_min_is_read_from_login_defs() {
        let text = "# comment\nUID_MAX\t\t60000\nUID_MIN\t\t\t 500\nSYS_UID_MIN 100\n";
        assert_eq!(parse_uid_min(text), Some(500));
        assert_eq!(parse_uid_min("#UID_MIN 1\n"), None);
        assert_eq!(parse_uid_min("UID_MIN x\n"), None);
        // The systemd-homed range counts as regular, `nobody` does not.
        let range = DEFAULT_UID_MIN..=REGULAR_UID_MAX;
        assert!(range.contains(&60100) && !range.contains(&65534));
    }

    #[test]
    fn an_unreadable_state_is_reported_as_a_failure() {
        let c = state_problem("cannot parse state.json: expected value");
        assert_eq!(c.kind, Event::Failing);
        let body = compose(Lang::De, Event::Failing, Some(&c)).body;
        assert!(body.contains("cannot parse state.json"), "{body}");
    }

    // --- sessions and processes ---------------------------------------------------

    #[test]
    fn sessions_are_regular_users_with_a_bus_socket() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let mut keep = Vec::new();
        for uid in ["1000", "1001", "999", "60100", "61184", "65534", "0"] {
            let user_dir = dir.path().join(uid);
            fs::create_dir(&user_dir).unwrap();
            keep.push(UnixListener::bind(user_dir.join("bus")).unwrap());
        }
        // 1002: runtime directory but no bus; 1003: "bus" is a plain file;
        // 1004: no account; "foo": not a uid.
        fs::create_dir(dir.path().join("1002")).unwrap();
        fs::create_dir(dir.path().join("1003")).unwrap();
        fs::write(dir.path().join("1003/bus"), "").unwrap();
        fs::create_dir(dir.path().join("1004")).unwrap();
        keep.push(UnixListener::bind(dir.path().join("1004/bus")).unwrap());
        fs::create_dir(dir.path().join("foo")).unwrap();

        let names = |uid: u32| match uid {
            1000 => Some("alice".to_string()),
            1001 => Some("bob".to_string()),
            60100 => Some("carol".to_string()),
            999 | 61184 | 65534 | 0 | 1002 | 1003 => Some("system".to_string()),
            _ => None,
        };
        let found = find_sessions(dir.path(), DEFAULT_UID_MIN..=REGULAR_UID_MAX, names);
        assert_eq!(
            found,
            [
                Session {
                    uid: 1000,
                    name: "alice".into()
                },
                Session {
                    uid: 1001,
                    name: "bob".into()
                },
                Session {
                    uid: 60100,
                    name: "carol".into()
                }
            ]
        );
        assert!(
            find_sessions(
                &dir.path().join("missing"),
                DEFAULT_UID_MIN..=REGULAR_UID_MAX,
                |_| None
            )
            .is_empty()
        );
    }

    #[test]
    fn the_hook_gets_the_event_in_its_environment_and_the_message_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let config = NotifyConfig {
            desktop: true,
            command: Some(
                [
                    "sh",
                    "-c",
                    "{ echo \"$NESTEGG_EVENT|$NESTEGG_TITLE|$NESTEGG_HOSTNAME|$NESTEGG_LAST_SUCCESS|$NESTEGG_MESSAGE\"; cat; } > \"$1\"",
                    "hook",
                    out.to_str().unwrap(),
                ]
                .map(String::from)
                .to_vec(),
            ),
        };
        let context = Context {
            lang: Lang::En,
            hostname: "desktop".into(),
            last_success: Some(t0()),
        };
        let mut channels = SystemChannels {
            config: &config,
            context: &context,
            runtime_root: dir.path().to_path_buf(),
        };
        let message = compose(Lang::En, Event::Failing, Some(&failing("disk full")));
        channels.send_command(&message).unwrap();
        let text = fs::read_to_string(&out).unwrap();
        let mut lines = text.lines();
        let first = lines.next().unwrap();
        assert!(
            first.starts_with("failing|Backup failed|desktop|2026-11-09T08:00:00Z|"),
            "{first}"
        );
        let stdin = lines.collect::<Vec<_>>().join("\n");
        assert_eq!(stdin, message.body);
    }

    #[test]
    fn a_hook_that_fails_or_hangs_is_an_error_not_a_hang() {
        let context = Context {
            lang: Lang::En,
            hostname: "desktop".into(),
            last_success: None,
        };
        let message = compose(Lang::En, Event::Test, None);
        let fails = NotifyConfig {
            desktop: true,
            command: Some(
                ["sh", "-c", "echo broken >&2; exit 3"]
                    .map(String::from)
                    .to_vec(),
            ),
        };
        let mut channels = SystemChannels {
            config: &fails,
            context: &context,
            runtime_root: PathBuf::from("/nonexistent"),
        };
        let e = channels.send_command(&message).unwrap_err().to_string();
        assert!(e.contains("broken") && e.contains('3'), "{e}");
        let missing = NotifyConfig {
            desktop: true,
            command: Some(vec!["/nonexistent/hook".into()]),
        };
        channels.config = &missing;
        assert!(channels.send_command(&message).is_err());

        // The timeout kills the whole process group, including children the
        // program started, and returns promptly.
        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & sleep 30"]);
        let outcome = run_with_timeout(cmd, b"", StdDuration::from_millis(300)).unwrap();
        assert!(outcome.status.is_none());
        assert!(started.elapsed() < StdDuration::from_secs(10));

        // A program that leaves a background process holding the pipe does
        // not keep the check waiting.
        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo done; (sleep 20 &) ; exit 0"]);
        let outcome = run_with_timeout(cmd, b"", StdDuration::from_secs(5)).unwrap();
        assert!(outcome.status.unwrap().success());
        assert_eq!(outcome.output, "done");
        assert!(started.elapsed() < StdDuration::from_secs(5));
    }

    #[test]
    fn the_desktop_message_reaches_notify_send_with_the_session_bus_of_the_user() {
        // A stub notify-send records its arguments and environment. The test
        // runs as the current (unprivileged) user, who stands in for the
        // session user, so the identity switch is a no-op here; the
        // container test (tests/e2e/systemd.sh) covers the switch itself.
        use std::os::unix::fs::PermissionsExt;
        let uid = nix::unistd::geteuid().as_raw();
        if uid == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("record");
        let stub = dir.path().join("notify-send");
        fs::write(
            &stub,
            format!(
                "#!/bin/sh\n{{ echo \"bus=$DBUS_SESSION_BUS_ADDRESS\"; echo \"xdg=$XDG_RUNTIME_DIR\"; \
                 echo \"home=$HOME\"; for a in \"$@\"; do echo \"arg=$a\"; done; }} > {}\n",
                record.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();

        let config = NotifyConfig::default();
        let context = Context {
            lang: Lang::En,
            hostname: "desktop".into(),
            last_success: None,
        };
        let root = dir.path().join("run-user");
        let channels = SystemChannels {
            config: &config,
            context: &context,
            runtime_root: root.clone(),
        };
        let session = Session {
            uid,
            name: "me".into(),
        };
        for (event, urgency) in [(Event::Failing, "critical"), (Event::Incomplete, "normal")] {
            let message = compose(Lang::En, event, Some(&incomplete("something is missing")));
            let cmd = channels.desktop_command(&stub, &session, &message).unwrap();
            let outcome = run_with_timeout(cmd, b"", DESKTOP_TIMEOUT).unwrap();
            assert!(outcome.status.unwrap().success(), "{}", outcome.output);
            let recorded = fs::read_to_string(&record).unwrap();
            assert!(recorded.contains(&format!("bus=unix:path={}/{uid}/bus", root.display())));
            assert!(recorded.contains(&format!("xdg={}/{uid}", root.display())));
            assert!(recorded.contains("arg=--app-name=nestegg"));
            assert!(recorded.contains(&format!("arg=--urgency={urgency}")));
            assert!(recorded.contains(&format!("arg={}", message.title)));
            assert!(recorded.contains(&format!("arg={}", message.body)));
        }
    }
}
