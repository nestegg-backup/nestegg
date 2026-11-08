//! One backup run: lock → checks → wake PBS → inventory → backup → notes.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use nix::fcntl::{Flock, FlockArg};

use crate::client::{ArchiveSpec, BackupSpec, Client, snapshot_path};
use crate::config::{Config, Paths};
use crate::secrets::{self, SecretStore};
use crate::state::{ArchiveInfo, ErrorKind, Run, RunError, RunState, Store, Trigger, is_due};
use crate::wake::{self, Availability};

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub trigger: Trigger,
    /// Pre-assigned run ID (used when the daemon starts a run).
    pub run_id: Option<String>,
    /// Run even if the last success is recent (manual and CLI runs).
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunExit {
    Succeeded,
    Failed,
    PbsUnreachable,
    NotDue,
    AlreadyRunning,
}

impl RunExit {
    /// Process exit code. 75 (EX_TEMPFAIL) for an unreachable PBS so the
    /// systemd unit can treat it as "try again later", not as a failure.
    pub fn code(self) -> i32 {
        match self {
            RunExit::Succeeded | RunExit::NotDue | RunExit::AlreadyRunning => 0,
            RunExit::PbsUnreachable => 75,
            RunExit::Failed => 1,
        }
    }
}

/// Exclusive run lock, held for the lifetime of the value.
pub struct RunLock {
    _lock: Flock<File>,
}

impl RunLock {
    pub fn try_acquire(paths: &Paths) -> Result<Option<RunLock>> {
        fs::create_dir_all(&paths.runtime_dir)
            .with_context(|| format!("cannot create {}", paths.runtime_dir.display()))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(paths.lock_file())
            .with_context(|| format!("cannot open {}", paths.lock_file().display()))?;
        // Group members probe the lock read-only; do not depend on the umask.
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o644))
                .with_context(|| format!("cannot set mode of {}", paths.lock_file().display()))?;
        }
        // A short retry: a reader that briefly holds a shared lock (older
        // versions, fallback probe) must not make a start fail.
        let mut file = file;
        for attempt in 0..10 {
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(lock) => return Ok(Some(RunLock { _lock: lock })),
                Err((f, nix::errno::Errno::EWOULDBLOCK)) => {
                    if attempt == 9 || lock_is_held(paths) == Some(true) {
                        return Ok(None);
                    }
                    file = f;
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err((_, e)) => return Err(e).context("cannot lock run file"),
            }
        }
        Ok(None)
    }
}

/// The run currently in progress, if any. A leftover `current.json` without
/// a held lock (crashed run) is ignored after a grace period.
pub fn active_run(paths: &Paths) -> Result<Option<Run>> {
    let store = Store::new(paths);
    let Some(current) = store.load_current()? else {
        return Ok(None);
    };
    let young = Utc::now() - current.started_at <= Duration::seconds(60);
    match lock_is_held(paths) {
        // A run holds the lock.
        Some(true) => Ok(Some(current)),
        // No lock holder: a fresh reservation (run still starting) counts,
        // an old leftover from a crashed run does not.
        Some(false) => Ok(young.then_some(current)),
        // Lock file missing or unreadable for this user: trust current.json
        // while it is fresh or still being updated.
        None => Ok(young.then_some(current)),
    }
}

/// Probe the run lock without write access, so group members can call it.
/// `None` if the lock file cannot be opened.
/// Primary method: look the lock up in `/proc/locks` (no lock is taken, so
/// a status query can never disturb a starting run). Fallback: a shared
/// non-blocking probe.
fn lock_is_held(paths: &Paths) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(paths.lock_file()).ok()?;
    if let Ok(locks) = fs::read_to_string("/proc/locks") {
        return Some(proc_locks_has_write_flock(&locks, meta.dev(), meta.ino()));
    }
    let file = File::open(paths.lock_file()).ok()?;
    match Flock::lock(file, FlockArg::LockSharedNonblock) {
        Ok(_shared) => Some(false),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => Some(true),
        Err(_) => None,
    }
}

/// `/proc/locks` lines look like
/// `1: FLOCK  ADVISORY  WRITE 1234 00:19:5678 0 EOF` (device as hex
/// major:minor, inode decimal). Blocked waiters are prefixed with `->`.
pub fn proc_locks_has_write_flock(locks: &str, dev: u64, ino: u64) -> bool {
    let major = nix::sys::stat::major(dev);
    let minor = nix::sys::stat::minor(dev);
    locks.lines().any(|line| {
        if line.contains("->") {
            return false;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 || fields[1] != "FLOCK" || fields[3] != "WRITE" {
            return false;
        }
        let mut id = fields[5].split(':');
        let (Some(ma), Some(mi), Some(i)) = (id.next(), id.next(), id.next()) else {
            return false;
        };
        u64::from_str_radix(ma, 16).ok() == Some(major)
            && u64::from_str_radix(mi, 16).ok() == Some(minor)
            && i.parse::<u64>().ok() == Some(ino)
    })
}

struct Reporter<'a> {
    store: &'a Store,
    run: Run,
    log: File,
}

impl Reporter<'_> {
    fn phase(&mut self, state: RunState, message: &str) {
        self.run.state = state;
        self.line(message);
        if let Err(e) = self.store.save_current(&self.run) {
            self.line(&format!("warning: cannot update current run state: {e:#}"));
        }
    }

    fn line(&mut self, message: &str) {
        let _ = writeln!(
            self.log,
            "[{}] {message}",
            crate::state::format_time(Utc::now())
        );
        eprintln!("{message}");
    }

    fn warn(&mut self, message: String) {
        self.line(&format!("warning: {message}"));
        self.run.warnings.push(message);
    }

    fn fail(&mut self, state: RunState, error: RunError) {
        self.line(&format!("error: {}", error.message));
        self.run.error = Some(error);
        self.run.finish(state, Utc::now());
    }
}

pub fn execute(paths: &Paths, options: &RunOptions) -> Result<RunExit> {
    let Some(_lock) = RunLock::try_acquire(paths)? else {
        eprintln!("another backup run is in progress");
        // A run promised by the daemon (pre-assigned ID) gets a final record
        // so it does not stay "starting" forever. state.json belongs to the
        // run holding the lock and is not touched.
        if let Some(id) = options
            .run_id
            .as_deref()
            .filter(|id| crate::state::is_valid_run_id(id))
        {
            let store = Store::new(paths);
            let mut run = Run::new(options.trigger, Utc::now());
            run.id = id.to_string();
            run.error = Some(RunError {
                kind: ErrorKind::AlreadyRunning,
                message: "another backup run was already in progress".into(),
            });
            run.finish(RunState::Cancelled, Utc::now());
            store.record_run_only(&run)?;
            store.clear_current_if(id)?;
        }
        return Ok(RunExit::AlreadyRunning);
    };
    let store = Store::new(paths);
    let config = Config::load(&paths.config_file);
    let state = store.load_state()?;

    if let (Ok(cfg), Trigger::Timer, false) = (&config, options.trigger, options.force)
        && !is_due(&state, cfg.schedule.interval_hours, Utc::now())
    {
        eprintln!(
            "not due yet (last success within {} h)",
            cfg.schedule.interval_hours
        );
        return Ok(RunExit::NotDue);
    }

    let mut run = Run::new(options.trigger, Utc::now());
    if let Some(id) = &options.run_id
        && crate::state::is_valid_run_id(id)
    {
        run.id = id.clone();
    }
    fs::create_dir_all(paths.runs_dir())?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(store.log_path(&run.id))
        .context("cannot open run log")?;
    let mut rep = Reporter {
        store: &store,
        run,
        log,
    };
    rep.phase(
        RunState::Starting,
        &format!(
            "nestegg {} run {} started",
            env!("CARGO_PKG_VERSION"),
            rep.run.id
        ),
    );

    let keep = config.as_ref().map(|c| c.backup.keep_runs).unwrap_or(200);
    let result = match &config {
        Ok(cfg) => pipeline(paths, cfg, &mut rep),
        Err(e) => {
            rep.fail(
                RunState::Failed,
                RunError {
                    kind: ErrorKind::Config,
                    message: format!("{e:#}"),
                },
            );
            Ok(())
        }
    };
    if let Err(e) = result {
        rep.fail(
            RunState::Failed,
            RunError {
                kind: ErrorKind::Internal,
                message: format!("{e:#}"),
            },
        );
    }
    if !rep.run.state.is_finished() {
        rep.run.finish(RunState::Succeeded, Utc::now());
    }
    let final_state = rep.run.state;
    rep.line(&format!("run finished: {:?}", final_state));
    store.record_finished(&rep.run, keep)?;
    store.clear_current_if(&rep.run.id)?;
    Ok(match final_state {
        RunState::Succeeded => RunExit::Succeeded,
        RunState::PbsUnreachable => RunExit::PbsUnreachable,
        _ => RunExit::Failed,
    })
}

fn pipeline(paths: &Paths, config: &Config, rep: &mut Reporter<'_>) -> Result<()> {
    let secrets = SecretStore::new(&paths.credentials_dir);
    let mut state = rep.store.load_state()?;

    // 1. Encryption precondition: a key that has been exported.
    if config.backup.encryption {
        if !secrets.exists(secrets::ENCRYPTION_KEY) {
            rep.fail(
                RunState::Failed,
                config_error("encryption is enabled but no key exists; run `nestegg key create`"),
            );
            return Ok(());
        }
        let key_digest = secrets::digest(&secrets.load(secrets::ENCRYPTION_KEY)?);
        if state.key_exported_digest.as_deref() != Some(key_digest.as_str()) {
            rep.fail(
                RunState::Failed,
                config_error(
                    "the encryption key has not been exported yet; run `nestegg key paperkey` or \
                     `nestegg key export <file>`, store it safely, then `nestegg key confirm-exported`",
                ),
            );
            return Ok(());
        }
    }

    // 2. Sources. Resolve the paths first: touching a path may trigger an
    // automount, and the mount table must be read afterwards.
    let resolved: Vec<Option<PathBuf>> = config
        .backup
        .sources
        .iter()
        .map(|s| fs::canonicalize(&s.path).ok())
        .collect();
    let (mounts, fstab) = match crate::mounts::load() {
        Ok(t) => t,
        Err(e) => {
            rep.fail(
                RunState::Failed,
                RunError {
                    kind: ErrorKind::Internal,
                    message: format!("{e:#}"),
                },
            );
            return Ok(());
        }
    };
    let mut covered: Vec<PathBuf> = config
        .backup
        .sources
        .iter()
        .map(|s| s.path.clone())
        .collect();
    covered.extend(config.backup.include_dev.iter().cloned());
    let covered: Vec<PathBuf> = covered
        .into_iter()
        .map(|p| fs::canonicalize(&p).unwrap_or(p))
        .collect();
    let mut archives = Vec::new();
    for (source, canonical) in config.backup.sources.iter().zip(resolved) {
        if let Some(canonical) = canonical {
            let check = crate::mounts::check_source(
                &canonical,
                source.require_mount,
                &covered,
                &mounts,
                &fstab,
            );
            for w in check.warnings {
                rep.warn(w);
            }
            if !check.errors.is_empty() {
                if source.optional {
                    rep.warn(format!(
                        "optional source {} skipped: {}",
                        source.path.display(),
                        check.errors.join("; ")
                    ));
                    continue;
                }
                rep.fail(
                    RunState::Failed,
                    RunError {
                        kind: ErrorKind::SourceMissing,
                        message: check.errors.join("; "),
                    },
                );
                return Ok(());
            }
            archives.push(ArchiveSpec {
                name: source.archive.clone(),
                path: source.path.clone(),
            });
        } else if source.optional {
            rep.warn(format!(
                "optional source {} does not exist, skipped",
                source.path.display()
            ));
        } else {
            rep.fail(
                RunState::Failed,
                RunError {
                    kind: ErrorKind::SourceMissing,
                    message: format!("source {} does not exist", source.path.display()),
                },
            );
            return Ok(());
        }
    }
    for dev in &config.backup.include_dev {
        if !dev.exists() {
            rep.warn(format!("include_dev {} does not exist", dev.display()));
        }
    }

    // 3. PBS availability.
    rep.phase(
        RunState::WakingPbs,
        &format!("checking PBS {}:{}", config.pbs.server, config.pbs.port),
    );
    let availability = wake::ensure_available(config, |msg| rep.line(msg))?;
    state.pbs_checked_at = Some(Utc::now());
    state.pbs_reachable = Some(availability != Availability::Unreachable);
    rep.store.save_state(&state)?;
    match availability {
        Availability::Reachable => rep.line("PBS reachable"),
        Availability::ReachableAfterWake { waited_seconds } => rep.line(&format!(
            "PBS reachable after Wake-on-LAN ({waited_seconds} s)"
        )),
        Availability::Unreachable => {
            let message = if config.wake.is_some() {
                "PBS did not answer, also not after Wake-on-LAN"
            } else {
                "PBS did not answer"
            };
            rep.fail(
                RunState::PbsUnreachable,
                RunError {
                    kind: ErrorKind::PbsUnreachable,
                    message: message.into(),
                },
            );
            return Ok(());
        }
    }

    // 4. Inventory.
    if config.backup.inventory {
        rep.phase(RunState::Inventory, "writing system inventory");
        let dir: PathBuf = paths.inventory_dir();
        match crate::inventory::generate(&dir) {
            Ok(report) => {
                rep.line(&format!(
                    "inventory: {} files written, {} skipped",
                    report.written.len(),
                    report.skipped.len()
                ));
                archives.push(ArchiveSpec {
                    name: "inventory".into(),
                    path: dir,
                });
            }
            Err(e) => rep.warn(format!("inventory failed: {e:#}")),
        }
    }

    // 5. Backup.
    let backup_id = config.effective_backup_id();
    // The PBS rejects a snapshot that is not newer than the previous one
    // (second precision); wait if the last backup was in this second.
    let previous = state
        .last_success
        .as_ref()
        .and_then(|r| r.snapshot.as_deref())
        .and_then(crate::client::snapshot_epoch);
    let mut backup_time = Utc::now().timestamp();
    if let Some(prev) = previous {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while backup_time <= prev && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
            backup_time = Utc::now().timestamp();
        }
    }
    let spec = BackupSpec {
        backup_id: backup_id.clone(),
        namespace: config.pbs.namespace.clone(),
        backup_time,
        archives: archives.clone(),
        excludes: config.backup.excludes.clone(),
        include_dev: config.backup.include_dev.clone(),
        encrypt: config.backup.encryption,
        change_detection_mode: config.backup.change_detection_mode,
        rate_limit: config.backup.rate_limit.clone(),
    };
    let snapshot = snapshot_path(&backup_id, backup_time);
    rep.phase(RunState::Uploading, &format!("backing up to {snapshot}"));
    let client = Client::new(config, secrets);
    let outcome = {
        let log = &mut rep.log;
        client.backup(&spec, log)
    };
    let outcome = match outcome {
        Ok(o) => o,
        Err(error) => {
            let state = if error.kind == ErrorKind::PbsUnreachable {
                RunState::PbsUnreachable
            } else {
                RunState::Failed
            };
            rep.fail(state, error);
            return Ok(());
        }
    };
    rep.run.snapshot = Some(snapshot.clone());
    rep.run.archives = archives
        .iter()
        .map(|a| ArchiveInfo {
            name: a.file_name(),
            source: a.path.display().to_string(),
            size_bytes: outcome
                .archives
                .iter()
                .find(|s| s.name == a.file_name())
                .and_then(|s| s.total_bytes),
        })
        .collect();
    let uploaded: u64 = outcome
        .archives
        .iter()
        .filter_map(|s| s.uploaded_bytes)
        .sum();
    rep.run.uploaded_bytes = (!outcome.archives.is_empty()).then_some(uploaded);

    // 6. Notes on the snapshot, readable by the read-only overview.
    rep.phase(RunState::Finishing, "writing snapshot notes");
    let duration = (Utc::now() - rep.run.started_at).num_seconds();
    let notes = snapshot_notes(&rep.run, duration);
    if let Err(e) = client.set_notes(&snapshot, &notes) {
        rep.warn(format!("could not write snapshot notes: {e:#}"));
    }
    rep.run.finish(RunState::Succeeded, Utc::now());
    Ok(())
}

fn config_error(message: &str) -> RunError {
    RunError {
        kind: ErrorKind::Config,
        message: message.into(),
    }
}

/// First line is shown as the snapshot comment in PBS. The JSON below it
/// contains no paths or personal data.
pub fn snapshot_notes(run: &Run, duration_seconds: i64) -> String {
    let summary = serde_json::json!({
        "nestegg": env!("CARGO_PKG_VERSION"),
        "run": run.id,
        "trigger": run.trigger,
        "duration_seconds": duration_seconds,
        "uploaded_bytes": run.uploaded_bytes,
        "archives": run.archives.iter().map(|a| &a.name).collect::<Vec<_>>(),
        "warnings": run.warnings.len(),
    });
    format!(
        "nestegg {}: ok, {} s, {} warnings\n\n{}",
        env!("CARGO_PKG_VERSION"),
        duration_seconds,
        run.warnings.len(),
        summary
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(RunExit::Succeeded.code(), 0);
        assert_eq!(RunExit::NotDue.code(), 0);
        assert_eq!(RunExit::PbsUnreachable.code(), 75);
        assert_eq!(RunExit::Failed.code(), 1);
    }

    #[test]
    fn notes_have_summary_first_line_and_no_paths() {
        let mut run = Run::new(Trigger::Timer, Utc::now());
        run.archives.push(ArchiveInfo {
            name: "home.pxar".into(),
            source: "/home".into(),
            size_bytes: None,
        });
        let notes = snapshot_notes(&run, 42);
        assert!(notes.lines().next().unwrap().contains("ok, 42 s"));
        assert!(!notes.contains("/home"));
    }

    #[test]
    fn proc_locks_parsing() {
        let dev = nix::sys::stat::makedev(0, 0x19);
        let text = "1: FLOCK  ADVISORY  WRITE 1234 00:19:5678 0 EOF\n2: POSIX  ADVISORY  WRITE 99 00:19:1 0 EOF\n";
        assert!(proc_locks_has_write_flock(text, dev, 5678));
        assert!(!proc_locks_has_write_flock(text, dev, 1));
        assert!(!proc_locks_has_write_flock(
            "1: FLOCK  ADVISORY  READ 1 00:19:5678 0 EOF\n",
            dev,
            5678
        ));
    }

    #[test]
    fn held_lock_is_visible_without_taking_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: dir.path().join("c.toml"),
            credentials_dir: dir.path().join("cred"),
            state_dir: dir.path().join("state"),
            runtime_dir: dir.path().join("run"),
        };
        let lock = RunLock::try_acquire(&paths).unwrap().unwrap();
        assert_eq!(lock_is_held(&paths), Some(true));
        drop(lock);
        assert_eq!(lock_is_held(&paths), Some(false));
    }

    #[test]
    fn lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: dir.path().join("c.toml"),
            credentials_dir: dir.path().join("cred"),
            state_dir: dir.path().join("state"),
            runtime_dir: dir.path().join("run"),
        };
        let first = RunLock::try_acquire(&paths).unwrap();
        assert!(first.is_some());
        assert!(RunLock::try_acquire(&paths).unwrap().is_none());
        drop(first);
        assert!(RunLock::try_acquire(&paths).unwrap().is_some());
    }
}
