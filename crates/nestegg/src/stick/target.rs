//! Registered ext4 media lifecycle. All mounts are private to the run process.
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::launcher::{LauncherOptions, LocalPbs};
use super::{capture, private_dir};
use crate::config::{Config, Paths};
use crate::state::{ErrorKind, RunError};
use anyhow::{Context, Result, bail};

pub fn full_verify_due(
    last: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    last.is_none_or(|last| {
        now - last >= chrono::Duration::days(30) || last > now + crate::state::future_tolerance()
    })
}

/// e2fsck 1.47 can return zero after reporting a wrong free-block summary
/// when -n declines its repair. A successful process alone is not a clean FS.
pub fn filesystem_healthy(success: bool, stdout: &[u8], stderr: &[u8]) -> bool {
    success
        && [stdout, stderr].iter().all(|bytes| {
            let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
            !text.contains("? no") && !text.contains("skipping journal recovery")
        })
}

pub struct Session {
    pub uuid: String,
    pub mount: PathBuf,
    pbs: Option<LocalPbs>,
    _device_lock: nix::fcntl::Flock<File>,
    _usage: UsageGuard,
}

/// Held from before opening the medium until PBS shutdown, flush and unmount.
/// The kernel lock, rather than the saved text, makes crashed workers idle.
#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Usage {
    Backup,
    Restore,
}
struct UsageGuard(nix::fcntl::Flock<File>);
impl UsageGuard {
    fn mark_unsafe(&mut self) {
        use std::io::{Seek, Write};
        // Retain a conservative refusal after cleanup fails, even once the
        // worker exits. A later successful session or reboot clears it.
        let _ = self.0.rewind().and_then(|_| self.0.write_all(b"unsafe"));
    }
    fn mark_safe(&mut self) -> Result<()> {
        use std::io::{Seek, Write};
        self.0.set_len(0)?;
        self.0.rewind()?;
        self.0.write_all(b"idle")?;
        Ok(())
    }
    fn acquire(paths: &Paths, uuid: &str, usage: Usage) -> Result<Self> {
        use std::io::{Read, Seek, Write};
        let path = paths
            .runtime_dir
            .join("stick")
            .join(format!("{}.usage", uuid.to_ascii_lowercase()));
        let file = crate::state::open_private_lock(&path)?;
        let mut guard = Self(
            nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock)
                .map_err(|(_, e)| error(ErrorKind::StickBusy, format!("stick is in use: {e}")))?,
        );
        let mut previous = String::new();
        guard.0.read_to_string(&mut previous)?;
        if previous.starts_with("unsafe") {
            return Ok(guard);
        }
        guard.0.set_len(0)?;
        guard.0.rewind()?;
        guard.0.write_all(match usage {
            Usage::Backup => b"backup",
            Usage::Restore => b"restore",
        })?;
        Ok(guard)
    }
}
/// Read-only status probe; never mounts or starts a server. Unreadable or
/// partially written usage is conservatively busy, never safe to unplug.
pub fn in_use(paths: &Paths, uuid: &str) -> Option<Usage> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let path = paths
        .runtime_dir
        .join("stick")
        .join(format!("{}.usage", uuid.to_ascii_lowercase()));
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some(Usage::Restore),
    };
    match nix::fcntl::Flock::lock(
        match file.try_clone() {
            Ok(file) => file,
            Err(_) => return Some(Usage::Restore),
        },
        nix::fcntl::FlockArg::LockSharedNonblock,
    ) {
        Ok(_probe) => {
            let mut text = String::new();
            match file.read_to_string(&mut text) {
                Ok(_) if text.starts_with("unsafe") => Some(Usage::Restore),
                Ok(_) => None,
                Err(_) => Some(Usage::Restore),
            }
        }
        Err((_, e)) if e == nix::errno::Errno::EWOULDBLOCK => {
            let mut text = String::new();
            let _ = file.read_to_string(&mut text);
            Some(if text == "backup" {
                Usage::Backup
            } else {
                Usage::Restore
            })
        }
        Err(_) => Some(Usage::Restore),
    }
}

pub fn attached(config: &Config, requested: Option<&str>) -> Result<Option<String>> {
    let stick = config
        .stick
        .as_ref()
        .context("no stick target configured")?;
    if let Some(uuid) = requested {
        if !stick
            .data_uuids
            .iter()
            .any(|v| v.eq_ignore_ascii_case(uuid))
        {
            bail!("stick UUID is not registered");
        }
        return Ok(device(uuid).exists().then(|| uuid.to_ascii_lowercase()));
    }
    let mut present = stick.data_uuids.iter().filter(|u| device(u).exists());
    let first = present.next().cloned();
    if present.next().is_some() {
        bail!("several registered sticks are attached; select a stick UUID for this operation");
    }
    Ok(first.map(|v| v.to_ascii_lowercase()))
}

pub fn present(uuid: &str) -> bool {
    super::valid_uuid(uuid) && device(uuid).exists()
}

fn device(uuid: &str) -> PathBuf {
    PathBuf::from(format!("/dev/disk/by-uuid/{}", uuid.to_ascii_lowercase()))
}

pub fn error(kind: ErrorKind, message: impl Into<String>) -> RunError {
    RunError {
        kind,
        message: message.into(),
    }
}

pub fn isolate_mounts() -> Result<()> {
    // This is called by the single-threaded CLI before starting any helpers.
    // It never alters the host mount table or the daemon's namespace.
    if unsafe { nix::libc::unshare(nix::libc::CLONE_NEWNS) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot create private stick mount namespace");
    }
    capture(Command::new("mount").args(["--make-rslave", "/"]))?;
    Ok(())
}

fn classify_open_error(e: anyhow::Error, present: bool) -> RunError {
    if !present {
        error(
            ErrorKind::StickNotPluggedIn,
            "stick disappeared while opening it",
        )
    } else if let Some(known) = e.downcast_ref::<RunError>() {
        known.clone()
    } else {
        RunError::from_error(&e, ErrorKind::Internal)
    }
}

impl Session {
    pub fn open(paths: &Paths, uuid: &str) -> std::result::Result<Self, RunError> {
        Self::open_as(paths, uuid, Usage::Backup)
    }
    pub fn open_for_restore(paths: &Paths, uuid: &str) -> std::result::Result<Self, RunError> {
        Self::open_as(paths, uuid, Usage::Restore)
    }
    fn open_as(paths: &Paths, uuid: &str, usage: Usage) -> std::result::Result<Self, RunError> {
        Self::open_inner(paths, uuid, usage)
            .map_err(|e| classify_open_error(e, super::valid_uuid(uuid) && device(uuid).exists()))
    }
    fn open_inner(paths: &Paths, uuid: &str, usage: Usage) -> Result<Self> {
        use std::os::unix::fs::FileTypeExt;
        anyhow::ensure!(super::valid_uuid(uuid), "invalid stick UUID");
        let base = paths.runtime_dir.join("stick");
        private_dir(&base)?;
        let lock = crate::state::open_private_lock(&base.join(format!("{uuid}.lock")))?;
        let lock = nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .map_err(|(_, e)| {
                error(
                    ErrorKind::StickBusy,
                    format!("stick is already in use: {e}"),
                )
            })?;
        let usage = UsageGuard::acquire(paths, uuid, usage)?;
        // A by-UUID symlink hides cloned filesystems. Apply the media commands'
        // complete-device check before opening any registered desktop target.
        let tree = super::cli::registered_tree()?;
        super::cli::registered_candidates(&tree, &[uuid])?;
        let dev = fs::canonicalize(device(uuid)).context("stick disappeared")?;
        anyhow::ensure!(
            fs::metadata(&dev)?.file_type().is_block_device(),
            "stick must be a block device"
        );
        let actual = capture(
            Command::new("blkid")
                .args(["-p", "-s", "UUID", "-o", "value"])
                .arg(&dev),
        )?;
        let kind = capture(
            Command::new("blkid")
                .args(["-p", "-s", "TYPE", "-o", "value"])
                .arg(&dev),
        )?;
        anyhow::ensure!(
            actual.eq_ignore_ascii_case(uuid) && kind == "ext4",
            error(
                ErrorKind::FilesystemDamaged,
                "stick identity or filesystem changed"
            )
        );
        let mounted = Command::new("findmnt")
            .args(["-rn", "--source"])
            .arg(&dev)
            .stdout(Stdio::null())
            .status()?;
        anyhow::ensure!(
            mounted.code() == Some(1),
            error(
                ErrorKind::StickAlreadyMounted,
                "stick is already mounted; unmount it before use"
            )
        );
        // Never replay a journal or attempt repair implicitly. PBS verify is a
        // different check and cannot repair ext4. This also covers power loss
        // while the desktop was off and has no local removal record.
        // The checker parses untrusted filesystem metadata. It needs read
        // access to the root-owned device, but no Linux capabilities and no
        // way to gain privileges through exec. util-linux is already required.
        let health = Command::new("/usr/bin/setpriv")
            .args([
                "--no-new-privs",
                "--bounding-set=-all",
                "--inh-caps=-all",
                "--ambient-caps=-all",
                "e2fsck",
                "-f",
                "-n",
            ])
            .arg(&dev)
            .env("LC_ALL", "C")
            .output()?;
        anyhow::ensure!(
            filesystem_healthy(health.status.success(), &health.stdout, &health.stderr),
            error(
                ErrorKind::FilesystemDamaged,
                format!(
                    "stick filesystem needs offline repair (not PBS verification); unmount it and run e2fsck before reuse: {}",
                    crate::rescue::printable_block(
                        &String::from_utf8_lossy(&health.stdout)
                            .chars()
                            .rev()
                            .take(2048)
                            .collect::<String>()
                            .chars()
                            .rev()
                            .collect::<String>()
                    )
                )
            )
        );
        let mount = base.join(format!("data-{uuid}"));
        private_dir(&mount)?;
        capture(
            Command::new("mount")
                .args([
                    "-t",
                    "ext4",
                    "-o",
                    "nodev,nosuid,noexec,relatime,errors=remount-ro",
                ])
                .arg(&dev)
                .arg(&mount),
        )?;
        Ok(Self {
            uuid: uuid.into(),
            mount,
            pbs: None,
            _device_lock: lock,
            _usage: usage,
        })
    }
    pub fn start(&mut self, paths: &Paths) -> Result<()> {
        self.pbs = Some(LocalPbs::start(&LauncherOptions {
            data_mount: self.mount.clone(),
            runtime_dir: paths.runtime_dir.join("stick/pbs"),
        })?);
        Ok(())
    }
    pub(crate) fn start_live(&mut self, paths: &Paths) -> Result<()> {
        self.pbs = Some(LocalPbs::start_live(&LauncherOptions {
            data_mount: self.mount.clone(),
            runtime_dir: paths.runtime_dir.join("stick/pbs"),
        })?);
        Ok(())
    }
    pub fn pbs(&self) -> &LocalPbs {
        self.pbs.as_ref().expect("session started")
    }
    pub fn close(mut self) -> Result<()> {
        let result = self.shutdown().and_then(|()| self._usage.mark_safe());
        if result.is_err() {
            self._usage.mark_unsafe();
        }
        result
    }
    fn shutdown(&mut self) -> Result<()> {
        if let Some(pbs) = self.pbs.take() {
            pbs.stop()?;
        }
        if self.mount.as_os_str().is_empty() {
            return Ok(());
        }
        let fd = File::open(&self.mount)?;
        use std::os::fd::AsRawFd;
        let flushed = if unsafe { nix::libc::syncfs(fd.as_raw_fd()) } != 0 {
            Err(std::io::Error::last_os_error()).context("stick flush failed")
        } else {
            Ok(())
        };
        drop(fd);
        let unmounted = capture(Command::new("umount").arg(&self.mount));
        if unmounted.is_ok() {
            let _ = fs::remove_dir(&self.mount);
            self.mount = PathBuf::new();
        }
        // A failed flush is still a failed run, even after successful unmount.
        // Do not leave a damaged filesystem mounted just because syncfs failed.
        flushed?;
        unmounted?;
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown() {
            self._usage.mark_unsafe();
            eprintln!("stick cleanup failed: {e:#}");
        }
    }
}

/// udev uses a read-only predicate. No file on an unknown medium is opened.
/// The parent query hides every partition of the registered medium, including
/// its ESP, regardless of udev event ordering.
pub fn registered(paths: &Paths, uuid: Option<&str>, parent: Option<&Path>) -> bool {
    let Ok(config) = Config::load(&paths.config_file) else {
        return false;
    };
    let Some(stick) = config.stick else {
        return false;
    };
    if uuid.is_some_and(|u| stick.data_uuids.iter().any(|v| v.eq_ignore_ascii_case(u))) {
        return true;
    }
    let Some(parent) = parent else {
        return false;
    };
    let Ok(parent) = fs::canonicalize(parent) else {
        return false;
    };
    let Ok(tree) = capture(
        Command::new("/usr/bin/lsblk")
            .args(["-s", "-n", "-r", "-p", "-o", "NAME"])
            .arg(&parent),
    ) else {
        return false;
    };
    let disk = tree.lines().last().unwrap_or_default().trim();
    if !disk.starts_with("/dev/") {
        return false;
    }
    let Ok(parts) = capture(
        Command::new("/usr/bin/lsblk")
            .args(["-l", "-n", "-p", "-o", "NAME"])
            .arg(disk),
    ) else {
        return false;
    };
    parts.lines().any(|part| {
        let Ok(actual) = capture(
            Command::new("/usr/sbin/blkid")
                .args(["-p", "-s", "UUID", "-o", "value"])
                .arg(part.trim()),
        ) else {
            return false;
        };
        stick
            .data_uuids
            .iter()
            .any(|u| actual.eq_ignore_ascii_case(u))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_is_per_medium_live_and_keeps_backups_out_until_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_dir: root.path().join("run"),
            state_dir: root.path().join("state"),
            config_file: root.path().join("config.toml"),
            credentials_dir: root.path().join("credentials"),
            systemctl: root.path().join("no-systemctl"),
        };
        let uuid = "01234567-89ab-cdef-0123-456789abcdef";
        let other = "01234567-89ab-cdef-0123-456789abcdee";
        fs::create_dir_all(paths.runtime_dir.join("stick")).unwrap();
        let config =
            Config::parse(&format!("[stick]\ndata_uuids = ['{uuid}', '{other}']")).unwrap();
        assert_eq!(in_use(&paths, uuid), None);
        for usage in [Usage::Backup, Usage::Restore] {
            let run_lock = crate::run::RunLock::try_acquire(&paths).unwrap().unwrap();
            let guard = UsageGuard::acquire(&paths, uuid, usage).unwrap();
            assert!(crate::run::RunLock::try_acquire(&paths).unwrap().is_none());
            let status = crate::status::build(&paths, Some(&config), false).unwrap();
            let stick = &status.targets[&crate::state::Target::Stick];
            assert_eq!(stick.in_use, Some(usage));
            assert_eq!(stick.sticks[uuid].in_use, Some(usage));
            assert_eq!(in_use(&paths, &uuid.to_ascii_uppercase()), Some(usage));
            assert_eq!(stick.sticks[other].in_use, None);
            assert!(stick.plugged_in_uuids.is_empty());
            drop(guard);
            assert_eq!(in_use(&paths, uuid), None, "stale text must not mean busy");
            drop(run_lock);
            assert!(crate::run::RunLock::try_acquire(&paths).unwrap().is_some());
        }
    }

    #[test]
    fn failed_cleanup_does_not_become_safe_when_the_worker_exits() {
        let root = tempfile::tempdir().unwrap();
        let mut paths = Paths::from_env();
        paths.runtime_dir = root.path().into();
        fs::create_dir(root.path().join("stick")).unwrap();
        let uuid = "01234567-89ab-cdef-0123-456789abcdef";
        let mut guard = UsageGuard::acquire(&paths, uuid, Usage::Restore).unwrap();
        guard.mark_unsafe();
        drop(guard);
        assert_eq!(in_use(&paths, uuid), Some(Usage::Restore));
        let mut guard = UsageGuard::acquire(&paths, uuid, Usage::Restore).unwrap();
        drop(guard);
        assert_eq!(in_use(&paths, uuid), Some(Usage::Restore));
        guard = UsageGuard::acquire(&paths, uuid, Usage::Restore).unwrap();
        guard.mark_safe().unwrap();
        drop(guard);
        assert_eq!(in_use(&paths, uuid), None);
    }

    #[test]
    fn read_only_fsck_must_be_clean_even_when_its_exit_code_is_zero() {
        assert!(filesystem_healthy(
            true,
            b"Pass 5: Checking group summary information",
            b""
        ));
        assert!(!filesystem_healthy(
            true,
            b"Free blocks count wrong (1, counted=26599).\nFix? no",
            b""
        ));
        assert!(!filesystem_healthy(
            true,
            b"",
            b"Warning: skipping journal recovery because doing a read-only filesystem check."
        ));
        assert!(!filesystem_healthy(false, b"", b""));
    }
    #[test]
    fn opening_errors_keep_their_distinct_kinds() {
        for kind in [
            ErrorKind::StickAlreadyMounted,
            ErrorKind::StickAmbiguous,
            ErrorKind::StickBusy,
            ErrorKind::FilesystemDamaged,
        ] {
            assert_eq!(
                classify_open_error(error(kind, "fixture").into(), true).kind,
                kind
            );
            assert_eq!(
                classify_open_error(error(kind, "fixture").into(), false).kind,
                ErrorKind::StickNotPluggedIn
            );
        }
        assert_eq!(
            classify_open_error(anyhow::anyhow!("cannot spawn helper"), true).kind,
            ErrorKind::Internal
        );
    }

    #[test]
    fn monthly_verification_handles_new_and_future_media() {
        let now = chrono::Utc::now();
        assert!(full_verify_due(None, now));
        assert!(!full_verify_due(
            Some(now - chrono::Duration::days(29)),
            now
        ));
        assert!(full_verify_due(Some(now - chrono::Duration::days(30)), now));
        assert!(full_verify_due(Some(now + chrono::Duration::days(1)), now));
        assert!(!full_verify_due(
            Some(now + chrono::Duration::minutes(1)),
            now
        ));
    }
}
