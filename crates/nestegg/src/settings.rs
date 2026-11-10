//! Settings that members of group `nestegg` may change through the API
//! (`PATCH /api/v1/config`, see decision 0013): backup interval, overdue
//! threshold, excludes, Wake-on-LAN on/off and the backup timer.
//!
//! Everything else (server, datastore, namespace, credentials, encryption,
//! sources, ...) stays terminal-only: anyone who can reach the socket could
//! otherwise redirect backups to a foreign server.
//!
//! The configuration file is changed with `toml_edit`, so comments and layout
//! survive and only the touched keys change. The result is checked with
//! [`Config::parse`] before it atomically replaces the file.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::ops::RangeInclusive;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use pathpatterns::{MatchEntry, MatchType, PatternFlag};
use serde::{Deserialize, Serialize};
use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value};

use crate::config::Config;
use crate::status::TIMER_UNIT;

/// Allowed backup interval: one hour to one week.
pub const INTERVAL_HOURS: RangeInclusive<u32> = 1..=168;
/// Smallest overdue threshold the API accepts (two hours). It must also be
/// greater than the backup interval.
pub const OVERDUE_MIN_HOURS: u32 = 2;
/// Base cap on the overdue threshold through the API: seven days. The real cap
/// is this or the backup interval plus [`OVERDUE_INTERVAL_EXTRA_HOURS`],
/// whichever is larger (so a weekly rhythm can still be overdue after up to ten
/// days), and a larger value set by root in the file is kept and can only be
/// lowered through the API (decision 0022). The extreme "a month without a
/// message" stays out of reach of a group member.
pub const OVERDUE_API_MAX_HOURS: u32 = 168;
/// How far above the backup interval the overdue threshold may be set through
/// the API when that is larger than [`OVERDUE_API_MAX_HOURS`] (decision 0022,
/// item on the weekly rhythm): three days.
pub const OVERDUE_INTERVAL_EXTRA_HOURS: u32 = 72;
pub const MAX_EXCLUDES: usize = 200;
/// Maximum length of one exclude pattern, in bytes (an argument to
/// `proxmox-backup-client` and part of a notification; counted in bytes so a
/// few hundred multi-byte characters cannot blow past the kernel's argument
/// and environment limits, decision 0022).
pub const MAX_EXCLUDE_LEN: usize = 256;

/// The overdue threshold the API accepts for a given backup interval and the
/// value currently in the file: the base cap, or the interval plus the extra,
/// whichever is larger, but never below a larger value root already set (which
/// can then only be lowered, decision 0022).
pub fn overdue_cap(interval_hours: u32, current_overdue: u32) -> u32 {
    OVERDUE_API_MAX_HOURS
        .max(interval_hours.saturating_add(OVERDUE_INTERVAL_EXTRA_HOURS))
        .max(current_overdue)
}

/// Top-level directories that make up the operating system. In system mode a
/// pattern that would take a whole one of these out of `system.pxar` is refused
/// (decision 0022).
const SYSTEM_DIRS: &[&str] = &[
    "/etc", "/home", "/root", "/usr", "/var", "/opt", "/boot", "/srv",
];

/// A change, made through the API, that lowers the protection of the backups
/// (decision 0022). Each variant carries enough to name the change to the
/// user in a notification. Changes that raise the protection (more frequent
/// backups, a lower overdue threshold, a removed exclude, Wake-on-LAN on)
/// are not reported and have no variant here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reduction {
    /// Automatic backups (the timer) were switched off.
    AutomaticBackupsOff,
    /// Patterns were added to the exclude list, so that data is no longer
    /// backed up.
    ExcludesAdded { patterns: Vec<String> },
    /// A `!` re-include pattern was removed, so what it kept in the backup is
    /// now excluded again.
    ReincludesRemoved { patterns: Vec<String> },
    /// The exclude list kept the same patterns but in a different order; later
    /// patterns win, so a re-include may no longer take effect.
    ExcludesReordered,
    /// The "overdue after" threshold was raised, so a stopped backup is
    /// noticed later.
    OverdueRaised { from: u32, to: u32 },
    /// The backup interval was raised, so backups run less often.
    IntervalRaised { from: u32, to: u32 },
    /// Wake-on-LAN was switched off, so a sleeping backup server is no longer
    /// woken for a run.
    WakeOnLanOff,
}

/// Which protection-reducing changes a settings change made (empty if it
/// only raised the protection or changed nothing relevant). `was_enabled`
/// and `now_enabled` are the backup timer before and after the change.
pub fn reductions(
    old: &Config,
    new: &Config,
    was_enabled: bool,
    now_enabled: bool,
) -> Vec<Reduction> {
    let mut out = Vec::new();
    if was_enabled && !now_enabled {
        out.push(Reduction::AutomaticBackupsOff);
    }
    // Added exclude patterns lower the protection (a `!` re-include added
    // widens the backup, so it is not reported).
    let added: Vec<String> = new
        .backup
        .excludes
        .iter()
        .filter(|p| !p.starts_with('!') && !old.backup.excludes.contains(p))
        .cloned()
        .collect();
    if !added.is_empty() {
        out.push(Reduction::ExcludesAdded { patterns: added });
    }
    // A removed `!` re-include lowers the protection: what it kept in the
    // backup is now excluded again (decision 0022).
    let removed_reincludes: Vec<String> = old
        .backup
        .excludes
        .iter()
        .filter(|p| p.starts_with('!') && !new.backup.excludes.contains(p))
        .cloned()
        .collect();
    if !removed_reincludes.is_empty() {
        out.push(Reduction::ReincludesRemoved {
            patterns: removed_reincludes,
        });
    }
    // Reordering the same patterns can lower the protection when a `!`
    // re-include is involved (later patterns win), so it is reported too. A
    // pure reorder of plain excludes changes nothing and is not reported.
    // Compared on the patterns both lists have, so that it is also seen when
    // something else was added or removed in the same request.
    // Each pattern once (its last position counts, as for the client): a
    // dropped duplicate is not a change of order.
    let kept = |list: &[String], other: &[String]| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for p in list.iter().rev() {
            if other.contains(p) && !out.contains(p) {
                out.push(p.clone());
            }
        }
        out.reverse();
        out
    };
    let kept_old = kept(&old.backup.excludes, &new.backup.excludes);
    let kept_new = kept(&new.backup.excludes, &old.backup.excludes);
    if kept_old != kept_new && kept_new.iter().any(|p| p.starts_with('!')) {
        out.push(Reduction::ExcludesReordered);
    }
    if new.overdue_after_hours > old.overdue_after_hours {
        out.push(Reduction::OverdueRaised {
            from: old.overdue_after_hours,
            to: new.overdue_after_hours,
        });
    }
    if new.schedule.interval_hours > old.schedule.interval_hours {
        out.push(Reduction::IntervalRaised {
            from: old.schedule.interval_hours,
            to: new.schedule.interval_hours,
        });
    }
    if old.active_wake().is_some() && new.active_wake().is_none() {
        out.push(Reduction::WakeOnLanOff);
    }
    out
}

/// Keys a settings change may contain.
pub const EDITABLE: [&str; 5] = [
    "interval_hours",
    "overdue_after_hours",
    "excludes",
    "wake_on_lan",
    "automatic_backups",
];

/// Keys that are known (from `GET /api/v1/config` or the configuration
/// file) but can only be changed in the terminal. They get a clearer error
/// than unknown keys.
const TERMINAL_ONLY: &[&str] = &[
    "backup_id",
    "sources",
    "schedule",
    "encryption",
    "pbs",
    "server",
    "port",
    "datastore",
    "namespace",
    "auth_id",
    "fingerprint",
    "wake",
    "mac",
    "broadcast",
    "timeout_seconds",
    "wake_on_lan_configured",
    "backup",
    "include_dev",
    "inventory",
    "system",
    "change_detection_mode",
    "rate_limit",
    "keep_runs",
    "overview",
    "notify",
    "desktop",
    "command",
    "password",
    "key",
    "editable",
    "limits",
];

/// A partial settings change. Absent (or `null`) fields stay unchanged.
#[derive(Debug, Default, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    pub interval_hours: Option<u32>,
    pub overdue_after_hours: Option<u32>,
    /// The complete new list (replaces the old one).
    pub excludes: Option<Vec<String>>,
    pub wake_on_lan: Option<bool>,
    /// Enable (`systemctl enable --now`) or disable the backup timer.
    pub automatic_backups: Option<bool>,
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// The request is not acceptable (API: 400 `bad_request`).
    #[error("{0}")]
    Invalid(String),
    /// Reading, writing or switching failed (API: 500 `internal`).
    #[error("{0:#}")]
    Failed(#[from] anyhow::Error),
}

fn invalid(msg: impl Into<String>) -> SettingsError {
    SettingsError::Invalid(msg.into())
}

impl SettingsPatch {
    /// Parse a request body. Terminal-only and unknown keys are rejected
    /// with a message that says which keys can be changed.
    pub fn from_json(value: serde_json::Value) -> Result<SettingsPatch, SettingsError> {
        let serde_json::Value::Object(map) = &value else {
            return Err(invalid("the request body must be a JSON object"));
        };
        for key in map.keys() {
            if EDITABLE.contains(&key.as_str()) {
                continue;
            }
            if TERMINAL_ONLY.contains(&key.as_str()) {
                return Err(invalid(format!(
                    "{key:?} cannot be changed here; as root, edit /etc/nestegg/config.toml \
                     or use the nestegg commands in the terminal"
                )));
            }
            return Err(invalid(format!(
                "unknown setting {key:?}; editable settings are: {}",
                EDITABLE.join(", ")
            )));
        }
        serde_json::from_value(value).map_err(|e| invalid(format!("invalid settings: {e}")))
    }
}

/// Allowed values, for `GET /api/v1/config` (`limits`). The overdue maximum
/// is the API cap, or a larger value already in the file (which can then only
/// be lowered through the API, decision 0022).
pub fn limits_json(current: &Config) -> serde_json::Value {
    let interval = current.schedule.interval_hours;
    serde_json::json!({
        "interval_hours": { "min": INTERVAL_HOURS.start(), "max": INTERVAL_HOURS.end() },
        "overdue_after_hours": {
            "min": OVERDUE_MIN_HOURS,
            // The effective maximum for the interval that is in the file now.
            // Both keys can change in one request, so the web interface
            // recomputes this from `base` and `per_interval_extra` for the
            // interval it is about to set (decision 0022).
            "max": overdue_cap(interval, current.overdue_after_hours),
            "greater_than_interval": true,
            // The rule: max(base, interval + per_interval_extra), and never
            // below a larger value already in the file.
            "base": OVERDUE_API_MAX_HOURS,
            "per_interval_extra": OVERDUE_INTERVAL_EXTRA_HOURS,
            "file_minimum": current.overdue_after_hours,
        },
        "excludes": { "max_items": MAX_EXCLUDES, "max_length": MAX_EXCLUDE_LEN },
    })
}

/// Check a change against the current configuration. `home_dirs` are the home
/// directories of regular users, from the account database, used to refuse an
/// exclude that would drop a whole home directory (decision 0022). `identity`
/// is the requester (peer credentials of the socket): the folder guard reads a
/// folder's real entries only with that identity, in a child process dropped
/// to it, never as root, so its answer never depends on a name the requester
/// could not list themselves. `None` (the requester is unknown on this path)
/// reads nothing and judges by computation alone.
pub fn validate(
    patch: &SettingsPatch,
    current: &Config,
    home_dirs: &[PathBuf],
    identity: Option<PeerIdentity>,
) -> Result<(), SettingsError> {
    let reader: Box<FolderReader> = match identity {
        Some(id) => Box::new(move |folder: &Path| real_children(folder, id)),
        None => Box::new(|_: &Path| Vec::new()),
    };
    validate_with_reader(patch, current, home_dirs, reader.as_ref())
}

/// The body of [`validate`] with the folder reader injected, so tests can
/// supply the real top-level entries of a folder (or a reader that panics, to
/// prove it is never called when the request is refused for another reason).
fn validate_with_reader(
    patch: &SettingsPatch,
    current: &Config,
    home_dirs: &[PathBuf],
    read_children: &FolderReader,
) -> Result<(), SettingsError> {
    if let Some(h) = patch.interval_hours
        && !INTERVAL_HOURS.contains(&h)
    {
        return Err(invalid(format!(
            "interval_hours must be between {} and {} (one week)",
            INTERVAL_HOURS.start(),
            INTERVAL_HOURS.end()
        )));
    }
    if let Some(h) = patch.overdue_after_hours {
        // The API cap is seven days, or the backup interval plus three days
        // when that is larger (so a weekly rhythm can still be overdue after up
        // to ten days), evaluated against the interval in effect after this
        // same request. A larger value that root put in the file is kept and
        // can only be lowered here (decision 0022).
        let interval = patch
            .interval_hours
            .unwrap_or(current.schedule.interval_hours);
        let max = overdue_cap(interval, current.overdue_after_hours);
        if h < OVERDUE_MIN_HOURS || h > max {
            let extra = if current.overdue_after_hours > OVERDUE_API_MAX_HOURS {
                format!(
                    "; the current value of {} can only be lowered here",
                    current.overdue_after_hours
                )
            } else {
                String::new()
            };
            return Err(invalid(format!(
                "overdue_after_hours must be between {OVERDUE_MIN_HOURS} and \
                 {max} hours through the web interface{extra}. \
                 A longer time can only be set as root in /etc/nestegg/config.toml."
            )));
        }
    }
    // Only checked when one of the two changes, so an existing file that
    // does not follow the rule can still get other changes.
    if patch.interval_hours.is_some() || patch.overdue_after_hours.is_some() {
        let interval = patch
            .interval_hours
            .unwrap_or(current.schedule.interval_hours);
        let overdue = patch
            .overdue_after_hours
            .unwrap_or(current.overdue_after_hours);
        if overdue <= interval {
            return Err(invalid(format!(
                "overdue_after_hours ({overdue}) must be greater than interval_hours \
                 ({interval}); otherwise the dashboard reports \"overdue\" before the next \
                 backup is due"
            )));
        }
    }
    // Pattern validity is content-independent, so it is checked here with the
    // other refusals. The content-dependent folder guard runs last (below).
    if let Some(list) = &patch.excludes {
        validate_excludes(list).map_err(invalid)?;
    }
    if patch.wake_on_lan == Some(true) && current.wake.is_none() {
        return Err(invalid(
            "Wake-on-LAN is not set up: the configuration has no [wake] section with the \
             MAC address of the backup server. Add it in the terminal (see \
             docs/configuration.md), then switch it on here.",
        ));
    }
    // The folder guard is the LAST validation step: it is the only check whose
    // answer can depend on a folder's real contents, so everything that can
    // refuse a request for another reason is checked before any folder is read.
    // A refusal for another reason therefore never reveals anything about the
    // folders, and the only read that happens is for a request that is
    // otherwise acceptable (decision 0022).
    //
    // The whole new list is judged as the client applies it: a change that
    // would leave out a whole configured source, a whole home directory of a
    // regular user, or (in system mode) a whole top-level system directory is
    // refused, unless the old list already left it out (root may have put the
    // pattern there). Judging the list as a whole catches character-class and
    // several-patterns bypasses and reordering too (decision 0022, items 5, 9).
    if let Some(list) = &patch.excludes
        && let Some(why) = excludes_reduce_protection(
            &current.backup.excludes,
            list,
            &current.backup.sources,
            home_dirs,
            current.backup.system,
            read_children,
        )
    {
        return Err(invalid(why));
    }
    Ok(())
}

pub fn validate_excludes(list: &[String]) -> Result<(), String> {
    if list.len() > MAX_EXCLUDES {
        return Err(format!(
            "at most {MAX_EXCLUDES} exclude patterns are allowed"
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for pattern in list {
        validate_exclude(pattern)?;
        if !seen.insert(pattern) {
            return Err(format!("exclude pattern `{pattern}` is listed twice"));
        }
    }
    Ok(())
}

pub fn validate_exclude(pattern: &str) -> Result<(), String> {
    if pattern.is_empty() || pattern == "!" {
        return Err("exclude patterns must not be empty".into());
    }
    if pattern.len() > MAX_EXCLUDE_LEN {
        return Err(format!(
            "exclude patterns may have at most {MAX_EXCLUDE_LEN} bytes"
        ));
    }
    if pattern.chars().any(unprintable) {
        return Err(format!(
            "exclude pattern {pattern:?} contains a control character or an invisible \
             format character (line break, tab, NUL, zero-width or bidirectional override, ...)"
        ));
    }
    if pattern.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}') != pattern {
        return Err(format!(
            "exclude pattern {pattern:?} must not start or end with spaces"
        ));
    }
    // proxmox-backup-client gets `--exclude <pattern>` as two arguments; a
    // leading `-` would be read as an option and break every backup run.
    if pattern.starts_with('-') {
        return Err(format!(
            "exclude pattern `{pattern}` must not start with `-`"
        ));
    }
    if excludes_everything(pattern) {
        return Err(format!(
            "exclude pattern `{pattern}` would exclude everything; the backups would be empty"
        ));
    }
    // The real backup client parses every `--exclude` with the `pathpatterns`
    // crate; nestegg links an older release (0.1.2) of the client's own version
    // (1.0.0). A pattern neither can parse — an unclosed character class
    // (`[abc`), a trailing backslash (`foo\`), a malformed range (`[a-]`) —
    // makes every backup fail until root repairs the file; some even panic the
    // parser (the same off-by-one bug in both releases), which would otherwise
    // reach the user as an HTTP 500. Refuse such a pattern here, catching any
    // panic so none escapes into the request (decision 0022, item 4).
    match parse_pattern_result(pattern) {
        ParseOutcome::Ok => {}
        ParseOutcome::Error(why) => {
            return Err(format!(
                "exclude pattern `{pattern}` is not a valid pattern ({why}); \
                 the backup client would reject it"
            ));
        }
        ParseOutcome::Panic => {
            return Err(format!(
                "exclude pattern `{pattern}` is malformed (an unclosed `[`…`]` character \
                 class or a dangling `\\`, for example) and would break every backup run"
            ));
        }
    }
    Ok(())
}

/// What happened when the pattern matcher parsed a pattern.
enum ParseOutcome {
    Ok,
    Error(String),
    Panic,
}

/// Parse a pattern the way `proxmox-backup-client` does, catching both a clean
/// parse error and a panic (the `pathpatterns` releases panic on some malformed
/// character classes). The panic hook is silenced for the duration so a refused
/// pattern does not spill a backtrace into the daemon journal (decision 0022).
fn parse_pattern_result(pattern: &str) -> ParseOutcome {
    let owned = pattern.to_string();
    let parsed = with_silent_panics(|| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            MatchEntry::parse_pattern(&owned, PatternFlag::PATH_NAME, MatchType::Exclude)
        }))
    });
    match parsed {
        Ok(Ok(_)) => ParseOutcome::Ok,
        Ok(Err(e)) => ParseOutcome::Error(e.to_string()),
        Err(_) => ParseOutcome::Panic,
    }
}

/// Run `f` with the panic hook replaced by a silent one, so an expected,
/// caught panic (a malformed pattern) does not print a message and backtrace.
/// Serialised, because the panic hook is process-global.
fn with_silent_panics<T>(f: impl FnOnce() -> T) -> T {
    use std::sync::Mutex;
    static HOOK_LOCK: Mutex<()> = Mutex::new(());
    let _guard = HOOK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = f();
    std::panic::set_hook(previous);
    result
}

/// Patterns that match every entry of an archive: only slashes, or every
/// path component made of wildcards only (`*`, `**`, `/`, `/*`, `**/*`,
/// `*/*`, `?*`, ...). Character classes such as `[a-z]*` are not analysed;
/// they cannot match everything with certainty and are left to the user.
fn excludes_everything(pattern: &str) -> bool {
    if pattern.starts_with('!') {
        // Re-includes; never removes anything.
        return false;
    }
    pattern
        .split('/')
        .filter(|part| !part.is_empty())
        .all(|part| part.contains('*') && part.chars().all(|c| matches!(c, '*' | '?')))
}

/// Control characters, and the invisible Unicode format and default-ignorable
/// characters that reorder or hide text in a terminal or a notification
/// (bidirectional overrides, zero-width joiners, tag characters, variation
/// selectors, line/paragraph separators, Hangul fillers, ...). This is the one
/// shared list used for both rejecting such a character in an exclude pattern
/// and replacing it in a notification (`crate::notify::visible`), so the two
/// never drift apart (decision 0022, item 11). `crate::rescue` keeps its own,
/// narrower copy on purpose, so the settings path does not depend on it.
pub fn unprintable(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00ad}'                      // soft hyphen
            | '\u{034f}'                    // combining grapheme joiner
            | '\u{061c}'                    // arabic letter mark
            | '\u{115f}' | '\u{1160}'       // Hangul choseong/jungseong fillers
            | '\u{17b4}' | '\u{17b5}'       // Khmer inherent vowels
            | '\u{180b}'..='\u{180f}'       // Mongolian variation selectors + MVS
            | '\u{13430}'..='\u{1343f}'   // Egyptian hieroglyph format controls
            | '\u{200b}'..='\u{200f}'       // zero-width space/joiners, LRM/RLM
            | '\u{2028}' | '\u{2029}'       // line / paragraph separators
            | '\u{202a}'..='\u{202e}'       // bidi embeddings / overrides
            | '\u{2060}'..='\u{206f}'       // word joiner, bidi isolates, deprecated
            | '\u{3164}'                    // Hangul filler
            | '\u{fe00}'..='\u{fe0f}'       // variation selectors
            | '\u{feff}'                    // zero-width no-break space / BOM
            | '\u{ffa0}'                    // halfwidth Hangul filler
            | '\u{fff9}'..='\u{fffb}'       // interlinear annotation anchors
            | '\u{1bca0}'..='\u{1bca3}'     // shorthand format controls
            | '\u{1d173}'..='\u{1d17a}'     // musical beam/slur/phrase controls
            | '\u{e0000}'..='\u{e007f}'     // tag characters
            | '\u{e0100}'..='\u{e01ef}') // variation selectors supplement
}

/// Build the exclude list as `proxmox-backup-client` sees it: one `MatchEntry`
/// per pattern, in order (a `!` pattern becomes a re-include; later entries
/// win). A pattern the matcher cannot parse, or that makes it panic, is left
/// out of the list — the real client would fail on it, and for a *new* list
/// `validate_exclude` has already refused it; a dangerous pattern root put in
/// the file by hand is simply not analysed, which only means the guard does not
/// count it (decision 0022, item 4/5).
fn match_entries(list: &[String]) -> Vec<MatchEntry> {
    list.iter().filter_map(|p| parse_entry(p)).collect()
}

/// Parse one pattern into a [`MatchEntry`], catching a parse error and a panic
/// (the `pathpatterns` releases panic on some malformed classes).
fn parse_entry(pattern: &str) -> Option<MatchEntry> {
    let owned = pattern.to_string();
    with_silent_panics(|| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            MatchEntry::parse_pattern(&owned, PatternFlag::PATH_NAME, MatchType::Exclude).ok()
        }))
        .ok()
        .flatten()
    })
}

/// Two arbitrary, structurally different child names. A list that excludes both
/// of them excludes *every* possible child, so the folder would be emptied even
/// if the folder entry itself stays (`alice/**`, `/alice/[a-zA-Z0-9]*`). They
/// are chosen so that an ordinary narrow pattern (`alice/Downloads`,
/// `**/.cache`) matches neither; one starts with a digit, the other with an
/// uppercase letter and contains a dot.
const PROBE_A: &str = "0_nestegg_probe_one";
const PROBE_B: &str = "Zq.keep-me_two";
/// Further made-up names, one per kind of first character (digit and upper
/// case are covered by the two above). They are used when the real entries of
/// a folder are not known: only a list that leaves out ALL of them is taken
/// to empty the folder.
const PROBES_MORE: [&str; 4] = [
    "_nestegg_probe_three",
    ".nestegg_probe_four",
    "nestegg_probe_five",
    "\u{e4}nestegg_probe_six",
];

/// At most this many real top-level entries of a protected folder are read to
/// judge whether the new list empties it (only the names, never the contents;
/// the daemon is root). A folder with more entries is judged by the first
/// this many (decision 0022): a home directory with a few hundred entries is
/// normal, and a low bound would switch the real-entry test off exactly there.
const MAX_PROBE_ENTRIES: usize = 4096;
/// Reading the names must not hang the request on a dead network file system.
const PROBE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// At most this many folder reads (child processes) may run at once. A read
/// requested while the cap is reached is not started: the folder counts as
/// unknown and the guard falls back to computation, so a reader hung on a dead
/// file system cannot make further requests pile up readers without bound
/// (decision 0022).
const MAX_READS_IN_FLIGHT: usize = 16;
static READS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// The identity the daemon reads a protected folder's entries with: the peer
/// credentials of the socket connection — the requester's uid and primary gid.
/// The read runs in a child process dropped to this identity with no
/// supplementary groups, so the guard only ever sees what the requester could
/// list themselves and its answer never reveals a name in another user's home
/// directory or in `/root` (decision 0022).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    pub uid: u32,
    pub gid: u32,
}

/// Returns a folder's real top-level entries (name, whether a directory).
/// Injected so tests can supply entries or a reader that must not be called.
type FolderReader = dyn Fn(&Path) -> Vec<(String, bool)>;

/// The final match decision of the whole exclude list for one archive-relative
/// path, exactly as the client applies it: the *last* entry whose pattern
/// matches decides, a `!` re-include can bring the path back, and no match
/// keeps it (so the default is "included").
fn list_excludes_path(entries: &[MatchEntry], rel: &str, mode: u32) -> bool {
    let mut excluded = false;
    for e in entries {
        if e.matches(rel.as_bytes(), Some(mode)) {
            excluded = e.match_type() == MatchType::Exclude;
        }
    }
    excluded
}

/// Whether the exclude list leaves nothing of the folder at archive-relative
/// path of `folder` inside `archive_root`: the folder entry itself is excluded,
/// or every child is — tested against the folder's real top-level entries
/// (`children`); only when those are unknown, against made-up names that
/// catch a pattern matching every possible child.
fn list_empties_folder(
    entries: &[MatchEntry],
    archive_root: &Path,
    folder: &Path,
    children: &[(String, bool)],
) -> bool {
    let Some(rel) = archive_rel(archive_root, folder) else {
        return false;
    };
    let dir = nix::libc::S_IFDIR;
    // The folder is left out when any ancestor directory of it inside the
    // archive — from the first path component down to the folder itself — is
    // excluded: a pattern that takes out a parent (`/var/home` above the home
    // `/var/home/alice`) takes out everything below it. Pure computation; the
    // archive root "/" itself cannot be named here (a pattern that empties it
    // is refused earlier as "excludes everything"). (decision 0022, item 3.)
    for ancestor in ancestor_dirs(&rel) {
        if list_excludes_path(entries, &ancestor, dir) {
            return true;
        }
    }
    let base = if rel == "/" { "" } else { rel.as_str() };
    let probe = |name: &str| list_excludes_path(entries, &format!("{base}/{name}"), dir);
    if children.is_empty() {
        // The real entries are not known (empty, unreadable or too slow to
        // read): only a list that leaves out every kind of made-up name is
        // taken to empty the folder.
        return probe(PROBE_A) && probe(PROBE_B) && PROBES_MORE.iter().all(|p| probe(p));
    }
    // The real top-level entries decide (each with its real file type, so a
    // directory-only pattern does not count a file as removed): the folder
    // is emptied when every one of them is left out. Made-up names are not
    // consulted then, so a list that keeps a real entry (`/alice/*` followed
    // by `!/alice/Documents`) is not refused.
    children.iter().all(|(name, is_dir)| {
        let mode = if *is_dir {
            nix::libc::S_IFDIR
        } else {
            nix::libc::S_IFREG
        };
        list_excludes_path(entries, &format!("{base}/{name}"), mode)
    })
}

/// Every ancestor directory path of `rel` inside the archive, from the first
/// path component down to `rel` itself (archive-relative, each with a leading
/// slash). Empty for the archive root "/".
fn ancestor_dirs(rel: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        cur.push('/');
        cur.push_str(part);
        out.push(cur.clone());
    }
    out
}

/// The path of `folder` inside an archive rooted at `root`, with a leading
/// slash (`/` when `folder == root`); `None` when `folder` is not inside
/// `root`.
fn archive_rel(root: &Path, folder: &Path) -> Option<String> {
    let rel = folder.strip_prefix(root).ok()?;
    if rel.as_os_str().is_empty() {
        return Some("/".to_string());
    }
    Some(format!(
        "/{}",
        String::from_utf8_lossy(rel.as_os_str().as_bytes())
    ))
}

/// The deepest configured source that strictly contains `folder`, if any.
fn deepest_source<'a>(
    folder: &Path,
    sources: &'a [crate::config::Source],
) -> Option<&'a crate::config::Source> {
    sources
        .iter()
        .filter(|s| folder != s.path && folder.starts_with(&s.path))
        .max_by_key(|s| s.path.as_os_str().len())
}

/// The (archive root, protected folder) pairs for the archives the
/// configuration really produces (decision 0022, item 14). In normal mode each
/// configured source is its own archive (`home.pxar` from `/home`), so a home
/// directory inside it is tested there (`/alice` in `home.pxar`) and the source
/// as a whole by emptying its top-level entries. In system mode the operating
/// system is a single `system.pxar` rooted at `/`: the top-level system
/// directories and the home directories are tested there by their full path
/// (`/home` and `/home/alice` in `system.pxar`), and a source that keeps its
/// own archive (a data disk that must be its own mount) is tested separately.
fn protected_folders(
    sources: &[crate::config::Source],
    home_dirs: &[PathBuf],
    system: bool,
) -> Vec<(PathBuf, PathBuf)> {
    let root = PathBuf::from("/");
    let mut out: Vec<(PathBuf, PathBuf)> = Vec::new();
    if system {
        for d in SYSTEM_DIRS {
            out.push((root.clone(), PathBuf::from(d)));
        }
        for h in home_dirs {
            out.push((root.clone(), h.clone()));
        }
        let own: Vec<crate::config::Source> = sources
            .iter()
            .filter(|s| s.require_mount)
            .cloned()
            .collect();
        for s in &own {
            out.push((s.path.clone(), s.path.clone()));
        }
        for h in home_dirs {
            if let Some(src) = deepest_source(h, &own) {
                out.push((src.path.clone(), h.clone()));
            }
        }
    } else {
        for s in sources {
            out.push((s.path.clone(), s.path.clone()));
        }
        for h in home_dirs {
            if let Some(src) = deepest_source(h, sources) {
                out.push((src.path.clone(), h.clone()));
            }
        }
    }
    out
}

/// Caps the number of concurrent folder reads (see [`MAX_READS_IN_FLIGHT`]).
/// The slot is released only when the owning child is fully reaped (`Drop`),
/// so a reader hung on a dead file system keeps its slot until the kernel
/// finally lets it die — exactly the pile-up the bound protects against.
struct InFlightGuard;

impl InFlightGuard {
    /// A slot, or `None` when [`MAX_READS_IN_FLIGHT`] is already reached.
    fn acquire() -> Option<InFlightGuard> {
        let mut cur = READS_IN_FLIGHT.load(Ordering::Relaxed);
        loop {
            if cur >= MAX_READS_IN_FLIGHT {
                return None;
            }
            match READS_IN_FLIGHT.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(InFlightGuard),
                Err(now) => cur = now,
            }
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        READS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Real top-level entry names (and whether each is a directory) of `folder`,
/// read with the requester's own identity and never as root, so the guard only
/// ever sees names the requester could list themselves (decision 0022). The
/// read runs in a child process (`find`) dropped to `identity`. A folder the
/// requester cannot list, an empty one, a read that does not finish within
/// [`PROBE_READ_TIMEOUT`], a missing `find`, or more than
/// [`MAX_READS_IN_FLIGHT`] reads already in flight all yield an empty list;
/// the guard then falls back to the made-up names. At most
/// [`MAX_PROBE_ENTRIES`] names are read, only the names, never the contents.
fn real_children(folder: &Path, identity: PeerIdentity) -> Vec<(String, bool)> {
    let Some(guard) = InFlightGuard::acquire() else {
        return Vec::new();
    };
    match spawn_find(folder, identity) {
        Ok(child) => collect_with_timeout(child, guard),
        Err(_) => Vec::new(),
    }
}

/// Start `find` listing the top-level entries of `folder` as `identity`. When
/// the daemon runs as root it drops the child to `identity`'s uid and primary
/// gid and clears the supplementary groups (as `std::process::Command` does
/// when a uid/gid is set as root), so the child sees exactly what the requester
/// would. When the process is not root it can only read as itself: the read is
/// started only when `identity` is the current user, otherwise it is refused
/// (the caller then treats the folder as unknown). The child runs no nestegg
/// code; `find` is part of the required `findutils` package on Debian/Ubuntu,
/// and a missing one is handled as "unknown".
fn spawn_find(folder: &Path, identity: PeerIdentity) -> std::io::Result<std::process::Child> {
    let mut cmd = Command::new("find");
    // Only the direct children, printed as `<type><name>\0`: the type is one
    // byte (`d` for a directory) and the NUL separates records, so a name with
    // spaces or newlines cannot confuse the parse.
    cmd.arg(folder)
        .args(["-maxdepth", "1", "-mindepth", "1", "-printf", "%y%f\\0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if nix::unistd::geteuid().is_root() {
        let (uid, gid) = (identity.uid, identity.gid);
        // Drop to the requester before `exec`, in the forked child: clear the
        // supplementary groups, then the gid, then the uid (order matters — once
        // the uid is dropped the group changes are no longer permitted). Only
        // async-signal-safe syscalls run here. `std::os::unix`'s own `groups`
        // helper is still unstable, so the drop is done by hand.
        unsafe {
            cmd.pre_exec(move || {
                if nix::libc::setgroups(0, std::ptr::null()) != 0
                    || nix::libc::setgid(gid as nix::libc::gid_t) != 0
                    || nix::libc::setuid(uid as nix::libc::uid_t) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    } else if identity.uid != nix::unistd::geteuid().as_raw()
        || identity.gid != nix::unistd::getegid().as_raw()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cannot read a folder as another user without root",
        ));
    }
    cmd.spawn()
}

/// Read a spawned `find`'s output with a time limit. On time-out the child is
/// killed and reaped on a detached thread, so a reader hung on a dead file
/// system neither blocks the request (which holds the settings lock) nor leaks
/// a thread; the in-flight slot (`guard`) is released only once the child is
/// finally reaped, so the bound keeps such stuck reads from piling up.
fn collect_with_timeout(
    mut child: std::process::Child,
    guard: InFlightGuard,
) -> Vec<(String, bool)> {
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Vec::new();
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let _ = tx.send(parse_find_output(BufReader::new(stdout)));
    });
    match rx.recv_timeout(PROBE_READ_TIMEOUT) {
        Ok(entries) => {
            // The reader hit EOF or the entry bound: the child is done or will
            // get EPIPE on its next write; reap it so no zombie is left.
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            drop(guard);
            entries
        }
        Err(_) => {
            // Timed out: kill now, reap in the background so the request is not
            // slowed down by a child stuck on a dead file system.
            let _ = child.kill();
            std::thread::spawn(move || {
                let _ = reader.join();
                let _ = child.wait();
                drop(guard);
            });
            Vec::new()
        }
    }
}

/// Parse `find … -printf '%y%f\0'`: one NUL-terminated record per entry, the
/// first byte the type (`d` for a directory) and the rest the name. At most
/// [`MAX_PROBE_ENTRIES`] entries and a bounded number of bytes are read, so a
/// folder with very many or very long names cannot grow the daemon's memory;
/// names that are not valid UTF-8 are skipped.
fn parse_find_output(mut reader: impl BufRead) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut rec: Vec<u8> = Vec::new();
    let mut total: usize = 0;
    let max_bytes = MAX_PROBE_ENTRIES.saturating_mul(MAX_EXCLUDE_LEN.max(512));
    while out.len() < MAX_PROBE_ENTRIES {
        rec.clear();
        match reader.read_until(0, &mut rec) {
            Ok(0) => break,
            Ok(n) => {
                total = total.saturating_add(n);
                if total > max_bytes {
                    break;
                }
                if rec.last() == Some(&0) {
                    rec.pop();
                }
                let Some((&kind, name)) = rec.split_first() else {
                    continue;
                };
                if let Ok(name) = std::str::from_utf8(name) {
                    out.push((name.to_string(), kind == b'd'));
                }
            }
            Err(_) => break,
        }
    }
    out
}

/// Reject a new exclude list that would, as the client applies it, leave out a
/// whole configured source, a whole home directory of a regular user, or (in
/// system mode) a whole top-level system directory — but only when the *old*
/// list did not already leave it out, so a dangerous pattern root put in the
/// file by hand is grandfathered and a plain reorder is caught only when it
/// changes the outcome (decision 0022, items 5, 9). `read_children` reads a
/// folder's real top-level entry names (injected for tests). Returns the
/// plain-words reason, or `None` when the change is fine. This is a guard
/// against accidents, not a security boundary: a group member can still exclude
/// folders one by one, which is reported.
fn excludes_reduce_protection(
    old_list: &[String],
    new_list: &[String],
    sources: &[crate::config::Source],
    home_dirs: &[PathBuf],
    system: bool,
    read_children: &FolderReader,
) -> Option<String> {
    let old = match_entries(old_list);
    let new = match_entries(new_list);
    for (archive_root, folder) in protected_folders(sources, home_dirs, system) {
        let children = read_children(&folder);
        if !list_empties_folder(&new, &archive_root, &folder, &children) {
            continue;
        }
        if list_empties_folder(&old, &archive_root, &folder, &children) {
            // Already left out before this change: keep it (root may have put
            // it there), so the rest of the list can still be edited.
            continue;
        }
        return Some(format!(
            "this exclude list would leave out the whole folder {} from the backup; \
             an administrator can still set it as root in /etc/nestegg/config.toml.",
            folder.display()
        ));
    }
    None
}

/// The home directories the exclude guard needs — read from the account
/// database only when the exclude list actually changes, so an ordinary
/// interval or timer change does not walk NSS on every request (decision 0022,
/// item 6). When no regular user account is found at all, the home-directory
/// part of the guard is off, which is logged so it is not a silent gap.
fn excludes_guard_home_dirs(patch: &SettingsPatch, current: &Config) -> Vec<PathBuf> {
    let changed = patch
        .excludes
        .as_ref()
        .is_some_and(|new| new != &current.backup.excludes);
    if !changed {
        return Vec::new();
    }
    let homes = regular_home_dirs();
    if homes.is_empty() {
        eprintln!(
            "warning: no regular user account found; the exclude guard for home \
             directories is off for this settings change"
        );
    }
    homes
}

/// The home directories of regular users, from the account database (NSS, not
/// a directory listing), for the exclude guard (decision 0022). Only uids in
/// the regular range with an absolute, non-root home are returned.
pub fn regular_home_dirs() -> Vec<PathBuf> {
    use std::sync::Mutex;
    // `getpwent` walks a process-global cursor, so serialise the walk.
    static PWENT: Mutex<()> = Mutex::new(());
    let _guard = PWENT.lock().unwrap_or_else(|e| e.into_inner());
    let regular = crate::notify::regular_uids();
    let mut out: Vec<PathBuf> = Vec::new();
    // SAFETY: the walk is serialised by the mutex above; the returned pointers
    // are read before the next `getpwent`/`endpwent` and not kept.
    unsafe {
        nix::libc::setpwent();
        loop {
            let ent = nix::libc::getpwent();
            if ent.is_null() {
                break;
            }
            let uid = (*ent).pw_uid;
            if !regular.contains(&uid) {
                continue;
            }
            if (*ent).pw_dir.is_null() {
                continue;
            }
            let dir = std::ffi::CStr::from_ptr((*ent).pw_dir);
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes()));
            if path.is_absolute() && path != Path::new("/") && !out.contains(&path) {
                out.push(path);
            }
        }
        nix::libc::endpwent();
    }
    out
}

/// Apply `patch` to the text of the configuration file. Only the touched
/// keys change; comments, order and formatting of everything else stay.
pub fn rewrite(text: &str, patch: &SettingsPatch) -> Result<String> {
    let mut doc: DocumentMut = text.parse().context("cannot parse the configuration")?;
    if let Some(hours) = patch.overdue_after_hours {
        set_value(doc.as_table_mut(), "overdue_after_hours", i64::from(hours));
    }
    if let Some(hours) = patch.interval_hours {
        set_value(
            table_mut(&mut doc, "schedule")?,
            "interval_hours",
            i64::from(hours),
        );
    }
    if let Some(list) = &patch.excludes {
        let backup = table_mut(&mut doc, "backup")?;
        let old = backup
            .get("excludes")
            .and_then(Item::as_value)
            .and_then(Value::as_array);
        let array = rebuild_array(old, list);
        set_value(backup, "excludes", array);
    }
    if let Some(on) = patch.wake_on_lan {
        match doc.get_mut("wake").and_then(Item::as_table_like_mut) {
            Some(wake) => {
                // `enabled` defaults to true: leave the file alone when
                // switching on a [wake] section that never had the key.
                if !on || wake.contains_key("enabled") {
                    set_value(wake, "enabled", on);
                }
            }
            None if on => bail!("the configuration has no [wake] section"),
            None => {}
        }
    }
    Ok(doc.to_string())
}

/// The table `name` of the document, created at the end if missing.
fn table_mut<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut dyn TableLike> {
    if !doc.contains_key(name) {
        doc.insert(name, Item::Table(Table::new()));
    }
    doc.get_mut(name)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| anyhow!("`{name}` in the configuration is not a table"))
}

/// Set `key` to `value`, keeping the key and the decoration of the old
/// value (spacing and a trailing `# comment`).
fn set_value(table: &mut dyn TableLike, key: &str, value: impl Into<Value>) {
    let mut value = value.into();
    match table.get_mut(key) {
        Some(Item::Value(old)) => {
            *value.decor_mut() = old.decor().clone();
            *old = value;
        }
        Some(item) => *item = Item::Value(value),
        None => {
            table.insert(key, Item::Value(value));
        }
    }
}

/// A string array with the items of `list`. Items that were already in
/// `old` are reused with their formatting and comments (also a comment
/// behind the item on the same line); a multi-line array stays multi-line.
fn rebuild_array(old: Option<&Array>, list: &[String]) -> Array {
    let mut array = Array::new();
    let Some(old) = old.filter(|_| !list.is_empty()) else {
        for item in list {
            array.push(item.as_str());
        }
        return array;
    };
    let raw =
        |r: Option<&toml_edit::RawString>| r.and_then(|r| r.as_str()).unwrap_or("").to_string();
    let prefixes: Vec<String> = old.iter().map(|v| raw(v.decor().prefix())).collect();
    let trailing = raw(Some(old.trailing()));
    let multiline = prefixes.iter().any(|p| p.contains('\n')) || trailing.contains('\n');
    if !multiline {
        for item in list {
            array.push(item.as_str());
        }
        array.fmt();
        return array;
    }
    // In a multi-line array, the text between an item's comma and the end of
    // its line (a `# comment`) is stored in front of the next item. Split
    // every prefix at its first line break: the part before belongs to the
    // previous item (`tail`), the part after to the item itself (`own`).
    let split = |p: &str| match p.split_once('\n') {
        Some((before, after)) => (before.to_string(), after.to_string()),
        None => (String::new(), p.to_string()),
    };
    // An empty multi-line array (all items commented out) has no prefixes.
    let (open_tail, _) = split(prefixes.first().map_or("", String::as_str));
    let (mut last_tail, mut closing) = split(&trailing);
    // Without a trailing comma, the comment behind the last item is part of
    // that item's suffix.
    let last_suffix = raw(old.iter().last().and_then(|v| v.decor().suffix()));
    if let Some((before, after)) = last_suffix.split_once('\n')
        && !trailing.contains('\n')
    {
        last_tail = before.to_string();
        closing = format!("{after}{trailing}");
    }
    let indent = prefixes
        .iter()
        .find_map(|p| p.rsplit_once('\n').map(|(_, indent)| indent.to_string()))
        .unwrap_or_else(|| "    ".to_string());
    // (value, own prefix, tail) of every old item, consumed when reused.
    let mut unused: Vec<Option<(Value, String, String)>> = old
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let own = split(&prefixes[i]).1;
            let tail = match prefixes.get(i + 1) {
                Some(next) => split(next).0,
                None => last_tail.clone(),
            };
            Some((v.clone(), own, tail))
        })
        .collect();
    let mut before = open_tail;
    for item in list {
        let reused = unused
            .iter_mut()
            .find(|e| e.as_ref().and_then(|(v, _, _)| v.as_str()) == Some(item.as_str()))
            .and_then(Option::take);
        let (mut value, own, tail) =
            reused.unwrap_or_else(|| (Value::from(item.as_str()), indent.clone(), String::new()));
        value.decor_mut().set_prefix(format!("{before}\n{own}"));
        if raw(value.decor().suffix()).contains('\n') {
            value.decor_mut().set_suffix("");
        }
        array.push_formatted(value);
        before = tail;
    }
    array.set_trailing_comma(true);
    array.set_trailing(format!("{before}\n{closing}"));
    array
}

/// The `systemctl` side of `automatic_backups`; replaceable in tests.
pub trait Timer {
    fn enabled(&self) -> bool;
    fn set(&self, enabled: bool) -> Result<()>;
}

/// `nestegg-backup.timer`, switched with `systemctl enable|disable --now`.
pub struct SystemdTimer<'a> {
    pub systemctl: &'a Path,
}

impl Timer for SystemdTimer<'_> {
    fn enabled(&self) -> bool {
        crate::status::systemctl_is_enabled(self.systemctl)
    }

    fn set(&self, enabled: bool) -> Result<()> {
        let verb = if enabled { "enable" } else { "disable" };
        let out = Command::new(self.systemctl)
            .args([verb, "--now", TIMER_UNIT])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| {
                format!(
                    "cannot run {} (automatic backups need systemd)",
                    self.systemctl.display()
                )
            })?;
        if !out.status.success() {
            bail!(
                "systemctl {verb} --now {TIMER_UNIT} failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

/// Result of an [`update`] whose change took effect, in whole or in part.
#[derive(Debug)]
pub struct Outcome {
    pub config: Config,
    pub timer_enabled: bool,
    /// Human-readable list of what changed (for the journal), empty if
    /// nothing did.
    pub changes: Vec<String>,
    /// Changes that lowered the protection, for the `settings` notification
    /// (decision 0022). Empty if the change only raised it. Reported even when
    /// `error` is set: the change took effect, so it must be announced.
    pub reductions: Vec<Reduction>,
    /// A problem that happened after the change already took effect (the timer
    /// could not be switched, the old file could not be put back). The settings
    /// are saved; the caller returns this as an error to the client but still
    /// reports the reductions.
    pub error: Option<anyhow::Error>,
}

/// Validate and apply a change: rewrite the configuration file, then switch
/// the timer. If the timer cannot be switched, the old file is put back, so
/// a failed request changes nothing. The caller serialises calls.
pub fn update(
    config_file: &Path,
    patch: &SettingsPatch,
    timer: &dyn Timer,
    identity: Option<PeerIdentity>,
) -> Result<Outcome, SettingsError> {
    let old_text = fs::read_to_string(config_file)
        .with_context(|| format!("cannot read configuration {}", config_file.display()))?;
    let current = Config::parse(&old_text).with_context(|| {
        format!(
            "invalid configuration {}; fix it in the terminal first",
            config_file.display()
        )
    })?;
    validate(
        patch,
        &current,
        &excludes_guard_home_dirs(patch, &current),
        identity,
    )?;
    let new_text = rewrite(&old_text, patch)?;
    let new = Config::parse(&new_text).map_err(|e| {
        invalid(format!(
            "the change would make the configuration invalid: {e:#}"
        ))
    })?;
    check_applied(&new, patch)?;
    let mut changes = describe(&current, &new);
    let wrote = new_text != old_text;
    if wrote {
        write_atomically(config_file, &new_text)?;
    }
    let was_enabled = timer.enabled();
    let mut timer_enabled = was_enabled;
    // A problem that happens after the file was already written. The settings
    // then took effect, so the outcome still carries the reductions to report
    // (decision 0022); only the timer switch or the rollback failed.
    let mut after_write_error: Option<anyhow::Error> = None;
    if let Some(on) = patch.automatic_backups
        && on != was_enabled
    {
        if let Err(e) = timer.set(on) {
            let what = if on { "on" } else { "off" };
            // `systemctl enable --now` links the unit before starting it (and
            // `disable --now` unlinks before stopping), so a failure can leave
            // the switch changed. Report what actually happened then.
            if timer.enabled() == on {
                timer_enabled = on;
                changes.push(format!(
                    "automatic_backups: {} -> {}",
                    on_off(was_enabled),
                    on_off(on)
                ));
                after_write_error = Some(e.context(format!(
                    "automatic backups were switched {what} and the settings saved, but the timer could not be {}",
                    if on { "started" } else { "stopped" }
                )));
            } else if wrote && let Err(restore) = write_atomically(config_file, &old_text) {
                // The timer did not switch and the other settings could not be
                // put back: they took effect, so they are still reported.
                after_write_error = Some(anyhow!(
                    "cannot switch automatic backups {what}: {e:#}; the other settings were \
                     saved, putting the old file back failed: {restore:#}"
                ));
            } else {
                // Nothing took effect: a clean refusal, no report.
                return Err(SettingsError::Failed(e.context(format!(
                    "cannot switch automatic backups {what}; nothing was changed"
                ))));
            }
        } else {
            timer_enabled = on;
            changes.push(format!(
                "automatic_backups: {} -> {}",
                on_off(was_enabled),
                on_off(on)
            ));
        }
    }
    // Whenever this point is reached with `after_write_error` set, the new file
    // is in place (either the timer switched but could not start, or the
    // rollback itself failed); the clean "nothing changed" case returned above.
    let reductions = reductions(&current, &new, was_enabled, timer_enabled);
    Ok(Outcome {
        config: new,
        timer_enabled,
        changes,
        reductions,
        error: after_write_error,
    })
}

/// Guard against a rewrite that did not end up where `Config` reads it.
fn check_applied(new: &Config, patch: &SettingsPatch) -> Result<()> {
    let ok = patch
        .interval_hours
        .is_none_or(|h| new.schedule.interval_hours == h)
        && patch
            .overdue_after_hours
            .is_none_or(|h| new.overdue_after_hours == h)
        && patch
            .excludes
            .as_ref()
            .is_none_or(|e| &new.backup.excludes == e)
        && patch.wake_on_lan.is_none_or(|on| {
            new.wake.is_none() || new.wake.as_ref().is_some_and(|w| w.enabled == on)
        });
    if !ok {
        bail!("the configuration file could not be changed as requested; edit it in the terminal");
    }
    Ok(())
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

fn describe(old: &Config, new: &Config) -> Vec<String> {
    let mut changes = Vec::new();
    if old.schedule.interval_hours != new.schedule.interval_hours {
        changes.push(format!(
            "interval_hours: {} -> {}",
            old.schedule.interval_hours, new.schedule.interval_hours
        ));
    }
    if old.overdue_after_hours != new.overdue_after_hours {
        changes.push(format!(
            "overdue_after_hours: {} -> {}",
            old.overdue_after_hours, new.overdue_after_hours
        ));
    }
    let added: Vec<&String> = new
        .backup
        .excludes
        .iter()
        .filter(|p| !old.backup.excludes.contains(p))
        .collect();
    let removed: Vec<&String> = old
        .backup
        .excludes
        .iter()
        .filter(|p| !new.backup.excludes.contains(p))
        .collect();
    if !added.is_empty() || !removed.is_empty() {
        changes.push(format!("excludes: added {added:?}, removed {removed:?}"));
    } else if old.backup.excludes != new.backup.excludes {
        changes.push("excludes: reordered".into());
    }
    let wake = |c: &Config| c.active_wake().is_some();
    if wake(old) != wake(new) {
        changes.push(format!(
            "wake_on_lan: {} -> {}",
            on_off(wake(old)),
            on_off(wake(new))
        ));
    }
    changes
}

/// Write `text` to `path` atomically with a fixed `mode`: a new temporary file
/// (exclusive, no symlinks) is created with the final mode from the start,
/// written, synced and renamed over any old file, and the directory is synced.
/// A symlink at `path` is refused rather than followed and truncated. Unlike
/// [`write_atomically`] the file need not already exist, so `nestegg init` can
/// create the configuration this way with no window at a looser mode and no
/// symlink following (decision 0022, item 14).
pub fn write_file_atomic(path: &Path, text: &str, mode: u32) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        bail!(
            "{} is a symbolic link; refusing to overwrite it (remove it first)",
            path.display()
        );
    }
    let dir = path.parent().context("path without parent")?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("path without file name")?;
    let tmp = dir.join(format!(".{name}.{:016x}.tmp", rand::random::<u64>()));
    let write = || -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&tmp)
            .with_context(|| format!("cannot create {}", tmp.display()))?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        Ok(())
    };
    let result = write().and_then(|()| {
        fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))
    });
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Replace `path` atomically with `text`: a new file in the same directory
/// (exclusive, no symlinks) gets the owner and mode of the old one, is
/// synced and renamed over it. A symlink at `path` is refused instead of
/// being replaced by a regular file.
pub fn write_atomically(path: &Path, text: &str) -> Result<()> {
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    if meta.file_type().is_symlink() {
        bail!(
            "{} is a symbolic link; change the settings in the terminal",
            path.display()
        );
    }
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let dir = path.parent().context("path without parent")?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("path without file name")?;
    let tmp = dir.join(format!(".{name}.{:016x}.tmp", rand::random::<u64>()));
    let write = || -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&tmp)
            .with_context(|| format!("cannot create {}", tmp.display()))?;
        let created = file.metadata()?;
        if (created.uid(), created.gid()) != (meta.uid(), meta.gid()) {
            nix::unistd::fchown(
                &file,
                Some(nix::unistd::Uid::from_raw(meta.uid())),
                Some(nix::unistd::Gid::from_raw(meta.gid())),
            )
            .with_context(|| format!("cannot set the owner of {}", tmp.display()))?;
        }
        file.set_permissions(fs::Permissions::from_mode(meta.mode() & 0o7777))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        Ok(())
    };
    let result = write().and_then(|()| {
        fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))
    });
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // Make the rename itself durable.
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// A hand-written file with comments in all the places users put them.
    const COMMENTED: &str = r#"# My backup settings - keep this comment.
backup_id = "desktop"
overdue_after_hours = 36 # a day and a half

[pbs]
# The server in the basement.
server = "pbs.example.com"
datastore = "backups"
auth_id = "backup@pbs!desktop"

[wake]
mac = "00:00:5e:00:53:01"   # PBS NIC
timeout_seconds = 300

[backup]
encryption = true
excludes = [
    "**/.cache",      # browser caches
    "**/node_modules",
    "**/.local/share/Trash",
]

[schedule]
# Daily is enough.
interval_hours = 24
"#;

    fn patch() -> SettingsPatch {
        SettingsPatch::default()
    }

    fn current() -> Config {
        Config::parse(COMMENTED).unwrap()
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// Test shim for [`super::validate`]: these checks exercise everything but
    /// the folder reader (the reader is covered through `guard_with` and
    /// `validate_with_reader`), so they run with no requester identity — every
    /// folder counts as unknown, judged by computation alone.
    fn validate(
        patch: &SettingsPatch,
        current: &Config,
        home_dirs: &[PathBuf],
    ) -> Result<(), SettingsError> {
        super::validate(patch, current, home_dirs, None)
    }

    /// Test shim for [`super::update`]: no requester identity (see `validate`).
    fn update(
        config_file: &Path,
        patch: &SettingsPatch,
        timer: &dyn Timer,
    ) -> Result<Outcome, SettingsError> {
        super::update(config_file, patch, timer, None)
    }

    /// The current process's own identity, for the real-process reader tests.
    fn current_ids() -> PeerIdentity {
        PeerIdentity {
            uid: nix::unistd::geteuid().as_raw(),
            gid: nix::unistd::getegid().as_raw(),
        }
    }

    /// Serialises the tests that take in-flight reader slots, so the global
    /// in-flight counter is not disturbed by a concurrent reader test.
    static READER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Whether `find` (findutils) is on this host; the reader tests skip
    /// without it, since a missing tool is handled as "unknown".
    fn find_available() -> bool {
        Command::new("find")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn body_rejects_terminal_only_and_unknown_keys() {
        let ok = SettingsPatch::from_json(serde_json::json!({"interval_hours": 12})).unwrap();
        assert_eq!(ok.interval_hours, Some(12));
        let null = SettingsPatch::from_json(serde_json::json!({"interval_hours": null})).unwrap();
        assert_eq!(null, patch());
        // The notification hook runs as root: never editable through the API.
        for key in [
            "server",
            "sources",
            "auth_id",
            "encryption",
            "backup_id",
            "notify",
            "command",
            "desktop",
        ] {
            let e = SettingsPatch::from_json(serde_json::json!({ key: "x" })).unwrap_err();
            assert!(
                e.to_string().contains("cannot be changed here"),
                "{key}: {e}"
            );
        }
        let e = SettingsPatch::from_json(serde_json::json!({"intervall": 1})).unwrap_err();
        assert!(
            e.to_string().contains("unknown setting \"intervall\""),
            "{e}"
        );
        // Control characters in a key are escaped, so a key cannot forge
        // a line in the journal.
        let e = SettingsPatch::from_json(serde_json::json!({"x\nsettings changed by root": 1}))
            .unwrap_err();
        assert!(!e.to_string().contains('\n'), "{e}");
        let e = SettingsPatch::from_json(serde_json::json!({"interval_hours": -1})).unwrap_err();
        assert!(matches!(e, SettingsError::Invalid(_)));
        let e = SettingsPatch::from_json(serde_json::json!({"interval_hours": 1.5})).unwrap_err();
        assert!(matches!(e, SettingsError::Invalid(_)));
        let e = SettingsPatch::from_json(serde_json::json!(["interval_hours"])).unwrap_err();
        assert!(matches!(e, SettingsError::Invalid(_)));
    }

    #[test]
    fn ranges_are_enforced() {
        let c = current(); // interval 24, overdue 36
        let check = |p: SettingsPatch| validate(&p, &c, &[]);
        for h in [0, 169, 1000] {
            let p = SettingsPatch {
                interval_hours: Some(h),
                overdue_after_hours: Some(168),
                ..patch()
            };
            assert!(check(p).is_err(), "interval {h}");
        }
        for h in [1, 2, 168] {
            let p = SettingsPatch {
                interval_hours: Some(1),
                overdue_after_hours: Some(h),
                ..patch()
            };
            assert_eq!(check(p).is_ok(), h == 2 || h == 168, "overdue {h}");
        }
        // With the default interval (24 h) the cap is 7 days (168 h): a larger
        // value is refused (the weekly-rhythm case is covered separately).
        assert!(
            check(SettingsPatch {
                overdue_after_hours: Some(169),
                ..patch()
            })
            .is_err()
        );
        assert!(
            check(SettingsPatch {
                overdue_after_hours: Some(744),
                ..patch()
            })
            .is_err()
        );
        // A larger value already in the file can be lowered, even above the
        // cap, but not raised past what the file already has (decision 0022).
        let mut grandfathered = current();
        grandfathered.overdue_after_hours = 744;
        assert!(
            validate(
                &SettingsPatch {
                    overdue_after_hours: Some(500),
                    ..patch()
                },
                &grandfathered,
                &[],
            )
            .is_ok(),
            "lowering a grandfathered value stays allowed"
        );
        assert!(
            validate(
                &SettingsPatch {
                    overdue_after_hours: Some(745),
                    ..patch()
                },
                &grandfathered,
                &[],
            )
            .is_err(),
            "raising above the file value is refused"
        );
        assert!(
            validate(
                &SettingsPatch {
                    overdue_after_hours: Some(100),
                    ..patch()
                },
                &grandfathered,
                &[],
            )
            .is_ok(),
            "lowering back into the normal range is allowed"
        );
    }

    #[test]
    fn a_weekly_rhythm_allows_a_longer_overdue_threshold() {
        // The cap is max(168, interval + 72): a weekly rhythm (interval 168)
        // can be overdue after up to 240 h (10 days), evaluated against the
        // interval in effect after the same request (decision 0022, item 12).
        let c = current(); // interval 24, overdue 36
        let v = |interval: Option<u32>, overdue: u32, cfg: &Config| {
            validate(
                &SettingsPatch {
                    interval_hours: interval,
                    overdue_after_hours: Some(overdue),
                    ..patch()
                },
                cfg,
                &[],
            )
        };
        // Interval 24 alone: cap 168.
        assert!(v(Some(24), 168, &c).is_ok());
        assert!(v(Some(24), 169, &c).is_err());
        // Interval 48 alone: cap max(168, 120) = 168.
        assert!(v(Some(48), 168, &c).is_ok());
        assert!(v(Some(48), 169, &c).is_err());
        // Interval 168 alone: cap max(168, 240) = 240; must still exceed the
        // interval, so 168 is refused but 169..=240 is allowed.
        assert!(
            v(Some(168), 168, &c).is_err(),
            "overdue must exceed interval"
        );
        assert!(v(Some(168), 169, &c).is_ok());
        assert!(v(Some(168), 240, &c).is_ok());
        assert!(v(Some(168), 241, &c).is_err());
        // Both keys changed together: the cap follows the new interval.
        let mut weekly = c.clone();
        weekly.schedule.interval_hours = 168;
        assert!(
            v(None, 240, &weekly).is_ok(),
            "overdue 240 with weekly in file"
        );
        assert!(v(None, 241, &weekly).is_err());
        // Lowering the interval while "overdue after" stays above the new cap:
        // the value is grandfathered, so only lowering it is then possible.
        let mut weekly_long = c.clone();
        weekly_long.schedule.interval_hours = 168;
        weekly_long.overdue_after_hours = 240;
        // Lower the interval to 24 (new base cap 168) but keep overdue 240.
        assert!(
            v(Some(24), 240, &weekly_long).is_ok(),
            "keeping the grandfathered overdue while lowering the interval is allowed"
        );
        assert!(
            v(Some(24), 230, &weekly_long).is_ok(),
            "lowering the overdue is allowed"
        );
        assert!(
            v(Some(24), 241, &weekly_long).is_err(),
            "raising past the grandfathered value is refused"
        );
    }

    #[test]
    fn overdue_must_exceed_interval() {
        let c = current(); // interval 24, overdue 36
        let e = validate(
            &SettingsPatch {
                interval_hours: Some(48),
                ..patch()
            },
            &c,
            &[],
        )
        .unwrap_err();
        assert!(
            e.to_string()
                .contains("must be greater than interval_hours"),
            "{e}"
        );
        assert!(
            validate(
                &SettingsPatch {
                    overdue_after_hours: Some(24),
                    ..patch()
                },
                &c,
                &[],
            )
            .is_err()
        );
        assert!(
            validate(
                &SettingsPatch {
                    interval_hours: Some(48),
                    overdue_after_hours: Some(60),
                    ..patch()
                },
                &c,
                &[],
            )
            .is_ok()
        );
        // An existing file that breaks the rule can still get other changes.
        let mut odd = c.clone();
        odd.overdue_after_hours = 12;
        assert!(
            validate(
                &SettingsPatch {
                    excludes: Some(vec![]),
                    ..patch()
                },
                &odd,
                &[],
            )
            .is_ok()
        );
    }

    #[test]
    fn exclude_validation() {
        for good in [
            "**/.cache",
            "/home/*/Videos",
            "*.iso",
            "!**/keep",
            "?",
            "[a-z]*",
        ] {
            assert!(validate_exclude(good).is_ok(), "{good}");
        }
        for bad in [
            "", "!", "*", "**", "/", "//", "/*", "/**", "**/*", "*/*", "**/", "?*", " x", "x ",
            "a\nb", "a\0b", "a\tb",
            // Patterns the backup client cannot parse: an unclosed character
            // class (panics the parser), a dangling range, a trailing backslash
            // (decision 0022, item 4). None may escape as a panic.
            "[abc", "a[b", "foo[bar", "[a-]", "[a-", "foo\\", "x[^",
        ] {
            assert!(validate_exclude(bad).is_err(), "{bad:?}");
        }
        assert!(validate_exclude(&"a".repeat(MAX_EXCLUDE_LEN)).is_ok());
        assert!(validate_exclude(&"a".repeat(MAX_EXCLUDE_LEN + 1)).is_err());
        // The limit is in bytes: a pattern of multi-byte characters that is
        // short in characters but long in bytes is refused (decision 0022).
        let two_byte = "ä".repeat(MAX_EXCLUDE_LEN); // 2 bytes each => over the limit
        assert_eq!(two_byte.chars().count(), MAX_EXCLUDE_LEN);
        assert!(two_byte.len() > MAX_EXCLUDE_LEN);
        assert!(validate_exclude(&two_byte).is_err());
        // Invisible format characters are rejected like control characters.
        assert!(validate_exclude("foo\u{200b}bar").is_err());
        assert!(validate_exclude("a\u{202e}b").is_err());
        assert!(validate_excludes(&strings(&["a", "a"])).is_err());
        let many: Vec<String> = (0..=MAX_EXCLUDES).map(|i| format!("p{i}")).collect();
        assert!(validate_excludes(&many).is_err());
        assert!(validate_excludes(&many[..MAX_EXCLUDES]).is_ok());
        assert!(validate_excludes(&[]).is_ok());
    }

    /// Test harness for the whole-list guard: add `new` as the only exclude
    /// list (old list empty) and report whether it is refused, with the given
    /// real top-level entries per folder (default: none, so only the probes and
    /// the folder entry decide). The expected outcomes were confirmed with the
    /// real tool (`pxar create --exclude`) in the e2e client image.
    fn guard(new: &[&str], system: bool) -> Option<String> {
        guard_with(&[], new, system, &[])
    }

    fn guard_with(
        old: &[&str],
        new: &[&str],
        system: bool,
        children: &[(&str, &[(&str, bool)])],
    ) -> Option<String> {
        let sources = current().backup.sources;
        let homes = [PathBuf::from("/home/alice"), PathBuf::from("/home/bob")];
        let owned: Vec<(PathBuf, Vec<(String, bool)>)> = children
            .iter()
            .map(|(folder, entries)| {
                (
                    PathBuf::from(folder),
                    entries
                        .iter()
                        .map(|(n, d)| (n.to_string(), *d))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        let read = move |folder: &Path| -> Vec<(String, bool)> {
            owned
                .iter()
                .find(|(f, _)| f == folder)
                .map(|(_, e)| e.clone())
                .unwrap_or_default()
        };
        excludes_reduce_protection(
            &strings(old),
            &strings(new),
            &sources,
            &homes,
            system,
            &read,
        )
    }

    #[test]
    fn whole_folder_excludes_are_refused_but_ordinary_ones_pass() {
        // current() has the default sources: /home, /etc, /root, /usr/local.
        // alice and bob are regular users with homes inside the /home source.
        // Excludes are matched archive-relative, so in the default (non-system)
        // configuration each source is its own archive (home.pxar from /home,
        // with entries /alice, /bob): a pattern that empties a home directory is
        // refused (decision 0022, item 5).
        for bad in [
            "/alice",
            "alice",
            "alice/",
            "**/alice",
            "alice/**", // alice's home in home.pxar
            "/[a-z]*",
            "/a*",
            "/*[a-z0-9]", // match alice (and more) in home.pxar
        ] {
            assert!(guard(&[bad], false).is_some(), "{bad} should be refused");
        }

        // Harmless in the default configuration. The source paths and their
        // bare names (`/home`, `/etc`, `home`, `local`, `/usr/local/*`) match
        // nothing inside their own archive, so excluding them loses nothing
        // there (confirmed with the real tool); they used to be refused with a
        // misleading message (decision 0022, item 14).
        for ok in [
            "/home",
            "/home/",
            "/home/*",
            "/home/**",
            "/etc",
            "/root",
            "/usr/local",
            "/usr/local/*",
            "/usr/local/**",
            "home",
            "etc",
            "local",
            "**/.cache",
            "**/node_modules",
            "**/lost+found",
            "/alice/Downloads", // a subfolder of a home, not the whole home
            "/home/*/Videos",   // a subfolder, not the whole source
            "*.iso",
            "node_modules",
            "/usr/lib",
            "!/home",       // a re-include widens the backup
            "/.cache",      // anchored one component, but not a home
            "/Downloads",   // likewise
            "Downloads",    // a subfolder name, not a home
            "/usr",         // only a system directory in system mode
            "/var",         // likewise
            "/home/[a-z]*", // no-op in home.pxar (entries are /alice, not /home/alice)
            "/home/[!.]*",  // `[!.]` is a class of `!` and `.`, so dot-names only
        ] {
            assert!(guard(&[ok], false).is_none(), "{ok} should pass");
        }

        // System mode backs the operating system up as a single system.pxar
        // rooted at "/": a pattern that names a whole top-level system
        // directory, or a home directory by its full path, is refused there.
        for bad in [
            "/etc",
            "/home",
            "/root",
            "/usr",
            "/var",
            "/opt",
            "/boot",
            "/srv",         // top-level system directories
            "/home/[a-z]*", // matches /home/alice in system.pxar
            "/home/alice",  // a home by its full path
        ] {
            assert!(
                guard(&[bad], true).is_some(),
                "{bad} should be refused in system mode"
            );
        }
        // Still harmless in system mode.
        for ok in [
            "**/.cache",
            "/home/*/Videos",
            "/alice/Downloads",
            "/home/[!.]*",
            "/usr/local", // a subfolder of /usr in system.pxar, not a whole dir
        ] {
            assert!(
                guard(&[ok], true).is_none(),
                "{ok} should pass in system mode"
            );
        }
    }

    #[test]
    fn the_whole_list_is_judged_together() {
        // A character class that skips the two synthetic probes still empties a
        // home directory once its real entries are read (decision 0022, item 5).
        let alice_entries: &[(&str, bool)] =
            &[("Documents", true), ("photos", true), ("notes.txt", false)];
        assert!(
            guard_with(
                &[],
                &["/alice/[.a-zA-Z]*"],
                false,
                &[("/home/alice", alice_entries)],
            )
            .is_some(),
            "a class that covers every real entry empties the home"
        );
        // Several patterns that are each harmless but together leave nothing.
        assert!(
            guard_with(
                &[],
                &[
                    "/alice/[a-m]*",
                    "/alice/[n-z]*",
                    "/alice/[A-Z]*",
                    "/alice/.*"
                ],
                false,
                &[("/home/alice", alice_entries)],
            )
            .is_some(),
            "several patterns together empty the home"
        );
        // One of the real entries survives, so the home is not emptied.
        assert!(
            guard_with(
                &[],
                &["/alice/[a-m]*"],
                false,
                &[("/home/alice", alice_entries)],
            )
            .is_none(),
            "photos and notes.txt survive"
        );
    }

    #[test]
    fn a_big_home_directory_is_judged_by_its_real_entries_too() {
        // A normal desktop home has far more than a few dozen entries; the
        // guard must not fall back to made-up names there.
        let names: Vec<String> = (0..200).map(|n| format!("Dir{n}")).collect();
        let entries: Vec<(&str, bool)> = names.iter().map(|n| (n.as_str(), true)).collect();
        for list in [
            &["/alice/[a-zA-Z.]*"][..],
            &[
                "/alice/[a-m]*",
                "/alice/[n-z]*",
                "/alice/[A-Z]*",
                "/alice/.*",
            ][..],
        ] {
            assert!(
                guard_with(&[], list, false, &[("/home/alice", &entries)]).is_some(),
                "{list:?} empties a home with 200 folders"
            );
        }
        assert!(
            guard_with(&[], &["/alice/Dir1*"], false, &[("/home/alice", &entries)]).is_none(),
            "most folders stay"
        );
    }

    #[test]
    fn the_child_reader_reads_a_folder_as_the_requester() {
        let _serial = READER_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if !find_available() {
            eprintln!("skipping: `find` (findutils) is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        for n in 0..200 {
            fs::create_dir(dir.path().join(format!("Dir{n}"))).unwrap();
        }
        fs::write(dir.path().join("notes.txt"), "x").unwrap();
        // Read as the current user (the daemon reads as root; a unit test runs
        // unprivileged, so the identity is the current user, which `spawn_find`
        // runs as without a privilege change).
        let read = real_children(dir.path(), current_ids());
        assert_eq!(read.len(), 201, "200 folders and one file");
        assert_eq!(read.iter().filter(|(_, is_dir)| *is_dir).count(), 200);
        assert!(
            read.iter().any(|(n, is_dir)| n == "notes.txt" && !is_dir),
            "the file is seen as a non-directory"
        );
        // A folder that does not exist yields "unknown" (empty).
        assert!(real_children(&dir.path().join("missing"), current_ids()).is_empty());

        // A folder the requester cannot list counts as "unknown". Only
        // meaningful unprivileged: root bypasses the mode bits.
        if nix::unistd::geteuid().is_root() {
            eprintln!("skipping the mode-000 case: running as root bypasses permissions");
        } else {
            let locked = dir.path().join("locked");
            fs::create_dir(&locked).unwrap();
            fs::create_dir(locked.join("inside")).unwrap();
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
            let got = real_children(&locked, current_ids());
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(got.is_empty(), "an unreadable folder is unknown, not read");
        }
    }

    #[test]
    fn parse_find_output_bounds_entries_and_skips_bad_names() {
        // One record per entry: first byte the type, then the name, NUL-ended.
        let mut input: Vec<u8> = Vec::new();
        input.extend_from_slice(b"dDocuments\0");
        input.extend_from_slice(b"fnotes.txt\0");
        input.extend_from_slice(b"dwith space and\nnewline\0");
        // A name that is not valid UTF-8 is skipped, not guessed.
        input.push(b'f');
        input.extend_from_slice(&[0xff, 0xfe]);
        input.push(0);
        let got = parse_find_output(std::io::Cursor::new(input));
        assert_eq!(
            got,
            vec![
                ("Documents".to_string(), true),
                ("notes.txt".to_string(), false),
                ("with space and\nnewline".to_string(), true),
            ]
        );
        // The entry bound holds even for a flood of records.
        let mut many: Vec<u8> = Vec::new();
        for i in 0..(MAX_PROBE_ENTRIES + 500) {
            many.extend_from_slice(format!("df{i}\0").as_bytes());
        }
        assert_eq!(
            parse_find_output(std::io::Cursor::new(many)).len(),
            MAX_PROBE_ENTRIES
        );
    }

    #[test]
    fn a_hung_reader_is_cut_off_and_does_not_slow_the_next_read() {
        let _serial = READER_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A child that never produces output (stand-in for a dead file system):
        // it is killed at the timeout and the call returns empty, promptly.
        let Ok(child) = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            eprintln!("skipping: `sleep` is not available");
            return;
        };
        let guard = InFlightGuard::acquire().expect("a slot is free");
        let start = std::time::Instant::now();
        let out = collect_with_timeout(child, guard);
        let elapsed = start.elapsed();
        assert!(out.is_empty());
        assert!(
            elapsed < PROBE_READ_TIMEOUT + std::time::Duration::from_secs(2),
            "cut off near the timeout, not after sleep ends: {elapsed:?}"
        );
        // The killed child is reaped on a detached thread; wait for it to
        // release its in-flight slot before the next serialised test runs.
        for _ in 0..1000 {
            if READS_IN_FLIGHT.load(Ordering::Relaxed) == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(READS_IN_FLIGHT.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn reads_in_flight_are_bounded() {
        let _serial = READER_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Hold the cap; a further acquire is refused (so that read is judged by
        // computation and does not wait), and freeing one slot lets it in again.
        let mut held: Vec<InFlightGuard> = Vec::new();
        for _ in 0..MAX_READS_IN_FLIGHT {
            held.push(InFlightGuard::acquire().expect("under the cap"));
        }
        assert!(
            InFlightGuard::acquire().is_none(),
            "no more than the cap at once"
        );
        held.pop();
        assert!(
            InFlightGuard::acquire().is_some(),
            "a freed slot can be taken"
        );
    }

    #[test]
    fn real_entries_decide_when_they_are_known_and_probes_only_otherwise() {
        let entries: &[(&str, bool)] = &[("Documents", true), ("Music", true)];
        // Everything below the home is left out, but a real folder is taken
        // back in: not emptied, so not refused (made-up names say nothing
        // when the real ones are known).
        assert!(
            guard_with(
                &[],
                &["/alice/*", "!/alice/Documents"],
                false,
                &[("/home/alice", entries)]
            )
            .is_none()
        );
        assert!(
            guard_with(&[], &["/alice/*"], false, &[("/home/alice", entries)]).is_some(),
            "nothing is taken back in"
        );
        // Entries unknown (unreadable or empty home): only a list that leaves
        // out every kind of name counts as emptying it.
        assert!(guard_with(&[], &["/alice/*"], false, &[]).is_some());
        assert!(
            guard_with(&[], &["/alice/[a-zA-Z.]*"], false, &[]).is_none(),
            "names starting with a digit or an underscore would stay"
        );
    }

    #[test]
    fn the_folder_guard_is_the_last_check_and_an_earlier_refusal_reads_nothing() {
        // A reader that panics if it is ever called: for a request refused for a
        // reason that does not depend on folder contents, no folder is read
        // (decision 0022, item 2). So a refusal never leaks whether a name
        // exists, and the probing-with-a-second-failing-change trick leaves no
        // content-dependent trace.
        let panicking =
            |_: &Path| -> Vec<(String, bool)> { panic!("the folder guard must not run here") };
        let c = current();
        let homes = [PathBuf::from("/home/alice")];
        // Wake-on-LAN without a [wake] section: refused before any read, even
        // though the exclude list alone would empty a home.
        let mut no_wake = c.clone();
        no_wake.wake = None;
        let e = validate_with_reader(
            &SettingsPatch {
                excludes: Some(strings(&["/alice"])),
                wake_on_lan: Some(true),
                ..patch()
            },
            &no_wake,
            &homes,
            &panicking,
        )
        .unwrap_err();
        assert!(e.to_string().contains("[wake]"), "{e}");
        // A malformed pattern is refused before any read.
        let e = validate_with_reader(
            &SettingsPatch {
                excludes: Some(strings(&["[abc"])),
                ..patch()
            },
            &c,
            &homes,
            &panicking,
        )
        .unwrap_err();
        assert!(e.to_string().contains("malformed"), "{e}");
        // A bad interval, with an emptying exclude in the same request: refused
        // for the interval, nothing read.
        let e = validate_with_reader(
            &SettingsPatch {
                interval_hours: Some(0),
                excludes: Some(strings(&["/alice"])),
                ..patch()
            },
            &c,
            &homes,
            &panicking,
        )
        .unwrap_err();
        assert!(e.to_string().contains("interval_hours"), "{e}");
        // When nothing else refuses, the guard does run, as the last step.
        let reached = std::rc::Rc::new(std::cell::Cell::new(false));
        let watching = {
            let reached = reached.clone();
            move |_: &Path| -> Vec<(String, bool)> {
                reached.set(true);
                Vec::new()
            }
        };
        let e = validate_with_reader(
            &SettingsPatch {
                excludes: Some(strings(&["/alice"])),
                ..patch()
            },
            &c,
            &homes,
            &watching,
        )
        .unwrap_err();
        assert!(reached.get(), "the guard ran when nothing else refused");
        assert!(e.to_string().contains("whole folder"), "{e}");
    }

    #[test]
    fn an_excluded_parent_empties_a_folder_below_it() {
        // In system mode the whole system is one archive rooted at "/". A home
        // below an intermediate directory (`/var/home/alice`) is emptied by a
        // pattern that excludes the parent (`/var/home`), although the pattern
        // never names the home itself — the parent takes everything below it
        // with it (decision 0022, item 3). Pure computation, so the reader is
        // never consulted (identity `None`).
        let mut c = current();
        c.backup.system = true;
        let homes = [PathBuf::from("/var/home/alice")];
        assert!(
            super::validate(
                &SettingsPatch {
                    excludes: Some(strings(&["/var/home"])),
                    ..patch()
                },
                &c,
                &homes,
                None,
            )
            .is_err(),
            "excluding the parent /var/home empties /var/home/alice"
        );
        // A directory that is not an ancestor of the home does not.
        assert!(
            super::validate(
                &SettingsPatch {
                    excludes: Some(strings(&["/var/cache"])),
                    ..patch()
                },
                &c,
                &homes,
                None,
            )
            .is_ok(),
            "an unrelated directory leaves the home alone"
        );
        // The computation also sees it through `ancestor_dirs` directly.
        assert_eq!(
            ancestor_dirs("/var/home/alice"),
            vec![
                "/var".to_string(),
                "/var/home".to_string(),
                "/var/home/alice".to_string()
            ]
        );
        assert!(ancestor_dirs("/").is_empty());
    }

    #[test]
    fn reordering_next_to_another_change_is_reported() {
        let cfg = |excludes: &[&str]| {
            let mut c = current();
            c.backup.excludes = excludes.iter().map(|s| s.to_string()).collect();
            c
        };
        // The re-include moves in front of the exclude it tamed, and an
        // unrelated pattern is removed in the same request.
        let r = reductions(
            &cfg(&["*.bak", "!keep.bak", "y"]),
            &cfg(&["!keep.bak", "*.bak"]),
            true,
            true,
        );
        assert!(r.contains(&Reduction::ExcludesReordered), "{r:?}");
        // The same order of what both lists have, with something removed: no
        // reorder.
        let r = reductions(
            &cfg(&["*.bak", "!keep.bak", "y"]),
            &cfg(&["*.bak", "!keep.bak"]),
            true,
            true,
        );
        assert!(!r.contains(&Reduction::ExcludesReordered), "{r:?}");
        // Without a re-include the order of plain excludes means nothing.
        let r = reductions(&cfg(&["a", "b", "c"]), &cfg(&["c", "a"]), true, true);
        assert!(!r.contains(&Reduction::ExcludesReordered), "{r:?}");
        // A duplicate that is dropped is not a change of order.
        let r = reductions(
            &cfg(&["*.bak", "!k", "*.bak"]),
            &cfg(&["!k", "*.bak"]),
            true,
            true,
        );
        assert!(!r.contains(&Reduction::ExcludesReordered), "{r:?}");
        // The invisible characters refused in patterns (decision 0022).
        for c in ['\u{180f}', '\u{13430}', '\u{202e}', '\u{200b}'] {
            assert!(unprintable(c), "{c:?}");
        }
        assert!(!unprintable('ä') && !unprintable('-'));
    }

    #[test]
    fn reordering_that_changes_the_outcome_is_refused() {
        // The old list keeps alice via a re-include after a broad exclude; the
        // new list reorders them so the exclude wins and alice is emptied. The
        // whole-list comparison catches it (decision 0022, item 9).
        let old = ["/alice/**", "!/alice/**"];
        let new = ["!/alice/**", "/alice/**"];
        assert!(guard(&old, false).is_none(), "old list keeps alice");
        assert!(
            guard_with(&old, &new, false, &[]).is_some(),
            "the reorder empties alice and is refused"
        );
        // A plain reorder that changes no outcome is not refused.
        assert!(guard_with(&["a", "b"], &["b", "a"], false, &[]).is_none());
    }

    #[test]
    fn an_added_whole_folder_exclude_is_refused_but_an_existing_one_is_kept() {
        let c = current(); // non-system mode; home.pxar from /home
        let homes = [PathBuf::from("/home/alice")];
        // A newly added exclude of a whole home directory is refused.
        let e = validate(
            &SettingsPatch {
                excludes: Some(strings(&["/alice"])),
                ..patch()
            },
            &c,
            &homes,
        )
        .unwrap_err();
        assert!(e.to_string().contains("whole folder"), "{e}");
        assert!(e.to_string().contains("/etc/nestegg/config.toml"), "{e}");
        assert!(e.to_string().contains("/home/alice"), "{e}");
        // Root already put /alice in the file: a change that keeps it passes
        // and may add ordinary patterns.
        let mut with = c.clone();
        with.backup.excludes = strings(&["/alice"]);
        assert!(
            validate(
                &SettingsPatch {
                    excludes: Some(strings(&["/alice", "*.iso"])),
                    ..patch()
                },
                &with,
                &homes,
            )
            .is_ok()
        );
        // In system mode the same list is refused by the full path in
        // system.pxar.
        let mut sys = c.clone();
        sys.backup.system = true;
        let e = validate(
            &SettingsPatch {
                excludes: Some(strings(&["/home/alice"])),
                ..patch()
            },
            &sys,
            &homes,
        )
        .unwrap_err();
        assert!(e.to_string().contains("/home/alice"), "{e}");
    }

    #[test]
    fn reductions_report_only_what_lowers_the_protection() {
        let old = current(); // interval 24, overdue 36, Wake-on-LAN on
        assert!(reductions(&old, &old, true, true).is_empty());
        assert_eq!(
            reductions(&old, &old, true, false),
            vec![Reduction::AutomaticBackupsOff]
        );
        // Switching the timer on is not a reduction.
        assert!(reductions(&old, &old, false, true).is_empty());

        let mut slower = old.clone();
        slower.schedule.interval_hours = 48;
        slower.overdue_after_hours = 100;
        let r = reductions(&old, &slower, true, true);
        assert!(r.contains(&Reduction::IntervalRaised { from: 24, to: 48 }));
        assert!(r.contains(&Reduction::OverdueRaised { from: 36, to: 100 }));

        // Lowering the interval / overdue raises the protection.
        let mut faster = old.clone();
        faster.schedule.interval_hours = 12;
        faster.overdue_after_hours = 30;
        assert!(reductions(&old, &faster, true, true).is_empty());

        // An added exclude is reported; a removed one or a re-include is not.
        let mut more = old.clone();
        more.backup.excludes.push("*.iso".into());
        assert_eq!(
            reductions(&old, &more, true, true),
            vec![Reduction::ExcludesAdded {
                patterns: vec!["*.iso".into()]
            }]
        );
        let mut reinclude = old.clone();
        reinclude.backup.excludes.push("!**/keep".into());
        assert!(reductions(&old, &reinclude, true, true).is_empty());
        let mut fewer = old.clone();
        fewer.backup.excludes.pop();
        assert!(reductions(&old, &fewer, true, true).is_empty());

        // Wake-on-LAN off is a reduction; on is not.
        let mut nowake = old.clone();
        nowake.wake.as_mut().unwrap().enabled = false;
        assert_eq!(
            reductions(&old, &nowake, true, true),
            vec![Reduction::WakeOnLanOff]
        );
        assert!(reductions(&nowake, &old, true, true).is_empty());
    }

    #[test]
    fn removing_a_reinclude_or_reordering_is_a_reduction() {
        // Removing a `!` re-include excludes again what it kept (decision 0022,
        // item 5).
        let mut old = current();
        old.backup.excludes = strings(&["**/node_modules", "!**/node_modules/keep"]);
        let mut without = old.clone();
        without.backup.excludes = strings(&["**/node_modules"]);
        assert_eq!(
            reductions(&old, &without, true, true),
            vec![Reduction::ReincludesRemoved {
                patterns: vec!["!**/node_modules/keep".into()]
            }]
        );
        // Reordering the same patterns while a re-include is present is a
        // reduction (later patterns win).
        let mut reordered = old.clone();
        reordered.backup.excludes = strings(&["!**/node_modules/keep", "**/node_modules"]);
        assert_eq!(
            reductions(&old, &reordered, true, true),
            vec![Reduction::ExcludesReordered]
        );
        // A pure reorder of plain excludes (no re-include) changes nothing.
        let mut plain = current();
        plain.backup.excludes = strings(&["a", "b"]);
        let mut swapped = plain.clone();
        swapped.backup.excludes = strings(&["b", "a"]);
        assert!(reductions(&plain, &swapped, true, true).is_empty());
    }

    #[test]
    fn limits_report_the_api_cap_and_a_grandfathered_value() {
        let c = current(); // interval 24
        let overdue = &limits_json(&c)["overdue_after_hours"];
        assert_eq!(overdue["max"], 168);
        // The rule's numbers are reported so the UI can recompute the cap for
        // an interval it is about to set (decision 0022, item 12).
        assert_eq!(overdue["base"], 168);
        assert_eq!(overdue["per_interval_extra"], 72);
        assert_eq!(overdue["file_minimum"], 36);
        assert_eq!(overdue["greater_than_interval"], true);
        let mut g = c.clone();
        g.overdue_after_hours = 744;
        assert_eq!(limits_json(&g)["overdue_after_hours"]["max"], 744);
        // A weekly rhythm raises the reported max to interval + 72.
        let mut weekly = c.clone();
        weekly.schedule.interval_hours = 168;
        assert_eq!(limits_json(&weekly)["overdue_after_hours"]["max"], 240);
    }

    #[test]
    fn wake_on_needs_a_wake_section() {
        let mut c = current();
        assert!(
            validate(
                &SettingsPatch {
                    wake_on_lan: Some(true),
                    ..patch()
                },
                &c,
                &[],
            )
            .is_ok()
        );
        c.wake = None;
        let e = validate(
            &SettingsPatch {
                wake_on_lan: Some(true),
                ..patch()
            },
            &c,
            &[],
        )
        .unwrap_err();
        assert!(e.to_string().contains("[wake]"), "{e}");
        // Switching off what is not there is a no-op.
        assert!(
            validate(
                &SettingsPatch {
                    wake_on_lan: Some(false),
                    ..patch()
                },
                &c,
                &[],
            )
            .is_ok()
        );
    }

    /// Lines of `new` that are not in `old` and vice versa.
    fn diff(old: &str, new: &str) -> (Vec<String>, Vec<String>) {
        let o: Vec<&str> = old.lines().collect();
        let n: Vec<&str> = new.lines().collect();
        (
            n.iter()
                .filter(|l| !o.contains(l))
                .map(|l| l.to_string())
                .collect(),
            o.iter()
                .filter(|l| !n.contains(l))
                .map(|l| l.to_string())
                .collect(),
        )
    }

    #[test]
    fn rewrite_keeps_the_notify_section() {
        let text = format!(
            "{COMMENTED}\n[notify]\n# Hook for push messages.\ndesktop = false\n\
             command = [\"/usr/local/bin/my-hook\", \"arg\"] # runs as root\n"
        );
        let new = rewrite(
            &text,
            &SettingsPatch {
                interval_hours: Some(12),
                ..patch()
            },
        )
        .unwrap();
        let (added, removed) = diff(&text, &new);
        assert_eq!(added, ["interval_hours = 12"]);
        assert_eq!(removed, ["interval_hours = 24"]);
        let c = Config::parse(&new).unwrap();
        assert!(!c.notify.desktop);
        assert_eq!(
            c.notify.command,
            Some(vec![
                "/usr/local/bin/my-hook".to_string(),
                "arg".to_string()
            ])
        );
    }

    #[test]
    fn rewrite_changes_only_touched_keys_and_keeps_comments() {
        let new = rewrite(
            COMMENTED,
            &SettingsPatch {
                interval_hours: Some(12),
                overdue_after_hours: Some(20),
                ..patch()
            },
        )
        .unwrap();
        let (added, removed) = diff(COMMENTED, &new);
        assert_eq!(
            added,
            [
                "overdue_after_hours = 20 # a day and a half",
                "interval_hours = 12"
            ]
        );
        assert_eq!(
            removed,
            [
                "overdue_after_hours = 36 # a day and a half",
                "interval_hours = 24"
            ]
        );
        let c = Config::parse(&new).unwrap();
        assert_eq!(c.schedule.interval_hours, 12);
        assert_eq!(c.overdue_after_hours, 20);
    }

    #[test]
    fn rewrite_excludes_keeps_layout_and_comments_of_kept_items() {
        let new = rewrite(
            COMMENTED,
            &SettingsPatch {
                excludes: Some(strings(&["**/.cache", "**/.local/share/Trash", "*.iso"])),
                ..patch()
            },
        )
        .unwrap();
        assert!(
            new.contains(
                "excludes = [\n    \"**/.cache\",      # browser caches\n    \"**/.local/share/Trash\",\n    \"*.iso\",\n]\n"
            ),
            "{new}"
        );
        let (added, removed) = diff(COMMENTED, &new);
        assert_eq!(added, ["    \"*.iso\","]);
        assert_eq!(removed, ["    \"**/node_modules\","]);
        assert_eq!(
            Config::parse(&new).unwrap().backup.excludes,
            strings(&["**/.cache", "**/.local/share/Trash", "*.iso"])
        );
        // Empty list.
        let empty = rewrite(
            COMMENTED,
            &SettingsPatch {
                excludes: Some(vec![]),
                ..patch()
            },
        )
        .unwrap();
        assert!(empty.contains("excludes = []\n"), "{empty}");
        assert!(Config::parse(&empty).unwrap().backup.excludes.is_empty());
    }

    #[test]
    fn rewrite_excludes_moves_same_line_comments_with_their_item() {
        let text = "[pbs]\nserver = \"pbs.example.com\"\ndatastore = \"backups\"\nauth_id = \"backup@pbs!desktop\"\n\n[backup]\nexcludes = [ # patterns\n  # caches first\n  \"a\", # A\n  \"b\", # B\n  \"c\"  # C\n] # end\n";
        let new = rewrite(
            text,
            &SettingsPatch {
                excludes: Some(strings(&["c", "a", "d"])),
                ..patch()
            },
        )
        .unwrap();
        assert!(
            new.ends_with(
                "excludes = [ # patterns\n  \"c\",  # C\n  # caches first\n  \"a\", # A\n  \"d\",\n] # end\n"
            ),
            "{new}"
        );
        assert_eq!(
            Config::parse(&new).unwrap().backup.excludes,
            strings(&["c", "a", "d"])
        );
    }

    #[test]
    fn rewrite_of_a_file_written_by_init() {
        let text = format!(
            "# nestegg configuration. Contains no secrets.\n\n{}",
            crate::config::example("pbs.example.com", "backups", "backup@pbs!desktop")
                .to_toml()
                .unwrap()
        );
        let mut excludes = crate::config::default_excludes();
        excludes.retain(|p| p != "**/node_modules");
        excludes.push("*.iso".into());
        let new = rewrite(
            &text,
            &SettingsPatch {
                interval_hours: Some(12),
                overdue_after_hours: Some(30),
                excludes: Some(excludes.clone()),
                ..patch()
            },
        )
        .unwrap();
        let (added, removed) = diff(&text, &new);
        assert_eq!(
            added,
            [
                "overdue_after_hours = 30",
                "    \"*.iso\",",
                "interval_hours = 12"
            ],
            "{new}"
        );
        assert_eq!(
            removed,
            [
                "overdue_after_hours = 36",
                "    \"**/node_modules\",",
                "interval_hours = 24"
            ]
        );
        let c = Config::parse(&new).unwrap();
        assert_eq!(c.backup.excludes, excludes);
        assert!(new.starts_with("# nestegg configuration."));
    }

    #[test]
    fn rewrite_single_line_array_stays_single_line() {
        let text = COMMENTED.replace(
            "excludes = [\n    \"**/.cache\",      # browser caches\n    \"**/node_modules\",\n    \"**/.local/share/Trash\",\n]",
            "excludes = [\"**/.cache\", \"**/node_modules\"] # short",
        );
        let new = rewrite(
            &text,
            &SettingsPatch {
                excludes: Some(strings(&["**/node_modules", "*.iso"])),
                ..patch()
            },
        )
        .unwrap();
        assert!(
            new.contains("excludes = [\"**/node_modules\", \"*.iso\"] # short\n"),
            "{new}"
        );
    }

    #[test]
    fn rewrite_adds_missing_keys_and_tables() {
        let minimal = "# minimal\n[pbs]\nserver = \"pbs.example.com\"\ndatastore = \"backups\"\nauth_id = \"backup@pbs!desktop\"\n";
        let new = rewrite(
            minimal,
            &SettingsPatch {
                interval_hours: Some(6),
                overdue_after_hours: Some(10),
                excludes: Some(strings(&["*.iso"])),
                ..patch()
            },
        )
        .unwrap();
        assert!(
            new.starts_with("overdue_after_hours = 10\n# minimal\n[pbs]\n"),
            "{new}"
        );
        let c = Config::parse(&new).unwrap();
        assert_eq!(c.schedule.interval_hours, 6);
        assert_eq!(c.overdue_after_hours, 10);
        assert_eq!(c.backup.excludes, strings(&["*.iso"]));
        // Defaults for everything else, e.g. the default sources.
        assert_eq!(c.backup.sources.len(), 4);

        // [backup] only implied by [[backup.sources]].
        let implied =
            format!("{minimal}\n[[backup.sources]]\narchive = \"home\"\npath = \"/home\"\n");
        let new = rewrite(
            &implied,
            &SettingsPatch {
                excludes: Some(strings(&["*.iso"])),
                ..patch()
            },
        )
        .unwrap();
        let c = Config::parse(&new).unwrap();
        assert_eq!(c.backup.excludes, strings(&["*.iso"]));
        assert_eq!(c.backup.sources.len(), 1, "{new}");

        // Dotted keys and inline tables are changed where they are.
        let dotted = format!("schedule.interval_hours = 24\n{minimal}");
        let new = rewrite(
            &dotted,
            &SettingsPatch {
                interval_hours: Some(8),
                ..patch()
            },
        )
        .unwrap();
        assert!(new.starts_with("schedule.interval_hours = 8\n"), "{new}");
        let inline = format!("schedule = {{ interval_hours = 24 }}\n{minimal}");
        let new = rewrite(
            &inline,
            &SettingsPatch {
                interval_hours: Some(8),
                ..patch()
            },
        )
        .unwrap();
        assert_eq!(Config::parse(&new).unwrap().schedule.interval_hours, 8);
    }

    #[test]
    fn wake_off_keeps_the_mac_and_on_restores() {
        let off = rewrite(
            COMMENTED,
            &SettingsPatch {
                wake_on_lan: Some(false),
                ..patch()
            },
        )
        .unwrap();
        let c = Config::parse(&off).unwrap();
        let wake = c.wake.as_ref().unwrap();
        assert_eq!(wake.mac, "00:00:5e:00:53:01");
        assert!(!wake.enabled);
        assert!(c.active_wake().is_none());
        assert!(off.contains("mac = \"00:00:5e:00:53:01\"   # PBS NIC\n"));
        let (added, removed) = diff(COMMENTED, &off);
        assert_eq!(added, ["enabled = false"]);
        assert!(removed.is_empty());

        let on = rewrite(
            &off,
            &SettingsPatch {
                wake_on_lan: Some(true),
                ..patch()
            },
        )
        .unwrap();
        assert!(Config::parse(&on).unwrap().active_wake().is_some());
        // Switching on a section without the key does not touch the file.
        let same = rewrite(
            COMMENTED,
            &SettingsPatch {
                wake_on_lan: Some(true),
                ..patch()
            },
        )
        .unwrap();
        assert_eq!(same, COMMENTED);
    }

    #[test]
    fn empty_patch_keeps_the_file_byte_for_byte() {
        assert_eq!(rewrite(COMMENTED, &patch()).unwrap(), COMMENTED);
    }

    struct FakeTimer {
        on: Cell<bool>,
        fail: bool,
        calls: RefCell<Vec<bool>>,
    }

    impl FakeTimer {
        fn new(on: bool, fail: bool) -> FakeTimer {
            FakeTimer {
                on: Cell::new(on),
                fail,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Timer for FakeTimer {
        fn enabled(&self) -> bool {
            self.on.get()
        }
        fn set(&self, enabled: bool) -> Result<()> {
            self.calls.borrow_mut().push(enabled);
            if self.fail {
                bail!("Failed to connect to bus");
            }
            self.on.set(enabled);
            Ok(())
        }
    }

    fn config_file(text: &str, mode: u32) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        (dir, path)
    }

    #[test]
    fn update_writes_atomically_and_keeps_mode() {
        let (dir, path) = config_file(COMMENTED, 0o640);
        let timer = FakeTimer::new(true, false);
        let out = update(
            &path,
            &SettingsPatch {
                interval_hours: Some(12),
                automatic_backups: Some(false),
                ..patch()
            },
            &timer,
        )
        .unwrap();
        assert_eq!(out.config.schedule.interval_hours, 12);
        assert!(!out.timer_enabled);
        assert_eq!(
            out.changes,
            ["interval_hours: 24 -> 12", "automatic_backups: on -> off"]
        );
        assert_eq!(*timer.calls.borrow(), [false]);
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o640);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("interval_hours = 12")
        );
        // No temporary files left behind.
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.toml"]);
        // The same request again changes nothing and does not call systemctl.
        let again = update(
            &path,
            &SettingsPatch {
                interval_hours: Some(12),
                automatic_backups: Some(false),
                ..patch()
            },
            &timer,
        )
        .unwrap();
        assert!(again.changes.is_empty());
        assert_eq!(timer.calls.borrow().len(), 1);
    }

    #[test]
    fn update_rejects_invalid_requests_without_touching_the_file() {
        let (_dir, path) = config_file(COMMENTED, 0o644);
        let timer = FakeTimer::new(true, false);
        let e = update(
            &path,
            &SettingsPatch {
                excludes: Some(strings(&["**"])),
                automatic_backups: Some(false),
                ..patch()
            },
            &timer,
        )
        .unwrap_err();
        assert!(matches!(e, SettingsError::Invalid(_)), "{e}");
        assert_eq!(fs::read_to_string(&path).unwrap(), COMMENTED);
        assert!(timer.calls.borrow().is_empty());
    }

    #[test]
    fn update_refuses_an_invalid_existing_file() {
        let (_dir, path) = config_file("[pbs]\nserver = \"\"\n", 0o644);
        let e = update(
            &path,
            &SettingsPatch {
                interval_hours: Some(12),
                ..patch()
            },
            &FakeTimer::new(true, false),
        )
        .unwrap_err();
        assert!(matches!(e, SettingsError::Failed(_)), "{e}");
        assert!(e.to_string().contains("fix it in the terminal"), "{e}");
    }

    #[test]
    fn timer_failure_puts_the_old_file_back() {
        let (_dir, path) = config_file(COMMENTED, 0o644);
        let timer = FakeTimer::new(false, true);
        let e = update(
            &path,
            &SettingsPatch {
                interval_hours: Some(12),
                automatic_backups: Some(true),
                ..patch()
            },
            &timer,
        )
        .unwrap_err();
        assert!(matches!(e, SettingsError::Failed(_)));
        let msg = e.to_string();
        assert!(
            msg.contains("nothing was changed") && msg.contains("Failed to connect to bus"),
            "{msg}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), COMMENTED);
    }

    /// `systemctl enable --now` that links the unit but cannot start it.
    struct HalfTimer(Cell<bool>);

    impl Timer for HalfTimer {
        fn enabled(&self) -> bool {
            self.0.get()
        }
        fn set(&self, enabled: bool) -> Result<()> {
            self.0.set(enabled);
            bail!("Job for nestegg-backup.timer failed");
        }
    }

    #[test]
    fn timer_switched_but_not_started_is_reported_honestly() {
        let (_dir, path) = config_file(COMMENTED, 0o644);
        let timer = HalfTimer(Cell::new(false));
        // The change took effect (the file was written, the timer switched on),
        // so `update` returns Ok with an `error`: the caller answers with an
        // error but still reports the change (decision 0022, item 3).
        let out = update(
            &path,
            &SettingsPatch {
                interval_hours: Some(12),
                automatic_backups: Some(true),
                ..patch()
            },
            &timer,
        )
        .unwrap();
        let msg = format!("{:#}", out.error.as_ref().expect("an error is reported"));
        assert!(
            msg.contains("switched on and the settings saved")
                && !msg.contains("nothing was changed"),
            "{msg}"
        );
        // The timer switch counts as applied, so it is in the changes list.
        assert!(out.timer_enabled);
        assert!(out.changes.iter().any(|c| c.contains("automatic_backups")));
        // The file matches the switch: the change is kept.
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("interval_hours = 12")
        );
    }

    #[test]
    fn empty_multi_line_exclude_list_can_be_filled() {
        let text = "[pbs]\nserver = \"pbs.example.com\"\ndatastore = \"backups\"\n\
                    auth_id = \"backup@pbs!desktop\"\n\n[backup]\nexcludes = [\n    # \"**/.cache\",\n]\n";
        let out = rewrite(
            text,
            &SettingsPatch {
                excludes: Some(strings(&["**/.cache", "*.iso"])),
                ..patch()
            },
        )
        .unwrap();
        let config = Config::parse(&out).unwrap();
        assert_eq!(config.backup.excludes, strings(&["**/.cache", "*.iso"]));
        assert!(out.contains("# \"**/.cache\","), "{out}");
    }

    #[test]
    fn excludes_must_not_look_like_options() {
        for p in ["-foo", "--repository=x"] {
            let e = validate_exclude(p).unwrap_err();
            assert!(e.contains("must not start with `-`"), "{p}: {e}");
        }
        assert!(validate_exclude("**/-cache").is_ok());
        assert!(validate_exclude("\u{feff}foo").is_err());
    }

    #[test]
    fn write_file_atomic_sets_the_final_mode_and_refuses_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // A fresh file gets mode 0640 from the start.
        write_file_atomic(&path, "a = 1\n", 0o640).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o640);
        // Overwriting an existing file (as `init --force` does) keeps 0640 even
        // if the old one was looser, with no window at the looser mode.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        write_file_atomic(&path, "a = 2\n", 0o640).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o640);
        assert_eq!(fs::read_to_string(&path).unwrap(), "a = 2\n");
        // No temporary files are left behind.
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.toml"]);
        // A symlink at the path is refused rather than followed.
        let target = dir.path().join("real.toml");
        fs::write(&target, "keep\n").unwrap();
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let e = write_file_atomic(&link, "x", 0o640).unwrap_err();
        assert!(e.to_string().contains("symbolic link"), "{e}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep\n");
    }

    #[test]
    fn atomic_write_refuses_symlinks() {
        let (dir, path) = config_file(COMMENTED, 0o644);
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let e = write_atomically(&link, "x").unwrap_err();
        assert!(e.to_string().contains("symbolic link"), "{e}");
        assert_eq!(fs::read_to_string(&path).unwrap(), COMMENTED);
    }

    #[test]
    fn systemd_timer_reports_systemctl_errors() {
        let missing = Path::new("/nonexistent/systemctl");
        let timer = SystemdTimer { systemctl: missing };
        assert!(!timer.enabled());
        let e = timer.set(true).unwrap_err();
        assert!(format!("{e:#}").contains("need systemd"), "{e:#}");
    }
}
