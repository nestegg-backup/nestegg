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
use crate::wake::{self, Availability, WakeEvent};

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
    Cancelled,
}

impl RunExit {
    /// Process exit code. 75 (EX_TEMPFAIL) for an unreachable PBS so the
    /// systemd unit can treat it as "try again later", not as a failure.
    pub fn code(self) -> i32 {
        match self {
            RunExit::Succeeded | RunExit::NotDue | RunExit::AlreadyRunning | RunExit::Cancelled => {
                0
            }
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
        // 0600, not 0644: a group member must not be able to open the lock
        // read-only and hold a shared `flock` on it, which would make the
        // exclusive lock below block and so keep backups from starting. Status
        // queries no longer need to open it — they read `/proc/locks` instead
        // (decision 0022, item 2).
        let file = crate::state::open_private_lock(&paths.lock_file())?;
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

/// Probe the run lock. Primary method: look the lock up in `/proc/locks` (no
/// lock is taken, so a status query can never disturb a starting run, and a
/// group member needs no access to the 0600 lock file itself — only to stat it
/// through the state directory, decision 0022, item 2). Fallback (only if
/// `/proc/locks` is unreadable): a shared non-blocking probe, which needs to
/// open the file and so works only for root. `None` if neither method applies.
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
    /// Written by the daemon before it cancels the run: who asked.
    cancel_note: PathBuf,
}

/// File in which the daemon records who cancelled run `run_id`.
pub fn cancel_note_path(paths: &Paths, run_id: &str) -> PathBuf {
    paths.tmp_dir().join(format!("cancel-{run_id}"))
}

/// Who asked to cancel, from the note the daemon wrote (first line, short).
fn read_cancel_note(path: &std::path::Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let who: String = text
        .lines()
        .next()?
        .chars()
        .filter(|c| !c.is_control())
        .take(100)
        .collect();
    let who = who.trim();
    (!who.is_empty()).then(|| who.to_string())
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

    /// The backup goes on, but will not contain everything the
    /// configuration asks for (decision 0015). Also a warning.
    fn incomplete(&mut self, reason: String) {
        self.run.incomplete.push(reason.clone());
        self.warn(reason);
    }

    /// Finish as cancelled if a cancel was requested. Returns true then.
    fn cancelled(&mut self) -> bool {
        if crate::cancel::requested() && !self.run.state.is_finished() {
            self.fail(
                RunState::Cancelled,
                RunError {
                    kind: ErrorKind::Cancelled,
                    message: "the backup was cancelled".into(),
                },
            );
            return true;
        }
        false
    }

    fn fail(&mut self, state: RunState, mut error: RunError) {
        if error.kind == ErrorKind::Cancelled
            && let Some(who) = read_cancel_note(&self.cancel_note)
        {
            error.message = format!("{}; cancel requested by {who}", error.message);
        }
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
    crate::cancel::install_handler();
    let store = Store::new(paths);
    // Taken before the file is parsed: this is what a failure is tied to.
    let config_stamp = crate::state::config_stamp(&paths.config_file);
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
    // After failures in a row the timer pauses longer and longer; manual
    // runs and a changed configuration are not held back.
    if let (Ok(cfg), Trigger::Timer, false) = (&config, options.trigger, options.force) {
        let now = Utc::now();
        if let Some(not_before) = crate::state::retry_not_before(
            &state,
            cfg.schedule.interval_hours,
            config_stamp.as_deref(),
            now,
        ) && now < not_before
        {
            eprintln!(
                "not retrying yet: {} failed runs in a row, next automatic attempt after {} \
                 (start one now with `nestegg backup`)",
                state.consecutive_failures,
                crate::state::format_time(not_before)
            );
            return Ok(RunExit::NotDue);
        }
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
    // Lets the daemon cancel this run (`POST /runs/{id}/cancel`).
    let pid_file = paths.runtime_dir.join("run.pid");
    let _ = crate::secrets::write_private_file(
        &pid_file,
        format!("{} {}\n", std::process::id(), run.id).as_bytes(),
    )
    .and_then(|()| {
        use std::os::unix::fs::PermissionsExt;
        Ok(fs::set_permissions(
            &pid_file,
            fs::Permissions::from_mode(0o644),
        )?)
    });
    run.expected_bytes = state
        .last_success
        .as_ref()
        .map(|r| r.archives.iter().filter_map(|a| a.size_bytes).sum::<u64>())
        .filter(|b| *b > 0);
    let cancel_note = cancel_note_path(paths, &run.id);
    let mut rep = Reporter {
        store: &store,
        run,
        log,
        cancel_note: cancel_note.clone(),
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
            RunError::from_error(&e, ErrorKind::Internal),
        );
    }
    if !rep.run.state.is_finished() {
        // The pipeline always finishes the run itself (success path and every
        // failure path). Reaching this means a code path returned without
        // deciding the outcome; that must never be reported as a success.
        rep.fail(
            RunState::Failed,
            RunError {
                kind: ErrorKind::Internal,
                message: "the run pipeline returned without finishing the run".into(),
            },
        );
    }
    let final_state = rep.run.state;
    rep.line(&format!("run finished: {:?}", final_state));
    store.record_finished_with_config(&rep.run, keep, config_stamp)?;
    store.clear_current_if(&rep.run.id)?;
    let _ = fs::remove_file(&pid_file);
    let _ = fs::remove_file(&cancel_note);
    Ok(match final_state {
        RunState::Succeeded => RunExit::Succeeded,
        RunState::PbsUnreachable => RunExit::PbsUnreachable,
        RunState::Cancelled => RunExit::Cancelled,
        _ => RunExit::Failed,
    })
}

fn pipeline(paths: &Paths, config: &Config, rep: &mut Reporter<'_>) -> Result<()> {
    // System mode: the effective sources depend on the current disk layout.
    let system_config;
    let mut system_layout = None;
    let config = if config.backup.system {
        // Touch the sources first: that triggers automounts, so the mount
        // table read by capture() shows them.
        for s in &config.backup.sources {
            let _ = fs::canonicalize(&s.path);
        }
        match crate::layout::capture() {
            Ok(layout) => {
                let (c, notes) = crate::layout::system_mode_config(
                    config,
                    &layout,
                    |p| fs::canonicalize(p).ok(),
                    |p| p.symlink_metadata().is_ok(),
                );
                for n in notes {
                    match n.strip_prefix("warning: ") {
                        Some(w) if w.starts_with(crate::layout::SYSTEM_SKIPPED) => {
                            rep.incomplete(w.to_string())
                        }
                        Some(w) => rep.warn(w.to_string()),
                        None => rep.line(&n),
                    }
                }
                system_config = c;
                system_layout = Some(layout);
                &system_config
            }
            Err(e) => {
                rep.fail(
                    RunState::Failed,
                    RunError {
                        kind: ErrorKind::Internal,
                        message: format!("cannot read the disk layout: {e:#}"),
                    },
                );
                return Ok(());
            }
        }
    } else {
        config
    };
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
        let key_digest = match secrets.load(secrets::ENCRYPTION_KEY) {
            Ok(key) => secrets::digest(&key),
            Err(e) => {
                rep.fail(
                    RunState::Failed,
                    RunError::from_error(&e, ErrorKind::Config),
                );
                return Ok(());
            }
        };
        if state.key_exported_digest.as_deref() != Some(key_digest.as_str()) {
            rep.fail(
                RunState::Failed,
                config_error(
                    "the encryption key has not been exported yet; run `nestegg key export <file>` \
                     (and move it to a USB stick) or `nestegg key paperkey --format html --output \
                     key.html` (print it, then delete the file), then prove it with `nestegg key \
                     confirm-exported --file <copy>` or `--paper-code <code> --printed`",
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
    let mut covered: Vec<PathBuf> = config
        .backup
        .sources
        .iter()
        .map(|s| s.path.clone())
        .collect();
    covered.extend(config.backup.include_dev.iter().cloned());
    if let Some(layout) = &system_layout {
        covered.extend(crate::layout::intentionally_skipped(layout));
    }
    let covered: Vec<PathBuf> = covered
        .into_iter()
        .map(|p| fs::canonicalize(&p).unwrap_or(p))
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
    let mut archives = Vec::new();
    for (source, canonical) in config.backup.sources.iter().zip(resolved) {
        if let Some(canonical) = canonical {
            // system.pxar only needs the system's own file systems; a missing
            // USB or network disk listed in fstab is not part of it.
            let system_fstab: Vec<crate::mounts::FstabEntry>;
            let fstab_for_source = match &system_layout {
                Some(layout) if source.archive == crate::layout::SYSTEM_ARCHIVE => {
                    system_fstab = fstab
                        .iter()
                        .filter(|e| crate::layout::is_system_entry(layout, &e.spec, &e.point))
                        .cloned()
                        .collect();
                    &system_fstab
                }
                _ => &fstab,
            };
            let check = crate::mounts::check_source(
                &canonical,
                source.require_mount,
                &covered,
                &mounts,
                fstab_for_source,
            );
            for w in check.warnings {
                rep.warn(w);
            }
            if !check.errors.is_empty() {
                if source.optional {
                    rep.incomplete(format!(
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
            rep.incomplete(format!(
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
    let mut wake_send_failed = false;
    let availability = wake::ensure_available(config, |event| match event {
        WakeEvent::Info(msg) => rep.line(msg),
        WakeEvent::SendFailed(msg) => {
            wake_send_failed = true;
            rep.warn(msg.to_string());
        }
        WakeEvent::SendRecovered => {
            wake_send_failed = false;
            rep.line(&event.to_string());
        }
    })?;
    if rep.cancelled() {
        return Ok(());
    }
    state.pbs_checked_at = Some(Utc::now());
    state.pbs_reachable = Some(availability != Availability::Unreachable);
    rep.store.save_state(&state)?;
    match availability {
        Availability::Reachable => rep.line("PBS reachable"),
        Availability::ReachableAfterWake { waited_seconds } => rep.line(&format!(
            "PBS reachable after Wake-on-LAN ({waited_seconds} s)"
        )),
        Availability::Unreachable => {
            let message = if wake_send_failed {
                "PBS did not answer, also not after Wake-on-LAN (sending the wake-up packet \
                 failed, see the warning above)"
            } else if config.active_wake().is_some() {
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

    if rep.cancelled() {
        return Ok(());
    }
    // 4. Inventory.
    let mut inventory_dir = None;
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
                if config.backup.system
                    && report
                        .skipped
                        .iter()
                        .any(|f| f == crate::layout::LAYOUT_FILE)
                {
                    rep.incomplete(format!(
                        "the disk layout ({}) could not be recorded; the rescue stick cannot \
                         restore this snapshot",
                        crate::layout::LAYOUT_FILE
                    ));
                }
                archives.push(ArchiveSpec {
                    name: "inventory".into(),
                    path: dir.clone(),
                });
                inventory_dir = Some(dir);
            }
            Err(e) if config.backup.system => rep.incomplete(format!(
                "inventory failed: {e:#}; without {} the rescue stick cannot restore this snapshot",
                crate::layout::LAYOUT_FILE
            )),
            Err(e) => rep.incomplete(format!("inventory failed: {e:#}")),
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
    // Metadata blob, part of every snapshot (decisions 0015 and 0016).
    let ctx = report_context(config, &state);
    let meta = backup_metadata(config, &ctx, &backup_id, backup_time, &archives, &rep.run);
    let meta_bytes = meta.to_bytes();
    let meta_file = match write_metadata_file(paths, &rep.run.id, &meta_bytes) {
        Ok(f) => f,
        Err(e) => {
            rep.fail(
                RunState::Failed,
                RunError {
                    kind: ErrorKind::Internal,
                    message: format!("cannot write the backup metadata: {e:#}"),
                },
            );
            return Ok(());
        }
    };
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
        metadata: Some(meta_file.path.clone()),
    };
    let snapshot = snapshot_path(&backup_id, backup_time);
    rep.phase(RunState::Uploading, &format!("backing up to {snapshot}"));
    if rep.cancelled() {
        return Ok(());
    }
    let client = Client::new(config, secrets);
    let mut log = rep.log.try_clone().context("cannot duplicate log handle")?;
    let outcome = client.backup(&spec, &mut log, &mut |stats| {
        // Live progress for the dashboard after each finished archive.
        rep.run.processed_bytes =
            Some(rep.run.processed_bytes.unwrap_or(0) + stats.total_bytes.unwrap_or(0));
        rep.run.uploaded_bytes =
            Some(rep.run.uploaded_bytes.unwrap_or(0) + stats.uploaded_bytes.unwrap_or(0));
        let _ = rep.store.save_current(&rep.run);
    });
    let outcome = match outcome {
        Ok(o) => o,
        Err(error) => {
            let state = match error.kind {
                ErrorKind::PbsUnreachable => RunState::PbsUnreachable,
                ErrorKind::Cancelled => RunState::Cancelled,
                _ => RunState::Failed,
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

    // 6. Read the snapshot back (decision 0015).
    rep.phase(
        RunState::Finishing,
        &format!("reading {snapshot} back from the server"),
    );
    let verification = {
        let expected = crate::verify::Expected {
            snapshot: &snapshot,
            archives: archives.iter().map(ArchiveSpec::file_name).collect(),
            encrypted: config.backup.encryption,
            metadata: &meta_bytes,
            inventory: inventory_dir.as_deref(),
        };
        match crate::secrets::create_private_dir(&paths.tmp_dir())
            .and_then(|()| crate::staging::StagingDir::create(&paths.tmp_dir().join("verify")))
        {
            Ok(work) => crate::verify::read_back(
                &client,
                &expected,
                work.path(),
                &mut |msg| rep.line(msg),
                &crate::cancel::requested,
            ),
            Err(e) => {
                crate::verify::Outcome::Failed(format!("cannot prepare the read-back check: {e:#}"))
            }
        }
    };
    drop(meta_file);
    match verification {
        crate::verify::Outcome::Verified => {
            rep.run.verified = true;
            rep.line("snapshot read back from the server and verified");
        }
        crate::verify::Outcome::Cancelled => {
            rep.fail(
                RunState::Cancelled,
                RunError {
                    kind: ErrorKind::Cancelled,
                    message: format!(
                        "the backup was cancelled during the read-back check; {snapshot} was \
                         uploaded but not verified"
                    ),
                },
            );
            return Ok(());
        }
        crate::verify::Outcome::Failed(message) => {
            record_server_verification(rep, &client, &backup_id);
            rep.fail(
                RunState::Failed,
                RunError {
                    kind: ErrorKind::VerificationFailed,
                    message,
                },
            );
            return Ok(());
        }
    }
    // Verified: a cancel request that arrives now comes too late.
    record_server_verification(rep, &client, &backup_id);

    // 7. Notes on the snapshot, readable by the read-only overview.
    rep.phase(RunState::Finishing, "writing snapshot notes");
    let duration = (Utc::now() - rep.run.started_at).num_seconds();
    let notes = snapshot_notes(&rep.run, duration, &ctx);
    if let Err(e) =
        client.set_notes_bounded(&snapshot, &notes, crate::client::readback_list_timeout())
    {
        rep.warn(format!("could not write snapshot notes: {e:#}"));
    }
    rep.run.finish(RunState::Succeeded, Utc::now());
    Ok(())
}

/// The metadata file of a run; removed when dropped.
struct MetadataFile {
    path: PathBuf,
}

impl Drop for MetadataFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Write the metadata into the root-only runtime directory.
fn write_metadata_file(paths: &Paths, run_id: &str, bytes: &[u8]) -> Result<MetadataFile> {
    secrets::create_private_dir(&paths.tmp_dir())?;
    let path = paths.tmp_dir().join(format!("{run_id}.meta.json"));
    secrets::write_private_file(&path, bytes)?;
    Ok(MetadataFile { path })
}

/// Content of the metadata blob of this run.
fn backup_metadata(
    config: &Config,
    ctx: &ReportContext,
    backup_id: &str,
    backup_time: i64,
    archives: &[ArchiveSpec],
    run: &Run,
) -> crate::metadata::BackupMetadata {
    crate::metadata::BackupMetadata {
        format: crate::metadata::FORMAT,
        nestegg_version: env!("CARGO_PKG_VERSION").into(),
        backup_id: backup_id.into(),
        hostname: ctx.hostname.clone(),
        backup_time,
        snapshot: snapshot_path(backup_id, backup_time),
        archive_sources: ctx
            .archive_sources
            .iter()
            .filter(|(name, _)| archives.iter().any(|a| &a.file_name() == *name))
            .filter_map(|(name, path)| Some((name.clone(), path.as_str()?.to_string())))
            .collect(),
        sources: ctx.sources.clone(),
        system_mode: config.backup.system,
        incomplete: run.incomplete.clone(),
    }
}

/// Store the snapshots of this machine that the server's own verification
/// marked as failed (`State::server_verify_failed`).
fn record_server_verification(rep: &mut Reporter<'_>, client: &Client, backup_id: &str) {
    let failed =
        match client.list_snapshots_bounded(backup_id, crate::client::readback_list_timeout()) {
            Ok(list) => crate::verify::failed_snapshots(&list),
            Err(e) => {
                rep.warn(format!(
                    "could not read the server's verification state: {e:#}"
                ));
                return;
            }
        };
    if !failed.is_empty() {
        rep.line(&format!(
            "the server's verification marked {} snapshot(s) as failed: {}",
            failed.len(),
            failed.join(", ")
        ));
    }
    let saved = rep.store.load_state().and_then(|mut state| {
        state.server_verify_failed = failed;
        rep.store.save_state(&state)
    });
    if let Err(e) = saved {
        rep.warn(format!("cannot update state: {e:#}"));
    }
}

fn config_error(message: &str) -> RunError {
    RunError {
        kind: ErrorKind::Config,
        message: message.into(),
    }
}

/// Facts about this machine that go into the snapshot report.
pub struct ReportContext {
    pub hostname: String,
    pub os: Option<String>,
    pub key_exported: Option<bool>,
    pub sources: Vec<String>,
    /// `home.pxar` → canonical source path, so later restores can map home
    /// directories onto archive paths of exactly this snapshot.
    pub archive_sources: serde_json::Map<String, serde_json::Value>,
    pub overdue_after_hours: u32,
}

fn report_context(config: &Config, state: &crate::state::State) -> ReportContext {
    ReportContext {
        hostname: crate::config::hostname(),
        os: crate::status::os_name(),
        key_exported: config.backup.encryption.then(|| state.key_exported()),
        sources: config
            .backup
            .sources
            .iter()
            .map(|s| s.path.display().to_string())
            .collect(),
        archive_sources: config
            .backup
            .sources
            .iter()
            .map(|s| {
                let path = fs::canonicalize(&s.path).unwrap_or_else(|_| s.path.clone());
                (
                    format!("{}.pxar", s.archive),
                    serde_json::json!(path.display().to_string()),
                )
            })
            .collect(),
        overdue_after_hours: config.overdue_after_hours,
    }
}

/// Snapshot notes: the first line is shown as the snapshot comment in PBS;
/// the JSON below is the status report read by the overview of all
/// machines (`GET /api/v1/machines`). It contains no file names.
pub fn snapshot_notes(run: &Run, duration_seconds: i64, ctx: &ReportContext) -> String {
    // Only archives that are part of this snapshot.
    let archive_sources: serde_json::Map<String, serde_json::Value> = ctx
        .archive_sources
        .iter()
        .filter(|(name, _)| run.archives.iter().any(|a| &a.name == *name))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let report = serde_json::json!({
        "nestegg": env!("CARGO_PKG_VERSION"),
        "nestegg_version": env!("CARGO_PKG_VERSION"),
        "reported_at": crate::state::format_time(Utc::now()),
        "result": "succeeded",
        "run": run.id,
        "run_started_at": crate::state::format_time(run.started_at),
        "trigger": run.trigger,
        "duration_seconds": duration_seconds,
        "uploaded_bytes": run.uploaded_bytes,
        "archives": run.archives.iter().map(|a| &a.name).collect::<Vec<_>>(),
        "warnings": run.warnings.len(),
        "incomplete": run.incomplete,
        "verified": run.verified,
        "hostname": ctx.hostname,
        "os": ctx.os,
        "key_exported": ctx.key_exported,
        "sources": ctx.sources,
        "archive_sources": archive_sources,
        "overdue_after_hours": ctx.overdue_after_hours,
    });
    format!(
        "nestegg {}: {}, {} s, {} warnings\n\n{}",
        env!("CARGO_PKG_VERSION"),
        if run.incomplete.is_empty() {
            "ok"
        } else {
            "incomplete"
        },
        duration_seconds,
        run.warnings.len(),
        report
    )
}

/// Extract the JSON report from snapshot notes (text after the first blank
/// line, or the whole text if it is JSON).
pub fn parse_report(notes: &str) -> Option<serde_json::Value> {
    let body = notes.split_once("\n\n").map(|(_, b)| b).unwrap_or(notes);
    serde_json::from_str(body.trim()).ok()
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
        let ctx = ReportContext {
            hostname: "desktop".into(),
            os: None,
            key_exported: Some(true),
            sources: vec!["/home".into()],
            archive_sources: Default::default(),
            overdue_after_hours: 36,
        };
        let notes = snapshot_notes(&run, 42, &ctx);
        let report = parse_report(&notes).unwrap();
        assert_eq!(report["result"], "succeeded");
        assert_eq!(report["hostname"], "desktop");
        assert!(notes.lines().next().unwrap().contains("ok, 42 s"));
        assert!(
            !notes.contains("home.pxar:"),
            "no client arguments in notes"
        );
    }

    #[test]
    fn cancel_note_names_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("cancel-x");
        assert_eq!(read_cancel_note(&note), None);
        fs::write(&note, "alice (uid 1000)\nignored\n").unwrap();
        assert_eq!(read_cancel_note(&note).as_deref(), Some("alice (uid 1000)"));
        fs::write(&note, "\x1b[2Jbob\n").unwrap();
        assert_eq!(read_cancel_note(&note).as_deref(), Some("[2Jbob"));
        fs::write(&note, "\n").unwrap();
        assert_eq!(read_cancel_note(&note), None);
    }

    #[test]
    fn metadata_lists_only_archives_of_the_snapshot() {
        let config = crate::config::example("pbs.example.com", "store", "a@pbs!t");
        let ctx = ReportContext {
            hostname: "desktop".into(),
            os: None,
            key_exported: Some(true),
            sources: vec!["/home".into(), "/etc".into()],
            archive_sources: [
                ("home.pxar".to_string(), serde_json::json!("/home")),
                ("etc.pxar".to_string(), serde_json::json!("/etc")),
            ]
            .into_iter()
            .collect(),
            overdue_after_hours: 36,
        };
        let mut run = Run::new(Trigger::Timer, Utc::now());
        run.incomplete.push("optional source /etc skipped".into());
        let archives = vec![
            ArchiveSpec {
                name: "home".into(),
                path: "/home".into(),
            },
            ArchiveSpec {
                name: "inventory".into(),
                path: "/var/lib/nestegg/inventory".into(),
            },
        ];
        let meta = backup_metadata(&config, &ctx, "desktop", 1_794_000_000, &archives, &run);
        assert_eq!(meta.snapshot, "host/desktop/2026-11-06T21:20:00Z");
        assert!(meta.belongs_to("host/desktop/2026-11-06T21:20:00Z"));
        assert_eq!(meta.source_of("home.pxar"), Some("/home"));
        assert_eq!(meta.source_of("etc.pxar"), None);
        assert_eq!(meta.source_of("inventory.pxar"), None);
        assert_eq!(meta.incomplete, run.incomplete);
        assert_eq!(meta.sources, ctx.sources);
    }

    #[test]
    fn incomplete_notes_say_so() {
        let mut run = Run::new(Trigger::Timer, Utc::now());
        run.incomplete
            .push("optional source /data does not exist, skipped".into());
        run.warnings = run.incomplete.clone();
        run.verified = true;
        let ctx = ReportContext {
            hostname: "desktop".into(),
            os: None,
            key_exported: None,
            sources: vec![],
            archive_sources: Default::default(),
            overdue_after_hours: 36,
        };
        let notes = snapshot_notes(&run, 5, &ctx);
        assert!(notes.lines().next().unwrap().contains(": incomplete, 5 s"));
        let report = parse_report(&notes).unwrap();
        assert_eq!(report["incomplete"][0], run.incomplete[0]);
        assert_eq!(report["verified"], true);
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
            systemctl: PathBuf::from("systemctl"),
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
            systemctl: PathBuf::from("systemctl"),
        };
        let first = RunLock::try_acquire(&paths).unwrap();
        assert!(first.is_some());
        assert!(RunLock::try_acquire(&paths).unwrap().is_none());
        drop(first);
        assert!(RunLock::try_acquire(&paths).unwrap().is_some());
    }
}
