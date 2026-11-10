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

use anyhow::{Context as _, Result, bail};
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
/// Beyond this many pending settings announcements they are combined per user
/// instead of growing without bound, a brake on a flood from switching a
/// setting back and forth. Nothing unreported is dropped (decision 0022,
/// item 7).
const MAX_SETTINGS: usize = 50;
/// Quiet period before a recorded settings change is delivered: the check waits
/// until the newest pending change is this old, so a burst of changes made in
/// quick succession becomes a single run and a single message (decision 0022,
/// item 3).
const SETTINGS_QUIET: Duration = Duration::seconds(20);
/// Upper bound on that wait, measured from the oldest pending change, so a
/// continuous stream of changes cannot postpone delivery forever.
const SETTINGS_MAX_WAIT: Duration = Duration::seconds(120);
/// After a recipient has been sent a settings message, further settings
/// messages to them are combined and held for this long, so a stream of changes
/// cannot notify anyone more than once every few minutes (decision 0022,
/// item 3). The first message after a quiet spell is sent at once.
const SETTINGS_RATE_LIMIT: Duration = Duration::minutes(5);
/// A settings hook that keeps failing is retried with a growing pause instead
/// of at every run (decision 0017, R3 item 1): the wait doubles from
/// [`SETTINGS_RATE_LIMIT`] up to this cap, so an endlessly failing hook is
/// still retried for ever — nothing is dropped — but at most a few times an
/// hour, not on every check and not several times within one run.
const SETTINGS_COMMAND_MAX_BACKOFF: Duration = Duration::hours(6);
/// Start of the log line of a check that did not try the failing settings
/// command because of the pause between attempts.
const STILL_FAILS_PREFIX: &str = "settings: the command still fails";
/// An announcement that no member of group `nestegg` ever received (nobody who
/// gets the full text logged in) stops being held for one after this long, so
/// the record stays bounded even on a computer where no member logs in. The
/// users who only get the general text have had their message by then.
const SETTINGS_FULL_MAX_WAIT: Duration = Duration::days(30);
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
    /// The last run carries a state this version does not understand (a record
    /// written by a newer nestegg): nestegg should be updated (decision 0017,
    /// R3 item 3).
    Outdated,
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
            Event::Outdated => "outdated",
            Event::Test => "test",
        }
    }

    /// Critical desktop notifications stay on screen until dismissed.
    pub fn urgent(self) -> bool {
        matches!(
            self,
            Event::Failing
                | Event::Overdue
                | Event::AutomaticOff
                | Event::Damaged
                | Event::Outdated
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
    /// `overdue` / `automatic_off`: the time of the last successful backup lies
    /// in the future (the clock of this computer was wrong), so it says nothing
    /// about how old that backup is; `age_hours` is `None` then.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clock_wrong: bool,
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
            clock_wrong: false,
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
    // A success stamp clearly ahead of the clock (the clock was ahead when the
    // run finished and was corrected later) says nothing about the age of the
    // last backup. Decided here from the stamp itself, so it is right whether
    // or not `status.health` already reports it.
    let clock_wrong = last_success_at.is_some_and(|t| t - now > crate::state::future_tolerance());
    let age_hours = last_success_at
        .filter(|_| !clock_wrong)
        .map(|t| (now - t).num_hours().max(0));
    let failed = status
        .last_run
        .as_ref()
        .filter(|run| run.state == RunState::Failed);
    // The last run carries a state this version does not understand (a record
    // written by a newer nestegg). It must never pass for a success or for
    // "nothing happened": it is reported like a failure, and it suppresses the
    // quieter overdue/incomplete just as a real failure does. Detected from the
    // run state directly, so it is right whether or not `state::health` already
    // reports `failing` for an unknown state (decision 0017, R3 item 3).
    let last_unknown = status
        .last_run
        .as_ref()
        .is_some_and(|run| run.state == RunState::Unknown);
    if last_unknown {
        conditions.push(unknown_run_condition());
    }

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
        c.clock_wrong = clock_wrong;
        conditions.push(c);
    }
    // Overdue and incomplete are only judged when the last run did not fail and
    // is not an unknown state (either is the louder message), and overdue only
    // while the timer is on; a switched-off timer is covered by `automatic_off`
    // above.
    if failed.is_none() && !last_unknown {
        let overdue_limit = Duration::hours(i64::from(status.overdue_after_hours));
        let never_and_old = status.health == Health::Never
            && unbacked_since.is_some_and(|since| now - since > overdue_limit);
        // With a success stamp in the future the backup counts as overdue
        // whatever `health` says: nothing proves a recent backup, and the next
        // check backs up again.
        if timer_enabled && (status.health == Health::Overdue || never_and_old || clock_wrong) {
            let key = if clock_wrong { "clock" } else { "timer on" };
            let mut c = Condition::new(Event::Overdue, key.to_string());
            c.age_hours = age_hours;
            c.clock_wrong = clock_wrong;
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

/// The last run has a state this version does not understand (a record written
/// by a newer nestegg). A stable key, so it is announced once and then only
/// repeated at the usual pace, never at every check (decision 0017, R3 item 3).
pub fn unknown_run_condition() -> Condition {
    Condition::new(Event::Outdated, "unknown-run-state".to_string())
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
    // The one shared list of control and invisible format characters lives in
    // `crate::settings` (decision 0022, item 11), so validation and the message
    // text never disagree on what counts as invisible.
    text.chars()
        .map(|c| {
            if crate::settings::unprintable(c) {
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
        // These carry no private path or name, so the generic version equals
        // the full one.
        Event::Damaged | Event::Recovered | Event::Outdated | Event::Test => return None,
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
                // The stamp of the last backup lies in the future: its age is
                // unknown, not "never".
                None if c.clock_wrong => (
                    tr(lang, "Backups are overdue", "Datensicherung überfällig"),
                    tr(
                        lang,
                        "The time of the last backup of this computer lies in the future: the \
                         clock of this computer was wrong. nestegg backs up again at the next \
                         check.",
                        "Die Zeit der letzten Sicherung dieses Computers liegt in der Zukunft: \
                         Die Uhr dieses Computers ging falsch. nestegg sichert beim nächsten \
                         Prüflauf erneut.",
                    ),
                    tr(
                        lang,
                        "Details: \"nestegg ui\".",
                        "Mehr dazu: \"nestegg ui\".",
                    ),
                ),
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
                None if c.clock_wrong => match lang {
                    Lang::En => "Automatic backups are switched off, and the time of the last \
                                 backup lies in the future (the clock of this computer was wrong)."
                        .to_string(),
                    Lang::De => "Automatische Sicherungen sind ausgeschaltet, und die Zeit der \
                                 letzten Sicherung liegt in der Zukunft (die Uhr dieses Computers \
                                 ging falsch)."
                        .to_string(),
                },
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
        Event::Outdated => (
            tr(
                lang,
                "nestegg needs an update",
                "nestegg muss aktualisiert werden",
            ),
            tr(
                lang,
                "This version of nestegg does not understand the last backup record on \
                 this computer, so it cannot tell whether the backup worked. Please update \
                 nestegg. Details: \"nestegg ui\".",
                "Diese Version von nestegg versteht den letzten Sicherungseintrag dieses \
                 Computers nicht und kann daher nicht sagen, ob die Sicherung geklappt hat. \
                 Bitte nestegg aktualisieren. Mehr dazu: \"nestegg ui\".",
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
            if reductions.is_empty() {
                // A write-ahead intent whose details a crash lost, or a record
                // that was damaged and set aside: name the user if known and
                // point to the journal (decision 0022, item 8).
                return match (lang, user.is_empty()) {
                    (Lang::En, false) => format!(
                        "{user} changed the backup settings of this computer; the details are \
                         in the system log"
                    ),
                    (Lang::En, true) => "the backup settings of this computer were changed; the \
                         details are in the system log"
                        .to_string(),
                    (Lang::De, false) => format!(
                        "{user} hat die Sicherungseinstellungen dieses Computers geändert; die \
                         Einzelheiten stehen im Systemprotokoll"
                    ),
                    (Lang::De, true) => "die Sicherungseinstellungen dieses Computers wurden \
                         geändert; die Einzelheiten stehen im Systemprotokoll"
                        .to_string(),
                };
            }
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
    /// Users who were told about this problem in an earlier wording (the
    /// error text changed and the condition was announced anew). They do not
    /// count for the announcement of the current wording, but they are owed
    /// the "works again" message like everybody in `users`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub told_users: BTreeSet<u32>,
    /// The same for the command: it received an earlier wording.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub told_command: bool,
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
    /// The desktop message with the full text (who, what) has reached at least
    /// one member of group `nestegg`. Users outside the group only get the
    /// general text; they are in `users_done` after they got it, but they do not
    /// release the announcement: a member who logs in later must still see it.
    #[serde(default)]
    pub full_done: bool,
    /// Non-zero while this is a write-ahead *intent*: the daemon recorded it
    /// before changing the configuration and has not yet filled in the real
    /// reductions. The id lets the daemon find and complete (or cancel) it
    /// after the change. A crash in between leaves the provisional entry, which
    /// is delivered as a generic "settings were changed" message (item 8).
    #[serde(default)]
    pub id: u64,
    /// This is still a write-ahead intent (details not yet known).
    #[serde(default)]
    pub provisional: bool,
}

impl SettingsAnnouncement {
    /// Whether this announcement has reached every channel that is switched on
    /// (R3 item 1): the hook, if one is configured, has run; the desktop
    /// message, if the desktop channel is on, has reached at least one
    /// recipient who gets the full text (a member of group `nestegg`; users who
    /// only get the general text do not count). A switched-off channel holds
    /// nothing back. Only once this is true may the keep window (counted from
    /// `at`, the newest change in a merged entry) remove it; before that, time
    /// alone never drops it — with one bound: when no member has logged in for
    /// [`SETTINGS_FULL_MAX_WAIT`], the desktop part counts as done, so the
    /// record stays bounded on a computer where nobody of the group ever logs in.
    fn delivered(&self, desktop_on: bool, command_on: bool, now: DateTime<Utc>) -> bool {
        let desktop_ok = !desktop_on || self.full_done || self.full_wait_over(now);
        let command_ok = !command_on || self.command_done;
        desktop_ok && command_ok
    }

    /// The announcement has waited for a member longer than
    /// [`SETTINGS_FULL_MAX_WAIT`] (counted from its newest change).
    fn full_wait_over(&self, now: DateTime<Utc>) -> bool {
        now - self.at > SETTINGS_FULL_MAX_WAIT
    }
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
    /// When each user (by uid) last received a settings message, for the
    /// per-recipient rate limit (decision 0022, item 3).
    #[serde(default)]
    pub settings_sent_users: BTreeMap<u32, DateTime<Utc>>,
    /// When the hook last received a settings message.
    #[serde(default, with = "crate::state::rfc3339_opt")]
    pub settings_sent_command: Option<DateTime<Utc>>,
    /// How many times in a row the settings hook has failed, and the earliest
    /// time it should be retried: a hook that keeps failing is retried with a
    /// growing, bounded pause instead of at every run (R3 item 1). Cleared on a
    /// success. `settings_command_retry_after` is a deadline in the future by
    /// design, so it is *not* clamped like the other stamps; its hold is bounded
    /// at the point of use instead (R3 item 2).
    #[serde(default)]
    pub settings_command_failures: u32,
    #[serde(default, with = "crate::state::rfc3339_opt")]
    pub settings_command_retry_after: Option<DateTime<Utc>>,
}

impl NotifyState {
    /// Clamp every stored "when it happened" time to `now`: a time later than
    /// now (the clock was ahead when it was written and has since been
    /// corrected) is treated as now, so a future stamp cannot silence repeats,
    /// keep an announcement from ever expiring, or make an "in flight" window
    /// run forever (R3 item 2). Returns whether anything changed.
    ///
    /// The rate-limit stamps (`settings_sent_*`) and the retry deadline
    /// (`settings_command_retry_after`) are deliberately left out: a future
    /// value there must not hold anything back, which clamping to now would not
    /// achieve, so those are bounded where they are read instead.
    fn clamp_future_times(&mut self, now: DateTime<Utc>) -> bool {
        let mut changed = false;
        for a in self.active.values_mut() {
            changed |= clamp_future(&mut a.since, now);
            changed |= clamp_future(&mut a.announced_at, now);
        }
        if let Some(r) = &mut self.recovery {
            changed |= clamp_future(&mut r.since, now);
        }
        if let Some(t) = &mut self.unbacked_since {
            changed |= clamp_future(t, now);
        }
        for a in &mut self.settings {
            changed |= clamp_future(&mut a.at, now);
        }
        changed
    }
}

/// Pull a stored time back to `now` if it is in the future (a corrected clock).
fn clamp_future(t: &mut DateTime<Utc>, now: DateTime<Utc>) -> bool {
    if *t > now {
        *t = now;
        true
    } else {
        false
    }
}

/// A stored "last sent at" stamp holds a recipient back only when it lies in
/// the past and within `window`. A stamp in the future (a clock that was ahead)
/// is ignored, so it can never suppress a message (R3 item 2).
fn within_rate_limit(sent_at: DateTime<Utc>, now: DateTime<Utc>, window: Duration) -> bool {
    sent_at <= now && now - sent_at < window
}

/// The pause before the settings hook is retried after `failures` failures in a
/// row: it doubles from [`SETTINGS_RATE_LIMIT`] and is capped at
/// [`SETTINGS_COMMAND_MAX_BACKOFF`] (R3 item 1).
fn settings_command_backoff(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(16);
    let secs = SETTINGS_RATE_LIMIT
        .num_seconds()
        .saturating_mul(1i64 << shift);
    Duration::seconds(secs).min(SETTINGS_COMMAND_MAX_BACKOFF)
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
    /// Whether the desktop channel is switched on at all (`[notify] desktop`).
    /// Distinct from "nobody is logged in right now": an announcement is held
    /// for a late login only while this channel is on (R3 item 1).
    fn desktop_enabled(&self) -> bool;
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
            told_users.extend(entry.told_users);
            told_command |= entry.command_seq > 0 || entry.told_command;
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
            // The wording changed: everybody is told again, so the counters
            // that steer the announcement start over. Who was told before is
            // kept apart from them: those users (and the command) are owed
            // the "works again" message even if they never see the new
            // wording, for example because they logged out in between.
            let replaced = state.active.remove(&c.kind);
            log.push(format!(
                "{}: {}",
                c.kind.as_str(),
                if replaced.is_some() {
                    "changed, announcing again"
                } else {
                    "new, announcing"
                }
            ));
            let (told_users, told_command) = match replaced {
                Some(old) => {
                    let mut users = old.told_users;
                    users.extend(old.users.keys().copied());
                    (users, old.told_command || old.command_seq > 0)
                }
                None => (BTreeSet::new(), false),
            };
            state.active.insert(
                c.kind,
                Active {
                    condition: c.clone(),
                    since: now,
                    announced_at: now,
                    seq: 1,
                    command_seq: 0,
                    users: BTreeMap::new(),
                    told_users,
                    told_command,
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

/// Keep endless back-and-forth from growing the record without bound, but never
/// drop a reduction that has not been reported yet (decision 0022, item 7).
/// Once there are more than [`MAX_SETTINGS`] announcements, they are combined
/// per user into one each — the union of their reductions — which bounds the
/// count by the number of distinct users. An announcement counts as delivered
/// to a recipient only if every announcement merged into it was (the
/// intersection), so nothing unreported is lost; at worst a recipient sees one
/// combined message again. A write-ahead intent is left untouched so its id
/// still finds it.
fn cap_settings(settings: &mut Vec<SettingsAnnouncement>) {
    if settings.len() <= MAX_SETTINGS {
        return;
    }
    let mut merged: Vec<SettingsAnnouncement> = Vec::new();
    for a in settings.drain(..) {
        if a.provisional {
            merged.push(a);
            continue;
        }
        if let Some(m) = merged
            .iter_mut()
            .find(|m| !m.provisional && m.user == a.user)
        {
            for r in a.reductions {
                if !m.reductions.contains(&r) {
                    m.reductions.push(r);
                }
            }
            // Keep the newest change's time: the keep window must count from the
            // newest change merged in, so a fresh change folded into an old
            // entry does not expire early (R3 item 1).
            m.at = m.at.max(a.at);
            m.users_done = m.users_done.intersection(&a.users_done).copied().collect();
            m.command_done &= a.command_done;
            m.full_done &= a.full_done;
        } else {
            merged.push(a);
        }
    }
    *settings = merged;
}

/// Deliver the recorded protection-reducing settings changes (decision 0022):
/// once to every logged-in user (per-user bookkeeping, so a late login still
/// gets it) and once to the hook (retried on failure). All pending changes for
/// one recipient are combined into a single message, so a flurry of changes
/// does not flood anyone. Independent of the problem conditions, so it runs
/// even while a backup is in progress. The record is saved after each delivery.
///
/// A recipient who was sent a settings message within [`SETTINGS_RATE_LIMIT`]
/// is skipped (their pending changes stay and are combined into the next
/// message), so a continuous stream cannot notify anyone more than once every
/// few minutes — unless they are already in `delivered_users` / `*delivered_cmd`
/// because this same run delivered to them, in which case a change that arrived
/// mid-run is still delivered now (decision 0022, items 1, 3). Those sets grow
/// as deliveries happen so the caller can call again (the end-of-run re-drain).
#[allow(clippy::too_many_arguments)]
pub fn deliver_settings(
    state: &mut NotifyState,
    now: DateTime<Utc>,
    lang: Lang,
    channels: &mut dyn Channels,
    save: &mut dyn FnMut(&NotifyState),
    delivered_users: &mut BTreeSet<u32>,
    delivered_cmd: &mut bool,
) -> ApplyOutcome {
    let mut log = Vec::new();
    let mut hook_failed = false;
    if state.settings.is_empty() {
        return ApplyOutcome { log, hook_failed };
    }
    let sessions = channels.sessions();
    let desktop_on = channels.desktop_enabled();
    let command = channels.command_enabled();
    for session in &sessions {
        // A recipient of the full text is also owed an announcement they have
        // only seen in the general wording (they joined group `nestegg` after
        // it went out, or entries were combined) as long as nobody has had the
        // full text yet: otherwise it would wait for a member who is already
        // here and end up dropped unread.
        let pending: Vec<usize> = (0..state.settings.len())
            .filter(|&i| {
                let a = &state.settings[i];
                !a.users_done.contains(&session.uid) || (session.privileged && !a.full_done)
            })
            .collect();
        if pending.is_empty() {
            continue;
        }
        // Rate limit across runs, bypassed for a recipient this run already
        // notified (so a mid-run arrival still goes out). A stamp in the future
        // (a corrected clock) is ignored, never held against the recipient.
        if !delivered_users.contains(&session.uid)
            && state
                .settings_sent_users
                .get(&session.uid)
                .is_some_and(|t| within_rate_limit(*t, now, SETTINGS_RATE_LIMIT))
        {
            log.push(format!(
                "settings: holding back {} (one message per few minutes)",
                session.name
            ));
            continue;
        }
        let groups = combine_reductions(&state.settings, &pending);
        let message = compose_settings_many(lang, &groups, session.detail());
        match channels.send_desktop(session, &message) {
            Ok(Delivery::Sent) => {
                for &i in &pending {
                    state.settings[i].users_done.insert(session.uid);
                    // Only the full text releases the announcement.
                    if session.privileged {
                        state.settings[i].full_done = true;
                    }
                }
                state.settings_sent_users.insert(session.uid, now);
                delivered_users.insert(session.uid);
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
    if !command
        && (state.settings_command_failures > 0 || state.settings_command_retry_after.is_some())
    {
        // The hook was switched off: its failure is no longer pending.
        state.settings_command_failures = 0;
        state.settings_command_retry_after = None;
        save(state);
    }
    if command {
        let pending: Vec<usize> = (0..state.settings.len())
            .filter(|&i| !state.settings[i].command_done)
            .collect();
        // Across runs, after the first message the hook is held to the rate
        // limit; after repeated failures it is held to a growing, bounded
        // backoff so an endlessly failing hook is not hammered at every check.
        // Both are bypassed for a run that already delivered to the hook (a
        // mid-run arrival still goes out). A future stamp is ignored, and the
        // backoff deadline is capped so even a future one cannot hold forever.
        let rate_hold = state
            .settings_sent_command
            .is_some_and(|t| within_rate_limit(t, now, SETTINGS_RATE_LIMIT));
        let backoff_hold = state
            .settings_command_retry_after
            .is_some_and(|t| now < t && t - now <= SETTINGS_COMMAND_MAX_BACKOFF);
        if !pending.is_empty() && !*delivered_cmd && (rate_hold || backoff_hold) {
            if state.settings_command_failures > 0 {
                // The failure is still pending, only the next try waits for the
                // pause: the check ends as non-zero as the one that really tried,
                // so the unit does not flip between failed and fine.
                hook_failed = true;
                log.push(format!(
                    "{STILL_FAILS_PREFIX} ({} failures in a row), the \
                     announcement is kept and tried again after the pause",
                    state.settings_command_failures
                ));
            } else {
                log.push("settings: holding back the hook (one call per few minutes)".to_string());
            }
        } else if !pending.is_empty() {
            let groups = combine_reductions(&state.settings, &pending);
            let message = compose_settings_many(lang, &groups, Detail::Full);
            match channels.send_command(&message) {
                Ok(()) => {
                    for &i in &pending {
                        state.settings[i].command_done = true;
                    }
                    state.settings_sent_command = Some(now);
                    state.settings_command_failures = 0;
                    state.settings_command_retry_after = None;
                    *delivered_cmd = true;
                    save(state);
                    log.push("settings: command ran".to_string());
                }
                Err(e) => {
                    hook_failed = true;
                    state.settings_command_failures =
                        state.settings_command_failures.saturating_add(1);
                    state.settings_command_retry_after =
                        Some(now + settings_command_backoff(state.settings_command_failures));
                    // Persist the backoff so the pause survives a restart and a
                    // settings change that starts a fresh run moments later.
                    save(state);
                    log.push(format!(
                        "settings: command failed, backing off before the next try: {e:#}"
                    ));
                }
            }
        }
    }
    // An announcement leaves the state only after it has reached every channel
    // that is switched on; then it is kept for the keep window (counted from its
    // newest change) so a late login still gets it. Time alone never drops an
    // announcement that has not yet been delivered on an enabled channel (R3
    // item 1). A future `at` was clamped to now on read, so the window is honest.
    let before = state.settings.len();
    // One line for an announcement that gives up waiting for a member of the
    // group (it is dropped below), so this does not repeat at every check.
    for a in &state.settings {
        if desktop_on
            && !a.full_done
            && a.full_wait_over(now)
            && a.delivered(desktop_on, command, now)
            && now - a.at > RECOVERY_KEEP
        {
            log.push(format!(
                "settings: dropping the change by {}: no member of group nestegg logged in for {} days",
                if a.user.is_empty() {
                    "?".to_string()
                } else {
                    visible(&a.user)
                },
                SETTINGS_FULL_MAX_WAIT.num_days()
            ));
        }
    }
    state
        .settings
        .retain(|a| !a.delivered(desktop_on, command, now) || now - a.at <= RECOVERY_KEEP);
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

    fn desktop_enabled(&self) -> bool {
        self.config.desktop
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
    fn desktop_enabled(&self) -> bool {
        self.inner.desktop_enabled()
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

/// Longest the check waits for the notification lock before giving up (R3
/// item 4). Comfortably longer than one check holds it (a 60 s hook plus a few
/// desktop users) so a waiting check still gets its turn, but well below the
/// unit's `TimeoutStartSec=5min`, even after the settings debounce, so a stuck
/// lock can never make the unit hang until systemd kills it.
const LOCK_TIMEOUT: StdDuration = StdDuration::from_secs(90);

/// Serialises concurrent checks (timer, `OnSuccess=`, by hand). 0600 so a group
/// member cannot hold a shared lock on it (decision 0022, item 2). The wait is
/// time-bounded: if another check (or anything else) holds the lock past the
/// deadline, this one gives up with an error rather than hanging until the unit
/// times out — the caller then exits non-zero and the timer tries again
/// (R3 item 4).
fn lock(paths: &Paths) -> Result<nix::fcntl::Flock<fs::File>> {
    lock_with_timeout(paths, LOCK_TIMEOUT)
}

fn lock_with_timeout(paths: &Paths, timeout: StdDuration) -> Result<nix::fcntl::Flock<fs::File>> {
    fs::create_dir_all(&paths.state_dir)?;
    match crate::state::lock_private(&paths.state_dir.join(".notify.lock"), timeout)
        .context("cannot lock the notification state")?
    {
        Some(lock) => Ok(lock),
        None => bail!(
            "the notification state is locked by another nestegg notify \
             (waited {} s); the timer will try again",
            timeout.as_secs()
        ),
    }
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
    // Debounce before taking the lock: when a settings change was just
    // recorded, wait out a short quiet period so a burst of changes coalesces
    // into one run and one message. Sleeping here, before the lock, means it
    // never blocks the hourly check or the post-backup report; concurrent
    // `systemctl start --no-block` triggers are collapsed by systemd while this
    // one sleeps (decision 0022, item 3).
    if !options.dry_run && !options.test {
        settings_debounce(paths, Utc::now, std::thread::sleep);
    }
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
            .map(|run| run.finished_at.unwrap_or(run.started_at))
            // A stamp from a clock that was ahead says nothing; the hook gets
            // an empty value then, as if there were none.
            .filter(|t| *t - now <= crate::state::future_tolerance()),
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
    // Pull any stored time that lies in the future (the clock was ahead when it
    // was written and has since been corrected) back to now, so a future stamp
    // cannot silence repeats, hold a rate limit open or keep an announcement
    // from ever expiring (R3 item 2). This is the one place the loaded state is
    // read; the correction is written back by the saves that follow.
    state.clamp_future_times(now);
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
    }
    // The configuration file's mtime can also be in the future (a corrected
    // clock); treat it as now so "configured but never backed up" is not
    // pushed forward (R3 item 2).
    .map(|t| t.min(now));
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
    // Finally, before the lock is released, the inbox is drained again so a
    // change recorded while this check ran (its hook can be slow) is delivered
    // now instead of waiting for the next hourly timer (decision 0022, item 1).
    #[allow(clippy::too_many_arguments)]
    fn drive(
        paths: &Paths,
        state: &mut NotifyState,
        problem: &Option<(Vec<Condition>, bool)>,
        now: DateTime<Utc>,
        lang: Lang,
        channels: &mut dyn Channels,
        save: &mut dyn FnMut(&NotifyState),
        redrain: bool,
    ) -> ApplyOutcome {
        let mut delivered_users = BTreeSet::new();
        let mut delivered_cmd = false;
        let mut out = deliver_settings(
            state,
            now,
            lang,
            channels,
            save,
            &mut delivered_users,
            &mut delivered_cmd,
        );
        if let Some((conditions, settled)) = problem
            && !(conditions.is_empty() && !settled)
        {
            let a = apply(state, conditions, !settled, now, lang, channels, save);
            out.log.extend(a.log);
            out.hook_failed |= a.hook_failed;
        }
        if redrain {
            for _ in 0..MAX_REDRAIN {
                // By what was moved, not by the length of the record: above
                // its bound entries are combined and the length stays.
                if drain_settings_inbox(paths, state) == 0 {
                    break;
                }
                let a = deliver_settings(
                    state,
                    now,
                    lang,
                    channels,
                    save,
                    &mut delivered_users,
                    &mut delivered_cmd,
                );
                // One line per check about a command that fails: the try of
                // this check has said it, the pass after it need not add
                // "still fails".
                let said = out.hook_failed;
                out.log.extend(
                    a.log
                        .into_iter()
                        .filter(|l| !(said && l.starts_with(STILL_FAILS_PREFIX))),
                );
                out.hook_failed |= a.hook_failed;
            }
        }
        out
    }

    let outcome = if options.dry_run {
        let mut dry = DryChannels {
            inner: channels,
            log: Vec::new(),
        };
        let mut out = drive(
            paths,
            &mut state,
            &problem,
            now,
            context.lang,
            &mut dry,
            &mut save,
            false,
        );
        out.log.extend(dry.log);
        out
    } else {
        let mut channels = channels;
        let out = drive(
            paths,
            &mut state,
            &problem,
            now,
            context.lang,
            &mut channels,
            &mut save,
            true,
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

/// How many times the end-of-run re-drain repeats, a bound so it cannot loop
/// forever against a daemon that keeps recording changes (decision 0022,
/// item 1).
const MAX_REDRAIN: usize = 5;

/// The timestamps of the changes currently in the inbox (read under its own
/// short lock). Empty on any error.
fn inbox_entry_times(paths: &Paths) -> Vec<DateTime<Utc>> {
    if !settings_inbox_file(paths).exists() {
        return Vec::new();
    }
    let Ok(_lock) = settings_inbox_lock(paths) else {
        return Vec::new();
    };
    read_json::<Vec<SettingsAnnouncement>>(&settings_inbox_file(paths))
        .map(|inbox| inbox.iter().map(|a| a.at).collect())
        .unwrap_or_default()
}

/// How long to wait before delivering, given the inbox entry times: until the
/// newest is [`SETTINGS_QUIET`] old, but never past [`SETTINGS_MAX_WAIT`] after
/// the oldest, and zero when the inbox is empty or already quiet (decision
/// 0022, item 3). Pure, so the pacing is tested without sleeping.
fn settings_debounce_wait(times: &[DateTime<Utc>], now: DateTime<Utc>) -> Duration {
    // A time in the future (a clock that was ahead and has been corrected) is
    // treated as now, so it can never ask for a wait longer than the quiet
    // period (R3 item 2).
    let clamped: Vec<DateTime<Utc>> = times.iter().map(|t| (*t).min(now)).collect();
    let (Some(newest), Some(oldest)) = (clamped.iter().max(), clamped.iter().min()) else {
        return Duration::zero();
    };
    let deadline = (*newest + SETTINGS_QUIET).min(*oldest + SETTINGS_MAX_WAIT);
    (deadline - now).max(Duration::zero())
}

/// Wait out the quiet period before the main lock is taken, re-reading the
/// inbox so a new arrival extends the wait up to the bound (decision 0022,
/// item 3). `now_fn`/`sleep_fn` are injected so the loop is driven by a test
/// clock without really sleeping.
fn settings_debounce(
    paths: &Paths,
    now_fn: impl Fn() -> DateTime<Utc>,
    sleep_fn: impl Fn(StdDuration),
) {
    // Bounded two ways: the clock deadline never exceeds oldest +
    // SETTINGS_MAX_WAIT, and a hard monotonic deadline caps the real time spent
    // here regardless of what the stored times say (a corrupt or future stamp),
    // so the whole debounce stays well below the unit's TimeoutStartSec
    // (R3 item 2).
    let start = Instant::now();
    let hard_deadline = StdDuration::from_secs(SETTINGS_MAX_WAIT.num_seconds().max(0) as u64);
    for _ in 0..200 {
        if start.elapsed() >= hard_deadline {
            return;
        }
        let wait = settings_debounce_wait(&inbox_entry_times(paths), now_fn());
        if wait <= Duration::zero() {
            return;
        }
        let step = wait.min(Duration::seconds(2));
        sleep_fn(StdDuration::from_millis(
            step.num_milliseconds().max(1) as u64
        ));
    }
}

/// Longest the daemon waits for the settings-inbox lock in the request path
/// before giving up, so a stuck lock cannot tie up the HTTP worker thread
/// forever (decision 0022, item 2). The lock is held only for the moment a
/// change is read and written, and it is 0600 so only root processes can take
/// it, so this bound is reached only if something is badly wrong.
const INBOX_LOCK_TIMEOUT: StdDuration = StdDuration::from_secs(5);

/// Short lock for the settings inbox, separate from the main notification lock,
/// so a running check (which holds the main lock for the length of a slow hook)
/// never blocks the daemon from recording a change. 0600 so a group member
/// cannot hold a shared lock on it; the wait is bounded (decision 0022, item 2).
fn settings_inbox_lock(paths: &Paths) -> Result<nix::fcntl::Flock<fs::File>> {
    fs::create_dir_all(&paths.state_dir)?;
    let path = paths.state_dir.join(".notify-settings.lock");
    match crate::state::lock_private(&path, INBOX_LOCK_TIMEOUT)
        .context("cannot lock the settings inbox")?
    {
        Some(lock) => Ok(lock),
        None => bail!("the settings inbox is locked by another process; try again"),
    }
}

/// Read the inbox, setting a damaged file aside under a `.corrupt-<time>` name
/// (and logging it) instead of deleting it silently, so a report is never lost
/// without a trace. Returns the entries and, when the file was damaged, a note
/// for the record so the owner is told (decision 0022, item 8).
fn load_inbox(paths: &Paths) -> (Vec<SettingsAnnouncement>, Option<String>) {
    let file = settings_inbox_file(paths);
    if !file.exists() {
        return (Vec::new(), None);
    }
    match read_json::<Vec<SettingsAnnouncement>>(&file) {
        Ok(inbox) => (inbox, None),
        Err(e) => {
            let aside = file.with_extension(format!("corrupt-{}", Utc::now().timestamp()));
            let note = match fs::rename(&file, &aside) {
                Ok(()) => format!(
                    "the record of settings changes was damaged and set aside as {}",
                    aside.display()
                ),
                Err(re) => {
                    let _ = fs::remove_file(&file);
                    format!("the record of settings changes was damaged and discarded ({re:#})")
                }
            };
            eprintln!("warning: {note}: {e:#}");
            (Vec::new(), Some(note))
        }
    }
}

/// The inbox with a damaged file already dealt with: besides setting it aside
/// (see [`load_inbox`]) a notice is put into the returned list, so that the
/// loss is reported whichever process meets the damaged file first (the
/// entries that were in it cannot be recovered).
fn load_inbox_noting_damage(paths: &Paths) -> Vec<SettingsAnnouncement> {
    let (mut inbox, damaged) = load_inbox(paths);
    if damaged.is_some() {
        inbox.push(SettingsAnnouncement {
            user: String::new(),
            reductions: Vec::new(),
            at: Utc::now(),
            users_done: BTreeSet::new(),
            command_done: false,
            full_done: false,
            id: 0,
            provisional: true,
        });
    }
    inbox
}

/// How long a write-ahead intent counts as "the request is still being
/// handled": that long a check leaves it in the inbox, so that it is not
/// delivered as a generic message (and then a second time with the details,
/// or for a change that is refused in the end).
const INTENT_IN_FLIGHT: Duration = Duration::seconds(60);

/// A unique, non-zero id for a write-ahead intent. The requests that could
/// overlap are serialised by the daemon's settings lock, so the nanosecond
/// clock is unique enough.
fn new_intent_id() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        | 1
}

/// Record a finished protection-reducing settings change for `nestegg notify`
/// to deliver (decision 0022). Writes to a small inbox under a short lock of its
/// own and never waits for a running check; the next check drains it. Root's own
/// changes are not recorded by the caller (root is trusted).
pub fn record_settings_change(
    paths: &Paths,
    user: &str,
    reductions: Vec<crate::settings::Reduction>,
) -> Result<()> {
    if reductions.is_empty() {
        return Ok(());
    }
    let _lock = settings_inbox_lock(paths)?;
    let mut inbox = load_inbox_noting_damage(paths);
    inbox.push(SettingsAnnouncement {
        user: user.to_string(),
        reductions,
        at: Utc::now(),
        users_done: BTreeSet::new(),
        command_done: false,
        full_done: false,
        id: 0,
        provisional: false,
    });
    cap_settings(&mut inbox);
    write_json_atomic(&settings_inbox_file(paths), &inbox)
        .context("cannot record the settings change for the health check")
}

/// Write-ahead *intent*: record that `user` is about to change the settings,
/// **before** the configuration file is changed, and return its id. If this
/// fails the caller must refuse the change (the report could not be recorded),
/// so a change is never applied without a durable report of it (decision 0022,
/// item 8). A crash between here and [`finalize_settings_change`] leaves the
/// provisional entry, which the next check delivers as a generic "the settings
/// were changed by …; details are in the journal" message.
pub fn record_settings_intent(paths: &Paths, user: &str) -> Result<u64> {
    let _lock = settings_inbox_lock(paths)?;
    let mut inbox = load_inbox_noting_damage(paths);
    let id = new_intent_id();
    inbox.push(SettingsAnnouncement {
        user: user.to_string(),
        reductions: Vec::new(),
        at: Utc::now(),
        users_done: BTreeSet::new(),
        command_done: false,
        full_done: false,
        id,
        provisional: true,
    });
    cap_settings(&mut inbox);
    write_json_atomic(&settings_inbox_file(paths), &inbox)
        .context("cannot record the settings change for the health check")?;
    Ok(id)
}

/// Complete the intent `id` with the real reductions once the change has taken
/// effect, or drop it when the change lowered nothing (decision 0022, item 8).
/// Best effort: the change already happened, but the intent guarantees a report
/// even if this write fails.
pub fn finalize_settings_change(
    paths: &Paths,
    id: u64,
    user: &str,
    reductions: Vec<crate::settings::Reduction>,
) -> Result<()> {
    let _lock = settings_inbox_lock(paths)?;
    let mut inbox = load_inbox_noting_damage(paths);
    if reductions.is_empty() {
        // The change raised the protection (or left it unchanged): no report.
        inbox.retain(|a| a.id != id);
    } else if let Some(a) = inbox.iter_mut().find(|a| a.id == id) {
        a.reductions = reductions;
        a.provisional = false;
        a.at = Utc::now();
    } else {
        // The intent is gone (it was older than a check waits for, or it was
        // coalesced): record the reductions afresh, with the name, so the
        // report is not lost.
        inbox.push(SettingsAnnouncement {
            user: user.to_string(),
            reductions,
            at: Utc::now(),
            users_done: BTreeSet::new(),
            command_done: false,
            full_done: false,
            id: 0,
            provisional: false,
        });
    }
    cap_settings(&mut inbox);
    let file = settings_inbox_file(paths);
    if inbox.is_empty() {
        let _ = fs::remove_file(&file);
        return Ok(());
    }
    write_json_atomic(&file, &inbox).context("cannot record the settings change")
}

/// Drop the intent `id`: the change was refused, so nothing took effect and
/// there is nothing to report (decision 0022, item 8).
pub fn cancel_settings_intent(paths: &Paths, id: u64) -> Result<()> {
    let _lock = settings_inbox_lock(paths)?;
    let mut inbox = load_inbox_noting_damage(paths);
    inbox.retain(|a| a.id != id);
    let file = settings_inbox_file(paths);
    if inbox.is_empty() {
        let _ = fs::remove_file(&file);
        return Ok(());
    }
    write_json_atomic(&file, &inbox).context("cannot update the settings inbox")
}

/// Move everything the daemon left in the inbox into the check's record and
/// clear the inbox. Called under the main notification lock; takes the inbox's
/// own short lock only for the moment it reads and clears the file. The record
/// is persisted before the inbox is cleared, so a crash in between re-delivers
/// at worst, never drops a change. A damaged inbox is set aside and reported
/// (decision 0022, items 4, 8).
fn drain_settings_inbox(paths: &Paths, state: &mut NotifyState) -> usize {
    drain_settings_inbox_at(paths, state, Utc::now())
}

/// [`drain_settings_inbox`] with the time it runs at given (tests).
fn drain_settings_inbox_at(paths: &Paths, state: &mut NotifyState, now: DateTime<Utc>) -> usize {
    let file = settings_inbox_file(paths);
    if !file.exists() {
        return 0;
    }
    let _lock = match settings_inbox_lock(paths) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("warning: cannot read the settings inbox: {e:#}");
            return 0;
        }
    };
    // A damaged inbox comes back with a notice in it (a generic settings
    // message without a user; the detail is in the journal).
    let mut inbox = load_inbox_noting_damage(paths);
    // Clamp a time written while the clock was ahead to now, so an "in flight"
    // window measured below cannot run forever and the keep window counts from
    // an honest time (R3 item 2). The clamp is written back below so the
    // correction is durable.
    let mut clamped_any = false;
    for a in &mut inbox {
        clamped_any |= clamp_future(&mut a.at, now);
    }
    // An intent whose request is still being handled stays in the inbox: the
    // daemon completes or cancels it in a moment. Only when it is older (the
    // daemon crashed in between) is it delivered as it is.
    let (in_flight, ready): (Vec<_>, Vec<_>) = inbox
        .into_iter()
        .partition(|a| a.provisional && a.id != 0 && now - a.at < INTENT_IN_FLIGHT);
    let moved = ready.len();
    if moved == 0 {
        if in_flight.is_empty() {
            let _ = fs::remove_file(&file);
        } else if clamped_any {
            // Nothing is ready yet, but a future stamp was pulled back to now;
            // persist it so the in-flight window ends instead of running forever
            // on the original future time (R3 item 2).
            if let Err(e) = write_json_atomic(&file, &in_flight) {
                eprintln!("warning: cannot update the settings inbox: {e:#}");
            }
        }
        return 0;
    }
    state.settings.extend(ready);
    cap_settings(&mut state.settings);
    // Persist the merged record first, then take the moved entries out of
    // the inbox.
    if write_json_atomic(&paths.notify_state_file(), state).is_ok() {
        if in_flight.is_empty() {
            let _ = fs::remove_file(&file);
        } else if let Err(e) = write_json_atomic(&file, &in_flight) {
            eprintln!("warning: cannot update the settings inbox: {e:#}");
        }
    }
    moved
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

    /// A finished settings announcement (not an intent) for the tests.
    fn announce(
        user: &str,
        reductions: Vec<crate::settings::Reduction>,
        at: DateTime<Utc>,
    ) -> SettingsAnnouncement {
        SettingsAnnouncement {
            user: user.to_string(),
            reductions,
            at,
            users_done: BTreeSet::new(),
            command_done: false,
            full_done: false,
            id: 0,
            provisional: false,
        }
    }

    /// Deliver settings with fresh per-run delivered sets (no rate-limit
    /// bypass), the common case in the tests.
    fn deliver(
        state: &mut NotifyState,
        now: DateTime<Utc>,
        lang: Lang,
        channels: &mut dyn Channels,
        save: &mut dyn FnMut(&NotifyState),
    ) -> ApplyOutcome {
        deliver_settings(
            state,
            now,
            lang,
            channels,
            save,
            &mut BTreeSet::new(),
            &mut false,
        )
    }

    // --- policy -----------------------------------------------------------

    struct Fake {
        sessions: Vec<Session>,
        command: bool,
        command_fails: bool,
        desktop_unavailable: bool,
        /// Whether the desktop channel is switched on (`[notify] desktop`),
        /// distinct from whether anyone is logged in (`sessions`).
        desktop_on: bool,
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
                desktop_on: true,
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
        fn desktop_enabled(&self) -> bool {
            self.desktop_on
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
    fn who_was_told_an_earlier_wording_still_hears_that_it_works_again() {
        // Alice gets the first failure message and logs out. The error text
        // changes (announced anew, nobody is there to get it), then a backup
        // succeeds. Alice was told there was a problem: she is owed the end
        // of it when she comes back, and so is the command, whose second
        // call failed.
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], true);
        step(&mut state, &[failing("disk full")], hours(0), &mut ch);
        assert_eq!(ch.desktop, [(1000, Event::Failing)]);
        assert_eq!(ch.hook, [Event::Failing]);
        ch.sessions.clear();
        ch.command_fails = true;
        step(&mut state, &[failing("wrong password")], hours(2), &mut ch);
        assert!(ch.desktop.is_empty());
        let entry = &state.active[&Event::Failing];
        assert!(entry.users.is_empty(), "nobody has the new wording");
        assert!(entry.told_users.contains(&1000) && entry.told_command);
        // A third wording keeps them too.
        step(&mut state, &[failing("no space left")], hours(3), &mut ch);
        assert!(state.active[&Event::Failing].told_users.contains(&1000));
        // The backup works again while alice is away: the message waits.
        ch.command_fails = false;
        step(&mut state, &[], hours(5), &mut ch);
        assert_eq!(ch.hook, [Event::Recovered], "the command hears the end");
        assert!(ch.desktop.is_empty());
        assert!(state.recovery.is_some(), "kept for alice");
        ch.login(1000, "alice");
        step(&mut state, &[], hours(6), &mut ch);
        assert_eq!(ch.desktop, [(1000, Event::Recovered)]);
        assert!(state.recovery.is_none());
    }

    #[test]
    fn a_record_without_the_earlier_recipients_is_still_read() {
        let old = serde_json::json!({
            "condition": serde_json::to_value(failing("x")).unwrap(),
            "since": "2026-01-05T08:00:00Z",
            "announced_at": "2026-01-05T08:00:00Z",
            "seq": 1,
            "command_seq": 1,
            "users": {"1000": 1}
        });
        let entry: Active = serde_json::from_value(old).unwrap();
        assert!(entry.told_users.is_empty() && !entry.told_command);
        // And nothing new is written for a problem with one wording.
        let text = serde_json::to_string(&entry).unwrap();
        assert!(!text.contains("told_"), "{text}");
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
        // The very first snapshot already reflects exactly one delivery, not
        // zero and not both (decision 0022, item 13).
        assert_eq!(
            done(&snaps[0]).0,
            1,
            "the first snapshot records exactly one recipient"
        );
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
        state.settings.push(announce(
            "alice",
            vec![
                Reduction::AutomaticBackupsOff,
                Reduction::ExcludesAdded {
                    patterns: vec!["/home/alice/secret".into()],
                },
            ],
            hours(0),
        ));
        let mut ch = Fake::new(&[(1000, "carol")], true); // carol: group member
        ch.login_plain(1001, "guest");
        let out = deliver(&mut state, hours(0), Lang::En, &mut ch, &mut |_| {});
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
        deliver(&mut state, hours(1), Lang::En, &mut ch, &mut |_| {});
        assert!(ch.desktop.is_empty() && ch.hook.is_empty());
        // A user who logs in later still gets it, within the keep window.
        ch.clear();
        ch.login(1002, "dave");
        deliver(&mut state, hours(2), Lang::En, &mut ch, &mut |_| {});
        assert_eq!(ch.desktop, [(1002, Event::Settings)]);
        // After a few days the announcement is dropped.
        ch.clear();
        deliver(
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
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], hours(0)));
        let mut ch = Fake::new(&[], true);
        ch.command_fails = true;
        let out = deliver(&mut state, hours(0), Lang::En, &mut ch, &mut |_| {});
        assert!(out.hook_failed);
        assert!(!state.settings[0].command_done);
        // Retried at the next check.
        ch.command_fails = false;
        ch.clear();
        let out = deliver(&mut state, hours(1), Lang::En, &mut ch, &mut |_| {});
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
            state.settings.push(announce(
                "alice",
                vec![Reduction::AutomaticBackupsOff],
                hours(0),
            ));
        }
        state
            .settings
            .push(announce("bob", vec![Reduction::WakeOnLanOff], hours(0)));
        let mut ch = Fake::new(&[(1000, "carol")], true);
        deliver(&mut state, hours(0), Lang::En, &mut ch, &mut |_| {});
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

    fn test_paths(dir: &tempfile::TempDir) -> Paths {
        Paths {
            config_file: dir.path().join("config.toml"),
            credentials_dir: dir.path().join("cred"),
            state_dir: dir.path().to_path_buf(),
            runtime_dir: dir.path().join("run"),
            systemctl: PathBuf::from("systemctl"),
        }
    }

    #[test]
    fn a_change_recorded_during_a_check_is_delivered_by_the_same_run() {
        use crate::settings::Reduction;
        // A report that lands in the inbox while a check is running is delivered
        // by that same run's re-drain, not left for the hourly timer — even to a
        // recipient the run already notified (decision 0022, items 1, 3).
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        record_settings_change(&paths, "alice", vec![Reduction::AutomaticBackupsOff]).unwrap();
        let mut state = NotifyState::default();
        drain_settings_inbox(&paths, &mut state);
        let mut ch = Fake::new(&[(1000, "carol")], true);
        let mut du = BTreeSet::new();
        let mut dc = false;
        deliver_settings(
            &mut state,
            t0(),
            Lang::En,
            &mut ch,
            &mut |_| {},
            &mut du,
            &mut dc,
        );
        assert_eq!(ch.desktop.len(), 1);
        // A new change lands during the check.
        record_settings_change(&paths, "bob", vec![Reduction::WakeOnLanOff]).unwrap();
        ch.clear();
        // The re-drain delivers it now, carrying the run's delivered sets so the
        // rate limit does not hold carol back within the same run.
        drain_settings_inbox(&paths, &mut state);
        deliver_settings(
            &mut state,
            t0(),
            Lang::En,
            &mut ch,
            &mut |_| {},
            &mut du,
            &mut dc,
        );
        assert_eq!(ch.desktop, [(1000, Event::Settings)]);
        assert!(ch.bodies[0].contains("bob"), "{}", ch.bodies[0]);
        // A fresh run at the same instant (new delivered sets) would hold carol
        // back instead.
        record_settings_change(&paths, "carol", vec![Reduction::WakeOnLanOff]).unwrap();
        ch.clear();
        drain_settings_inbox(&paths, &mut state);
        deliver(&mut state, t0(), Lang::En, &mut ch, &mut |_| {});
        assert!(
            ch.desktop.is_empty(),
            "the rate limit holds a fresh run back"
        );
    }

    #[test]
    fn the_debounce_waits_for_quiet_but_not_forever() {
        let now = t0();
        assert_eq!(settings_debounce_wait(&[], now), Duration::zero());
        // One recent change: wait until it is SETTINGS_QUIET (20 s) old.
        assert_eq!(
            settings_debounce_wait(&[now - Duration::seconds(5)], now),
            Duration::seconds(15)
        );
        // Already quiet.
        assert_eq!(
            settings_debounce_wait(&[now - Duration::seconds(25)], now),
            Duration::zero()
        );
        // A fresh change keeps extending the wait, but never past the oldest
        // plus SETTINGS_MAX_WAIT (120 s).
        assert_eq!(
            settings_debounce_wait(
                &[now - Duration::seconds(118), now - Duration::seconds(1)],
                now
            ),
            Duration::seconds(2)
        );
        // Past the bound: deliver now.
        assert_eq!(
            settings_debounce_wait(&[now - Duration::seconds(121), now], now),
            Duration::zero()
        );
    }

    #[test]
    fn a_recipient_is_not_notified_more_than_once_in_a_few_minutes() {
        use crate::settings::Reduction;
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], t0()));
        let mut ch = Fake::new(&[(1000, "carol")], true);
        deliver(&mut state, t0(), Lang::En, &mut ch, &mut |_| {});
        assert_eq!(ch.desktop.len(), 1);
        // A new change one minute later: carol is within the window, so held back.
        state.settings.push(announce(
            "bob",
            vec![Reduction::AutomaticBackupsOff],
            t0() + Duration::minutes(1),
        ));
        ch.clear();
        deliver(
            &mut state,
            t0() + Duration::minutes(1),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert!(ch.desktop.is_empty(), "held back within the window");
        assert!(
            !state.settings[1].users_done.contains(&1000),
            "nothing is dropped; the change stays pending"
        );
        // After the window the held-back change is delivered, combined.
        ch.clear();
        deliver(
            &mut state,
            t0() + Duration::minutes(6),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert_eq!(ch.desktop, [(1000, Event::Settings)]);
        assert!(ch.bodies[0].contains("bob"), "{}", ch.bodies[0]);
    }

    #[test]
    fn the_cap_combines_per_user_without_dropping_unreported_changes() {
        use crate::settings::Reduction;
        // More pending announcements than the cap: they are combined per user,
        // and the one change from alice is never dropped to make room for bob's
        // flood (decision 0022, item 7).
        let mut v: Vec<SettingsAnnouncement> = vec![announce(
            "alice",
            vec![Reduction::AutomaticBackupsOff],
            hours(0),
        )];
        for _ in 0..=MAX_SETTINGS {
            v.push(announce("bob", vec![Reduction::WakeOnLanOff], hours(0)));
        }
        assert!(v.len() > MAX_SETTINGS);
        cap_settings(&mut v);
        assert_eq!(v.len(), 2, "one combined entry per user");
        assert!(
            v.iter().any(
                |a| a.user == "alice" && a.reductions.contains(&Reduction::AutomaticBackupsOff)
            ),
            "alice's change survives"
        );
    }

    #[test]
    fn a_write_ahead_intent_is_delivered_generically_after_a_crash() {
        use crate::settings::Reduction;
        // The intent is recorded before the change; a crash before it is
        // completed still leads to a generic message naming the user (item 8).
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let id = record_settings_intent(&paths, "alice").unwrap();
        assert_ne!(id, 0);
        let mut state = NotifyState::default();
        // While the request may still be handled, a check leaves it alone:
        // no generic message for a change that is completed (or refused) a
        // moment later.
        assert_eq!(drain_settings_inbox(&paths, &mut state), 0);
        assert!(state.settings.is_empty());
        assert!(settings_inbox_file(&paths).exists());
        // The daemon never came back: later it is delivered as it is.
        let later = Utc::now() + Duration::minutes(2);
        assert_eq!(drain_settings_inbox_at(&paths, &mut state, later), 1);
        assert_eq!(state.settings.len(), 1);
        assert!(state.settings[0].provisional);
        assert!(!settings_inbox_file(&paths).exists());
        let mut ch = Fake::new(&[(1000, "carol")], true);
        deliver(&mut state, hours(0), Lang::En, &mut ch, &mut |_| {});
        assert_eq!(ch.hook, [Event::Settings]);
        assert!(
            ch.hook_bodies[0].contains("alice") && ch.hook_bodies[0].contains("system log"),
            "{}",
            ch.hook_bodies[0]
        );
        // Completing the intent later yields the full detail instead.
        let dir2 = tempfile::tempdir().unwrap();
        let paths2 = test_paths(&dir2);
        let id2 = record_settings_intent(&paths2, "alice").unwrap();
        finalize_settings_change(&paths2, id2, "alice", vec![Reduction::WakeOnLanOff]).unwrap();
        let mut state2 = NotifyState::default();
        drain_settings_inbox(&paths2, &mut state2);
        assert_eq!(state2.settings.len(), 1);
        assert!(!state2.settings[0].provisional);
        assert_eq!(state2.settings[0].reductions, vec![Reduction::WakeOnLanOff]);
    }

    #[test]
    fn a_check_between_intent_and_completion_reports_the_change_once_with_the_name() {
        use crate::settings::Reduction;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let id = record_settings_intent(&paths, "alice").unwrap();
        // A check (the end of a run with a slow hook) looks at the inbox now.
        let mut state = NotifyState::default();
        assert_eq!(drain_settings_inbox(&paths, &mut state), 0);
        // The daemon completes the change.
        finalize_settings_change(&paths, id, "alice", vec![Reduction::AutomaticBackupsOff])
            .unwrap();
        assert_eq!(drain_settings_inbox(&paths, &mut state), 1);
        assert_eq!(state.settings.len(), 1, "one report, not two");
        assert_eq!(state.settings[0].user, "alice");
        assert!(!state.settings[0].provisional);

        // An intent that a check did take (older than the wait) is completed
        // as a new entry, and that one carries the name as well.
        let id = record_settings_intent(&paths, "bob").unwrap();
        let mut state = NotifyState::default();
        let later = Utc::now() + Duration::minutes(2);
        assert_eq!(drain_settings_inbox_at(&paths, &mut state, later), 1);
        finalize_settings_change(&paths, id, "bob", vec![Reduction::WakeOnLanOff]).unwrap();
        let mut second = NotifyState::default();
        assert_eq!(drain_settings_inbox(&paths, &mut second), 1);
        assert_eq!(second.settings[0].user, "bob");

        // A refused change that a check looked at meanwhile reports nothing.
        let id = record_settings_intent(&paths, "carol").unwrap();
        let mut state = NotifyState::default();
        assert_eq!(drain_settings_inbox(&paths, &mut state), 0);
        cancel_settings_intent(&paths, id).unwrap();
        assert_eq!(drain_settings_inbox(&paths, &mut state), 0);
        assert!(state.settings.is_empty());
    }

    #[test]
    fn a_damaged_inbox_met_by_the_daemon_is_reported_too() {
        use crate::settings::Reduction;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::write(settings_inbox_file(&paths), b"{ this is not json").unwrap();
        // The daemon records a change and meets the damaged file first.
        record_settings_change(&paths, "alice", vec![Reduction::WakeOnLanOff]).unwrap();
        let mut state = NotifyState::default();
        assert_eq!(drain_settings_inbox(&paths, &mut state), 2);
        // Alice's change and a notice that earlier entries were lost.
        assert!(
            state
                .settings
                .iter()
                .any(|a| a.user == "alice" && !a.provisional)
        );
        assert!(
            state
                .settings
                .iter()
                .any(|a| a.user.is_empty() && a.provisional && a.id == 0),
            "{:?}",
            state.settings
        );
    }

    #[test]
    fn a_cancelled_or_empty_intent_leaves_nothing_to_report() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        // A refused change cancels its intent.
        let id = record_settings_intent(&paths, "alice").unwrap();
        cancel_settings_intent(&paths, id).unwrap();
        // A protection-raising change completes with no reductions: nothing left.
        let id2 = record_settings_intent(&paths, "bob").unwrap();
        finalize_settings_change(&paths, id2, "bob", Vec::new()).unwrap();
        let mut state = NotifyState::default();
        drain_settings_inbox(&paths, &mut state);
        assert!(state.settings.is_empty(), "no leftover intents");
    }

    #[test]
    fn a_damaged_inbox_is_set_aside_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::write(settings_inbox_file(&paths), b"{ this is not json").unwrap();
        let mut state = NotifyState::default();
        drain_settings_inbox(&paths, &mut state);
        // The damaged file is moved aside, not left in place or silently deleted.
        assert!(!settings_inbox_file(&paths).exists());
        let aside = std::fs::read_dir(&paths.state_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("corrupt"))
            .count();
        assert_eq!(aside, 1, "the damaged inbox was set aside once");
        // A generic 'settings were changed' message is recorded and delivered.
        assert_eq!(state.settings.len(), 1);
        assert!(state.settings[0].provisional);
        let mut ch = Fake::new(&[(1000, "carol")], true);
        deliver(&mut state, hours(0), Lang::En, &mut ch, &mut |_| {});
        assert!(
            ch.hook_bodies[0].contains("system log"),
            "{}",
            ch.hook_bodies[0]
        );
    }

    #[test]
    fn a_hook_that_never_reads_stdin_does_not_hang_the_check() {
        // The stdin write is non-blocking, so even an input larger than the pipe
        // buffer cannot hang the check when the hook ignores its stdin (item 13).
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exit 0"]);
        let huge = vec![b'x'; 256 * 1024];
        let start = Instant::now();
        let out = run_with_timeout(cmd, &huge, StdDuration::from_secs(10)).unwrap();
        assert!(out.status.is_some_and(|s| s.success()));
        assert!(
            start.elapsed() < StdDuration::from_secs(5),
            "took {:?}",
            start.elapsed()
        );
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

    // --- R3: settings announcements must not get lost (item 1) ----------------

    #[test]
    fn an_undelivered_settings_announcement_is_never_dropped_by_time() {
        use crate::settings::Reduction;
        // Desktop on but nobody logged in, no hook: nothing can be delivered,
        // so time alone must never drop the announcement (R3 item 1).
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], hours(0)));
        let mut ch = Fake::new(&[], false);
        deliver(&mut state, hours(0), Lang::En, &mut ch, &mut |_| {});
        assert!(ch.desktop.is_empty());
        // Far past the keep window it is still there (never delivered).
        deliver(&mut state, hours(24 * 9), Lang::En, &mut ch, &mut |_| {});
        assert_eq!(
            state.settings.len(),
            1,
            "undelivered: kept regardless of time"
        );
        // A user logs in long after and still gets it; delivery happens before
        // the expiry check, so the late login is served.
        ch.login(1000, "alice");
        deliver(
            &mut state,
            hours(24 * 9 + 1),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert_eq!(ch.desktop, [(1000, Event::Settings)]);
    }

    #[test]
    fn a_delivered_settings_announcement_is_kept_for_the_keep_window() {
        use crate::settings::Reduction;
        // Exact boundary: delivered, then kept until the keep window from its
        // (newest) change time is over, not a second longer (R3 items 1, 6).
        let at = hours(0);
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], at));
        let mut ch = Fake::new(&[(1000, "alice")], true);
        deliver(&mut state, at, Lang::En, &mut ch, &mut |_| {});
        assert_eq!(ch.desktop.len(), 1);
        // Exactly at the boundary: still kept.
        deliver(
            &mut state,
            at + RECOVERY_KEEP,
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert_eq!(state.settings.len(), 1, "kept at the boundary");
        // One second past: dropped.
        deliver(
            &mut state,
            at + RECOVERY_KEEP + Duration::seconds(1),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert!(state.settings.is_empty(), "dropped just past the window");
    }

    #[test]
    fn a_failing_settings_hook_is_retried_past_the_keep_window_then_delivers() {
        use crate::settings::Reduction;
        // Only the hook is enabled, and it keeps failing for longer than the
        // keep window: the announcement is never dropped, and a later success
        // still delivers it (R3 item 1).
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], hours(0)));
        let mut ch = Fake::new(&[], true);
        ch.desktop_on = false; // only the hook holds this announcement back
        ch.command_fails = true;
        // A failing attempt roughly once a day, spaced past the backoff.
        for day in 0..6 {
            deliver(&mut state, hours(24 * day), Lang::En, &mut ch, &mut |_| {});
        }
        assert_eq!(state.settings.len(), 1, "undelivered hook message is kept");
        assert!(!state.settings[0].command_done);
        // The hook recovers; the message finally goes out.
        ch.command_fails = false;
        ch.clear();
        let out = deliver(&mut state, hours(24 * 7), Lang::En, &mut ch, &mut |_| {});
        assert!(!out.hook_failed);
        assert_eq!(ch.hook, [Event::Settings]);
    }

    #[test]
    fn the_newest_change_in_a_merged_entry_sets_the_keep_window() {
        use crate::settings::Reduction;
        // 60 changes from one user within an hour are merged into one entry
        // whose time is the newest, so the fresh change does not expire minutes
        // after the oldest (R3 item 1).
        let mut v: Vec<SettingsAnnouncement> = Vec::new();
        for i in 0..60 {
            v.push(announce(
                "alice",
                vec![Reduction::WakeOnLanOff],
                hours(0) + Duration::minutes(i),
            ));
        }
        assert!(v.len() > MAX_SETTINGS);
        cap_settings(&mut v);
        assert_eq!(v.len(), 1, "combined into one entry per user");
        assert_eq!(
            v[0].at,
            hours(0) + Duration::minutes(59),
            "the merged entry keeps the newest change's time"
        );
    }

    #[test]
    fn a_failing_settings_hook_backs_off_instead_of_retrying_every_run() {
        use crate::settings::Reduction;
        // A hook that keeps failing is not retried at every run: after a failure
        // the next try is held back until a growing, bounded backoff is over
        // (R3 item 1).
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], t0()));
        let mut ch = Fake::new(&[], true);
        ch.desktop_on = false;
        ch.command_fails = true;
        let out = deliver(&mut state, t0(), Lang::En, &mut ch, &mut |_| {});
        assert!(out.hook_failed);
        assert_eq!(state.settings_command_failures, 1);
        assert!(state.settings_command_retry_after.is_some());
        // A run seconds later is held back, not retried.
        ch.clear();
        let out = deliver(
            &mut state,
            t0() + Duration::seconds(30),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        // The failure is still pending, so the check ends as failed too (R4
        // item 3), but the hook is not tried again.
        assert!(
            out.hook_failed,
            "held back within the backoff, still failing"
        );
        assert!(ch.hook.is_empty());
        assert_eq!(
            state.settings_command_failures, 1,
            "no new failure recorded"
        );
        // Past the backoff it is tried again (and fails again, growing it).
        ch.clear();
        let out = deliver(
            &mut state,
            t0() + SETTINGS_RATE_LIMIT + Duration::seconds(1),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert!(out.hook_failed);
        assert_eq!(state.settings_command_failures, 2);
        // Backoff is bounded.
        assert!(settings_command_backoff(100) <= SETTINGS_COMMAND_MAX_BACKOFF);
    }

    // --- R3: times in the future (item 2) ------------------------------------

    #[test]
    fn clamp_future_times_pulls_future_stamps_to_now() {
        let now = t0();
        let mut state = NotifyState {
            unbacked_since: Some(now + Duration::days(365)),
            ..Default::default()
        };
        state.settings.push(announce(
            "alice",
            vec![crate::settings::Reduction::WakeOnLanOff],
            now + Duration::days(365),
        ));
        state.active.insert(
            Event::Failing,
            Active {
                condition: failing("x"),
                since: now + Duration::days(10),
                announced_at: now + Duration::days(10),
                seq: 1,
                command_seq: 0,
                users: BTreeMap::new(),
                told_users: BTreeSet::new(),
                told_command: false,
            },
        );
        assert!(state.clamp_future_times(now));
        assert_eq!(state.unbacked_since, Some(now));
        assert_eq!(state.settings[0].at, now);
        assert_eq!(state.active[&Event::Failing].announced_at, now);
        // Nothing to do a second time.
        assert!(!state.clamp_future_times(now));
    }

    #[test]
    fn a_future_settings_stamp_does_not_hold_a_recipient_back() {
        use crate::settings::Reduction;
        // A rate-limit stamp left in the future (a clock that was ahead) must not
        // suppress delivery through the real delivery function (R3 items 2, 6).
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], t0()));
        state
            .settings_sent_users
            .insert(1000, t0() + Duration::days(365));
        state.settings_sent_command = Some(t0() + Duration::days(365));
        let mut ch = Fake::new(&[(1000, "carol")], true);
        let out = deliver(&mut state, t0(), Lang::En, &mut ch, &mut |_| {});
        assert_eq!(
            ch.desktop,
            [(1000, Event::Settings)],
            "future user stamp ignored"
        );
        assert_eq!(ch.hook, [Event::Settings], "future hook stamp ignored");
        assert!(!out.hook_failed);
    }

    #[test]
    fn a_future_intent_time_does_not_stay_in_flight_forever() {
        // A provisional intent whose time is far in the future (a clock that was
        // ahead) is pulled back to now on the first drain and written back, so
        // its in-flight window ends instead of running forever (R3 items 2, 6).
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let inbox = vec![SettingsAnnouncement {
            user: "alice".into(),
            reductions: Vec::new(),
            at: Utc::now() + Duration::days(365),
            users_done: BTreeSet::new(),
            command_done: false,
            full_done: false,
            id: 42,
            provisional: true,
        }];
        write_json_atomic(&settings_inbox_file(&paths), &inbox).unwrap();
        let mut state = NotifyState::default();
        let now = Utc::now();
        // Just created (clamped to now): still in flight, nothing moved, but the
        // corrected time is persisted.
        assert_eq!(drain_settings_inbox_at(&paths, &mut state, now), 0);
        assert!(state.settings.is_empty());
        // Two minutes later (past the in-flight window): delivered.
        assert_eq!(
            drain_settings_inbox_at(&paths, &mut state, now + Duration::minutes(2)),
            1
        );
        assert_eq!(state.settings.len(), 1);
        assert!(state.settings[0].at <= now + Duration::minutes(2));
    }

    #[test]
    fn the_debounce_wait_is_bounded_for_a_future_stamp() {
        let now = t0();
        // A stamp in the future asks for at most the quiet period, never the
        // hundreds of seconds a naive subtraction would give (R3 item 2).
        let wait = settings_debounce_wait(&[now + Duration::days(365)], now);
        assert!(wait <= SETTINGS_QUIET, "{wait:?}");
    }

    // --- R3: an unknown last run state (item 3) ------------------------------

    #[test]
    fn an_unknown_last_run_state_is_reported_like_a_failure() {
        // A run state this version does not understand is announced once as an
        // "update nestegg" message, not treated as success or as nothing
        // happening, and independent of how `health` is computed (R3 item 3).
        let ok = run_with(RunState::Succeeded, hours(-2));
        let unknown = run_with(RunState::Unknown, hours(-1));
        // `health` is deliberately passed as Ok here: detection must not rely on
        // it (a parallel change may make it Failing instead — either way works).
        let s = status(Health::Ok, Some(unknown), Some(ok), &[]);
        let (list, settled) = checked(evaluate(&s, true, None, t0()));
        assert!(!settled, "an unknown last run is not a confirmed success");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, Event::Outdated);
        // The message asks for an update, in both languages, same for everyone.
        for lang in [Lang::En, Lang::De] {
            let full = compose(lang, Event::Outdated, Some(&list[0]), Detail::Full);
            let generic = compose(lang, Event::Outdated, Some(&list[0]), Detail::Generic);
            assert_eq!(full.body, generic.body, "no private detail to hide");
            assert!(full.body.contains("nestegg ui"));
        }
        // Announced once, then quiet at the next check, not repeated every time.
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], true);
        step(&mut state, &list, hours(0), &mut ch);
        assert_eq!(ch.desktop, [(1000, Event::Outdated)]);
        assert_eq!(ch.hook, [Event::Outdated]);
        step(&mut state, &list, hours(1), &mut ch);
        assert!(ch.desktop.is_empty() && ch.hook.is_empty());
    }

    #[test]
    fn an_unknown_last_run_suppresses_the_quieter_overdue() {
        // Like a failure, an unknown last run hides overdue/incomplete, and it
        // coexists with the independent automatic_off condition.
        let old = run_with(RunState::Succeeded, hours(-50));
        let unknown = run_with(RunState::Unknown, hours(-1));
        let s = status(Health::Overdue, Some(unknown), Some(old), &[]);
        let (list, _) = checked(evaluate(&s, true, None, t0()));
        let kinds: Vec<Event> = list.iter().map(|c| c.kind).collect();
        assert!(kinds.contains(&Event::Outdated), "{kinds:?}");
        assert!(!kinds.contains(&Event::Overdue), "{kinds:?}");
        // Timer off: automatic_off is independent and still reported alongside.
        let (off, _) = checked(evaluate(&s, false, None, t0()));
        let off_kinds: Vec<Event> = off.iter().map(|c| c.kind).collect();
        assert!(off_kinds.contains(&Event::Outdated) && off_kinds.contains(&Event::AutomaticOff));
    }

    // --- R3: the lock wait is bounded (item 4) -------------------------------

    #[test]
    fn the_notify_lock_wait_is_bounded() {
        // When another descriptor holds the notification lock, the check does
        // not hang: it gives up after the timeout with an error, so the caller
        // exits non-zero and the timer tries again (R3 item 4).
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let held = lock_with_timeout(&paths, StdDuration::from_secs(5)).unwrap();
        let start = Instant::now();
        let err = lock_with_timeout(&paths, StdDuration::from_millis(200)).unwrap_err();
        assert!(start.elapsed() < StdDuration::from_secs(5), "did not hang");
        assert!(err.to_string().contains("locked by another"), "{err:#}");
        drop(held);
        // With the lock free again it succeeds.
        assert!(lock_with_timeout(&paths, StdDuration::from_millis(200)).is_ok());
    }

    // --- R4 item 1: who counts as "delivered" -------------------------------

    #[test]
    fn users_without_details_do_not_release_a_settings_announcement() {
        use crate::settings::Reduction;
        // Only a user outside the group is logged in: they get the general
        // text once, but the announcement stays for a member who logs in later,
        // also past the keep window.
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], hours(0)));
        let mut ch = Fake::new(&[], false);
        ch.login_plain(2000, "bob");
        let out = deliver(&mut state, hours(0), Lang::En, &mut ch, &mut |_| {});
        assert_eq!(ch.desktop, [(2000, Event::Settings)]);
        assert!(!ch.bodies[0].contains("alice"), "only the general text");
        assert!(out.log.iter().all(|l| !l.contains("dropping")));
        assert!(state.settings[0].users_done.contains(&2000));
        assert!(!state.settings[0].full_done);
        // Days later (past the keep window): kept, and bob is not told again.
        ch.clear();
        let out = deliver(&mut state, hours(24 * 10), Lang::En, &mut ch, &mut |_| {});
        assert!(ch.desktop.is_empty(), "bob has had his message");
        assert!(out.log.is_empty(), "no noise: {:?}", out.log);
        assert_eq!(state.settings.len(), 1, "still waiting for a member");
        // A member logs in and gets the full text; now it is delivered.
        ch.login(1000, "carol");
        deliver(
            &mut state,
            hours(24 * 10 + 1),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert_eq!(ch.desktop, [(1000, Event::Settings)]);
        assert!(ch.bodies[0].contains("alice"), "{}", ch.bodies[0]);
        assert!(
            state.settings.is_empty(),
            "delivered to a member and past the keep window"
        );
    }

    #[test]
    fn a_user_who_joins_the_group_after_the_general_text_gets_the_full_text() {
        use crate::settings::Reduction;
        // bob got the general text while he was not in the group. Then he is
        // added to group nestegg: he is the member the announcement waits
        // for, so he gets the full text once, and that releases it.
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], hours(0)));
        let mut ch = Fake::new(&[], false);
        ch.login_plain(2000, "bob");
        deliver(&mut state, hours(0), Lang::En, &mut ch, &mut |_| {});
        assert!(!state.settings[0].full_done);
        let mut ch = Fake::new(&[], false);
        ch.login(2000, "bob");
        deliver(&mut state, hours(2), Lang::En, &mut ch, &mut |_| {});
        assert_eq!(ch.desktop, [(2000, Event::Settings)]);
        assert!(ch.bodies[0].contains("alice"), "{}", ch.bodies[0]);
        assert!(state.settings[0].full_done);
        // Not again at the next check.
        ch.clear();
        deliver(&mut state, hours(3), Lang::En, &mut ch, &mut |_| {});
        assert!(ch.desktop.is_empty());
    }

    #[test]
    fn a_member_logging_in_inside_the_keep_window_gets_it_and_it_is_then_kept() {
        use crate::settings::Reduction;
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], hours(0)));
        let mut ch = Fake::new(&[], false);
        ch.login_plain(2000, "bob");
        deliver(&mut state, hours(0), Lang::En, &mut ch, &mut |_| {});
        ch.clear();
        ch.login(1000, "carol");
        deliver(&mut state, hours(1), Lang::En, &mut ch, &mut |_| {});
        assert_eq!(ch.desktop, [(1000, Event::Settings)]);
        assert!(state.settings[0].full_done);
        // Delivered: only the keep window is left, the same as before.
        assert_eq!(state.settings.len(), 1);
        deliver(
            &mut state,
            hours(0) + RECOVERY_KEEP + Duration::seconds(1),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert!(state.settings.is_empty());
    }

    #[test]
    fn the_only_member_who_made_the_change_releases_it_when_she_gets_it() {
        use crate::settings::Reduction;
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], hours(0)));
        // alice is the only member and is not logged in: it waits ...
        let mut ch = Fake::new(&[], false);
        deliver(&mut state, hours(1), Lang::En, &mut ch, &mut |_| {});
        assert_eq!(state.settings.len(), 1);
        // ... and counts as delivered once she gets it.
        ch.login(1000, "alice");
        deliver(&mut state, hours(2), Lang::En, &mut ch, &mut |_| {});
        assert_eq!(ch.desktop, [(1000, Event::Settings)]);
        assert!(state.settings[0].full_done);
    }

    #[test]
    fn without_any_member_the_record_stays_bounded_and_quiet() {
        use crate::settings::Reduction;
        // Nobody of the group ever logs in; one user outside it does. Hourly
        // checks for a month: one message to that user, no log line, one
        // entry, and then it is dropped with a single line.
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], hours(0)));
        let mut ch = Fake::new(&[], false);
        ch.login_plain(2000, "bob");
        let mut sent = 0;
        let mut lines = 0;
        for h in 0..(24 * 30) {
            ch.clear();
            let out = deliver(&mut state, hours(h), Lang::En, &mut ch, &mut |_| {});
            sent += ch.desktop.len();
            lines += out
                .log
                .iter()
                .filter(|l| !l.contains("desktop notification"))
                .count();
            assert_eq!(state.settings.len(), 1, "hour {h}");
        }
        assert_eq!(sent, 1, "bob gets it once");
        assert_eq!(lines, 0, "quiet while it waits");
        // Past the bound: dropped, with exactly one line, once.
        let out = deliver(
            &mut state,
            hours(0) + SETTINGS_FULL_MAX_WAIT + Duration::seconds(1),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert!(state.settings.is_empty());
        let dropped: Vec<_> = out.log.iter().filter(|l| l.contains("dropping")).collect();
        assert_eq!(dropped.len(), 1, "{:?}", out.log);
        assert!(dropped[0].contains("alice"));
    }

    #[test]
    fn a_hook_that_has_not_run_still_holds_an_announcement_past_the_member_bound() {
        use crate::settings::Reduction;
        // The bound only ends the wait for a member; a hook message that was
        // never delivered is still kept (R3 item 1).
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], hours(0)));
        let mut ch = Fake::new(&[], true);
        ch.command_fails = true;
        deliver(
            &mut state,
            hours(0) + SETTINGS_FULL_MAX_WAIT + Duration::days(1),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert_eq!(state.settings.len(), 1);
    }

    #[test]
    fn merged_entries_count_as_reached_by_a_member_only_if_all_were() {
        use crate::settings::Reduction;
        let mut a = announce("alice", vec![Reduction::WakeOnLanOff], hours(0));
        a.users_done = [1000, 2000].into();
        a.full_done = true;
        let mut b = announce("alice", vec![Reduction::AutomaticBackupsOff], hours(1));
        b.users_done = [2000].into();
        let mut v: Vec<SettingsAnnouncement> = Vec::new();
        for _ in 0..MAX_SETTINGS {
            v.push(a.clone());
        }
        v.push(b);
        cap_settings(&mut v);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].users_done, [2000].into(), "intersection");
        assert!(!v[0].full_done, "the member has not seen the newer change");
        // All reached by a member: stays reached.
        let mut v: Vec<SettingsAnnouncement> = (0..=MAX_SETTINGS).map(|_| a.clone()).collect();
        cap_settings(&mut v);
        assert_eq!(v.len(), 1);
        assert!(v[0].full_done);
    }

    #[test]
    fn with_the_desktop_off_only_the_hook_holds_an_announcement() {
        use crate::settings::Reduction;
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], hours(0)));
        let mut ch = Fake::new(&[], true);
        ch.desktop_on = false;
        deliver(&mut state, hours(0), Lang::En, &mut ch, &mut |_| {});
        assert_eq!(ch.hook, [Event::Settings]);
        assert!(!state.settings[0].full_done, "no desktop message was sent");
        // Delivered on the one channel that is on; the keep window then ends it.
        deliver(
            &mut state,
            hours(0) + RECOVERY_KEEP + Duration::seconds(1),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert!(state.settings.is_empty());
    }

    #[test]
    fn a_record_written_before_full_done_existed_is_still_read() {
        let text =
            r#"{"user":"alice","reductions":[],"at":"2026-11-09T08:00:00Z","users_done":[1000]}"#;
        let a: SettingsAnnouncement = serde_json::from_str(text).unwrap();
        assert!(!a.full_done);
        assert_eq!(a.users_done, [1000].into());
    }

    // --- R4 item 2: a success stamp in the future -------------------------------

    fn future_overdue_list(timer: bool, health: Health, ahead: Duration) -> Vec<Condition> {
        let future = run_with(RunState::Succeeded, t0() + ahead);
        let s = status(health, Some(future.clone()), Some(future), &[]);
        checked(evaluate(&s, timer, None, t0())).0
    }

    #[test]
    fn a_success_stamp_in_the_future_is_not_a_fresh_backup() {
        // Whatever `health` says: the stamp itself decides.
        for health in [Health::Ok, Health::Overdue, Health::Incomplete] {
            let list = future_overdue_list(true, health, Duration::days(10));
            assert_eq!(list.len(), 1, "{health:?}: {list:?}");
            assert_eq!(list[0].kind, Event::Overdue);
            assert!(list[0].clock_wrong);
            assert_eq!(list[0].age_hours, None, "the age is unknown, not 0");
            assert_eq!(list[0].key, "clock");
        }
        // A few minutes ahead is clock noise, not a problem.
        assert!(future_overdue_list(true, Health::Ok, Duration::minutes(3)).is_empty());
        // Just past the tolerance it counts.
        assert_eq!(
            future_overdue_list(true, Health::Ok, Duration::minutes(6)).len(),
            1
        );
        // With the timer off the separate condition says it, not "never".
        let list = future_overdue_list(false, Health::Ok, Duration::days(10));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, Event::AutomaticOff);
        assert!(list[0].clock_wrong && list[0].age_hours.is_none());
        // A failed last run stays the louder message.
        let future = run_with(RunState::Succeeded, t0() + Duration::days(10));
        let bad = run_with(RunState::Failed, hours(-1));
        let s = status(Health::Failing, Some(bad), Some(future), &[]);
        let kinds: Vec<Event> = checked(evaluate(&s, true, None, t0()))
            .0
            .iter()
            .map(|c| c.kind)
            .collect();
        assert_eq!(kinds, [Event::Failing]);
    }

    #[test]
    fn the_text_for_a_future_stamp_says_what_happened() {
        let overdue = &future_overdue_list(true, Health::Ok, Duration::days(10))[0];
        let off = &future_overdue_list(false, Health::Ok, Duration::days(10))[0];
        for (c, kind) in [(overdue, Event::Overdue), (off, Event::AutomaticOff)] {
            let en = compose(Lang::En, kind, Some(c), Detail::Full).body;
            assert!(en.contains("lies in the future"), "{en}");
            assert!(en.contains("clock"), "{en}");
            assert!(!en.contains("not been backed up"), "{en}");
            let de = compose(Lang::De, kind, Some(c), Detail::Full).body;
            assert!(de.contains("in der Zukunft"), "{de}");
            assert!(de.contains("Uhr"), "{de}");
        }
        let en = compose(Lang::En, Event::Overdue, Some(overdue), Detail::Full).body;
        assert!(en.contains("next check"), "{en}");
    }

    #[test]
    fn a_success_stamp_in_the_future_announces_no_recovery() {
        // Overdue was announced; then the clock jumps ahead and the last success
        // gets a future stamp. That is no "works again".
        let mut state = NotifyState::default();
        let mut ch = Fake::new(&[(1000, "alice")], true);
        step(&mut state, &[overdue(false, Some(50))], hours(0), &mut ch);
        assert_eq!(ch.hook, [Event::Overdue]);
        let list = future_overdue_list(true, Health::Ok, Duration::days(10));
        step(&mut state, &list, hours(1), &mut ch);
        assert!(!ch.hook.contains(&Event::Recovered), "{:?}", ch.hook);
        assert_eq!(
            ch.hook,
            [Event::Overdue],
            "announced again with the new text"
        );
        assert!(ch.hook_bodies[0].contains("lies in the future"));
        step(&mut state, &list, hours(2), &mut ch);
        assert!(ch.hook.is_empty() && ch.desktop.is_empty(), "then quiet");
        // The next backup (a fresh stamp) ends it with the recovery.
        step(&mut state, &[], hours(3), &mut ch);
        assert_eq!(ch.hook, [Event::Recovered]);
    }

    #[test]
    fn a_condition_stored_before_clock_wrong_existed_is_still_read() {
        let c = overdue(false, Some(5));
        let text = serde_json::to_string(&c).unwrap();
        assert!(!text.contains("clock_wrong"), "{text}");
        let back: Condition = serde_json::from_str(&text).unwrap();
        assert_eq!(back, c);
    }

    // --- R4 item 3: the exit code while the settings hook fails -----------------

    #[test]
    fn every_check_fails_while_the_settings_hook_failure_is_pending() {
        use crate::settings::Reduction;
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], t0()));
        let mut ch = Fake::new(&[], true);
        ch.desktop_on = false;
        ch.command_fails = true;
        // The check that really tries the hook.
        let out = deliver(&mut state, t0(), Lang::En, &mut ch, &mut |_| {});
        assert!(out.hook_failed);
        // Checks held back by the pause end the same way, with one line.
        for secs in [30, 60, 120] {
            let out = deliver(
                &mut state,
                t0() + Duration::seconds(secs),
                Lang::En,
                &mut ch,
                &mut |_| {},
            );
            assert!(out.hook_failed, "{secs} s: held back, but still failing");
            assert_eq!(out.log.len(), 1, "{:?}", out.log);
            assert!(out.log[0].contains("still fails"), "{:?}", out.log);
            assert_eq!(
                state.settings_command_failures, 1,
                "no new try, no new count"
            );
        }
        assert!(ch.hook.is_empty());
        // Past the pause it tries again and fails again.
        let out = deliver(
            &mut state,
            t0() + SETTINGS_RATE_LIMIT + Duration::seconds(1),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert!(out.hook_failed);
        assert_eq!(state.settings_command_failures, 2);
        // The hook works again: the next try that happens succeeds and ends it.
        ch.command_fails = false;
        let later = t0() + SETTINGS_COMMAND_MAX_BACKOFF + Duration::minutes(1);
        let out = deliver(&mut state, later, Lang::En, &mut ch, &mut |_| {});
        assert!(!out.hook_failed);
        assert_eq!(ch.hook, [Event::Settings]);
        assert_eq!(state.settings_command_failures, 0);
        let out = deliver(
            &mut state,
            later + Duration::minutes(1),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert!(!out.hook_failed, "nothing pending any more");
    }

    #[test]
    fn a_pause_after_a_success_is_not_a_failure() {
        use crate::settings::Reduction;
        // The rate limit after a delivered message holds a second one back, but
        // no failure is pending, so the check ends fine.
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], t0()));
        let mut ch = Fake::new(&[], true);
        ch.desktop_on = false;
        assert!(!deliver(&mut state, t0(), Lang::En, &mut ch, &mut |_| {}).hook_failed);
        state.settings.push(announce(
            "alice",
            vec![Reduction::AutomaticBackupsOff],
            t0(),
        ));
        let out = deliver(
            &mut state,
            t0() + Duration::seconds(30),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert!(!out.hook_failed);
        assert!(out.log.iter().any(|l| l.contains("holding back the hook")));
    }

    #[test]
    fn switching_the_hook_off_ends_the_pending_failure() {
        use crate::settings::Reduction;
        let mut state = NotifyState::default();
        state
            .settings
            .push(announce("alice", vec![Reduction::WakeOnLanOff], t0()));
        let mut ch = Fake::new(&[], true);
        ch.desktop_on = false;
        ch.command_fails = true;
        assert!(deliver(&mut state, t0(), Lang::En, &mut ch, &mut |_| {}).hook_failed);
        ch.command = false;
        let out = deliver(
            &mut state,
            t0() + Duration::seconds(30),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert!(!out.hook_failed);
        assert_eq!(state.settings_command_failures, 0);
        assert!(state.settings_command_retry_after.is_none());
        // Switched on again later: a fresh start, not held by the old pause.
        ch.command = true;
        ch.command_fails = false;
        let out = deliver(
            &mut state,
            t0() + Duration::seconds(60),
            Lang::En,
            &mut ch,
            &mut |_| {},
        );
        assert!(!out.hook_failed);
        assert_eq!(ch.hook, [Event::Settings]);
    }
}
