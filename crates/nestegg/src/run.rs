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

/// The run lock could not be taken, but nothing recognisable holds it: no
/// exclusive lock on the file according to `/proc/locks` and no fresh record of
/// a running backup. Something else (a stray shared lock, for example) keeps
/// backups from starting. This is an error, not "already running": reported
/// with exit code 1 and recorded as a failed run, so it does not go unnoticed.
#[derive(Debug)]
pub struct LockBlocked {
    pub message: String,
}

impl std::fmt::Display for LockBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LockBlocked {}

impl RunLock {
    /// Take the run lock. `Ok(None)` only when a backup run (or another
    /// command that takes the run lock, such as a key command) really holds
    /// it; a lock that cannot be taken although nothing of that kind is
    /// recognisable is an error of type [`LockBlocked`].
    pub fn try_acquire(paths: &Paths) -> Result<Option<RunLock>> {
        Self::try_acquire_for(paths, None)
    }

    /// Like [`RunLock::try_acquire`] for the run with ID `own_run_id`: the
    /// daemon's reservation of this very run in `current.json` is not taken
    /// for another run in progress.
    pub fn try_acquire_for(paths: &Paths, own_run_id: Option<&str>) -> Result<Option<RunLock>> {
        fs::create_dir_all(&paths.runtime_dir)
            .with_context(|| format!("cannot create {}", paths.runtime_dir.display()))?;
        // 0600, not 0644: a group member must not be able to open the lock
        // read-only and hold a shared `flock` on it, which would make the
        // exclusive lock below block and so keep backups from starting. Status
        // queries do not need to open it — they read `/proc/locks` instead
        // (decision 0022, item 2).
        // A short retry: a reader that briefly holds a shared lock (fallback
        // probe) must not make a start fail.
        for attempt in 0..10 {
            let one_try = std::time::Duration::ZERO;
            if let Some(lock) = crate::state::lock_private(&paths.lock_file(), one_try)
                .context("cannot lock run file")?
            {
                return Ok(Some(RunLock { _lock: lock }));
            }
            if lock_is_held(paths) == Some(true) {
                return Ok(None);
            }
            if attempt < 9 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        if run_in_progress(paths, own_run_id) {
            return Ok(None);
        }
        Err(LockBlocked {
            message: blocked_message(paths),
        }
        .into())
    }
}

/// Whether a backup run is recognisable: an exclusive lock on the run lock
/// file, or a fresh `current.json` of another run.
fn run_in_progress(paths: &Paths, own_run_id: Option<&str>) -> bool {
    if lock_is_held(paths) == Some(true) {
        return true;
    }
    matches!(active_run(paths), Ok(Some(run)) if Some(run.id.as_str()) != own_run_id)
}

/// The message for a run lock that is blocked without a recognisable run,
/// naming the holders `/proc/locks` lists.
fn blocked_message(paths: &Paths) -> String {
    use std::os::unix::fs::MetadataExt;
    let path = paths.lock_file();
    let locks = fs::read_to_string("/proc/locks").ok();
    let holders = fs::metadata(&path)
        .ok()
        .zip(locks.as_deref())
        .map(|(meta, locks)| proc_locks_holders(locks, meta.dev(), meta.ino()))
        .unwrap_or_default();
    let who = if locks.is_none() {
        "/proc/locks cannot be read, so the holder is not known".to_string()
    } else if holders.is_empty() {
        // Also the case on btrfs, where `/proc/locks` lists the lock under
        // another device number than the file has.
        "/proc/locks does not show who holds it".to_string()
    } else {
        let list: Vec<String> = holders
            .iter()
            .map(|(pid, kind)| match process_name(*pid) {
                Some(name) => format!("pid {pid} ({name}, {kind} lock)"),
                None => format!("pid {pid} ({kind} lock)"),
            })
            .collect();
        format!("held by {}", list.join(", "))
    };
    format!(
        "the run lock {} cannot be taken although no backup is running ({who}); no backup \
         can start until that lock is released (end the process holding it)",
        path.display()
    )
}

/// Short command name of a process, for messages.
fn process_name(pid: u32) -> Option<String> {
    let name = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let name: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(32)
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The processes holding a `flock` (read or write) on the file with
/// `dev`/`ino` according to `/proc/locks`, with the kind of lock. Blocked
/// waiters (`->`) are not holders. Only used for messages.
pub fn proc_locks_holders(locks: &str, dev: u64, ino: u64) -> Vec<(u32, &'static str)> {
    let major = nix::sys::stat::major(dev);
    let minor = nix::sys::stat::minor(dev);
    let mut holders = Vec::new();
    for line in locks.lines() {
        if line.contains("->") {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
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
        let kind = if fields[3] == "WRITE" {
            "exclusive"
        } else {
            "shared"
        };
        if !holders.contains(&(pid, kind)) {
            holders.push((pid, kind));
        }
    }
    holders
}

/// The run currently in progress, if any. A leftover `current.json` without
/// a held lock (crashed run) is ignored after a grace period.
pub fn active_run(paths: &Paths) -> Result<Option<Run>> {
    let store = Store::new(paths);
    let Some(current) = store.load_current()? else {
        return Ok(None);
    };
    // A start time in the future (the clock was ahead when it was written)
    // says nothing about the age: such a leftover is not "young", or it would
    // count as a run in progress until real time caught up.
    let age = Utc::now() - current.started_at;
    let young = age <= Duration::seconds(60) && age >= -crate::state::future_tolerance();
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

/// Whether a run holds the run lock. First `/proc/locks` (no lock is taken,
/// and a group member needs no access to the 0600 lock file itself — only to
/// stat it through the state directory, decision 0022, item 2): a listed
/// exclusive lock is a run. If it is not listed, or `/proc/locks` cannot be
/// read, a shared non-blocking probe decides for those who may open the file
/// (root). `None` if neither method applies.
fn lock_is_held(paths: &Paths) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(paths.lock_file()).ok()?;
    let listed = fs::read_to_string("/proc/locks")
        .ok()
        .map(|locks| proc_locks_has_write_flock(&locks, meta.dev(), meta.ino()));
    if listed == Some(true) {
        return Some(true);
    }
    // "Not listed" is not proof: on btrfs the device number in `/proc/locks`
    // is not the one `stat` gives, so a lock that is really held is not found
    // there. Whoever may open the file (root: the daemon, a run) asks the
    // kernel directly; a group member cannot, and keeps the answer above.
    exclusive_holder_probe(paths).or(listed)
}

/// Ask the kernel whether somebody holds the run lock exclusively: a shared
/// non-blocking lock on a second descriptor fails exactly then. It is let go
/// at once; a run that starts in that instant tries again (see
/// [`RunLock::try_acquire_for`]). `None` when the file cannot be opened (not
/// root) or the probe fails for another reason.
fn exclusive_holder_probe(paths: &Paths) -> Option<bool> {
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
    let _lock = match RunLock::try_acquire_for(paths, options.run_id.as_deref()) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
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
        }
        Err(e) => match e.downcast_ref::<LockBlocked>() {
            Some(blocked) => return record_blocked(paths, options, &blocked.message),
            None => return Err(e),
        },
    };
    crate::cancel::install_handler();
    let store = Store::new(paths);
    // Taken before the file is parsed: this is what a failure is tied to.
    let config_stamp = crate::state::config_stamp(&paths.config_file);
    let config = Config::load(&paths.config_file);
    let state = store.load_state()?;

    if let Some(reason) = timer_skip(&config, &state, config_stamp.as_deref(), options) {
        eprintln!("{reason}");
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

/// Why a timer run does not start now (not due yet, or the growing pause
/// after failures in a row), `None` when it starts. Manual and CLI runs and
/// forced runs always start.
fn timer_skip(
    config: &Result<Config>,
    state: &crate::state::State,
    config_stamp: Option<&str>,
    options: &RunOptions,
) -> Option<String> {
    let (Ok(cfg), Trigger::Timer, false) = (config, options.trigger, options.force) else {
        return None;
    };
    let now = Utc::now();
    if !is_due(state, cfg.schedule.interval_hours, now) {
        return Some(format!(
            "not due yet (last success within {} h)",
            cfg.schedule.interval_hours
        ));
    }
    // After failures in a row the timer pauses longer and longer; manual
    // runs and a changed configuration are not held back.
    if let Some(not_before) =
        crate::state::retry_not_before(state, cfg.schedule.interval_hours, config_stamp, now)
        && now < not_before
    {
        return Some(format!(
            "not retrying yet: {} failed runs in a row, next automatic attempt after {} \
             (start one now with `nestegg backup`)",
            state.consecutive_failures,
            crate::state::format_time(not_before)
        ));
    }
    None
}

/// The run lock is blocked although no backup runs ([`LockBlocked`]). A timer
/// run that would not have started anyway stays quiet; otherwise the attempt is
/// recorded as a failed run (like a missing key), so the health becomes
/// `failing` and the notification check reports it, and the exit code is 1.
/// No run holds the lock, so updating `state.json` here races with nobody.
fn record_blocked(paths: &Paths, options: &RunOptions, message: &str) -> Result<RunExit> {
    let store = Store::new(paths);
    let config_stamp = crate::state::config_stamp(&paths.config_file);
    let config = Config::load(&paths.config_file);
    let state = store.load_state()?;
    if let Some(reason) = timer_skip(&config, &state, config_stamp.as_deref(), options) {
        eprintln!("{reason}");
        return Ok(RunExit::NotDue);
    }
    let mut run = Run::new(options.trigger, Utc::now());
    if let Some(id) = &options.run_id
        && crate::state::is_valid_run_id(id)
    {
        run.id = id.clone();
    }
    eprintln!("error: {message}");
    fs::create_dir_all(paths.runs_dir())?;
    if let Ok(mut log) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(store.log_path(&run.id))
    {
        let _ = writeln!(
            log,
            "[{}] error: {message}",
            crate::state::format_time(Utc::now())
        );
    }
    run.error = Some(RunError {
        kind: ErrorKind::Internal,
        message: message.to_string(),
    });
    run.finish(RunState::Failed, Utc::now());
    let keep = config.as_ref().map(|c| c.backup.keep_runs).unwrap_or(200);
    store.record_finished_with_config(&run, keep, config_stamp)?;
    store.clear_current_if(&run.id)?;
    Ok(RunExit::Failed)
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
        WakeEvent::PacketSent => {}
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
    // Only the first read-back in a row that the server's unavailability
    // stopped is free (transient); the next one directly after it counts like
    // any other failure, so a server that always falls asleep right after the
    // upload does not cause a full upload at every hourly check forever
    // (decision 0015, amendment 2026-11-10; decision 0021).
    let unavailable_before = note_readback(rep, &verification);
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
        crate::verify::Outcome::Unavailable(message) => {
            record_server_verification(rep, &client, &backup_id);
            // Reported as a verification failure (the snapshot may exist,
            // nothing was proven), but only a server-availability problem: the
            // first one in a row does not pause the retries (decisions 0015,
            // 0021); the next one directly after it counts.
            rep.run.transient = unavailable_before == 0;
            let message = if rep.run.transient {
                message
            } else {
                format!(
                    "{message}; the server was unavailable during the read-back again \
                     ({} times in a row), so this counts as a failure",
                    unavailable_before + 1
                )
            };
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

/// Update `State::readback_unavailable_in_a_row` for the outcome of the
/// read-back and return its value from before. A cancelled read-back changes
/// nothing.
fn note_readback(rep: &mut Reporter<'_>, outcome: &crate::verify::Outcome) -> u32 {
    let mut before = 0;
    let saved = rep.store.load_state().and_then(|mut state| {
        before = state.readback_unavailable_in_a_row;
        let next = match outcome {
            crate::verify::Outcome::Unavailable(_) => before.saturating_add(1),
            crate::verify::Outcome::Cancelled => before,
            crate::verify::Outcome::Verified | crate::verify::Outcome::Failed(_) => 0,
        };
        if next != before {
            state.readback_unavailable_in_a_row = next;
            rep.store.save_state(&state)?;
        }
        Ok(())
    });
    if let Err(e) = saved {
        rep.warn(format!("cannot update state: {e:#}"));
    }
    before
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
    fn proc_locks_holders_names_every_holder_but_not_waiters() {
        let dev = nix::sys::stat::makedev(0, 0x19);
        let text = "1: FLOCK  ADVISORY  READ 1234 00:19:5678 0 EOF\n\
                    1: -> FLOCK  ADVISORY  WRITE 99 00:19:5678 0 EOF\n\
                    2: FLOCK  ADVISORY  WRITE 42 00:19:5678 0 EOF\n\
                    3: FLOCK  ADVISORY  READ 7 00:19:1 0 EOF\n\
                    4: POSIX  ADVISORY  WRITE 8 00:19:5678 0 EOF\n";
        assert_eq!(
            proc_locks_holders(text, dev, 5678),
            vec![(1234, "shared"), (42, "exclusive")]
        );
        assert!(proc_locks_holders(text, dev, 2).is_empty());
    }

    #[test]
    fn a_shared_lock_without_a_run_is_an_error_not_already_running() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: dir.path().join("c.toml"),
            credentials_dir: dir.path().join("cred"),
            state_dir: dir.path().join("state"),
            runtime_dir: dir.path().join("run"),
            systemctl: PathBuf::from("systemctl"),
        };
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        let file = File::open({
            drop(crate::state::open_private_lock(&paths.lock_file()).unwrap());
            paths.lock_file()
        })
        .unwrap();
        let shared = Flock::lock(file, FlockArg::LockSharedNonblock).unwrap();
        let err = match RunLock::try_acquire(&paths) {
            Err(e) => e,
            Ok(lock) => panic!("expected an error, got {}", lock.is_some()),
        };
        let blocked = err.downcast_ref::<LockBlocked>().expect("typed");
        assert!(
            blocked.message.contains("no backup is running"),
            "{}",
            blocked.message
        );
        if fs::read_to_string("/proc/locks").is_ok() {
            assert!(
                blocked
                    .message
                    .contains(&format!("pid {}", std::process::id())),
                "{}",
                blocked.message
            );
        }
        // A fresh record of another run: that run is starting, not an error.
        let store = Store::new(&paths);
        let other = Run::new(Trigger::Manual, Utc::now());
        store.save_current(&other).unwrap();
        assert!(RunLock::try_acquire(&paths).unwrap().is_none());
        // The daemon's reservation of this very run is not "another run".
        assert!(RunLock::try_acquire_for(&paths, Some(&other.id)).is_err());
        // A leftover whose start lies in the future is no run in progress.
        let mut future = Run::new(Trigger::Manual, Utc::now() + Duration::hours(3));
        future.id = "20990101T000000Z-manual-0001".into();
        store.save_current(&future).unwrap();
        assert!(active_run(&paths).unwrap().is_none());
        assert!(RunLock::try_acquire(&paths).is_err());
        drop(shared);
        assert!(RunLock::try_acquire(&paths).unwrap().is_some());
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
    fn the_kernel_is_asked_whether_a_run_holds_the_lock() {
        // `/proc/locks` cannot be relied on to list a held lock (on btrfs the
        // device numbers differ), so whoever may open the file asks the
        // kernel: an exclusive holder makes a shared probe fail, a shared
        // holder (something that merely blocks backups) does not.
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: dir.path().join("c.toml"),
            credentials_dir: dir.path().join("cred"),
            state_dir: dir.path().join("state"),
            runtime_dir: dir.path().join("run"),
            systemctl: PathBuf::from("systemctl"),
        };
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        assert_eq!(exclusive_holder_probe(&paths), None, "no lock file yet");
        let lock = RunLock::try_acquire(&paths).unwrap().unwrap();
        assert_eq!(exclusive_holder_probe(&paths), Some(true));
        assert_eq!(lock_is_held(&paths), Some(true));
        drop(lock);
        assert_eq!(exclusive_holder_probe(&paths), Some(false));
        let shared = Flock::lock(
            File::open(paths.lock_file()).unwrap(),
            FlockArg::LockSharedNonblock,
        )
        .map_err(|(_, e)| e)
        .unwrap();
        assert_eq!(exclusive_holder_probe(&paths), Some(false));
        assert_eq!(lock_is_held(&paths), Some(false));
        drop(shared);
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
