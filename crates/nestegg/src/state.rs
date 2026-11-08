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
}

impl RunState {
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            RunState::Succeeded | RunState::Failed | RunState::PbsUnreachable | RunState::Cancelled
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
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunError {
    pub kind: ErrorKind,
    pub message: String,
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
    #[serde(default)]
    pub warnings: Vec<String>,
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
            warnings: Vec::new(),
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
}

impl State {
    pub fn key_exported(&self) -> bool {
        self.key_digest.is_some() && self.key_digest == self.key_exported_digest
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Ok,
    Running,
    Overdue,
    Failing,
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
    Health::Ok
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

    pub fn save_current(&self, run: &Run) -> Result<()> {
        write_json_atomic(&self.paths.current_run_file(), run)
    }

    pub fn clear_current(&self) -> Result<()> {
        match fs::remove_file(self.paths.current_run_file()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Persist a finished run, update `state.json` and prune old records.
    pub fn record_finished(&self, run: &Run, keep: usize) -> Result<()> {
        let dir = self.paths.runs_dir();
        fs::create_dir_all(&dir)?;
        write_json_atomic(&dir.join(format!("{}.json", run.id)), run)?;
        let mut state = self.load_state()?;
        state.last_run = Some(run.clone());
        if run.state == RunState::Succeeded {
            state.last_success = Some(run.clone());
        }
        self.save_state(&state)?;
        self.prune(keep)?;
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
    Ok(())
}

pub fn format_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

mod rfc3339 {
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
    }
}
