//! Run records, persistent state and the dashboard health summary.
//!
//! Field names and enum values follow `api/openapi.yaml`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Paths;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    Timer,
    Manual,
    Cli,
    /// A trigger written by a newer version this one does not know. Kept so
    /// state.json and run records of a newer version stay readable.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Starting,
    WakingPbs,
    Inventory,
    Uploading,
    Finishing,
    Succeeded,
    Failed,
    PbsUnreachable,
    Cancelled,
    /// A run state written by a newer version this one does not know. It is
    /// kept so state.json, current.json and run records of a newer version
    /// stay readable. It may belong to a finished run or to a run still in
    /// progress (current.json), so it is never read as a success: health and
    /// the retry logic treat it as a failure (this version cannot understand
    /// the record).
    #[serde(other)]
    Unknown,
}

impl RunState {
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            RunState::Succeeded
                | RunState::Failed
                | RunState::PbsUnreachable
                | RunState::Cancelled
                | RunState::Unknown
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    PbsUnreachable,
    Authentication,
    SourceMissing,
    ClientMissing,
    ClientFailed,
    AlreadyRunning,
    Config,
    /// A stored secret exists but cannot be decrypted on this computer
    /// (TPM or mainboard changed, restored onto other hardware).
    CredentialsUnreadable,
    /// The upload finished, but reading the snapshot back from the server
    /// failed or did not return what was uploaded.
    VerificationFailed,
    Cancelled,
    Internal,
    /// An error kind written by a newer version this one does not know. Kept
    /// so state.json and run records of a newer version stay readable.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunError {
    pub kind: ErrorKind,
    pub message: String,
}

impl RunError {
    /// Turn an error into a run error of kind `fallback`, unless it is a
    /// secret that cannot be decrypted: that becomes
    /// [`ErrorKind::CredentialsUnreadable`] with a message naming the fix.
    pub fn from_error(e: &anyhow::Error, fallback: ErrorKind) -> RunError {
        match crate::secrets::find_unreadable(e) {
            Some(unreadable) => RunError {
                kind: ErrorKind::CredentialsUnreadable,
                message: format!(
                    "{unreadable}. Stored secrets cannot be decrypted after a TPM or mainboard \
                     change or a restore onto other hardware; store them again: \
                     sudo nestegg credentials set-password (token secret) and \
                     sudo nestegg key import <exported key file> (encryption key)"
                ),
            },
            None => RunError {
                kind: fallback,
                message: format!("{e:#}"),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArchiveInfo {
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Run {
    pub id: String,
    pub trigger: Trigger,
    pub state: RunState,
    #[serde(with = "rfc3339")]
    pub started_at: DateTime<Utc>,
    #[serde(default, with = "rfc3339_opt")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub duration_seconds: Option<i64>,
    #[serde(default)]
    pub snapshot: Option<String>,
    #[serde(default)]
    pub archives: Vec<ArchiveInfo>,
    #[serde(default)]
    pub uploaded_bytes: Option<u64>,
    /// Total size of the archives of the last successful run (estimate for
    /// a progress bar).
    #[serde(default)]
    pub expected_bytes: Option<u64>,
    /// Total size of the archives finished so far in this run.
    #[serde(default)]
    pub processed_bytes: Option<u64>,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Why this backup does not contain everything the configuration asks
    /// for (full system backup skipped, an optional source missing, ...).
    /// Empty for a complete backup. Each entry is also in `warnings`.
    #[serde(default)]
    pub incomplete: Vec<String>,
    /// The snapshot was read back from the server after the upload and
    /// matched (see decision 0015).
    #[serde(default)]
    pub verified: bool,
    /// A failure that is only a server-availability problem, not a persistent
    /// fault: the server was unreachable or stopped answering during the
    /// read-back (decision 0015), so the snapshot may exist but nothing was
    /// proven. It is still reported as `failed` with kind `verification_failed`,
    /// but, like `pbs_unreachable`, it does not count towards the growing pause
    /// between automatic retries (decision 0021) — the next hourly check tries
    /// again. Defaults to false; older records read as false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub transient: bool,
    #[serde(default)]
    pub error: Option<RunError>,
}

impl Run {
    pub fn new(trigger: Trigger, now: DateTime<Utc>) -> Run {
        Run {
            id: run_id(trigger, now),
            trigger,
            state: RunState::Starting,
            started_at: now,
            finished_at: None,
            duration_seconds: None,
            snapshot: None,
            archives: Vec::new(),
            uploaded_bytes: None,
            expected_bytes: None,
            processed_bytes: None,
            warnings: Vec::new(),
            incomplete: Vec::new(),
            verified: false,
            transient: false,
            error: None,
        }
    }

    pub fn finish(&mut self, state: RunState, now: DateTime<Utc>) {
        self.state = state;
        self.finished_at = Some(now);
        self.duration_seconds = Some((now - self.started_at).num_seconds().max(0));
    }
}

pub fn run_id(trigger: Trigger, now: DateTime<Utc>) -> String {
    let t = match trigger {
        Trigger::Timer => "timer",
        Trigger::Manual => "manual",
        Trigger::Cli => "cli",
        Trigger::Unknown => "unknown",
    };
    // Random suffix: two runs started within the same second stay distinct.
    format!(
        "{}-{t}-{:04x}",
        now.format("%Y%m%dT%H%M%SZ"),
        rand::random::<u16>()
    )
}

/// Run IDs end up in file names; only accept what [`run_id`] produces.
pub fn is_valid_run_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    #[serde(default)]
    pub last_run: Option<Run>,
    #[serde(default)]
    pub last_success: Option<Run>,
    #[serde(default)]
    pub pbs_reachable: Option<bool>,
    #[serde(default, with = "rfc3339_opt")]
    pub pbs_checked_at: Option<DateTime<Utc>>,
    /// Fingerprint reported by `proxmox-backup-client key show`.
    #[serde(default)]
    pub key_fingerprint: Option<String>,
    /// SHA-256 of the current key file (not secret; set by `key create`).
    #[serde(default)]
    pub key_digest: Option<String>,
    /// SHA-256 of the key the user confirmed as exported. Backups only run
    /// when this matches the key actually loaded, so replacing the key
    /// invalidates the confirmation automatically.
    #[serde(default)]
    pub key_exported_digest: Option<String>,
    /// SHA-256 of the key that `key export` / `key paperkey` last wrote out.
    /// Informational: `key confirm-exported` needs proof of a copy instead
    /// (decision 0018).
    #[serde(default)]
    pub key_last_exported_digest: Option<String>,
    /// Files that `key paperkey --output` wrote for the current key. Their
    /// paths are not secret. `key confirm-exported --paper-code` is refused
    /// while one of them still lies on this computer's disk (decision 0018).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paperkey_files: Vec<PaperKeyFile>,
    /// Snapshots of this machine (`host/<id>/<time>`) that the server's own
    /// verification marked as failed, as seen at the end of the last run.
    #[serde(default)]
    pub server_verify_failed: Vec<String>,
    /// Failed runs in a row since the last success. Runs that ended because
    /// the server was unreachable or that were cancelled do not count and do
    /// not reset it. Drives the growing pause between automatic retries
    /// ([`retry_not_before`]).
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Fingerprint ([`config_stamp`]) of the configuration file the last
    /// failed run read. The pause between retries only holds while the file
    /// is still that one.
    #[serde(default)]
    pub failed_config: Option<String>,
}

/// A paper key file written by `key paperkey --output`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaperKeyFile {
    /// Canonical absolute path at the time of writing.
    pub path: PathBuf,
    /// SHA-256 of the key the paper key was made from. A file made from an
    /// older key is no longer of interest once the key was replaced.
    pub key_digest: String,
}

/// At most this many paper key files are remembered for the current key. A
/// file that still exists is never forgotten to make room: past this number
/// `key paperkey` refuses to write another one (see
/// [`State::paperkey_files_full`]).
pub const MAX_PAPERKEY_FILES: usize = 50;

/// Whether a remembered paper key file may still exist. Only a clear "not
/// there" counts as gone: a file in a directory that cannot be searched is
/// still a file on this computer.
pub fn paperkey_file_exists(path: &Path) -> bool {
    match path.symlink_metadata() {
        Ok(_) => true,
        Err(e) => !matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ),
    }
}

impl State {
    pub fn key_exported(&self) -> bool {
        self.key_digest.is_some() && self.key_digest == self.key_exported_digest
    }

    /// Drop the entries that are of no interest any more: files made from
    /// another key, and files that are gone (`exists` says which still are).
    pub fn prune_paperkey_files(&mut self, key_digest: &str, exists: impl Fn(&Path) -> bool) {
        self.paperkey_files
            .retain(|f| f.key_digest == key_digest && exists(&f.path));
    }

    /// No room for another paper key file: the bound is reached and every
    /// entry is a file that exists (call [`State::prune_paperkey_files`]
    /// first). The caller refuses to write one more, because forgetting a
    /// file that still exists would let `--paper-code` miss it.
    pub fn paperkey_files_full(&self) -> bool {
        self.paperkey_files.len() >= MAX_PAPERKEY_FILES
    }

    /// Remember a paper key file written for the key with `key_digest`.
    /// Entries for other keys are dropped; no entry for this key is ever
    /// dropped here.
    pub fn remember_paperkey_file(&mut self, path: PathBuf, key_digest: &str) {
        self.paperkey_files.retain(|f| f.key_digest == key_digest);
        if !self.paperkey_files.iter().any(|f| f.path == path) {
            self.paperkey_files.push(PaperKeyFile {
                path,
                key_digest: key_digest.to_string(),
            });
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Ok,
    Running,
    Overdue,
    Failing,
    /// The last backup succeeded but does not contain everything the
    /// configuration asks for (`Run::incomplete`).
    Incomplete,
    Never,
    Unconfigured,
}

pub fn health(
    configured: bool,
    state: &State,
    current: Option<&Run>,
    overdue_after_hours: u32,
    now: DateTime<Utc>,
) -> Health {
    if !configured {
        return Health::Unconfigured;
    }
    if current.is_some() {
        return Health::Running;
    }
    // A last run whose state this version does not understand (written by a
    // newer version) is not a success and not "nothing happened", with or
    // without an earlier success: the safe reading is a failure that asks for
    // an update (see `health_message`).
    if state
        .last_run
        .as_ref()
        .is_some_and(|last| last.state == RunState::Unknown)
    {
        return Health::Failing;
    }
    let Some(success) = &state.last_success else {
        return Health::Never;
    };
    if let Some(last) = &state.last_run
        && last.state == RunState::Failed
    {
        return Health::Failing;
    }
    let finished = success.finished_at.unwrap_or(success.started_at);
    if now - finished > Duration::hours(i64::from(overdue_after_hours)) {
        return Health::Overdue;
    }
    if !success.incomplete.is_empty() {
        return Health::Incomplete;
    }
    Health::Ok
}

/// An explanatory note for the dashboard and `nestegg status` when the health
/// needs one. Currently only the last run having a state this version does not
/// understand: it is mapped to `failing`, and the note says why and that
/// nestegg should be updated. `None` when nothing needs explaining.
pub fn health_message(state: &State, current: Option<&Run>) -> Option<String> {
    let unknown = |run: Option<&Run>| run.is_some_and(|r| r.state == RunState::Unknown);
    if unknown(current) || unknown(state.last_run.as_ref()) {
        return Some(
            "the last run has a state this version of nestegg does not understand; it was \
             written by a newer version. Update nestegg to read it correctly."
                .to_string(),
        );
    }
    None
}

/// A run is due when there was no success within the interval.
pub fn is_due(state: &State, interval_hours: u32, now: DateTime<Utc>) -> bool {
    match &state.last_success {
        None => true,
        Some(run) => {
            let finished = run.finished_at.unwrap_or(run.started_at);
            // Small tolerance so an hourly timer with random delay does not
            // skip a whole day.
            now - finished >= Duration::hours(i64::from(interval_hours)) - Duration::minutes(30)
        }
    }
}

/// Tolerance for the hourly timer with its random delay.
const TIMER_SLACK_MINUTES: i64 = 30;

/// After two or more failed runs in a row the timer does not try again every
/// hour: the pause doubles (2, 4, 8, 16 hours) up to the backup interval, so
/// a problem that does not go away by itself does not wake the backup
/// server 24 times a day. Returns the earliest time for the next automatic
/// attempt, or `None` when no pause applies.
///
/// `config_now`: [`config_stamp`] of the configuration file as it is now. If
/// it differs from the one the failed run read (somebody is fixing it), the
/// pause ends; so it does after the commands that store credentials or keys
/// ([`Store::clear_failures`]) and for every manually started run.
///
/// `now` guards against a wrong clock (decision 0021): if the failure time
/// lies in the future (the clock was ahead when the run failed and was
/// corrected afterwards), the recorded time cannot be trusted and there is no
/// pause at all — the next hourly check retries at once. Otherwise the pause
/// can never end later than one backup interval from now.
pub fn retry_not_before(
    state: &State,
    interval_hours: u32,
    config_now: Option<&str>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    // A transient failure (a server that was unreachable during the read-back,
    // decision 0015) is not a persistent fault: like `pbs_unreachable` it does
    // not pause the retries.
    let last = state
        .last_run
        .as_ref()
        .filter(|run| run.state == RunState::Failed && !run.transient)?;
    if state.consecutive_failures < 2 {
        return None;
    }
    // A failure time in the future means the clock was ahead and was later
    // corrected. Capping it at "now" would still push the next attempt a full
    // pause into the future at every hourly check (now + pause), freezing the
    // retries. The recorded time is not trustworthy, so do not hold the retries
    // back at all.
    let failed_at = last.finished_at.unwrap_or(last.started_at);
    if failed_at > now {
        return None;
    }
    // Compared by content, not by time: file times and run times have
    // different precision, and a clock can be wrong. A state without a
    // recorded stamp (older version) never pauses.
    if state.failed_config.is_none() || state.failed_config.as_deref() != config_now {
        return None;
    }
    let doubled = 1u32 << (state.consecutive_failures - 1).min(16);
    let hours = doubled.min(interval_hours.max(1));
    let not_before =
        failed_at + Duration::hours(i64::from(hours)) - Duration::minutes(TIMER_SLACK_MINUTES);
    // Never longer than one backup interval from now, so the next attempt is
    // never pushed arbitrarily far out.
    let cap = now + Duration::hours(i64::from(interval_hours.max(1)));
    Some(not_before.min(cap))
}

/// Fingerprint of the configuration file's content (SHA-256), `None` if it
/// cannot be read. Only this one-way digest is stored in the state: the
/// file must not contain secrets, but a hook command in it might.
pub fn config_stamp(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    Some(crate::secrets::digest(&crate::secrets::Secret::new(bytes)))
}

pub fn next_due(state: &State, interval_hours: u32, now: DateTime<Utc>) -> DateTime<Utc> {
    match &state.last_success {
        None => now,
        Some(run) => {
            let due = run.finished_at.unwrap_or(run.started_at)
                + Duration::hours(i64::from(interval_hours));
            due.max(now)
        }
    }
}

/// Storage for `state.json`, `current.json` and `runs/*.json`.
pub struct Store {
    paths: Paths,
}

impl Store {
    pub fn new(paths: &Paths) -> Store {
        Store {
            paths: paths.clone(),
        }
    }

    pub fn load_state(&self) -> Result<State> {
        read_json_or_default(&self.paths.state_file())
    }

    pub fn save_state(&self, state: &State) -> Result<()> {
        write_json_atomic(&self.paths.state_file(), state)
    }

    pub fn load_current(&self) -> Result<Option<Run>> {
        let path = self.paths.current_run_file();
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(read_json(&path)?))
    }

    /// All changes to `current.json` happen under this lock, so "read, compare,
    /// remove" cannot interleave with another process replacing the file.
    fn with_current_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        fs::create_dir_all(&self.paths.state_dir)?;
        // 0600 so a group member cannot hold a shared lock on it and block the
        // daemon (decision 0022, item 2).
        // The lock is held only for a read or a small write, so a wait this
        // long means something is stuck: give up instead of hanging a request
        // or a unit forever.
        let _guard = lock_private(
            &self.paths.state_dir.join(".current.lock"),
            CURRENT_LOCK_WAIT,
        )?
        .context("the record of the running backup is locked by another process; try again")?;
        f()
    }

    pub fn save_current(&self, run: &Run) -> Result<()> {
        self.with_current_lock(|| write_json_atomic(&self.paths.current_run_file(), run))
    }

    /// Remove `current.json` only if it still describes run `id` (another
    /// run may have replaced it in the meantime).
    pub fn clear_current_if(&self, id: &str) -> Result<()> {
        self.with_current_lock(|| match self.load_current() {
            Ok(Some(run)) if run.id == id => self.remove_current_file(),
            _ => Ok(()),
        })
    }

    /// Write a run record without touching `state.json` (for runs that
    /// never got the run lock).
    pub fn record_run_only(&self, run: &Run) -> Result<()> {
        let dir = self.paths.runs_dir();
        fs::create_dir_all(&dir)?;
        write_json_atomic(&dir.join(format!("{}.json", run.id)), run)
    }

    pub fn clear_current(&self) -> Result<()> {
        self.with_current_lock(|| self.remove_current_file())
    }

    fn remove_current_file(&self) -> Result<()> {
        match fs::remove_file(self.paths.current_run_file()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Persist a finished run, update `state.json` and prune old records.
    /// `config_stamp`: [`config_stamp`] of the configuration the run read.
    pub fn record_finished_with_config(
        &self,
        run: &Run,
        keep: usize,
        config_stamp: Option<String>,
    ) -> Result<()> {
        self.record_finished(run, keep)?;
        // A transient failure (server unavailable during the read-back) does
        // not pause the retries, so it does not tie the pause to the config.
        if run.state == RunState::Failed && !run.transient {
            let mut state = self.load_state()?;
            state.failed_config = config_stamp;
            self.save_state(&state)?;
        }
        Ok(())
    }

    /// Persist a finished run, update `state.json` and prune old records.
    pub fn record_finished(&self, run: &Run, keep: usize) -> Result<()> {
        let dir = self.paths.runs_dir();
        fs::create_dir_all(&dir)?;
        write_json_atomic(&dir.join(format!("{}.json", run.id)), run)?;
        let mut state = self.load_state()?;
        state.last_run = Some(run.clone());
        match run.state {
            RunState::Succeeded => {
                state.last_success = Some(run.clone());
                state.consecutive_failures = 0;
            }
            // A transient failure (server unavailable during the read-back,
            // decision 0015) is treated like `pbs_unreachable`: it neither
            // counts nor resets the failures in a row (decision 0021).
            RunState::Failed if !run.transient => {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            }
            _ => {}
        }
        self.save_state(&state)?;
        self.prune(keep)?;
        Ok(())
    }

    /// Forget the failures in a row: something that can cure them was just
    /// changed (credentials stored, key created, imported or confirmed), so
    /// the next timer check tries again at once.
    pub fn clear_failures(&self) -> Result<()> {
        let mut state = self.load_state()?;
        if state.consecutive_failures != 0 {
            state.consecutive_failures = 0;
            self.save_state(&state)?;
        }
        Ok(())
    }

    pub fn list_runs(&self, limit: usize) -> Result<Vec<Run>> {
        let mut files = self.run_files()?;
        files.sort();
        files.reverse();
        let mut runs = Vec::new();
        for path in files.into_iter().take(limit) {
            match read_json::<Run>(&path) {
                Ok(run) => runs.push(run),
                Err(e) => eprintln!(
                    "warning: skipping unreadable run record {}: {e:#}",
                    path.display()
                ),
            }
        }
        runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
        Ok(runs)
    }

    pub fn get_run(&self, id: &str) -> Result<Option<Run>> {
        if !is_valid_run_id(id) {
            return Ok(None);
        }
        let path = self.paths.runs_dir().join(format!("{id}.json"));
        if path.exists() {
            return Ok(Some(read_json(&path)?));
        }
        Ok(self.load_current()?.filter(|run| run.id == id))
    }

    /// The kept run record that produced `snapshot` (`host/<id>/<time>`).
    /// Run records are written by root only, so unlike snapshot notes they
    /// can be trusted (decision 0016).
    pub fn run_for_snapshot(&self, snapshot: &str) -> Result<Option<Run>> {
        for path in self.run_files()? {
            if let Ok(run) = read_json::<Run>(&path)
                && run.snapshot.as_deref() == Some(snapshot)
            {
                return Ok(Some(run));
            }
        }
        Ok(None)
    }

    pub fn log_path(&self, id: &str) -> PathBuf {
        self.paths.runs_dir().join(format!("{id}.log"))
    }

    fn run_files(&self) -> Result<Vec<PathBuf>> {
        let dir = self.paths.runs_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json") {
                files.push(path);
            }
        }
        Ok(files)
    }

    fn prune(&self, keep: usize) -> Result<()> {
        let mut files = self.run_files()?;
        if files.len() <= keep {
            return Ok(());
        }
        files.sort();
        let remove = files.len() - keep;
        for path in files.into_iter().take(remove) {
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(path.with_extension("log"));
        }
        Ok(())
    }
}

/// Open (creating if needed) a lock file that only root processes need, with
/// mode 0600 and no symlink following. The caller takes the `flock` on the
/// returned file.
///
/// A lock file left 0644 by an older version lets a group member open it
/// read-only and hold a shared `flock`, which blocks the root process that
/// wants the exclusive lock (decision 0022, item 2). Tightening the mode alone
/// does not help after an upgrade: a group member that opened the old 0644 file
/// before the upgrade keeps a valid descriptor and can still hold the lock. So
/// a lock file with the wrong mode is replaced by a fresh 0600 file, which
/// leaves that stale descriptor pointing at the now-unlinked old inode where it
/// can no longer block anything. The file is only replaced when no root-owned
/// process holds a lock on it according to `/proc/locks`: a backup run started
/// before the upgrade legitimately holds the run lock, and replacing the file
/// under it would allow two runs at once; such a lock is only tightened in
/// place (a non-root reader cannot share an inode with the root run's exclusive
/// lock anyway).
pub fn open_private_lock(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let file = open_lock_file(path)?;
    let meta = file
        .metadata()
        .with_context(|| format!("cannot stat lock file {}", path.display()))?;
    if meta.mode() & 0o777 == 0o600 {
        return Ok(file);
    }
    // Wrong (too open) mode: an old lock file. Only the root daemon or a root
    // backup run replaces it (the hardening protects those root processes from
    // a group member's stale shared lock); a non-root caller only tightens the
    // mode. Replace it when no root-owned process holds a lock on it, so a
    // running backup's lock is never pulled out from under it.
    if nix::unistd::geteuid().is_root() && !root_holds_flock(meta.dev(), meta.ino()) {
        // Unlink the old inode and create a fresh 0600 one. O_NOFOLLOW on the
        // reopen guards against a symlink planted in between.
        let _ = fs::remove_file(path);
        let fresh = open_lock_file(path)?;
        fresh
            .set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot set mode of {}", path.display()))?;
        return Ok(fresh);
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot set mode of {}", path.display()))?;
    Ok(file)
}

/// Longest wait for the short lock around `current.json`.
const CURRENT_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Take the exclusive lock on the private lock file at `path`, waiting at most
/// `wait` (zero: one attempt). `None` when another process still holds it at
/// the deadline.
///
/// After the lock is taken the path is looked up again: [`open_private_lock`]
/// and the package's `postinst` may replace an old lock file, and a lock on an
/// inode that is no longer at the path excludes nobody. In that case the
/// attempt starts over with the file that is there now, so two processes can
/// never both believe they hold the lock.
pub fn lock_private(
    path: &Path,
    wait: std::time::Duration,
) -> Result<Option<nix::fcntl::Flock<fs::File>>> {
    use std::os::unix::fs::MetadataExt;
    let deadline = std::time::Instant::now() + wait;
    let mut file = open_private_lock(path)?;
    loop {
        match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
            Ok(lock) => {
                let mine = lock
                    .metadata()
                    .with_context(|| format!("cannot stat lock file {}", path.display()))?;
                let same = fs::symlink_metadata(path)
                    .is_ok_and(|at| at.dev() == mine.dev() && at.ino() == mine.ino());
                if same {
                    return Ok(Some(lock));
                }
                // Replaced while this process opened or waited for it.
                drop(lock);
                if std::time::Instant::now() >= deadline {
                    return Ok(None);
                }
                file = open_private_lock(path)?;
            }
            Err((f, nix::errno::Errno::EWOULDBLOCK)) => {
                if std::time::Instant::now() >= deadline {
                    return Ok(None);
                }
                file = f;
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err((_, e)) => {
                return Err(e).with_context(|| format!("cannot lock {}", path.display()));
            }
        }
    }
}

fn open_lock_file(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("cannot open lock file {}", path.display()))
}

/// Whether any process owned by root currently holds a lock (read or write) on
/// the file with `dev`/`ino`, according to `/proc/locks`. Blocked waiters (`->`)
/// are not holders. When `/proc/locks` cannot be read the answer is a
/// conservative `true`, so a lock file is never replaced while a root process
/// might hold it.
pub fn root_holds_flock(dev: u64, ino: u64) -> bool {
    let Ok(locks) = fs::read_to_string("/proc/locks") else {
        return true;
    };
    let major = nix::sys::stat::major(dev);
    let minor = nix::sys::stat::minor(dev);
    for line in locks.lines() {
        if line.contains("->") {
            continue; // a blocked waiter, not a holder
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        // `1: FLOCK  ADVISORY  WRITE 1234 00:19:5678 0 EOF`
        if fields.len() < 6 || fields[1] != "FLOCK" {
            continue;
        }
        let mut id = fields[5].split(':');
        let (Some(ma), Some(mi), Some(i)) = (id.next(), id.next(), id.next()) else {
            continue;
        };
        if u64::from_str_radix(ma, 16).ok() != Some(major)
            || u64::from_str_radix(mi, 16).ok() != Some(minor)
            || i.parse::<u64>().ok() != Some(ino)
        {
            continue;
        }
        let Ok(pid) = fields[4].parse::<u32>() else {
            continue;
        };
        if process_is_root(pid) {
            return true;
        }
    }
    false
}

/// Whether `/proc/<pid>` is owned by root (the process runs as uid 0).
fn process_is_root(pid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(format!("/proc/{pid}"))
        .map(|m| m.uid() == 0)
        .unwrap_or(false)
}

pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
}

fn read_json_or_default<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
    if !path.exists() {
        return Ok(T::default());
    }
    read_json(path)
}

/// Write via temporary file and rename, so readers never see partial JSON.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let dir = path.parent().context("path without parent")?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{:016x}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state"),
        rand::random::<u64>()
    ));
    let write = || -> Result<()> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        // Exclusive, no symlinks; 0644 so group members can read state
        // through the 0750 state directory.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&tmp)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    };
    if let Err(e) = write() {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    // Persist the rename itself: without an fsync on the directory a power loss
    // right after the rename can lose the directory entry and bring the old
    // file back. Best effort — some file systems refuse fsync on a directory.
    if let Ok(dir_file) = fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

pub fn format_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub(crate) mod rfc3339 {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::format_time(*t))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let s = String::deserialize(d)?;
        DateTime::parse_from_rfc3339(&s)
            .map(|t| t.with_timezone(&Utc))
            .map_err(serde::de::Error::custom)
    }
}

pub(crate) mod rfc3339_opt {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => s.serialize_str(&super::format_time(*t)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        let s = Option::<String>::deserialize(d)?;
        s.map(|s| {
            DateTime::parse_from_rfc3339(&s)
                .map(|t| t.with_timezone(&Utc))
                .map_err(serde::de::Error::custom)
        })
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_private_lock_is_not_waited_for_past_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".x.lock");
        let held = lock_private(&path, std::time::Duration::ZERO)
            .unwrap()
            .expect("free lock");
        let started = std::time::Instant::now();
        let second = lock_private(&path, std::time::Duration::from_millis(200)).unwrap();
        assert!(second.is_none(), "the lock is held by another descriptor");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        drop(held);
        assert!(
            lock_private(&path, std::time::Duration::ZERO)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_lock_on_a_replaced_file_is_not_taken_for_the_lock() {
        use std::os::unix::fs::MetadataExt;
        // A waiter has the old file open; the file is replaced (as an upgrade
        // does with an old lock file) and the old holder lets go. The waiter
        // must end up with the lock on the file that is at the path now, not
        // on the old inode that nobody else will ever look at again.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".x.lock");
        let old = lock_private(&path, std::time::Duration::ZERO)
            .unwrap()
            .expect("free lock");
        let old_ino = old.metadata().unwrap().ino();
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || {
                lock_private(&path, std::time::Duration::from_secs(20)).unwrap()
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        // Keep the old inode alive (so the new file cannot reuse its number).
        fs::remove_file(&path).unwrap();
        let fresh = open_lock_file(&path).unwrap();
        let fresh_ino = fresh.metadata().unwrap().ino();
        assert_ne!(old_ino, fresh_ino);
        drop(fresh);
        drop(old);
        let got = waiter.join().unwrap().expect("the waiter gets the lock");
        assert_eq!(got.metadata().unwrap().ino(), fresh_ino);
    }
    use chrono::TimeZone;

    fn t(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 8, h, 0, 0).unwrap()
    }

    fn finished(state: RunState, at: DateTime<Utc>) -> Run {
        let mut run = Run::new(Trigger::Timer, at - Duration::minutes(5));
        run.finish(state, at);
        run
    }

    #[test]
    fn run_ids_are_safe_file_names() {
        let id = run_id(Trigger::Manual, t(21));
        assert!(id.starts_with("20261108T210000Z-manual-"));
        let ids: std::collections::HashSet<_> =
            (0..20).map(|_| run_id(Trigger::Manual, t(21))).collect();
        assert!(ids.len() > 1);
        assert!(is_valid_run_id(&id));
        assert!(!is_valid_run_id("../etc/passwd"));
        assert!(!is_valid_run_id(""));
    }

    #[test]
    fn health_states() {
        let mut state = State::default();
        assert_eq!(health(false, &state, None, 36, t(12)), Health::Unconfigured);
        assert_eq!(health(true, &state, None, 36, t(12)), Health::Never);

        let ok = finished(RunState::Succeeded, t(10));
        state.last_success = Some(ok.clone());
        state.last_run = Some(ok);
        assert_eq!(health(true, &state, None, 36, t(12)), Health::Ok);
        assert_eq!(health(true, &state, None, 1, t(12)), Health::Overdue);

        let current = Run::new(Trigger::Manual, t(12));
        assert_eq!(
            health(true, &state, Some(&current), 36, t(12)),
            Health::Running
        );

        state.last_run = Some(finished(RunState::Failed, t(11)));
        assert_eq!(health(true, &state, None, 36, t(12)), Health::Failing);

        // An unreachable PBS alone is not a failure while backups are recent.
        state.last_run = Some(finished(RunState::PbsUnreachable, t(11)));
        assert_eq!(health(true, &state, None, 36, t(12)), Health::Ok);

        // A success that left something out is not "ok"; failing and overdue
        // take precedence.
        let mut partial = finished(RunState::Succeeded, t(10));
        partial.incomplete.push("full system backup skipped".into());
        state.last_success = Some(partial.clone());
        state.last_run = Some(partial);
        assert_eq!(health(true, &state, None, 36, t(12)), Health::Incomplete);
        assert_eq!(health(true, &state, None, 1, t(12)), Health::Overdue);
        state.last_run = Some(finished(RunState::Failed, t(11)));
        assert_eq!(health(true, &state, None, 36, t(12)), Health::Failing);

        // A last run whose state this version does not understand (written by a
        // newer version) is not read as a success: it is `failing` with a note
        // that nestegg should be updated.
        assert_eq!(health_message(&state, None), None);
        state.last_run = Some(finished(RunState::Unknown, t(11)));
        assert_eq!(health(true, &state, None, 36, t(12)), Health::Failing);
        // Also without any earlier success: not "never backed up".
        let only_unknown = State {
            last_run: Some(finished(RunState::Unknown, t(11))),
            ..State::default()
        };
        assert_eq!(
            health(true, &only_unknown, None, 36, t(12)),
            Health::Failing
        );
        assert!(health_message(&state, None).unwrap().contains("Update"));
        // Never read as ok even with a very long overdue window.
        assert_eq!(health(true, &state, None, 1000, t(12)), Health::Failing);
    }

    #[test]
    fn due_logic() {
        let mut state = State::default();
        assert!(is_due(&state, 24, t(12)));
        state.last_success = Some(finished(RunState::Succeeded, t(1)));
        assert!(!is_due(&state, 24, t(12)));
        assert!(!is_due(&state, 12, t(12)));
        assert!(is_due(&state, 11, t(12)));
        assert_eq!(next_due(&state, 24, t(12)), t(1) + Duration::hours(24));
    }

    #[test]
    fn retries_pause_longer_after_each_failure_in_a_row() {
        let mut state = State::default();
        let failed_at = t(10);
        state.last_run = Some(finished(RunState::Failed, failed_at));
        state.failed_config = Some("stamp-a".into());
        let same = Some("stamp-a");
        // "now" is the moment of the failure here, so the one-interval cap
        // never shortens the computed pause.
        let now = failed_at;
        let pause = |state: &State| {
            retry_not_before(state, 24, same, now).map(|at| (at - failed_at).num_minutes())
        };
        // The first failure is retried at the next hourly check as before.
        state.consecutive_failures = 1;
        assert_eq!(pause(&state), None);
        // Then 2, 4, 8, 16 hours (minus the timer tolerance), capped at the
        // backup interval.
        for (failures, hours) in [(2, 2), (3, 4), (4, 8), (5, 16), (6, 24), (40, 24)] {
            state.consecutive_failures = failures;
            assert_eq!(pause(&state), Some(hours * 60 - 30), "{failures} failures");
        }
        state.consecutive_failures = 4;
        assert_eq!(
            retry_not_before(&state, 6, same, now).map(|at| (at - failed_at).num_minutes()),
            Some(6 * 60 - 30),
            "never longer than the interval"
        );
        // A configuration file with other content than the failed run read
        // ends the pause (also a file that cannot be read any more); a state
        // of an older version without a stamp never pauses.
        assert_eq!(retry_not_before(&state, 24, Some("stamp-b"), now), None);
        assert_eq!(retry_not_before(&state, 24, None, now), None);
        let mut old = state.clone();
        old.failed_config = None;
        assert_eq!(retry_not_before(&old, 24, same, now), None);
        assert_eq!(retry_not_before(&old, 24, None, now), None);
        // An unreachable server or a cancelled run is not a failure.
        state.last_run = Some(finished(RunState::PbsUnreachable, t(11)));
        assert_eq!(retry_not_before(&state, 24, same, now), None);
    }

    #[test]
    fn retry_pause_is_bounded_by_a_wrong_clock() {
        let mut state = State {
            failed_config: Some("stamp-a".into()),
            consecutive_failures: 5, // would be a 16 h pause
            ..State::default()
        };
        let same = Some("stamp-a");

        // The failed run's time lies in the future (the clock was ahead and was
        // corrected afterwards). Capping it at "now" would still push the next
        // attempt a full pause into the future at every hourly check and freeze
        // the retries, so a future stamp means no pause at all: the timer
        // retries at once.
        let now = t(2);
        let future = t(12);
        state.last_run = Some(finished(RunState::Failed, future));
        assert_eq!(retry_not_before(&state, 24, same, now), None);

        // A normal (past) failure still pauses, and never beyond one backup
        // interval from now.
        state.consecutive_failures = 40;
        let now = t(2);
        state.last_run = Some(finished(RunState::Failed, t(1)));
        let not_before = retry_not_before(&state, 3, same, now).unwrap();
        assert!(not_before <= now + Duration::hours(3));
    }

    #[test]
    fn a_transient_verification_failure_does_not_pause_the_retries() {
        // A read-back that could not be completed because the server was
        // unavailable (decision 0015) is recorded as a failed run, but it must
        // not pause the retries: the next hourly check tries again, like after
        // `pbs_unreachable` (decision 0021).
        let mut state = State {
            failed_config: Some("stamp-a".into()),
            consecutive_failures: 5,
            ..State::default()
        };
        let mut run = finished(RunState::Failed, t(10));
        run.transient = true;
        state.last_run = Some(run);
        assert_eq!(retry_not_before(&state, 24, Some("stamp-a"), t(11)), None);

        // The counter does not move for a transient failure, the same as for
        // `pbs_unreachable`.
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: dir.path().join("config.toml"),
            credentials_dir: dir.path().join("cred"),
            state_dir: dir.path().join("state"),
            runtime_dir: dir.path().join("run"),
            systemctl: PathBuf::from("systemctl"),
        };
        let store = Store::new(&paths);
        store
            .record_finished(&finished(RunState::Failed, t(1)), 50)
            .unwrap();
        let mut transient = finished(RunState::Failed, t(2));
        transient.transient = true;
        store.record_finished(&transient, 50).unwrap();
        assert_eq!(store.load_state().unwrap().consecutive_failures, 1);
    }

    #[test]
    fn unknown_enum_values_from_a_newer_version_stay_readable() {
        // A newer version can write enum values this one does not know; they
        // must not make state.json or a run record unreadable.
        assert_eq!(
            serde_json::from_str::<ErrorKind>("\"future_kind\"").unwrap(),
            ErrorKind::Unknown
        );
        assert_eq!(
            serde_json::from_str::<ErrorKind>("\"config\"").unwrap(),
            ErrorKind::Config
        );
        assert_eq!(
            serde_json::from_str::<RunState>("\"future_state\"").unwrap(),
            RunState::Unknown
        );
        // An unknown state (newer version) counts as finished for this
        // process's own safety-net checks; it is never read as a success.
        assert!(RunState::Unknown.is_finished());
        assert_eq!(
            serde_json::from_str::<Trigger>("\"future_trigger\"").unwrap(),
            Trigger::Unknown
        );
        // The catch-all round-trips back out as "unknown".
        assert_eq!(
            serde_json::to_value(ErrorKind::Unknown).unwrap(),
            serde_json::json!("unknown")
        );

        // A whole state.json written by a newer version stays readable: the
        // unknown run state, trigger and error kind all parse.
        let json = r#"{
            "last_run": {
                "id": "20990101T000000Z-future-0000",
                "trigger": "chrono",
                "state": "replicating",
                "started_at": "2099-01-01T00:00:00Z",
                "error": { "kind": "quantum", "message": "from the future" }
            }
        }"#;
        let state: State = serde_json::from_str(json).unwrap();
        let run = state.last_run.unwrap();
        assert_eq!(run.state, RunState::Unknown);
        assert_eq!(run.trigger, Trigger::Unknown);
        assert_eq!(run.error.unwrap().kind, ErrorKind::Unknown);
    }

    #[test]
    fn store_counts_failures_in_a_row() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: dir.path().join("config.toml"),
            credentials_dir: dir.path().join("cred"),
            state_dir: dir.path().join("state"),
            runtime_dir: dir.path().join("run"),
            systemctl: PathBuf::from("systemctl"),
        };
        let store = Store::new(&paths);
        let count = || store.load_state().unwrap().consecutive_failures;
        store
            .record_finished(&finished(RunState::Failed, t(1)), 50)
            .unwrap();
        store
            .record_finished(&finished(RunState::Failed, t(2)), 50)
            .unwrap();
        assert_eq!(count(), 2);
        // Neither a failure nor a cure.
        store
            .record_finished(&finished(RunState::PbsUnreachable, t(3)), 50)
            .unwrap();
        store
            .record_finished(&finished(RunState::Cancelled, t(4)), 50)
            .unwrap();
        assert_eq!(count(), 2);
        store.clear_failures().unwrap();
        assert_eq!(count(), 0);
        store
            .record_finished(&finished(RunState::Failed, t(5)), 50)
            .unwrap();
        store
            .record_finished(&finished(RunState::Succeeded, t(6)), 50)
            .unwrap();
        assert_eq!(count(), 0);
    }

    #[test]
    fn json_matches_api_contract() {
        let mut run = Run::new(Trigger::Timer, t(21));
        run.finish(RunState::PbsUnreachable, t(22));
        run.error = Some(RunError {
            kind: ErrorKind::PbsUnreachable,
            message: "no answer".into(),
        });
        let v = serde_json::to_value(&run).unwrap();
        assert_eq!(v["state"], "pbs_unreachable");
        assert_eq!(v["trigger"], "timer");
        assert_eq!(v["started_at"], "2026-11-08T21:00:00Z");
        assert_eq!(v["duration_seconds"], 3600);
        assert_eq!(v["error"]["kind"], "pbs_unreachable");
        let back: Run = serde_json::from_value(v).unwrap();
        assert_eq!(back, run);
    }

    #[test]
    fn store_records_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: dir.path().join("config.toml"),
            credentials_dir: dir.path().join("cred"),
            state_dir: dir.path().join("state"),
            runtime_dir: dir.path().join("run"),
            systemctl: PathBuf::from("systemctl"),
        };
        let store = Store::new(&paths);
        for h in 0..5 {
            store
                .record_finished(&finished(RunState::Succeeded, t(h)), 3)
                .unwrap();
        }
        store
            .record_finished(&finished(RunState::Failed, t(6)), 3)
            .unwrap();
        let runs = store.list_runs(10).unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].state, RunState::Failed);
        let state = store.load_state().unwrap();
        assert_eq!(state.last_run.unwrap().state, RunState::Failed);
        assert_eq!(state.last_success.unwrap().finished_at, Some(t(4)));
        assert!(store.get_run(&runs[1].id).unwrap().is_some());
        assert!(store.get_run("../x").unwrap().is_none());

        let mut with_snapshot = finished(RunState::Succeeded, t(7));
        with_snapshot.snapshot = Some("host/desktop/2026-11-08T07:00:00Z".into());
        store.record_finished(&with_snapshot, 3).unwrap();
        let found = store
            .run_for_snapshot("host/desktop/2026-11-08T07:00:00Z")
            .unwrap()
            .unwrap();
        assert_eq!(found.id, with_snapshot.id);
        assert!(
            store
                .run_for_snapshot("host/desktop/2026-11-08T08:00:00Z")
                .unwrap()
                .is_none()
        );
    }
}
