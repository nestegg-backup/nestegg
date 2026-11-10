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
use std::io::Write;
use std::ops::RangeInclusive;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    if old.backup.excludes != new.backup.excludes
        && same_multiset(&old.backup.excludes, &new.backup.excludes)
        && new.backup.excludes.iter().any(|p| p.starts_with('!'))
    {
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

/// Whether two lists hold the same patterns, regardless of order.
fn same_multiset(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a: Vec<&String> = a.iter().collect();
    let mut b: Vec<&String> = b.iter().collect();
    a.sort();
    b.sort();
    a == b
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
/// exclude that would drop a whole home directory (decision 0022).
pub fn validate(
    patch: &SettingsPatch,
    current: &Config,
    home_dirs: &[PathBuf],
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
    if let Some(list) = &patch.excludes {
        validate_excludes(list).map_err(invalid)?;
        // A pattern that would leave out a whole configured source, a whole
        // home directory of a regular user, or (in system mode) a whole
        // top-level system directory is refused when it is newly added; one
        // that root already put in the file is kept (decision 0022).
        for pattern in list {
            if current.backup.excludes.contains(pattern) {
                continue;
            }
            if let Some(why) = removes_whole_folder(
                pattern,
                &current.backup.sources,
                home_dirs,
                current.backup.system,
            ) {
                return Err(invalid(why));
            }
        }
    }
    if patch.wake_on_lan == Some(true) && current.wake.is_none() {
        return Err(invalid(
            "Wake-on-LAN is not set up: the configuration has no [wake] section with the \
             MAC address of the backup server. Add it in the terminal (see \
             docs/configuration.md), then switch it on here.",
        ));
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
    Ok(())
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

/// Control characters, and the invisible Unicode format characters that
/// reorder or hide text in a terminal or a notification (bidirectional
/// overrides, zero-width characters). Kept in step with the same filter in
/// `crate::rescue` (decision 0022); duplicated on purpose so the settings path
/// does not depend on the rescue module.
pub fn unprintable(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2069}' | '\u{feff}')
}

/// Build the exclude [`MatchEntry`] exactly as `proxmox-backup-client` does for
/// one `--exclude <pattern>` (gitignore-style, `PatternFlag::PATH_NAME`). A `!`
/// re-include widens the backup and never removes anything, so it yields
/// `None`; a pattern that cannot be parsed is treated as harmless here (it is
/// not our job to reject it, the client will).
fn exclude_entry(pattern: &str) -> Option<MatchEntry> {
    if pattern.starts_with('!') {
        return None;
    }
    MatchEntry::parse_pattern(pattern, PatternFlag::PATH_NAME, MatchType::Exclude).ok()
}

/// Two arbitrary, structurally different child names. A pattern that excludes
/// both of them excludes *every* child, so the folder would be emptied even if
/// the folder entry itself stays (`alice/**`, `alice/*`). They are chosen so
/// that an ordinary narrow pattern (`alice/Downloads`, `**/.cache`) matches
/// neither.
const PROBE_A: &str = "0_nestegg_probe_one";
const PROBE_B: &str = "Zq.keep-me_two";

/// Whether this single exclude pattern, applied in one archive, would leave
/// nothing of the folder at archive-relative path `rel` (like `/alice`): the
/// folder itself is excluded, or every one of its children is.
fn removes_folder(entry: &MatchEntry, rel: &str) -> bool {
    let dir = Some(nix::libc::S_IFDIR);
    let hits = |p: &str| entry.matches(p.as_bytes(), dir);
    hits(rel) || (hits(&format!("{rel}/{PROBE_A}")) && hits(&format!("{rel}/{PROBE_B}")))
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

/// Reject an exclude that would, as `proxmox-backup-client` applies it to each
/// archive, leave out a whole configured source, a whole home directory of a
/// regular user inside a source, or (in system mode) a whole top-level system
/// directory (decision 0022). Returns the plain-words reason, or `None` when
/// the pattern is fine. Ordinary patterns (`**/.cache`, `**/node_modules`,
/// `/alice/Downloads`, `/home/*/Videos`) pass. This is a guard against
/// accidents, not a security boundary: a group member can still exclude a
/// user's folders one by one, which is reported.
fn removes_whole_folder(
    pattern: &str,
    sources: &[crate::config::Source],
    home_dirs: &[PathBuf],
    system: bool,
) -> Option<String> {
    let entry = exclude_entry(pattern)?;
    let root = PathBuf::from("/");
    // (archive root, protected folder) pairs to test. A source is tested as a
    // top-level entry ("/home" in a parent archive); a home directory in the
    // archive of the source that holds it ("/alice" in home.pxar), and in
    // system mode also in system.pxar ("/home/alice" at "/").
    let mut checks: Vec<(&Path, &Path)> = Vec::new();
    for s in sources {
        checks.push((&root, s.path.as_path()));
    }
    for h in home_dirs {
        if let Some(src) = deepest_source(h, sources) {
            checks.push((src.path.as_path(), h.as_path()));
        }
        if system {
            checks.push((&root, h.as_path()));
        }
    }
    let system_dirs: Vec<PathBuf> = if system {
        SYSTEM_DIRS.iter().map(PathBuf::from).collect()
    } else {
        Vec::new()
    };
    for d in &system_dirs {
        checks.push((&root, d.as_path()));
    }
    for (archive_root, folder) in checks {
        let Some(rel) = archive_rel(archive_root, folder) else {
            continue;
        };
        // The whole-archive case (`rel == "/"`) is an empty backup, caught by
        // `excludes_everything` in `validate_exclude`.
        if rel != "/" && removes_folder(&entry, &rel) {
            return Some(format!(
                "exclude pattern `{pattern}` would leave out the whole folder {} from the backup; \
                 an administrator can still set it as root in /etc/nestegg/config.toml.",
                folder.display()
            ));
        }
    }
    None
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
) -> Result<Outcome, SettingsError> {
    let old_text = fs::read_to_string(config_file)
        .with_context(|| format!("cannot read configuration {}", config_file.display()))?;
    let current = Config::parse(&old_text).with_context(|| {
        format!(
            "invalid configuration {}; fix it in the terminal first",
            config_file.display()
        )
    })?;
    validate(patch, &current, &regular_home_dirs())?;
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

    #[test]
    fn whole_folder_excludes_are_refused_but_ordinary_ones_pass() {
        // current() has the default sources: /home, /etc, /root, /usr/local.
        // alice and bob are regular users with homes inside the /home source.
        // The expected outcomes below were confirmed with the real tool
        // (`pxar create --exclude`) in the e2e client image (decision 0022).
        let sources = current().backup.sources;
        let homes = [PathBuf::from("/home/alice"), PathBuf::from("/home/bob")];
        let refused = |p: &str, system: bool| removes_whole_folder(p, &sources, &homes, system);

        // Refused in every mode: a whole source, or a whole home directory
        // inside the /home source.
        for bad in [
            "/home",
            "/home/",
            "/home/*",
            "/home/**", // the whole /home source
            "/etc",
            "/root",
            "/usr/local",
            "/usr/local/*",
            "/usr/local/**", // other sources
            "home",
            "etc",
            "local", // a bare name = a source's last component
            "/alice",
            "alice",
            "alice/",
            "**/alice",
            "alice/**", // alice's home in home.pxar
            "/[a-z]*",
            "/a*",
            "/*[a-z0-9]", // match alice (and more) in home.pxar
        ] {
            assert!(refused(bad, false).is_some(), "{bad} should be refused");
        }

        // Harmless in the default configuration: ordinary subfolder patterns,
        // and anchored single-component names that are not a home directory or
        // a source (they used to be refused with a misleading message).
        for ok in [
            "**/.cache",
            "**/node_modules",
            "**/lost+found",
            "/alice/Downloads", // a subfolder of a home, not the whole home
            "/home/*/Videos",   // a subfolder, not the whole source
            "*.iso",
            "node_modules", // not a source's last component
            "/usr/lib",     // not a source nor its parent
            "!/home",       // a re-include widens the backup
            "/.cache",      // anchored one component, but not a home or source
            "/Downloads",   // likewise
            "Downloads",    // a subfolder name, not a home
            "/usr",         // not a source; only a system directory in system mode
            "/var",         // likewise
            "/home/[a-z]*", // no-op in home.pxar (entries are /alice, not /home/alice)
            "/home/[!.]*",  // matches only dot-names, confirmed with the real tool
        ] {
            assert!(refused(ok, false).is_none(), "{ok} should pass");
        }

        // System mode adds system.pxar rooted at "/": a pattern that names a
        // whole top-level system directory, or a home directory by its full
        // path, is refused there.
        for bad in [
            "/usr",
            "/var",
            "/opt",
            "/boot",
            "/srv",         // top-level system directories
            "/home/[a-z]*", // matches /home/alice in system.pxar
            "/home/alice",  // a home by its full path
        ] {
            assert!(
                refused(bad, true).is_some(),
                "{bad} should be refused in system mode"
            );
        }
        // Still harmless in system mode.
        for ok in [
            "**/.cache",
            "/home/*/Videos",
            "/alice/Downloads",
            "/home/[!.]*",
        ] {
            assert!(
                refused(ok, true).is_none(),
                "{ok} should pass in system mode"
            );
        }
    }

    #[test]
    fn an_added_whole_folder_exclude_is_refused_but_an_existing_one_is_kept() {
        let c = current();
        let e = validate(
            &SettingsPatch {
                excludes: Some(strings(&["/home"])),
                ..patch()
            },
            &c,
            &[],
        )
        .unwrap_err();
        assert!(e.to_string().contains("whole folder"), "{e}");
        assert!(e.to_string().contains("/etc/nestegg/config.toml"), "{e}");
        // Root already put /home in the file: a change that keeps it passes
        // and may add ordinary patterns.
        let mut with = c.clone();
        with.backup.excludes = strings(&["/home"]);
        assert!(
            validate(
                &SettingsPatch {
                    excludes: Some(strings(&["/home", "*.iso"])),
                    ..patch()
                },
                &with,
                &[],
            )
            .is_ok()
        );
        // A home directory from the account database is refused by its path.
        let e = validate(
            &SettingsPatch {
                excludes: Some(strings(&["/alice"])),
                ..patch()
            },
            &c,
            &[PathBuf::from("/home/alice")],
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
