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
/// Hard upper bound on the length of any message body in bytes before it is
/// handed to `notify-send`, the hook environment or the hook's stdin
/// (decision 0022). Far below the kernel's argument and environment limits
/// (128 KiB each) and the pipe buffer (64 KiB), so a message can never make
/// `notify-send`/the hook fail with "Argument list too long" or block the
/// check on a full stdin pipe.
const MAX_BODY_BYTES: usize = 4096;
/// At most this many exclude patterns are named in a `settings` message; the
/// rest are summarised as "and N more" (decision 0022).
const MAX_LISTED_PATTERNS: usize = 5;
/// Each named pattern in a message is shortened to this many bytes.
const MAX_PATTERN_BYTES: usize = 80;
/// Most settings announcements kept at once (across the inbox and the record),
/// a brake on a flood from switching a setting back and forth (decision 0022).
const MAX_SETTINGS: usize = 50;
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
    /// The backup timer is switched off while nestegg is configured (its own
    /// condition, decision 0022). Independent of the overdue threshold. Stored
    /// and shown as `automatic_off` everywhere (the hook env, `notify.json`);
    /// the alias keeps records written by the first version readable.
    #[serde(rename = "automatic_off", alias = "automaticoff")]
    AutomaticOff,
    Incomplete,
    Damaged,
    Recovered,
    /// A group member lowered the protection through the API (decision 0022).
    Settings,
    Test,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Event::Failing => "failing",
            Event::Overdue => "overdue",
            Event::AutomaticOff => "automatic_off",
            Event::Incomplete => "incomplete",
            Event::Damaged => "damaged",
            Event::Recovered => "recovered",
            Event::Settings => "settings",
            Event::Test => "test",
        }
    }

    /// Critical desktop notifications stay on screen until dismissed.
    pub fn urgent(self) -> bool {
        matches!(
            self,
            Event::Failing | Event::Overdue | Event::AutomaticOff | Event::Damaged
        )
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
            // Switched off on purpose: a weekly reminder, not a daily nag.
            Event::Incomplete | Event::AutomaticOff => Duration::days(7),
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
    }
    // The backup timer is switched off while nestegg is configured: its own
    // condition, reported at once and repeated weekly, independent of the
    // overdue threshold and of whether the last run failed (decision 0022). It
    // replaces the "overdue while the timer is off" variant; the text still
    // says how old the last successful backup is.
    if !timer_enabled {
        let mut c = Condition::new(Event::AutomaticOff, "off".to_string());
        c.age_hours = age_hours;
        c.timer_off = true;
        conditions.push(c);
    }
    // Overdue and incomplete are only judged when the last run did not fail (a
    // failure is the louder message), and overdue only while the timer is on; a
    // switched-off timer is covered by `automatic_off` above.
    if failed.is_none() {
        let overdue_limit = Duration::hours(i64::from(status.overdue_after_hours));
        let never_and_old = status.health == Health::Never
            && unbacked_since.is_some_and(|since| now - since > overdue_limit);
        if timer_enabled && (status.health == Health::Overdue || never_and_old) {
            let mut c = Condition::new(Event::Overdue, "timer on".to_string());
            c.age_hours = age_hours;
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

/// How much of a message a recipient may see (decision 0022). Members of
/// group `nestegg` and root get the full text (error messages, paths, the
/// user who changed a setting); everyone else gets a short version that only
/// names the kind of problem and asks them to tell whoever looks after the
/// computer. The hook always gets the full text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    Full,
    Generic,
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

/// Truncate `text` to at most `max` bytes on a character boundary, adding an
/// ellipsis when it was cut.
fn truncate_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(3);
    let mut end = keep.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// Control characters and invisible Unicode format characters (bidirectional
/// overrides, zero-width) become `?`, so pattern text or an error message typed
/// by a user cannot hide or reorder a notification (decision 0022). Kept in
/// step with the same filter in `crate::rescue`; duplicated so this module does
/// not depend on it.
fn visible(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control()
                || matches!(c,
                    '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'..='\u{2069}' | '\u{feff}')
            {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// Finish a message body: make it printable and hold it under the hard byte
/// cap (decision 0022), so no event can produce a message that breaks
/// `notify-send`, the environment or the hook's stdin.
fn cap_body(body: String) -> String {
    truncate_bytes(&visible(&body), MAX_BODY_BYTES)
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

/// The short "tell the admin" sentence for the generic (non-member) version.
fn tell_admin(lang: Lang) -> String {
    tr(
        lang,
        "Please tell whoever looks after this computer.",
        "Bitte sag der Person Bescheid, die sich um diesen Computer kümmert.",
    )
}

/// The generic body for a problem event (decision 0022): the kind of problem
/// without any detail, plus the "tell the admin" sentence. Events without
/// private detail (`damaged`, `recovered`, `test`) keep their full body and
/// return `None` here.
fn generic_body(lang: Lang, event: Event) -> Option<String> {
    let kind = match event {
        Event::Failing => tr(
            lang,
            "The last backup of this computer failed.",
            "Die letzte Datensicherung dieses Computers ist fehlgeschlagen.",
        ),
        Event::Overdue => tr(
            lang,
            "The backups of this computer are overdue.",
            "Die Datensicherung dieses Computers ist überfällig.",
        ),
        Event::AutomaticOff => tr(
            lang,
            "Automatic backups of this computer are switched off.",
            "Die automatischen Sicherungen dieses Computers sind ausgeschaltet.",
        ),
        Event::Incomplete => tr(
            lang,
            "The last backup of this computer is incomplete.",
            "Die letzte Datensicherung dieses Computers ist unvollständig.",
        ),
        Event::Settings => tr(
            lang,
            "The backup protection settings of this computer were changed.",
            "Die Schutzeinstellungen der Datensicherung dieses Computers wurden geändert.",
        ),
        Event::Damaged | Event::Recovered | Event::Test => return None,
    };
    Some(format!("{kind} {}", tell_admin(lang)))
}

/// Words for a message. `condition` is needed for the problem events;
/// `detail` chooses the full or the short "tell the admin" version
/// (decision 0022). Use [`compose_settings`] for [`Event::Settings`].
pub fn compose(lang: Lang, event: Event, condition: Option<&Condition>, detail: Detail) -> Message {
    let fallback = Condition::new(event, String::new());
    let c = condition.unwrap_or(&fallback);
    let (title, full_body) = match event {
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
            let (title, first, last) = match &age {
                Some(age) => (
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
                None => (
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
        Event::AutomaticOff => {
            let title = tr(
                lang,
                "Automatic backups are off",
                "Automatische Sicherungen sind aus",
            );
            let last = tr(
                lang,
                "Switch them on again: \"nestegg ui\".",
                "Wieder einschalten: \"nestegg ui\".",
            );
            let first = match c.age_hours.map(|h| span(lang, h)) {
                Some(age) => match lang {
                    Lang::En => format!(
                        "Automatic backups are switched off, and the last successful \
                         backup was {age} ago."
                    ),
                    Lang::De => format!(
                        "Automatische Sicherungen sind ausgeschaltet; die letzte \
                         erfolgreiche Sicherung ist {age} her."
                    ),
                },
                None => tr(
                    lang,
                    "Automatic backups are switched off, and this computer has not been \
                     backed up yet.",
                    "Automatische Sicherungen sind ausgeschaltet, und dieser Computer \
                     wurde noch nie gesichert.",
                ),
            };
            (title, format!("{first} {last}"))
        }
        Event::Settings => (
            // Reached only through the fallback (no condition); the real text
            // comes from compose_settings.
            tr(
                lang,
                "Backup settings changed",
                "Sicherungseinstellungen geändert",
            ),
            generic_body(lang, Event::Settings).unwrap_or_default(),
        ),
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
    let body = match detail {
        Detail::Generic => generic_body(lang, event).unwrap_or(full_body),
        Detail::Full => full_body,
    };
    Message {
        event,
        title,
        body: cap_body(body),
    }
}

/// Words for a `settings` message: who lowered the protection and how
/// (decision 0022). The full version names the user and each change; the
/// generic version only says the settings were changed.
pub fn compose_settings(
    lang: Lang,
    user: &str,
    reductions: &[crate::settings::Reduction],
    detail: Detail,
) -> Message {
    compose_settings_many(lang, &[(user.to_string(), reductions.to_vec())], detail)
}

/// Words for a combined `settings` message (decision 0022): one message that
/// names each group member who lowered the protection and how, so a flurry of
/// changes becomes a single notification per recipient. The generic version
/// only says the settings were changed.
pub fn compose_settings_many(
    lang: Lang,
    groups: &[(String, Vec<crate::settings::Reduction>)],
    detail: Detail,
) -> Message {
    let title = tr(
        lang,
        "Backup settings changed",
        "Sicherungseinstellungen geändert",
    );
    if detail == Detail::Generic || groups.is_empty() {
        return Message {
            event: Event::Settings,
            title,
            body: cap_body(generic_body(lang, Event::Settings).unwrap_or_default()),
        };
    }
    let sentences: Vec<String> = groups
        .iter()
        .map(|(user, reductions)| {
            let user = visible(user);
            let list = reductions
                .iter()
                .map(|r| reduction_phrase(lang, r))
                .collect::<Vec<_>>()
                .join("; ");
            match lang {
                Lang::En => format!(
                    "{user} changed the backup settings of this computer so that it is \
                     protected less: {list}."
                ),
                Lang::De => format!(
                    "{user} hat die Sicherungseinstellungen dieses Computers so geändert, dass \
                     es weniger geschützt ist: {list}."
                ),
            }
        })
        .collect();
    let closing = tr(
        lang,
        "If that was not meant to happen, check it: \"nestegg ui\".",
        "Falls das nicht gewollt war, bitte prüfen: \"nestegg ui\".",
    );
    let body = format!("{} {closing}", sentences.join(" "));
    Message {
        event: Event::Settings,
        title,
        body: cap_body(body),
    }
}

/// Name a few patterns, each shortened, plus "and N more" for the rest, so a
/// long exclude list cannot blow up the message (decision 0022).
fn pattern_list(lang: Lang, patterns: &[String]) -> String {
    let shown: Vec<String> = patterns
        .iter()
        .take(MAX_LISTED_PATTERNS)
        .map(|p| format!("\"{}\"", truncate_bytes(&visible(p), MAX_PATTERN_BYTES)))
        .collect();
    let mut list = shown.join(", ");
    let rest = patterns.len().saturating_sub(MAX_LISTED_PATTERNS);
    if rest > 0 {
        list.push_str(&match lang {
            Lang::En => format!(", and {rest} more"),
            Lang::De => format!(" und {rest} weitere"),
        });
    }
    list
}

/// One phrase for a single protection-reducing change (full version).
fn reduction_phrase(lang: Lang, reduction: &crate::settings::Reduction) -> String {
    use crate::settings::Reduction;
    match reduction {
        Reduction::AutomaticBackupsOff => tr(
            lang,
            "automatic backups were switched off",
            "automatische Sicherungen wurden ausgeschaltet",
        ),
        Reduction::ExcludesAdded { patterns } => {
            let joined = pattern_list(lang, patterns);
            match lang {
                Lang::En => format!("these patterns are no longer backed up: {joined}"),
                Lang::De => format!("diese Muster werden nicht mehr gesichert: {joined}"),
            }
        }
        Reduction::ReincludesRemoved { patterns } => {
            let joined = pattern_list(lang, patterns);
            match lang {
                Lang::En => {
                    format!(
                        "these re-include patterns were removed, so what they kept is excluded again: {joined}"
                    )
                }
                Lang::De => format!(
                    "diese Wieder-Einschluss-Muster wurden entfernt, das von ihnen Bewahrte \
                     wird also wieder ausgeschlossen: {joined}"
                ),
            }
        }
        Reduction::ExcludesReordered => tr(
            lang,
            "the exclude patterns were reordered, so a re-include may no longer take effect",
            "die Ausschlussmuster wurden umsortiert, ein Wieder-Einschluss greift dadurch \
             vielleicht nicht mehr",
        ),
        Reduction::OverdueRaised { from, to } => match lang {
            Lang::En => format!(
                "the time before a stopped backup is reported was raised from {from} to {to} hours"
            ),
            Lang::De => format!(
                "die Zeit bis eine ausbleibende Sicherung gemeldet wird, wurde von {from} auf \
                 {to} Stunden erhöht"
            ),
        },
        Reduction::IntervalRaised { from, to } => match lang {
            Lang::En => {
                format!("backups now run less often, every {to} instead of every {from} hours")
            }
            Lang::De => {
                format!("Sicherungen laufen jetzt seltener, alle {to} statt alle {from} Stunden")
            }
        },
        Reduction::WakeOnLanOff => tr(
            lang,
            "Wake-on-LAN was switched off, so a sleeping backup server is no longer woken",
            "Wake-on-LAN wurde ausgeschaltet, ein schlafender Backup-Server wird also nicht \
             mehr geweckt",
        ),
    }
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

/// A protection-reducing settings change, recorded by the daemon for
/// `nestegg notify` to deliver (decision 0022). Delivered once to each user
/// and to the hook, then kept for a few days so a late login still sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsAnnouncement {
    /// Who made the change (user name).
    pub user: String,
    /// What was lowered.
    pub reductions: Vec<crate::settings::Reduction>,
    #[serde(with = "crate::state::rfc3339")]
    pub at: DateTime<Utc>,
    /// Users (by uid) who already received it.
    #[serde(default)]
    pub users_done: BTreeSet<u32>,
    /// The hook already received it.
    #[serde(default)]
    pub command_done: bool,
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
    /// Protection-reducing settings changes still to be delivered (0022).
    #[serde(default)]
    pub settings: Vec<SettingsAnnouncement>,
}

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

/// A logged-in user with a session bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub uid: u32,
    pub name: String,
    /// Member of group `nestegg` (or root): may see the full message text.
    /// Everyone else gets the short "tell the admin" version (decision 0022).
    pub privileged: bool,
}

impl Session {
    fn detail(&self) -> Detail {
        if self.privileged {
            Detail::Full
        } else {
            Detail::Generic
        }
    }
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

/// What a policy round did, beyond the journal lines.
#[derive(Debug, Default)]
pub struct ApplyOutcome {
    /// A line per action for the journal / `--dry-run`.
    pub log: Vec<String>,
    /// At least one hook delivery failed this round, so `nestegg notify`
    /// should end with a non-zero exit code (decision 0022). The delivery is
    /// retried at the next check.
    pub hook_failed: bool,
}

/// Run the policy for the current problems.
///
/// `carry_failing`: the last run was neither a success nor a failure
/// (server unreachable, cancelled), so an announced `failing` problem stays
/// open instead of being closed.
///
/// `save` persists the record; it is called after each delivery so a check
/// that is killed halfway (unit timeout, reboot) does not redeliver what it
/// already sent (decision 0022).
pub fn apply(
    state: &mut NotifyState,
    conditions: &[Condition],
    carry_failing: bool,
    now: DateTime<Utc>,
    lang: Lang,
    channels: &mut dyn Channels,
    save: &mut dyn FnMut(&NotifyState),
) -> ApplyOutcome {
    let mut log = Vec::new();
    let mut hook_failed = false;
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
    // later, a hook that failed). The record is saved after each delivery, so
    // a check that is killed halfway does not send the same thing twice.
    let kinds: Vec<Event> = state.active.keys().copied().collect();
    for kind in kinds {
        let (condition, seq) = {
            let entry = &state.active[&kind];
            (entry.condition.clone(), entry.seq)
        };
        for session in &sessions {
            if state.active[&kind]
                .users
                .get(&session.uid)
                .copied()
                .unwrap_or(0)
                >= seq
            {
                continue;
            }
            let message = compose(lang, kind, Some(&condition), session.detail());
            match channels.send_desktop(session, &message) {
                Ok(Delivery::Sent) => {
                    state
                        .active
                        .get_mut(&kind)
                        .unwrap()
                        .users
                        .insert(session.uid, seq);
                    save(state);
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
        if command && state.active[&kind].command_seq < seq {
            // The hook always gets the full text.
            let message = compose(lang, kind, Some(&condition), Detail::Full);
            match channels.send_command(&message) {
                Ok(()) => {
                    state.active.get_mut(&kind).unwrap().command_seq = seq;
                    save(state);
                    log.push(format!("{}: command ran", kind.as_str()));
                }
                Err(e) => {
                    hook_failed = true;
                    log.push(format!(
                        "{}: command failed, trying again at the next check: {e:#}",
                        kind.as_str()
                    ));
                }
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
        if state.recovery.is_some() {
            let message = compose(lang, Event::Recovered, None, Detail::Full);
            let users: Vec<u32> = state
                .recovery
                .as_ref()
                .map(|p| p.users.iter().copied().collect())
                .unwrap_or_default();
            for session in &sessions {
                if !users.contains(&session.uid)
                    || state
                        .recovery
                        .as_ref()
                        .is_some_and(|p| p.users_done.contains(&session.uid))
                {
                    continue;
                }
                match channels.send_desktop(session, &message) {
                    Ok(Delivery::Sent) => {
                        if let Some(p) = &mut state.recovery {
                            p.users_done.insert(session.uid);
                        }
                        save(state);
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
            let command_pending = state.recovery.as_ref().is_some_and(|p| p.command);
            if command_pending && command {
                match channels.send_command(&message) {
                    Ok(()) => {
                        if let Some(p) = &mut state.recovery {
                            p.command = false;
                        }
                        save(state);
                        log.push("recovered: command ran".to_string());
                    }
                    Err(e) => {
                        hook_failed = true;
                        log.push(format!(
                            "recovered: command failed, trying again at the next check: {e:#}"
                        ));
                    }
                }
            }
            if let Some(pending) = &state.recovery {
                let command_open = pending.command && command;
                let users_open = pending
                    .users
                    .iter()
                    .any(|u| !pending.users_done.contains(u));
                if (!command_open && !users_open) || now - pending.since > RECOVERY_KEEP {
                    state.recovery = None;
                    save(state);
                }
            }
        }
    }
    ApplyOutcome { log, hook_failed }
}

/// The reductions of the given announcements merged per actor, in order and
/// without repeating an identical reduction, so a change undone and redone
/// before delivery is listed once (decision 0022).
fn combine_reductions(
    settings: &[SettingsAnnouncement],
    idx: &[usize],
) -> Vec<(String, Vec<crate::settings::Reduction>)> {
    let mut groups: Vec<(String, Vec<crate::settings::Reduction>)> = Vec::new();
    for &i in idx {
        let a = &settings[i];
        let pos = match groups.iter().position(|(u, _)| *u == a.user) {
            Some(p) => p,
            None => {
                groups.push((a.user.clone(), Vec::new()));
                groups.len() - 1
            }
        };
        for r in &a.reductions {
            if !groups[pos].1.contains(r) {
                groups[pos].1.push(r.clone());
            }
        }
    }
    groups
}

/// Keep at most the newest [`MAX_SETTINGS`] announcements, so endless
/// back-and-forth cannot grow the record without bound (decision 0022).
fn cap_settings(settings: &mut Vec<SettingsAnnouncement>) {
    if settings.len() > MAX_SETTINGS {
        let drop = settings.len() - MAX_SETTINGS;
        settings.drain(0..drop);
    }
}

/// Deliver the recorded protection-reducing settings changes (decision 0022):
/// once to every logged-in user (per-user bookkeeping, so a late login still
/// gets it) and once to the hook (retried on failure). All pending changes for
/// one recipient are combined into a single message, so a flurry of changes
/// does not flood anyone. Independent of the problem conditions, so it runs
/// even while a backup is in progress. The record is saved after each delivery.
pub fn deliver_settings(
    state: &mut NotifyState,
    now: DateTime<Utc>,
    lang: Lang,
    channels: &mut dyn Channels,
    save: &mut dyn FnMut(&NotifyState),
) -> ApplyOutcome {
    let mut log = Vec::new();
    let mut hook_failed = false;
    if state.settings.is_empty() {
        return ApplyOutcome { log, hook_failed };
    }
    let sessions = channels.sessions();
    let command = channels.command_enabled();
    for session in &sessions {
        let pending: Vec<usize> = (0..state.settings.len())
            .filter(|&i| !state.settings[i].users_done.contains(&session.uid))
            .collect();
        if pending.is_empty() {
            continue;
        }
        let groups = combine_reductions(&state.settings, &pending);
        let message = compose_settings_many(lang, &groups, session.detail());
        match channels.send_desktop(session, &message) {
            Ok(Delivery::Sent) => {
                for &i in &pending {
                    state.settings[i].users_done.insert(session.uid);
                }
                save(state);
                log.push(format!(
                    "settings: desktop notification to {}",
                    session.name
                ));
            }
            Ok(Delivery::Unavailable(why)) => log.push(format!(
                "settings: no desktop notification to {} ({why})",
                session.name
            )),
            Err(e) => log.push(format!(
                "settings: desktop notification to {} failed: {e:#}",
                session.name
            )),
        }
    }
    if command {
        let pending: Vec<usize> = (0..state.settings.len())
            .filter(|&i| !state.settings[i].command_done)
            .collect();
        if !pending.is_empty() {
            let groups = combine_reductions(&state.settings, &pending);
            let message = compose_settings_many(lang, &groups, Detail::Full);
            match channels.send_command(&message) {
                Ok(()) => {
                    for &i in &pending {
                        state.settings[i].command_done = true;
                    }
                    save(state);
                    log.push("settings: command ran".to_string());
                }
                Err(e) => {
                    hook_failed = true;
                    log.push(format!(
                        "settings: command failed, trying again at the next check: {e:#}"
                    ));
                }
            }
        }
    }
    // Stop after a few days: a late login is caught within the keep window,
    // and a hook that keeps failing is not retried forever.
    let before = state.settings.len();
    state.settings.retain(|a| now - a.at <= RECOVERY_KEEP);
    if state.settings.len() != before {
        save(state);
    }
    ApplyOutcome { log, hook_failed }
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
/// `user_name` maps a uid to an account name (`None`: no such account);
/// `privileged` tells whether that user may see the full message text
/// (member of group `nestegg`, or root; decision 0022).
pub fn find_sessions(
    root: &Path,
    regular: std::ops::RangeInclusive<u32>,
    user_name: impl Fn(u32) -> Option<String>,
    privileged: impl Fn(u32) -> bool,
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
                privileged: privileged(uid),
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

/// Whether `uid` is root or a member of group `nestegg`, and so may see the
/// full detail of a message (decision 0022).
pub fn system_privileged(uid: u32) -> bool {
    if uid == 0 {
        return true;
    }
    let Some(group) = nix::unistd::Group::from_name(crate::daemon::GROUP)
        .ok()
        .flatten()
    else {
        return false;
    };
    crate::identity::UserIdentity::from_uid(uid)
        .map(|id| id.groups.contains(&group.gid.as_raw()))
        .unwrap_or(false)
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
    if let Some(stdin) = child.stdin.take() {
        // Write without blocking: the message is capped well below the pipe
        // buffer (decision 0022), but a hook that never reads its stdin must
        // not be able to hang the check even so. A partial or refused write is
        // fine - the data on stdin is a convenience, not the only channel.
        let _ = fcntl(&stdin, FcntlArg::F_SETFL(OFlag::O_NONBLOCK));
        let mut stdin = stdin;
        let _ = stdin.write_all(input);
        // Dropping `stdin` closes the write end so the hook sees EOF.
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
        find_sessions(
            &self.runtime_root,
            regular_uids(),
            system_user_name,
            system_privileged,
        )
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

pub fn run(paths: &Paths, options: &Options) -> Result<u8> {
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
        return send_test(channels, options.dry_run).map(|()| 0);
    }

    let mut state = load_state(paths);
    // Fold in the settings changes the daemon wrote to the inbox (its own short
    // lock) since the last check. The inbox is the durable record written
    // before the HTTP answer; draining it here, under this check's lock, is
    // what finally delivers it (decision 0022).
    if !options.dry_run {
        drain_settings_inbox(paths, &mut state);
    }
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
    let mut save = |state: &NotifyState| {
        if options.dry_run {
            return;
        }
        // A full disk must not turn every check into a repeat of everything
        // or stop the messages; the record is written again next time.
        if let Err(e) = write_json_atomic(&paths.notify_state_file(), state) {
            eprintln!("warning: cannot save the notification record: {e:#}");
        }
    };
    // Problem conditions this round, or `None` when there is nothing to judge
    // (not configured, backup running). The recorded settings changes are
    // delivered either way (decision 0022).
    let problem: Option<(Vec<Condition>, bool)> = if let Some(error) = &config_error {
        Some((vec![config_problem(error)], true))
    } else if let Some(error) = &state_error {
        Some((vec![state_problem(error)], true))
    } else {
        let status = status.as_ref().expect("status or state_error is set");
        match evaluate(status, timer, unbacked_since, now) {
            Evaluation::Skip(why) => {
                println!("nothing to check: {why}");
                None
            }
            Evaluation::Checked {
                conditions,
                settled,
            } => Some((conditions, settled)),
        }
    };

    // Deliver the settings changes first, then judge the problems; `apply` is
    // skipped when there is nothing to judge or when the last run gave no
    // result (an announced failure then stays open, nothing new to say).
    fn drive(
        state: &mut NotifyState,
        problem: &Option<(Vec<Condition>, bool)>,
        now: DateTime<Utc>,
        lang: Lang,
        channels: &mut dyn Channels,
        save: &mut dyn FnMut(&NotifyState),
    ) -> ApplyOutcome {
        let mut out = deliver_settings(state, now, lang, channels, save);
        if let Some((conditions, settled)) = problem
            && !(conditions.is_empty() && !settled)
        {
            let a = apply(state, conditions, !settled, now, lang, channels, save);
            out.log.extend(a.log);
            out.hook_failed |= a.hook_failed;
        }
        out
    }

    let outcome = if options.dry_run {
        let mut dry = DryChannels {
            inner: channels,
            log: Vec::new(),
        };
        let mut out = drive(&mut state, &problem, now, context.lang, &mut dry, &mut save);
        out.log.extend(dry.log);
        out
    } else {
        let mut channels = channels;
        let out = drive(
            &mut state,
            &problem,
            now,
            context.lang,
            &mut channels,
            &mut save,
        );
        save(&state);
        out
    };
    if outcome.log.is_empty()
        && let Some((conditions, settled)) = &problem
    {
        println!(
            "{}",
            if conditions.is_empty() && !settled {
                "nothing new: the last run ended without a result (server unreachable?)"
            } else if conditions.is_empty() {
                "everything is fine, nothing to report"
            } else {
                "known problems, nothing new to report"
            }
        );
    }
    for line in outcome.log {
        println!("{line}");
    }
    // A hook that failed makes the unit fail, so systemctl --failed and the
    // journal priority show it; the delivery is retried next check (0022).
    Ok(u8::from(outcome.hook_failed))
}

/// The inbox of settings changes the daemon has recorded but no check has
/// delivered yet: a small file with its own short lock, so the daemon never
/// waits for a running check to record a change (decision 0022).
fn settings_inbox_file(paths: &Paths) -> PathBuf {
    paths.state_dir.join("notify-settings.json")
}

/// Short lock for the settings inbox, separate from the main notification lock,
/// so a running check (which holds the main lock for the length of a slow hook)
/// never blocks the daemon from recording a change.
fn settings_inbox_lock(paths: &Paths) -> Result<nix::fcntl::Flock<fs::File>> {
    fs::create_dir_all(&paths.state_dir)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.state_dir.join(".notify-settings.lock"))?;
    nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, e)| e)
        .context("cannot lock the settings inbox")
}

/// Record a protection-reducing settings change for `nestegg notify` to
/// deliver (decision 0022). The daemon calls this synchronously, before it
/// answers the HTTP request, so a daemon restart (a package upgrade) in the
/// minutes a slow check may hold the main lock cannot lose it. It writes to a
/// small inbox under a short lock of its own and never waits for a running
/// check; the next check drains the inbox and announces it. Root's own changes
/// are not recorded by the caller (root is trusted).
pub fn record_settings_change(
    paths: &Paths,
    user: &str,
    reductions: Vec<crate::settings::Reduction>,
) -> Result<()> {
    if reductions.is_empty() {
        return Ok(());
    }
    let _lock = settings_inbox_lock(paths)?;
    let file = settings_inbox_file(paths);
    let mut inbox: Vec<SettingsAnnouncement> = if file.exists() {
        read_json(&file).unwrap_or_default()
    } else {
        Vec::new()
    };
    inbox.push(SettingsAnnouncement {
        user: user.to_string(),
        reductions,
        at: Utc::now(),
        users_done: BTreeSet::new(),
        command_done: false,
    });
    cap_settings(&mut inbox);
    write_json_atomic(&file, &inbox)
        .context("cannot record the settings change for the health check")
}

/// Move everything the daemon left in the inbox into the check's record and
/// clear the inbox. Called under the main notification lock; takes the inbox's
/// own short lock only for the moment it reads and clears the file. The record
/// is persisted before the inbox is cleared, so a crash in between re-delivers
/// at worst, never drops a change (decision 0022).
fn drain_settings_inbox(paths: &Paths, state: &mut NotifyState) {
    let file = settings_inbox_file(paths);
    if !file.exists() {
        return;
    }
    let _lock = match settings_inbox_lock(paths) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("warning: cannot read the settings inbox: {e:#}");
            return;
        }
    };
    let inbox: Vec<SettingsAnnouncement> = match read_json(&file) {
        Ok(inbox) => inbox,
        Err(e) => {
            eprintln!("warning: cannot read the settings inbox: {e:#}");
            let _ = fs::remove_file(&file);
            return;
        }
    };
    if inbox.is_empty() {
        let _ = fs::remove_file(&file);
        return;
    }
    state.settings.extend(inbox);
    cap_settings(&mut state.settings);
    // Persist the merged record first, then clear the inbox.
    if write_json_atomic(&paths.notify_state_file(), state).is_ok() {
        let _ = fs::remove_file(&file);
    }
}

fn send_test(mut channels: SystemChannels<'_>, dry_run: bool) -> Result<()> {
    let message = compose(channels.context.lang, Event::Test, None, Detail::Full);
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
        hook_bodies: Vec<String>,
    }

    impl Fake {
        fn new(users: &[(u32, &str)], command: bool) -> Fake {
            Fake {
                sessions: users
                    .iter()
                    .map(|(uid, name)| Session {
                        uid: *uid,
                        name: name.to_string(),
                        privileged: true,
                    })
                    .collect(),
                command,
                command_fails: false,
                desktop_unavailable: false,
                desktop: Vec::new(),
                hook: Vec::new(),
                bodies: Vec::new(),
                hook_bodies: Vec::new(),
            }
        }

        fn login(&mut self, uid: u32, name: &str) {
            self.sessions.push(Session {
                uid,
                name: name.into(),
                privileged: true,
            });
        }

        fn login_plain(&mut self, uid: u32, name: &str) {
            self.sessions.push(Session {
                uid,
                name: name.into(),
                privileged: false,
            });
        }

        fn clear(&mut self) {
            self.desktop.clear();
            self.hook.clear();
            self.bodies.clear();
            self.hook_bodies.clear();
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
            self.hook_bodies.push(message.body.clone());
            Ok(())
        }
    }

    fn failing(error: &str) -> Condition {
        let mut c = Condition::new(Event::Failing, format!("client_failed|{error}"));
        c.error = Some(error.to_string());
        c
    }

    /// `timer_off` now selects the separate `automatic_off` condition
    /// (decision 0022); `false` is a threshold-based overdue (timer on).
    fn overdue(timer_off: bool, age: Option<i64>) -> Condition {
        if timer_off {
            let mut c = Condition::new(Event::AutomaticOff, "off".into());
            c.timer_off = true;
            c.age_hours = age;
            c
        } else {
            let mut c = Condition::new(Event::Overdue, "timer on".into());
            c.age_hours = age;
            c
        }
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
        apply(state, conditions, false, at, Lang::En, ch, &mut |_| {}).log
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
        apply(
            &mut state,
            &[damaged(1)],
            true,
            hours(1),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert_eq!(ch.desktop, [(1000, Event::Damaged)]);
        assert!(state.active.contains_key(&Event::Failing));
        assert!(state.recovery.is_none());
        // Back to normal runs: the failure is gone for real.
        ch.clear();
        apply(
            &mut state,
            &[],
            false,
            hours(2),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert_eq!(ch.desktop, [(1000, Event::Recovered)]);
    }

    #[test]
    fn the_record_is_saved_after_each_delivery() {
        // A check that is killed halfway must not resend what it already sent
        // (decision 0022): the record is saved after every delivery, not only
        // at the end, and each saved snapshot already reflects the delivery
        // just made (item 11 checks the content, not only the call count).
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice"), (1001, "bob")], true);
        let snapshots = std::cell::RefCell::new(Vec::<NotifyState>::new());
        {
            let mut save = |s: &NotifyState| snapshots.borrow_mut().push(s.clone());
            apply(
                &mut state,
                &[failing("x")],
                false,
                hours(0),
                Lang::En,
                &mut ch,
                &mut save,
            );
        }
        assert_eq!(ch.desktop.len(), 2);
        let snaps = snapshots.into_inner();
        assert!(snaps.len() >= 3, "saved only {} times", snaps.len());
        // Each snapshot records a monotonically growing set of recipients; the
        // final one has both users and the hook marked as done.
        let done = |s: &NotifyState| {
            let a = &s.active[&Event::Failing];
            (a.users.len(), a.command_seq)
        };
        let mut users_seen = 0;
        for s in &snaps {
            let (users, _cmd) = done(s);
            assert!(users >= users_seen, "the recipient set must not shrink");
            users_seen = users;
        }
        let last = done(snaps.last().unwrap());
        assert_eq!(last.0, 2, "both users recorded as notified");
        assert_eq!(last.1, 1, "the hook recorded as notified");
    }

    #[test]
    fn automatic_off_is_stored_and_read_as_automatic_off() {
        // One name for the condition across notify.json and the hook (item 10),
        // with the first version's spelling still readable.
        assert_eq!(Event::AutomaticOff.as_str(), "automatic_off");
        assert_eq!(
            serde_json::to_string(&Event::AutomaticOff).unwrap(),
            "\"automatic_off\""
        );
        assert_eq!(
            serde_json::from_str::<Event>("\"automatic_off\"").unwrap(),
            Event::AutomaticOff
        );
        // A notify.json written by the first version used "automaticoff".
        assert_eq!(
            serde_json::from_str::<Event>("\"automaticoff\"").unwrap(),
            Event::AutomaticOff
        );
    }

    #[test]
    fn automatic_off_is_reported_even_when_the_last_run_failed() {
        // The switched-off timer is its own condition next to a failure, not
        // hidden in the else-branch of "last run failed" (item 7).
        let mut bad = run_with(RunState::Failed, hours(-1));
        bad.error = Some(RunError {
            kind: ErrorKind::ClientFailed,
            message: "upload failed".into(),
        });
        let s = status(Health::Failing, Some(bad), None, &[]);
        let (list, _) = checked(evaluate(&s, false, None, t0()));
        let kinds: Vec<Event> = list.iter().map(|c| c.kind).collect();
        assert!(kinds.contains(&Event::Failing), "{kinds:?}");
        assert!(kinds.contains(&Event::AutomaticOff), "{kinds:?}");
        // With the timer on, only the failure is reported.
        let s = status(
            Health::Failing,
            Some(run_with(RunState::Failed, hours(-1))),
            None,
            &[],
        );
        let (on, _) = checked(evaluate(&s, true, None, t0()));
        assert!(on.iter().all(|c| c.kind != Event::AutomaticOff));
    }

    #[test]
    fn a_failing_hook_makes_the_round_report_it_for_the_exit_code() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], true);
        ch.command_fails = true;
        let out = apply(
            &mut state,
            &[failing("x")],
            false,
            hours(0),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert!(out.hook_failed, "a failing hook must be reported");
        // The desktop still got the message.
        assert_eq!(ch.desktop, [(1000, Event::Failing)]);
        // Next check: the hook works, so the round is clean.
        ch.command_fails = false;
        ch.clear();
        let out = apply(
            &mut state,
            &[failing("x")],
            false,
            hours(2),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert!(!out.hook_failed);
        assert_eq!(ch.hook, [Event::Failing]);
    }

    #[test]
    fn members_see_the_detail_others_only_the_kind() {
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], true); // alice: group member
        ch.login_plain(1001, "guest"); // not a member
        step(
            &mut state,
            &[failing("the file /home/alice/secret broke")],
            hours(0),
            &mut ch,
        );
        assert_eq!(ch.desktop, [(1000, Event::Failing), (1001, Event::Failing)]);
        assert!(
            ch.bodies[0].contains("/home/alice/secret"),
            "{}",
            ch.bodies[0]
        );
        assert!(
            !ch.bodies[1].contains("/home/alice/secret"),
            "{}",
            ch.bodies[1]
        );
        assert!(ch.bodies[1].contains("tell whoever looks after"));
        // The hook always gets the full text.
        assert!(ch.hook_bodies[0].contains("/home/alice/secret"));
    }

    #[test]
    fn settings_changes_are_announced_to_users_and_the_hook() {
        use crate::settings::Reduction;
        let mut state = NotifyState::default();
        state.settings.push(SettingsAnnouncement {
            user: "alice".into(),
            reductions: vec![
                Reduction::AutomaticBackupsOff,
                Reduction::ExcludesAdded {
                    patterns: vec!["/home/alice/secret".into()],
                },
            ],
            at: hours(0),
            users_done: BTreeSet::new(),
            command_done: false,
        });
        let mut ch = Fake::new(&[(1000, "carol")], true); // carol: group member
        ch.login_plain(1001, "guest");
        let out = deliver_settings(&mut state, hours(0), Lang::En, &mut ch, &mut |_| {});
        assert!(!out.hook_failed);
        assert_eq!(
            ch.desktop,
            [(1000, Event::Settings), (1001, Event::Settings)]
        );
        assert_eq!(ch.hook, [Event::Settings]);
        // The member sees who changed what; the guest only the generic note.
        assert!(ch.bodies[0].contains("alice") && ch.bodies[0].contains("secret"));
        assert!(!ch.bodies[1].contains("secret"), "{}", ch.bodies[1]);
        assert!(ch.bodies[1].contains("tell whoever looks after"));
        assert!(ch.hook_bodies[0].contains("alice") && ch.hook_bodies[0].contains("secret"));

        // Delivered once: a later check sends nothing again.
        ch.clear();
        deliver_settings(&mut state, hours(1), Lang::En, &mut ch, &mut |_| {});
        assert!(ch.desktop.is_empty() && ch.hook.is_empty());
        // A user who logs in later still gets it, within the keep window.
        ch.clear();
        ch.login(1002, "dave");
        deliver_settings(&mut state, hours(2), Lang::En, &mut ch, &mut |_| {});
        assert_eq!(ch.desktop, [(1002, Event::Settings)]);
        // After a few days the announcement is dropped.
        ch.clear();
        deliver_settings(
            &mut state,
            hours(24 * 3 + 1),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert!(state.settings.is_empty());
    }

    #[test]
    fn a_failing_hook_keeps_a_settings_announcement_for_retry() {
        use crate::settings::Reduction;
        let mut state = NotifyState::default();
        state.settings.push(SettingsAnnouncement {
            user: "alice".into(),
            reductions: vec![Reduction::WakeOnLanOff],
            at: hours(0),
            users_done: BTreeSet::new(),
            command_done: false,
        });
        let mut ch = Fake::new(&[], true);
        ch.command_fails = true;
        let out = deliver_settings(&mut state, hours(0), Lang::En, &mut ch, &mut |_| {});
        assert!(out.hook_failed);
        assert!(!state.settings[0].command_done);
        // Retried at the next check.
        ch.command_fails = false;
        ch.clear();
        let out = deliver_settings(&mut state, hours(1), Lang::En, &mut ch, &mut |_| {});
        assert!(!out.hook_failed);
        assert_eq!(ch.hook, [Event::Settings]);
    }

    #[test]
    fn pending_settings_changes_are_combined_into_one_message() {
        use crate::settings::Reduction;
        // Several pending changes (even from switching a setting back and
        // forth) become a single message per recipient (decision 0022, item 8).
        let mut state = NotifyState::default();
        for _ in 0..3 {
            state.settings.push(SettingsAnnouncement {
                user: "alice".into(),
                reductions: vec![Reduction::AutomaticBackupsOff],
                at: hours(0),
                users_done: BTreeSet::new(),
                command_done: false,
            });
        }
        state.settings.push(SettingsAnnouncement {
            user: "bob".into(),
            reductions: vec![Reduction::WakeOnLanOff],
            at: hours(0),
            users_done: BTreeSet::new(),
            command_done: false,
        });
        let mut ch = Fake::new(&[(1000, "carol")], true);
        deliver_settings(&mut state, hours(0), Lang::En, &mut ch, &mut |_| {});
        // One desktop message and one hook call, not four each.
        assert_eq!(ch.desktop, [(1000, Event::Settings)]);
        assert_eq!(ch.hook, [Event::Settings]);
        // The identical reduction from alice is listed once; bob is named too.
        let body = &ch.bodies[0];
        assert_eq!(
            body.matches("automatic backups were switched off").count(),
            1
        );
        assert!(body.contains("alice") && body.contains("bob"), "{body}");
        // All announcements are marked delivered.
        assert!(
            state
                .settings
                .iter()
                .all(|a| a.users_done.contains(&1000) && a.command_done)
        );
    }

    #[test]
    fn the_inbox_is_recorded_before_the_check_and_drained_by_it() {
        use crate::settings::Reduction;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: dir.path().join("config.toml"),
            credentials_dir: dir.path().join("cred"),
            state_dir: dir.path().to_path_buf(),
            runtime_dir: dir.path().join("run"),
            systemctl: PathBuf::from("systemctl"),
        };
        // The daemon records two changes (its own short lock), before any
        // check runs (decision 0022, item 4).
        record_settings_change(&paths, "alice", vec![Reduction::AutomaticBackupsOff]).unwrap();
        record_settings_change(&paths, "bob", vec![Reduction::WakeOnLanOff]).unwrap();
        assert!(settings_inbox_file(&paths).exists());
        // A check drains the inbox into its record and clears the inbox.
        let mut state = NotifyState::default();
        drain_settings_inbox(&paths, &mut state);
        assert_eq!(state.settings.len(), 2);
        assert_eq!(state.settings[0].user, "alice");
        assert_eq!(state.settings[1].user, "bob");
        assert!(!settings_inbox_file(&paths).exists(), "inbox cleared");
        // The merged record was persisted to notify.json before clearing.
        let saved: NotifyState = read_json(&paths.notify_state_file()).unwrap();
        assert_eq!(saved.settings.len(), 2);
        // A second drain with an empty inbox changes nothing.
        drain_settings_inbox(&paths, &mut state);
        assert_eq!(state.settings.len(), 2);
    }

    #[test]
    fn a_settings_message_names_only_a_few_patterns_and_stays_small() {
        use crate::settings::Reduction;
        // A huge exclude list must not blow up the message: a few shortened
        // patterns, "and N more", and a hard byte cap (decision 0022, item 1).
        let patterns: Vec<String> = (0..200)
            .map(|i| format!("/home/alice/{}", "x".repeat(300) + &i.to_string()))
            .collect();
        let msg = compose_settings(
            Lang::En,
            "alice",
            &[Reduction::ExcludesAdded { patterns }],
            Detail::Full,
        );
        assert!(
            msg.body.len() <= MAX_BODY_BYTES,
            "body is {} bytes",
            msg.body.len()
        );
        assert!(msg.body.contains("and 195 more"), "{}", msg.body);
        // No single pattern is shown in full (each is shortened).
        assert!(!msg.body.contains(&"x".repeat(300)));
    }

    #[test]
    fn invisible_characters_in_a_settings_message_are_made_visible() {
        use crate::settings::Reduction;
        // A user name or pattern with a bidi override or zero-width character
        // cannot hide or reorder the message (decision 0022, item 10).
        let msg = compose_settings(
            Lang::En,
            "al\u{202e}ice",
            &[Reduction::ExcludesAdded {
                patterns: vec!["/home/\u{200b}secret".into()],
            }],
            Detail::Full,
        );
        assert!(!msg.body.contains('\u{202e}'), "{}", msg.body);
        assert!(!msg.body.contains('\u{200b}'), "{}", msg.body);
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
        // The timer is off: its own condition, not a threshold overdue.
        let (off, _) = checked(evaluate(&s, false, None, t0()));
        assert_eq!(off[0].kind, Event::AutomaticOff);
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
    fn automatic_off_is_reported_independent_of_the_overdue_threshold() {
        // The timer is off but the last success is recent (well within the
        // overdue threshold): it is still reported (decision 0022), unlike the
        // old behaviour that waited for "overdue".
        let recent = run_with(RunState::Succeeded, hours(-1));
        let s = status(Health::Ok, Some(recent.clone()), Some(recent), &[]);
        let (on, _) = checked(evaluate(&s, true, None, t0()));
        assert!(on.is_empty(), "timer on and healthy: nothing");
        let (off, _) = checked(evaluate(&s, false, None, t0()));
        assert_eq!(off.len(), 1);
        assert_eq!(off[0].kind, Event::AutomaticOff);
        assert_eq!(off[0].age_hours, Some(1));
        // Its repeat is weekly, not daily.
        let mut c = off[0].clone();
        assert_eq!(c.repeat_after(), Duration::days(7));
        c.kind = Event::Overdue;
        assert_eq!(c.repeat_after(), Duration::hours(24));
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
        let m = compose(Lang::En, c.kind, Some(&c), Detail::Full);
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
                let m = compose(lang, c.kind, Some(c), Detail::Full);
                assert!(!m.title.is_empty() && !m.body.is_empty());
                assert!(m.body.contains("nestegg ui"), "{lang:?} {c:?}: {}", m.body);
                assert!(m.body.ends_with('.'), "{}", m.body);
            }
            for event in [Event::Recovered, Event::Test] {
                let m = compose(lang, event, None, Detail::Full);
                assert!(m.body.contains("nestegg ui"), "{}", m.body);
            }
        }
    }

    #[test]
    fn the_texts_say_what_happened() {
        let en = compose(
            Lang::En,
            Event::Failing,
            Some(&failing("disk full")),
            Detail::Full,
        );
        assert_eq!(en.title, "Backup failed");
        assert!(en.body.contains("disk full"));
        let de = compose(
            Lang::De,
            Event::Failing,
            Some(&failing("disk full")),
            Detail::Full,
        );
        assert_eq!(de.title, "Datensicherung fehlgeschlagen");
        assert!(de.body.contains("disk full"));

        let m = compose(
            Lang::De,
            Event::Overdue,
            Some(&overdue(false, Some(100))),
            Detail::Full,
        );
        assert!(m.body.contains("4 Tage"), "{}", m.body);
        let m = compose(
            Lang::De,
            Event::Overdue,
            Some(&overdue(false, Some(40))),
            Detail::Full,
        );
        assert!(m.body.contains("40 Stunden"), "{}", m.body);
        let m = compose(
            Lang::En,
            Event::Overdue,
            Some(&overdue(false, Some(100))),
            Detail::Full,
        );
        assert!(m.body.contains("4 days"), "{}", m.body);
        // Timer off is now the separate automatic_off event.
        let m = compose(
            Lang::En,
            Event::AutomaticOff,
            Some(&overdue(true, Some(300))),
            Detail::Full,
        );
        assert_eq!(m.title, "Automatic backups are off");
        assert!(m.body.contains("12 days"));
        let m = compose(
            Lang::De,
            Event::AutomaticOff,
            Some(&overdue(true, None)),
            Detail::Full,
        );
        assert!(m.body.contains("ausgeschaltet"));
        let m = compose(
            Lang::En,
            Event::Overdue,
            Some(&overdue(false, None)),
            Detail::Full,
        );
        assert_eq!(m.title, "No backup yet");

        let one = compose(Lang::En, Event::Damaged, Some(&damaged(1)), Detail::Full);
        assert!(
            one.body
                .contains("1 saved backup of this computer is damaged")
        );
        let many = compose(Lang::En, Event::Damaged, Some(&damaged(3)), Detail::Full);
        assert!(
            many.body
                .contains("3 saved backups of this computer are damaged")
        );
        let de = compose(Lang::De, Event::Damaged, Some(&damaged(2)), Detail::Full);
        assert!(de.body.contains("2 gespeicherte Sicherungen") && de.body.contains("sind"));

        // A long, multi-line error is cut to its first line.
        let long = format!("first line\nsecond line\n{}", "x".repeat(1000));
        let m = compose(
            Lang::En,
            Event::Failing,
            Some(&failing(&long)),
            Detail::Full,
        );
        assert!(m.body.contains("first line") && !m.body.contains("second line"));
        let m = compose(
            Lang::En,
            Event::Failing,
            Some(&failing(&"y".repeat(1000))),
            Detail::Full,
        );
        assert!(m.body.len() < 500);

        // The generic version drops the detail and asks to tell the admin.
        let g = compose(
            Lang::En,
            Event::Failing,
            Some(&failing("disk full")),
            Detail::Generic,
        );
        assert!(!g.body.contains("disk full"), "{}", g.body);
        assert!(g.body.contains("tell whoever looks after"), "{}", g.body);
        assert_eq!(g.title, "Backup failed");
        let g = compose(
            Lang::De,
            Event::Incomplete,
            Some(&incomplete("/home/alice/x")),
            Detail::Generic,
        );
        assert!(!g.body.contains("/home/alice"), "{}", g.body);
        // Damaged has no private detail, so the generic body equals the full one.
        let full = compose(Lang::En, Event::Damaged, Some(&damaged(2)), Detail::Full);
        let plain = compose(Lang::En, Event::Damaged, Some(&damaged(2)), Detail::Generic);
        assert_eq!(full.body, plain.body);
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
        let message = compose(
            Lang::En,
            Event::Failing,
            Some(&failing("no space left.")),
            Detail::Full,
        );
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
        let body = compose(Lang::De, Event::Failing, Some(&c), Detail::Full).body;
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
        // Only alice and carol are group members (and so privileged).
        let privileged = |uid: u32| matches!(uid, 1000 | 60100);
        let found = find_sessions(
            dir.path(),
            DEFAULT_UID_MIN..=REGULAR_UID_MAX,
            names,
            privileged,
        );
        assert_eq!(
            found,
            [
                Session {
                    uid: 1000,
                    name: "alice".into(),
                    privileged: true,
                },
                Session {
                    uid: 1001,
                    name: "bob".into(),
                    privileged: false,
                },
                Session {
                    uid: 60100,
                    name: "carol".into(),
                    privileged: true,
                }
            ]
        );
        assert!(
            find_sessions(
                &dir.path().join("missing"),
                DEFAULT_UID_MIN..=REGULAR_UID_MAX,
                |_| None,
                |_| false,
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
        let message = compose(
            Lang::En,
            Event::Failing,
            Some(&failing("disk full")),
            Detail::Full,
        );
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
        let message = compose(Lang::En, Event::Test, None, Detail::Full);
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
            privileged: true,
        };
        for (event, urgency) in [(Event::Failing, "critical"), (Event::Incomplete, "normal")] {
            let message = compose(
                Lang::En,
                event,
                Some(&incomplete("something is missing")),
                Detail::Full,
            );
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
