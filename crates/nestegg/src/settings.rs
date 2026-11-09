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
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value};

use crate::config::Config;
use crate::status::TIMER_UNIT;

/// Allowed backup interval: one hour to one week.
pub const INTERVAL_HOURS: RangeInclusive<u32> = 1..=168;
/// Allowed overdue threshold: two hours to 31 days. It must also be greater
/// than the backup interval.
pub const OVERDUE_AFTER_HOURS: RangeInclusive<u32> = 2..=744;
pub const MAX_EXCLUDES: usize = 200;
/// Maximum length of one exclude pattern, in characters.
pub const MAX_EXCLUDE_LEN: usize = 256;

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

/// Allowed values, for `GET /api/v1/config` (`limits`).
pub fn limits_json() -> serde_json::Value {
    serde_json::json!({
        "interval_hours": { "min": INTERVAL_HOURS.start(), "max": INTERVAL_HOURS.end() },
        "overdue_after_hours": {
            "min": OVERDUE_AFTER_HOURS.start(),
            "max": OVERDUE_AFTER_HOURS.end(),
            "greater_than_interval": true,
        },
        "excludes": { "max_items": MAX_EXCLUDES, "max_length": MAX_EXCLUDE_LEN },
    })
}

/// Check a change against the current configuration.
pub fn validate(patch: &SettingsPatch, current: &Config) -> Result<(), SettingsError> {
    if let Some(h) = patch.interval_hours
        && !INTERVAL_HOURS.contains(&h)
    {
        return Err(invalid(format!(
            "interval_hours must be between {} and {} (one week)",
            INTERVAL_HOURS.start(),
            INTERVAL_HOURS.end()
        )));
    }
    if let Some(h) = patch.overdue_after_hours
        && !OVERDUE_AFTER_HOURS.contains(&h)
    {
        return Err(invalid(format!(
            "overdue_after_hours must be between {} and {} (31 days)",
            OVERDUE_AFTER_HOURS.start(),
            OVERDUE_AFTER_HOURS.end()
        )));
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
    if pattern.chars().count() > MAX_EXCLUDE_LEN {
        return Err(format!(
            "exclude patterns may have at most {MAX_EXCLUDE_LEN} characters"
        ));
    }
    if pattern.chars().any(char::is_control) {
        return Err(format!(
            "exclude pattern {pattern:?} contains a control character (line break, tab, NUL, ...)"
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

/// Result of a successful [`update`].
#[derive(Debug)]
pub struct Outcome {
    pub config: Config,
    pub timer_enabled: bool,
    /// Human-readable list of what changed (for the journal), empty if
    /// nothing did.
    pub changes: Vec<String>,
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
    validate(patch, &current)?;
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
    if let Some(on) = patch.automatic_backups
        && on != was_enabled
    {
        if let Err(e) = timer.set(on) {
            let what = if on { "on" } else { "off" };
            // `systemctl enable --now` links the unit before starting it (and
            // `disable --now` unlinks before stopping), so a failure can leave
            // the switch changed. Report what actually happened then.
            if timer.enabled() == on {
                return Err(SettingsError::Failed(e.context(format!(
                    "automatic backups were switched {what} and the settings saved, but the timer could not be {}",
                    if on { "started" } else { "stopped" }
                ))));
            }
            if wrote && let Err(restore) = write_atomically(config_file, &old_text) {
                return Err(SettingsError::Failed(anyhow!(
                    "cannot switch automatic backups {what}: {e:#}; the other settings were \
                     saved, putting the old file back failed: {restore:#}"
                )));
            }
            return Err(SettingsError::Failed(e.context(format!(
                "cannot switch automatic backups {what}; nothing was changed"
            ))));
        }
        timer_enabled = on;
        changes.push(format!(
            "automatic_backups: {} -> {}",
            on_off(was_enabled),
            on_off(on)
        ));
    }
    Ok(Outcome {
        config: new,
        timer_enabled,
        changes,
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
        for key in ["server", "sources", "auth_id", "encryption", "backup_id"] {
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
        let c = current();
        let check = |p: SettingsPatch| validate(&p, &c);
        for h in [0, 169, 1000] {
            let p = SettingsPatch {
                interval_hours: Some(h),
                overdue_after_hours: Some(744),
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
        assert!(
            check(SettingsPatch {
                overdue_after_hours: Some(745),
                ..patch()
            })
            .is_err()
        );
        assert!(
            check(SettingsPatch {
                interval_hours: Some(168),
                overdue_after_hours: Some(200),
                ..patch()
            })
            .is_ok()
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
                &c
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
                &c
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
                &odd
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
        assert!(validate_excludes(&strings(&["a", "a"])).is_err());
        let many: Vec<String> = (0..=MAX_EXCLUDES).map(|i| format!("p{i}")).collect();
        assert!(validate_excludes(&many).is_err());
        assert!(validate_excludes(&many[..MAX_EXCLUDES]).is_ok());
        assert!(validate_excludes(&[]).is_ok());
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
                &c
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
                &c
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
        let msg = format!("{e:#}");
        assert!(
            msg.contains("switched on and the settings saved")
                && !msg.contains("nothing was changed"),
            "{msg}"
        );
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
