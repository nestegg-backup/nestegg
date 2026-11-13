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
use crate::state::{
    ArchiveInfo, ErrorKind, Run, RunError, RunState, Store, Target, TargetSelection, Trigger,
    is_due,
};
use crate::wake::{self, Availability, WakeEvent};

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub target: TargetSelection,
    pub stick_uuid: Option<String>,
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

/// The run lock could not be taken, but nothing recognisable holds it: nobody
/// holds the file exclusively (asked of the kernel with a shared probe, see
/// [`lock_is_held`]) and there is no fresh record of a running backup, for
/// the whole of [`ACQUIRE_WINDOW`]. Something else (a stray shared lock, for example) keeps
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
        Self::try_acquire_with(paths, own_run_id, &proc_locks)
    }

    /// [`RunLock::try_acquire_for`] with the reader of `/proc/locks` passed
    /// in, so tests can show a list that does not contain the lock file.
    fn try_acquire_with(
        paths: &Paths,
        own_run_id: Option<&str>,
        read_locks: &dyn Fn() -> Option<String>,
    ) -> Result<Option<RunLock>> {
        // Probes of the lock (status queries of the daemon, another start)
        // hold a shared lock for an instant, and each one makes an exclusive
        // attempt at that instant fail. So the start keeps trying for a fixed
        // time, not a fixed number of times: only a lock that stays blocked
        // for all of it, with nobody holding it exclusively, is an error.
        let deadline = std::time::Instant::now() + ACQUIRE_WINDOW;
        loop {
            let one_try = std::time::Duration::ZERO;
            if let Some(lock) = crate::state::lock_private(&paths.lock_file(), one_try)
                .context("cannot lock run file")?
            {
                return Ok(Some(RunLock { _lock: lock }));
            }
            if lock_is_held_with(paths, read_locks().as_deref()) == Some(true) {
                return Ok(None);
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(ACQUIRE_STEP);
        }
        if run_in_progress(paths, own_run_id) {
            return Ok(None);
        }
        Err(LockBlocked {
            message: blocked_message(paths, read_locks().as_deref()),
        }
        .into())
    }
}

/// How long a start keeps trying to take the run lock while nobody holds it
/// exclusively. Long enough that momentary probes cannot use it up by chance
/// (about eighty attempts), short enough for a command at the terminal.
const ACQUIRE_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
const ACQUIRE_STEP: std::time::Duration = std::time::Duration::from_millis(25);

/// The kernel's list of file locks. `NESTEGG_PROC_LOCKS` names another file
/// for tests (as `NESTEGG_MOUNTINFO` does for the mount table).
fn proc_locks() -> Option<String> {
    let path = std::env::var_os("NESTEGG_PROC_LOCKS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/proc/locks"));
    fs::read_to_string(path).ok()
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
fn blocked_message(paths: &Paths, locks: Option<&str>) -> String {
    use std::os::unix::fs::MetadataExt;
    let path = paths.lock_file();
    let holders = fs::metadata(&path)
        .ok()
        .zip(locks)
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
    lock_is_held_with(paths, proc_locks().as_deref())
}

/// [`lock_is_held`] for a given content of `/proc/locks` (`None`: it cannot
/// be read).
fn lock_is_held_with(paths: &Paths, locks: Option<&str>) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(paths.lock_file()).ok()?;
    let listed = locks.map(|locks| proc_locks_has_write_flock(locks, meta.dev(), meta.ino()));
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
    use std::os::unix::fs::OpenOptionsExt;
    // Never through a symbolic link, as everywhere the lock file is opened.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(paths.lock_file())
        .ok()?;
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
    full_verification: Option<crate::state::FullVerification>,
    reclaim_after_failure: bool,
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
        if !self.run.state.is_finished() && (crate::cancel::requested() || self.stick_missing()) {
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

    fn stick_missing(&self) -> bool {
        self.run.target == Target::Stick
            && self
                .run
                .stick_uuid
                .as_deref()
                .is_some_and(|uuid| !crate::stick::target::present(uuid))
    }

    fn fail(&mut self, state: RunState, error: RunError) {
        let (state, mut error) = if self.run.target == Target::Stick {
            stick_failure(
                state,
                error,
                self.stick_missing(),
                crate::cancel::requested(),
            )
        } else {
            (state, error)
        };
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

fn stick_failure(
    state: RunState,
    error: RunError,
    missing: bool,
    cancelled: bool,
) -> (RunState, RunError) {
    if missing || error.kind == ErrorKind::StickNotPluggedIn {
        (
            RunState::StickNotPluggedIn,
            crate::stick::target::error(
                ErrorKind::StickNotPluggedIn,
                format!("stick not plugged in; {}", error.message),
            ),
        )
    } else if cancelled {
        (
            RunState::Cancelled,
            crate::stick::target::error(
                ErrorKind::Cancelled,
                format!("the stick backup was cancelled; {}", error.message),
            ),
        )
    } else {
        (state, error)
    }
}

pub fn execute(paths: &Paths, options: &RunOptions) -> Result<RunExit> {
    crate::cancel::install_handler();
    let wait_started = std::time::Instant::now();
    let _lock = loop {
        match RunLock::try_acquire_for(paths, options.run_id.as_deref()) {
            Ok(Some(lock)) => break lock,
            Ok(None) if options.trigger == Trigger::Plug => {
                if crate::cancel::requested() {
                    return Ok(RunExit::Cancelled);
                }
                if wait_started.elapsed() >= std::time::Duration::from_secs(12 * 3600) {
                    anyhow::bail!("timed out waiting for the shared run lock");
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
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
        }
    };
    let config_stamp = crate::state::config_stamp(&paths.config_file);
    let config = Config::load(&paths.config_file);
    let targets: Vec<Target> = match &config {
        Ok(config) => [Target::Stick, Target::Pbs]
            .into_iter()
            .filter(|t| {
                options.target.includes(*t)
                    && match t {
                        Target::Stick => config.stick.is_some(),
                        Target::Pbs => config.pbs.is_some(),
                    }
            })
            .collect(),
        Err(_) => vec![if options.target == TargetSelection::Stick {
            Target::Stick
        } else {
            Target::Pbs
        }],
    };
    anyhow::ensure!(
        !targets.is_empty(),
        "requested backup target is not configured"
    );
    // The lock spans all selected targets. A newly plugged-in stick therefore
    // completes before a possibly sleeping remote PBS is contacted.
    let mut result = RunExit::NotDue;
    for (index, target) in targets.into_iter().enumerate() {
        let mut one = options.clone();
        if index > 0 {
            one.run_id = None;
        }
        let outcome = execute_target(paths, &one, target, &config, config_stamp.clone())?;
        if outcome == RunExit::Cancelled {
            return Ok(outcome);
        }
        result = match (result, outcome) {
            (RunExit::Failed, _) | (_, RunExit::Failed) => RunExit::Failed,
            (RunExit::PbsUnreachable, _) | (_, RunExit::PbsUnreachable) => RunExit::PbsUnreachable,
            (_, RunExit::NotDue) => result,
            (_, other) => other,
        };
    }
    Ok(result)
}

fn execute_target(
    paths: &Paths,
    options: &RunOptions,
    target: Target,
    config: &Result<Config>,
    config_stamp: Option<String>,
) -> Result<RunExit> {
    crate::cancel::install_handler();
    let store = Store::for_target(paths, target);
    let mut state = store.load_state()?;

    // An hourly presence probe is not a backup attempt. In particular it must
    // not evict real runs or reset failure/last-success evidence.
    if target == Target::Stick && options.trigger == Trigger::Timer {
        state.last_checked_at = Some(Utc::now());
        store.save_state(&state)?;
        if let Ok(cfg) = config
            && matches!(
                crate::stick::target::attached(cfg, options.stick_uuid.as_deref()),
                Ok(None)
            )
        {
            eprintln!("stick not plugged in; presence checked without starting a run");
            return Ok(RunExit::NotDue);
        }
    }

    if plug_switched_off(paths, options) {
        return Ok(RunExit::NotDue);
    }
    if let Some(reason) = timer_skip(config, &state, config_stamp.as_deref(), options) {
        eprintln!("{reason}");
        return Ok(RunExit::NotDue);
    }

    let mut run = Run::new(options.trigger, Utc::now());
    run.target = target;
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
        full_verification: None,
        reclaim_after_failure: false,
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
        Ok(cfg) if target == Target::Stick => stick_pipeline(paths, cfg, options, &mut rep),
        Ok(cfg) => pipeline(paths, cfg, &mut rep, None),
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
    if target == Target::Stick
        && !matches!(
            rep.run.state,
            RunState::Cancelled | RunState::StickNotPluggedIn
        )
        && (crate::cancel::requested() || rep.stick_missing())
    {
        rep.fail(
            RunState::Cancelled,
            crate::stick::target::error(
                ErrorKind::Cancelled,
                "interrupted before completing stick cleanup",
            ),
        );
    }
    let final_state = rep.run.state;
    rep.line(&format!("run finished: {:?}", final_state));
    store.record_finished_with_maintenance(&rep.run, keep, config_stamp, rep.full_verification)?;
    // The run is finished and recorded. Tidying up must not turn its result
    // into an error: if `current.json` cannot be removed now (its lock is
    // held past the time limit), it is left behind, counts as stale after a
    // minute and is replaced by the next run.
    if let Err(e) = store.clear_current_if(&rep.run.id) {
        eprintln!("warning: could not remove the record of the running backup: {e:#}");
    }
    let _ = fs::remove_file(&pid_file);
    let _ = fs::remove_file(&cancel_note);
    Ok(match final_state {
        RunState::Succeeded => RunExit::Succeeded,
        RunState::PbsUnreachable | RunState::StickNotPluggedIn => RunExit::PbsUnreachable,
        RunState::Cancelled => RunExit::Cancelled,
        _ => RunExit::Failed,
    })
}

/// Plugging in a registered stick starts a backup only while automatic
/// backups are on (the backup timer, decision 0013). With them switched off
/// backups run only when started by hand, also for the stick.
fn plug_switched_off(paths: &Paths, options: &RunOptions) -> bool {
    let off = options.trigger == Trigger::Plug
        && !options.force
        && crate::status::timer_switched_off(&paths.systemctl);
    if off {
        eprintln!("automatic backups are switched off; plugging in the stick does not start one");
    }
    off
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
    let (Ok(cfg), Trigger::Timer | Trigger::Plug, false) = (config, options.trigger, options.force)
    else {
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
    if options.trigger == Trigger::Timer
        && let Some(not_before) =
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
/// No run holds the run lock here, but a second blocked start may be at the
/// same point: the update of `state.json` is serialised among them
/// ([`Store::record_finished_serialised`]), so the count of failures in a
/// row is not lost.
fn record_blocked(paths: &Paths, options: &RunOptions, message: &str) -> Result<RunExit> {
    let config_stamp = crate::state::config_stamp(&paths.config_file);
    let config = Config::load(&paths.config_file);
    let target = if options.target == TargetSelection::Stick
        || config.as_ref().is_ok_and(|c| c.pbs.is_none())
    {
        Target::Stick
    } else {
        Target::Pbs
    };
    let store = Store::for_target(paths, target);
    let state = store.load_state()?;
    if plug_switched_off(paths, options) {
        return Ok(RunExit::NotDue);
    }
    if let Some(reason) = timer_skip(&config, &state, config_stamp.as_deref(), options) {
        eprintln!("{reason}");
        return Ok(RunExit::NotDue);
    }
    let mut run = Run::new(options.trigger, Utc::now());
    run.target = target;
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
    store.record_finished_serialised(&run, keep, config_stamp)?;
    if let Err(e) = store.clear_current_if(&run.id) {
        eprintln!("warning: could not remove the record of the running backup: {e:#}");
    }
    Ok(RunExit::Failed)
}

fn stick_pipeline(
    paths: &Paths,
    config: &Config,
    options: &RunOptions,
    rep: &mut Reporter<'_>,
) -> Result<()> {
    let Some(uuid) = crate::stick::target::attached(config, options.stick_uuid.as_deref())? else {
        rep.fail(
            RunState::StickNotPluggedIn,
            crate::stick::target::error(ErrorKind::StickNotPluggedIn, "stick not plugged in"),
        );
        return Ok(());
    };
    rep.run.stick_uuid = Some(uuid.clone());
    if let Err(e) = crate::stick::launcher::preflight() {
        rep.fail(
            RunState::Failed,
            RunError::from_error(&e, ErrorKind::StickUnsupported),
        );
        return Ok(());
    }
    crate::stick::target::isolate_mounts()?;
    let mut session = match crate::stick::target::Session::open(paths, &uuid) {
        Ok(session) => session,
        Err(error) => {
            rep.fail(RunState::Failed, error);
            return Ok(());
        }
    };
    if let Err(e) = session.start(paths) {
        let kind = if format!("{e:#}")
            .to_ascii_lowercase()
            .contains("no space left")
        {
            ErrorKind::StickFull
        } else {
            ErrorKind::Internal
        };
        rep.fail(RunState::Failed, RunError::from_error(&e, kind));
        return Ok(());
    }
    if let Err(e) = crate::notify::stick_event(
        config,
        crate::notify::StickEvent::Start,
        "Backup to stick started",
    ) {
        rep.warn(format!("stick start notification failed: {e:#}"));
    }
    let result = pipeline(paths, config, rep, Some(session.pbs()));
    if rep.reclaim_after_failure
        && rep.run.state == RunState::Failed
        && !crate::cancel::requested()
        && !rep.stick_missing()
    {
        // Failed uploads can leave unreferenced chunks. GC alone may reclaim
        // them after PBS's atime cutoff; never prune as a response to failure.
        rep.line("reclaiming orphaned chunks after failed upload/full stick (no prune)");
        if let Err(e) = session
            .pbs()
            .maintenance(&["garbage-collection", "start", "stick"])
        {
            rep.warn(format!("garbage collection after failure failed: {e:#}"));
            if crate::cancel::requested() || rep.stick_missing() {
                rep.fail(
                    RunState::Failed,
                    RunError::from_error(&e, ErrorKind::Internal),
                );
            }
        }
    }
    // Publish the latest usage after retention or failure-only GC, rather
    // than leaving the UI with the space measured before the upload.
    if !crate::cancel::requested() && !rep.stick_missing() {
        let usage = Client::local(
            config,
            SecretStore::new(&paths.credentials_dir),
            session.pbs(),
        )
        .space_info();
        match usage {
            Ok((available, total)) => {
                let mut state = rep.store.load_state()?;
                state.available_bytes = Some(available);
                state.total_bytes = total;
                state.space_checked_at = Some(Utc::now());
                rep.store.save_state(&state)?;
            }
            Err(e) => rep.warn(format!("could not refresh stick capacity: {e:#}")),
        }
    }
    // Safe-to-unplug means server stopped, writes flushed and filesystem
    // unmounted. Never emit it merely because the upload finished.
    session.close()?;
    if rep.run.state == RunState::Succeeded && result.is_ok() {
        if let Err(e) = crate::notify::stick_event(
            config,
            crate::notify::StickEvent::SafeToUnplug,
            "Stick backup finished; safe to unplug",
        ) {
            rep.warn(format!("stick completion notification failed: {e:#}"));
        }
    }
    result
}

fn pipeline(
    paths: &Paths,
    config: &Config,
    rep: &mut Reporter<'_>,
    local: Option<&crate::stick::launcher::LocalPbs>,
) -> Result<()> {
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
        let stick_proof = local.is_some()
            && config.pbs.is_none()
            && rep
                .run
                .stick_uuid
                .as_ref()
                .and_then(|uuid| state.stick_media.get(uuid))
                .is_some_and(|proof| proof.key_digest.as_deref() == Some(&key_digest));
        if state.key_exported_digest.as_deref() != Some(key_digest.as_str()) && !stick_proof {
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

    // 2. Sources. Pin the directories first: touching a path may trigger an
    // automount, and the mount table must be read afterwards.
    let resolved: Vec<Result<crate::source::SourceDirectory>> = config
        .backup
        .sources
        .iter()
        .map(|s| crate::source::SourceDirectory::open(&s.path))
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
    let mut source_handles = std::collections::BTreeMap::new();
    for (source, canonical) in config.backup.sources.iter().zip(resolved) {
        let open_error = canonical
            .as_ref()
            .err()
            .map(|error| {
                if error.downcast_ref::<nix::errno::Errno>() == Some(&nix::errno::Errno::ENOENT)
                    || error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    "does not exist".to_string()
                } else {
                    format!("cannot be opened without symlinks ({error:#})")
                }
            })
            .unwrap_or_default();
        if let Ok(opened) = canonical {
            let canonical = &opened.path;
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
                canonical,
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
                path: opened.path.clone(),
            });
            source_handles.insert(source.archive.clone(), opened);
        } else if source.optional {
            rep.incomplete(format!(
                "optional source {} {open_error}, skipped",
                source.path.display()
            ));
        } else {
            rep.fail(
                RunState::Failed,
                RunError {
                    kind: ErrorKind::SourceMissing,
                    message: format!("source {} {open_error}", source.path.display()),
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

    if local.is_none() {
        // 3. PBS availability.
        rep.phase(
            RunState::WakingPbs,
            &format!(
                "checking PBS {}:{}",
                config.remote_pbs()?.server,
                config.remote_pbs()?.port
            ),
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
    }
    // 4. Inventory.
    let mut inventory_dir = None;
    if config.backup.inventory {
        rep.phase(RunState::Inventory, "writing system inventory");
        let dir: PathBuf = paths.inventory_dir();
        match crate::inventory::generate(&dir).and_then(|report| {
            let opened = crate::source::SourceDirectory::open(&dir)?;
            Ok((report, opened))
        }) {
            Ok((report, opened)) => {
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
                source_handles.insert("inventory".into(), opened);
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
    let ctx = report_context(config, &state, &source_handles);
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
        namespace: if local.is_some() {
            None
        } else {
            config.remote_pbs()?.namespace.clone()
        },
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
    let client = match local {
        Some(pbs) => Client::local(config, secrets, pbs),
        None => Client::new(config, secrets),
    };
    if local.is_some() {
        if let Err(e) = client.check_group_key(&backup_id) {
            rep.fail(
                RunState::Failed,
                RunError::from_error(&e, crate::client::group_key_error_kind(&e)),
            );
            return Ok(());
        }
        match client.space_info() {
            Ok((available, total)) => {
                state.available_bytes = Some(available);
                state.total_bytes = total;
                state.space_checked_at = Some(Utc::now());
                rep.store.save_state(&state)?;
                if available
                    < config
                        .stick
                        .as_ref()
                        .context("missing stick configuration")?
                        .minimum_free_bytes
                {
                    rep.reclaim_after_failure = true;
                    rep.fail(RunState::Failed, crate::stick::target::error(ErrorKind::StickFull, "stick full: insufficient space available to the PBS user; retention policy was kept"));
                    return Ok(());
                }
            }
            Err(e) => {
                rep.fail(
                    RunState::Failed,
                    RunError::from_error(&e, ErrorKind::Internal),
                );
                return Ok(());
            }
        }
    }
    let mut log = rep.log.try_clone().context("cannot duplicate log handle")?;
    rep.reclaim_after_failure = local.is_some();
    let outcome = client.backup(&spec, &source_handles, &mut log, &mut |stats| {
        // Live progress for the dashboard after each finished archive.
        rep.run.processed_bytes =
            Some(rep.run.processed_bytes.unwrap_or(0) + stats.total_bytes.unwrap_or(0));
        rep.run.uploaded_bytes =
            Some(rep.run.uploaded_bytes.unwrap_or(0) + stats.uploaded_bytes.unwrap_or(0));
        let _ = rep.store.save_current(&rep.run);
    });
    let outcome = match outcome {
        Ok(o) => o,
        Err(mut error) => {
            if local.is_some() {
                let message = error.message.to_ascii_lowercase();
                if message.contains("no space left") || message.contains("enospc") {
                    error.kind = ErrorKind::StickFull;
                }
            }
            let state = match error.kind {
                ErrorKind::PbsUnreachable => RunState::PbsUnreachable,
                ErrorKind::Cancelled => RunState::Cancelled,
                _ => RunState::Failed,
            };
            rep.fail(state, error);
            return Ok(());
        }
    };
    rep.reclaim_after_failure = false;
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
    // Remote backups are complete after read-back. Stick backups still need
    // server verification and maintenance, all of which remain cancellable.
    record_server_verification(rep, &client, &backup_id);

    if let Some(pbs) = local {
        if rep.cancelled() {
            return Ok(());
        }
        let uuid = rep.run.stick_uuid.clone().context("stick UUID missing")?;
        let state = rep.store.load_state()?;
        let full_due = crate::stick::target::full_verify_due(
            state
                .stick_media
                .get(&uuid)
                .and_then(|m| m.full_checked_at.or(m.full_verified_at)),
            Utc::now(),
        );
        rep.phase(
            RunState::Finishing,
            "verifying the new stick snapshot with PBS",
        );
        if let Err(e) = pbs.verify_snapshot(&backup_id, backup_time) {
            record_server_verification(rep, &client, &backup_id);
            rep.fail(
                RunState::Failed,
                RunError::from_error(&e, ErrorKind::VerificationFailed),
            );
            return Ok(());
        }
        if rep.cancelled() {
            return Ok(());
        }
        record_server_verification(rep, &client, &backup_id);
        let snapshots =
            client.list_snapshots_bounded(&backup_id, crate::client::readback_list_timeout())?;
        if !snapshots
            .iter()
            .any(|s| s.snapshot == snapshot && s.verified.as_deref() == Some("ok"))
        {
            rep.fail(
                RunState::Failed,
                crate::stick::target::error(
                    ErrorKind::VerificationFailed,
                    "PBS did not confirm the new stick snapshot as verified",
                ),
            );
            return Ok(());
        }
        if full_due {
            rep.phase(
                RunState::Finishing,
                "monthly PBS verification of the complete stick",
            );
            let checked = pbs.maintenance(&["verify", "stick", "--ignore-verified", "false"]);
            if rep.cancelled() {
                return Ok(());
            }
            rep.full_verification = Some(crate::state::FullVerification {
                checked_at: Utc::now(),
                succeeded: checked.is_ok(),
            });
            if let Err(e) = checked {
                rep.warn(format!("stick damaged: monthly datastore verification failed; the new snapshot is verified and this machine's retention continues: {e:#}"));
            }
            record_server_verification(rep, &client, &backup_id);
        }
        rep.phase(
            RunState::Finishing,
            "applying stick retention and garbage collection",
        );
        // Prove ownership again immediately before destructive maintenance.
        client.check_group_key(&backup_id)?;
        if rep.cancelled() {
            return Ok(());
        }
        client.prune_stick(
            &backup_id,
            &config
                .stick
                .as_ref()
                .context("missing stick configuration")?
                .retention,
        )?;
        if rep.cancelled() {
            return Ok(());
        }
        pbs.maintenance(&["garbage-collection", "start", "stick"])?;
        if rep.cancelled() {
            return Ok(());
        }
        record_server_verification(rep, &client, &backup_id);
    }

    // 7. Notes on the snapshot, readable by the read-only overview.
    rep.phase(RunState::Finishing, "writing snapshot notes");
    let duration = (Utc::now() - rep.run.started_at).num_seconds();
    let notes = snapshot_notes(&rep.run, duration, &ctx);
    if let Err(e) =
        client.set_notes_bounded(&snapshot, &notes, crate::client::readback_list_timeout())
    {
        rep.warn(format!("could not write snapshot notes: {e:#}"));
    }
    if local.is_some() && rep.cancelled() {
        return Ok(());
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
        machine_id: fs::read_to_string("/etc/machine-id")
            .ok()
            .and_then(|s| crate::metadata::machine_identity(&s)),
        backup_time,
        snapshot: snapshot_path(backup_id, backup_time),
        archive_sources: ctx
            .archive_sources
            .iter()
            .filter(|(name, _)| archives.iter().any(|a| &a.file_name() == *name))
            .filter_map(|(name, path)| Some((name.clone(), path.as_str()?.to_string())))
            .collect(),
        bound_sources: ctx
            .archive_sources
            .keys()
            .filter(|name| archives.iter().any(|a| a.file_name() == **name))
            .cloned()
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
    let failed = match if rep.run.target == Target::Stick {
        client.list_all_snapshots_bounded(crate::client::readback_list_timeout())
    } else {
        client.list_snapshots_bounded(backup_id, crate::client::readback_list_timeout())
    } {
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

fn report_context(
    config: &Config,
    state: &crate::state::State,
    sources: &std::collections::BTreeMap<String, crate::source::SourceDirectory>,
) -> ReportContext {
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
        archive_sources: sources
            .iter()
            .filter(|(name, _)| config.backup.sources.iter().any(|s| &s.archive == *name))
            .map(|(name, source)| (format!("{name}.pxar"), serde_json::json!(source.path)))
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
    fn every_stick_step_preserves_cancellation_and_device_loss() {
        for (step, fallback) in [
            ("new snapshot verify", ErrorKind::VerificationFailed),
            ("monthly verify", ErrorKind::VerificationFailed),
            ("prune", ErrorKind::Internal),
            ("GC", ErrorKind::Internal),
            ("group key", ErrorKind::KeyMismatch),
            ("PBS startup", ErrorKind::Internal),
            ("flush/unmount", ErrorKind::Internal),
        ] {
            let error = crate::stick::target::error(fallback, step);
            assert_eq!(
                stick_failure(RunState::Failed, error.clone(), false, false),
                (RunState::Failed, error.clone())
            );
            let (state, cancelled) = stick_failure(RunState::Failed, error.clone(), false, true);
            assert_eq!(
                (state, cancelled.kind),
                (RunState::Cancelled, ErrorKind::Cancelled),
                "{step}"
            );
            for cancel in [false, true] {
                let (state, missing) = stick_failure(RunState::Failed, error.clone(), true, cancel);
                assert_eq!(
                    (state, missing.kind),
                    (RunState::StickNotPluggedIn, ErrorKind::StickNotPluggedIn),
                    "{step}"
                );
            }
        }
    }

    #[test]
    fn plugging_in_does_not_back_up_while_automatic_backups_are_off() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = |answer: &str| {
            let path = dir.path().join(format!("systemctl-{answer}"));
            std::fs::write(&path, format!("#!/bin/sh\necho {answer}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let mut paths = Paths {
            config_file: dir.path().join("config.toml"),
            credentials_dir: dir.path().join("credentials"),
            state_dir: dir.path().join("state"),
            runtime_dir: dir.path().join("run"),
            systemctl: fake("disabled"),
        };
        let mut options = RunOptions {
            target: TargetSelection::Stick,
            stick_uuid: None,
            trigger: Trigger::Plug,
            run_id: None,
            force: false,
        };
        assert!(plug_switched_off(&paths, &options));
        paths.systemctl = fake("masked");
        assert!(plug_switched_off(&paths, &options));
        paths.systemctl = fake("enabled");
        assert!(!plug_switched_off(&paths, &options));
        // Without systemd nothing is known to be off.
        paths.systemctl = dir.path().join("no-systemctl");
        assert!(!plug_switched_off(&paths, &options));
        // Timer and manual runs are not gated here.
        paths.systemctl = fake("disabled");
        options.trigger = Trigger::Manual;
        assert!(!plug_switched_off(&paths, &options));
    }

    #[test]
    fn plug_respects_due_time_but_bypasses_failure_pause() {
        let cfg = crate::config::example("pbs.example.com", "store", "backup@pbs!desktop");
        let mut run = Run::new(Trigger::Timer, Utc::now());
        run.finish(RunState::Failed, Utc::now());
        let mut state = crate::state::State {
            last_run: Some(run),
            consecutive_failures: 3,
            failed_config: Some("same".into()),
            ..Default::default()
        };
        let mut options = RunOptions {
            target: TargetSelection::Stick,
            stick_uuid: None,
            trigger: Trigger::Timer,
            run_id: None,
            force: false,
        };
        let cfg = Ok(cfg);
        assert!(timer_skip(&cfg, &state, Some("same"), &options).is_some());
        options.trigger = Trigger::Plug;
        assert!(timer_skip(&cfg, &state, Some("same"), &options).is_none());
        let mut success = Run::new(Trigger::Plug, Utc::now());
        success.finish(RunState::Succeeded, Utc::now());
        state.last_success = Some(success);
        assert!(
            timer_skip(&cfg, &state, Some("same"), &options)
                .unwrap()
                .contains("not due yet")
        );
    }

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
        // A readable /proc/locks is not guaranteed to contain this inode
        // (btrfs uses a different device number; concurrent enumeration is
        // not a stable snapshot). Inject the diagnostic input, while still
        // exercising a real shared flock and the exclusive acquisition path.
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(paths.lock_file()).unwrap();
        let listed = format!(
            "1: FLOCK ADVISORY READ {} {:02x}:{:02x}:{} 0 EOF\n",
            std::process::id(),
            nix::sys::stat::major(meta.dev()),
            nix::sys::stat::minor(meta.dev()),
            meta.ino()
        );
        let err = match RunLock::try_acquire_with(&paths, None, &|| Some(listed.clone())) {
            Err(e) => e,
            Ok(lock) => panic!("expected an error, got {}", lock.is_some()),
        };
        let blocked = err.downcast_ref::<LockBlocked>().expect("typed");
        assert!(
            blocked.message.contains("no backup is running"),
            "{}",
            blocked.message
        );
        assert!(
            blocked
                .message
                .contains(&format!("pid {}", std::process::id())),
            "{}",
            blocked.message
        );
        for locks in [Some(""), None] {
            let err = RunLock::try_acquire_with(&paths, None, &|| locks.map(str::to_owned))
                .err()
                .expect("an unlisted shared holder still blocks backups");
            let message = &err.downcast_ref::<LockBlocked>().unwrap().message;
            assert!(
                message.contains(if locks.is_none() {
                    "cannot be read"
                } else {
                    "does not show who"
                }),
                "{message}"
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

    fn lock_test_paths(dir: &std::path::Path) -> Paths {
        Paths {
            config_file: dir.join("c.toml"),
            credentials_dir: dir.join("cred"),
            state_dir: dir.join("state"),
            runtime_dir: dir.join("run"),
            systemctl: PathBuf::from("systemctl"),
        }
    }

    #[test]
    fn of_two_starts_at_the_same_moment_one_runs_and_the_other_sees_it_running() {
        // Neither may come out as "blocked": each probes while the other
        // tries, and whoever loses must find the winner holding the lock.
        let dir = tempfile::tempdir().unwrap();
        let paths = lock_test_paths(dir.path());
        for round in 0..40 {
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let starters: Vec<_> = (0..2)
                .map(|_| {
                    let (paths, barrier) = (paths.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        RunLock::try_acquire(&paths)
                    })
                })
                .collect();
            // Both results are kept until both starts are over, so the loser
            // cannot get the lock after the winner let go.
            let results: Vec<_> = starters.into_iter().map(|t| t.join().unwrap()).collect();
            let got = results.iter().filter(|r| matches!(r, Ok(Some(_)))).count();
            let running = results.iter().filter(|r| matches!(r, Ok(None))).count();
            assert_eq!(
                (got, running),
                (1, 1),
                "round {round}: {:?}",
                results
                    .iter()
                    .map(|r| r.as_ref().map(|l| l.is_some()).map_err(|e| e.to_string()))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn probes_of_the_lock_do_not_make_a_start_fail() {
        // Status queries probe the lock with a momentary shared lock. Four
        // threads doing nothing else must not make a single start come out
        // as "blocked" (with a fixed number of attempts one in a few hundred
        // did).
        let dir = tempfile::tempdir().unwrap();
        let paths = lock_test_paths(dir.path());
        drop(RunLock::try_acquire(&paths).unwrap().unwrap());
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probers: Vec<_> = (0..4)
            .map(|_| {
                let (paths, stop) = (paths.clone(), stop.clone());
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = exclusive_holder_probe(&paths);
                    }
                })
            })
            .collect();
        let mut problems = Vec::new();
        for round in 0..300 {
            match RunLock::try_acquire(&paths) {
                Ok(Some(lock)) => drop(lock),
                Ok(None) => problems.push(format!("round {round}: taken for running")),
                Err(e) => problems.push(format!("round {round}: {e:#}")),
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for p in probers {
            p.join().unwrap();
        }
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_lock_list_that_does_not_show_the_file_still_gives_the_right_answer() {
        // On btrfs `/proc/locks` lists the lock under another device number,
        // so the list never shows the lock file; the same when it cannot be
        // read. A run that holds the lock must still be "already running",
        // and a mere shared holder must still be "blocked".
        let dir = tempfile::tempdir().unwrap();
        let paths = lock_test_paths(dir.path());
        let not_listed: [&dyn Fn() -> Option<String>; 2] = [&|| Some(String::new()), &|| None];
        for read_locks in not_listed {
            let run = RunLock::try_acquire(&paths).unwrap().unwrap();
            let started = std::time::Instant::now();
            let second = RunLock::try_acquire_with(&paths, None, read_locks).unwrap();
            assert!(second.is_none(), "a run holds the lock");
            assert!(
                started.elapsed() < ACQUIRE_WINDOW,
                "seen at once, not after the whole window"
            );
            drop(run);

            let shared = Flock::lock(
                File::open(paths.lock_file()).unwrap(),
                FlockArg::LockSharedNonblock,
            )
            .map_err(|(_, e)| e)
            .unwrap();
            let started = std::time::Instant::now();
            let err = match RunLock::try_acquire_with(&paths, None, read_locks) {
                Err(e) => e,
                Ok(_) => panic!("a shared holder blocks the lock without being a run"),
            };
            assert!(err.downcast_ref::<LockBlocked>().is_some(), "{err:#}");
            assert!(
                started.elapsed() >= ACQUIRE_WINDOW,
                "only after the whole window"
            );
            drop(shared);
            assert!(RunLock::try_acquire(&paths).unwrap().is_some());
        }
    }

    #[test]
    fn the_probe_does_not_follow_a_symbolic_link() {
        let dir = tempfile::tempdir().unwrap();
        let paths = lock_test_paths(dir.path());
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        fs::write(&elsewhere, "").unwrap();
        std::os::unix::fs::symlink(&elsewhere, paths.lock_file()).unwrap();
        assert_eq!(exclusive_holder_probe(&paths), None);
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
