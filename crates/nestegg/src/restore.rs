//! Restores, shared by the CLI and the daemon API.
//!
//! - Into a directory that only root can modify: the official client
//!   restores directly as root (original owners and permissions).
//! - Otherwise: root restores into a private, locked staging directory, and
//!   a tar process running as the requesting user copies the tree into the
//!   target (see [`crate::identity::copy_tree_as`]).
//!
//! Unprivileged requesters (API calls from group members) may only browse
//! and restore paths inside their own home directory; see [`UserScope`].

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result, bail};

use crate::client::{Client, RestoreKey, RestoreRequest};
use crate::config::{Config, Paths};
use crate::identity::UserIdentity;

/// Errors with a stable meaning for the API.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("{0} already exists; restores only go into a new directory")]
    TargetExists(PathBuf),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    BadRequest(String),
}

/// Who asked for the restore.
#[derive(Debug, Clone)]
pub enum Requester {
    /// root, optionally invoked through sudo by `user`.
    Root { sudo: Option<UserIdentity> },
    /// An unprivileged user (daemon API, identified by socket credentials).
    User(UserIdentity),
}

impl Requester {
    fn copy_identity(&self) -> Option<&UserIdentity> {
        match self {
            Requester::Root { sudo } => sudo.as_ref(),
            Requester::User(u) => Some(u),
        }
    }
}

pub struct Job<'a> {
    pub snapshot: &'a str,
    pub archive: &'a str,
    /// Absolute target path that must not exist yet.
    pub target: &'a Path,
    pub patterns: &'a [String],
    pub key: RestoreKey,
    pub staging_dir: Option<&'a Path>,
}

/// Progress and cancellation shared with a caller (daemon).
#[derive(Default, Clone)]
pub struct Progress {
    pub restored_bytes: Arc<AtomicU64>,
}

/// Absolute form of a restore target. `..` is rejected anywhere, so the
/// path that is checked is exactly the path the client uses.
pub fn absolute_target(target: &Path) -> Result<PathBuf> {
    if target
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(
            RestoreError::BadRequest("the restore target must not contain '..'".into()).into(),
        );
    }
    let absolute = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir()?.join(target)
    };
    if absolute.parent().is_none() {
        return Err(RestoreError::BadRequest("invalid restore target".into()).into());
    }
    Ok(absolute)
}

/// True if `absolute`'s existing ancestors are real directories that only
/// root can modify. Then writing there as root cannot be redirected by
/// another user (no symlink swaps, no renames).
pub fn root_controlled_parent(absolute: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let mut current = PathBuf::from("/");
    let parent = absolute.parent().unwrap_or(Path::new("/"));
    for component in parent.components() {
        match component {
            std::path::Component::Normal(name) => current.push(name),
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            _ => return Ok(false),
        }
        let meta = match std::fs::symlink_metadata(&current) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(e) => return Err(e.into()),
        };
        if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn free_bytes(path: &Path) -> Result<u64> {
    let stat = nix::sys::statvfs::statvfs(path)?;
    Ok(stat.blocks_available() as u64 * stat.fragment_size() as u64)
}

/// Identity of the storage behind `path`. btrfs subvolumes and ZFS datasets
/// have different device numbers but share one pool; they are recognised by
/// identical totals and free space, and then treated as one.
fn storage_of(path: &Path) -> Result<(u64, u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(path)?.dev();
    let st = nix::sys::statvfs::statvfs(path)?;
    Ok((dev, st.blocks() as u64, st.blocks_free() as u64))
}

pub fn same_storage(a: (u64, u64, u64), b: (u64, u64, u64)) -> bool {
    a.0 == b.0 || (a.1 == b.1 && a.2 == b.2)
}

/// Nearest existing ancestor of `path`.
fn existing_ancestor(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    while !p.exists() {
        if !p.pop() {
            return PathBuf::from("/");
        }
    }
    p
}

/// Space needed per file system for a staged restore of `size` bytes: the
/// staging copy and the user's copy exist at the same time.
pub fn space_requirements(size: u64, staging_dev: u64, target_dev: u64) -> Vec<(u64, u64)> {
    let need = size + size / 20;
    if staging_dev == target_dev {
        vec![(staging_dev, need * 2)]
    } else {
        vec![(staging_dev, need), (target_dev, need)]
    }
}

/// Check free space for a staged restore. With patterns only part of the
/// archive is restored, so the archive size only produces a warning.
fn check_staging_space(
    client: &Client,
    job: &Job<'_>,
    staging_root: &Path,
    log: &mut dyn Write,
) -> Result<()> {
    let size = match client.archive_size(job.snapshot, job.archive) {
        Ok(Some(size)) => size,
        Ok(None) => return Ok(()),
        Err(e) => {
            let _ = writeln!(log, "warning: could not determine the archive size: {e:#}");
            return Ok(());
        }
    };
    let target_dir = existing_ancestor(job.target.parent().unwrap_or(Path::new("/")));
    let staging_storage = storage_of(staging_root)?;
    let target_storage = storage_of(&target_dir)?;
    let staging_dev = staging_storage.0;
    let target_dev = if same_storage(staging_storage, target_storage) {
        staging_dev
    } else {
        target_storage.0
    };
    for (dev, need) in space_requirements(size, staging_dev, target_dev) {
        let dir = if dev == staging_dev {
            staging_root
        } else {
            target_dir.as_path()
        };
        let free = free_bytes(dir)?;
        if free < need {
            let msg = format!(
                "a staged restore of this archive (about {} MiB) needs about {} MiB in {}, \
                 but only {} MiB are free; use another staging directory or restore into \
                 a root-owned target",
                size / (1024 * 1024),
                need / (1024 * 1024),
                dir.display(),
                free / (1024 * 1024)
            );
            if job.patterns.is_empty() {
                bail!(msg);
            }
            let _ = writeln!(
                log,
                "warning: {msg} (only matching files are restored, continuing)"
            );
        }
    }
    Ok(())
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else {
            continue;
        };
        for entry in rd.flatten() {
            match entry.metadata() {
                Ok(m) if m.is_dir() => stack.push(entry.path()),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

/// Run one restore. Returns warnings (e.g. entries tar skipped).
pub fn execute(
    paths: &Paths,
    config: &Config,
    job: Job<'_>,
    requester: &Requester,
    progress: &Progress,
    log: &mut dyn Write,
) -> Result<Vec<String>> {
    let target = job.target;
    if !target.is_absolute() {
        return Err(
            RestoreError::BadRequest("the restore target must be an absolute path".into()).into(),
        );
    }
    if std::fs::symlink_metadata(target).is_ok() {
        return Err(RestoreError::TargetExists(target.to_path_buf()).into());
    }
    let direct = matches!(requester, Requester::Root { .. }) && root_controlled_parent(target)?;
    let client = Client::new(
        config,
        crate::secrets::SecretStore::new(&paths.credentials_dir),
    );
    if direct {
        client.restore(
            &RestoreRequest {
                snapshot: job.snapshot,
                archive: job.archive,
                target,
                patterns: job.patterns,
                key: job.key,
            },
            log,
        )?;
        progress
            .restored_bytes
            .store(dir_size(target), Ordering::Relaxed);
        return Ok(Vec::new());
    }
    let Some(user) = requester.copy_identity() else {
        return Err(RestoreError::Forbidden(format!(
            "{} is inside a directory that other users can modify. Restore into a \
             root-owned location (e.g. /root/restore) or run via sudo from the owning user.",
            target.display()
        ))
        .into());
    };
    let staging_root = match job.staging_dir {
        Some(dir) => {
            let dir = absolute_target(dir)?;
            if !dir.is_dir() || !root_controlled_parent(&dir.join("x"))? {
                bail!(
                    "staging directory {} must be an existing directory that only root can modify",
                    dir.display()
                );
            }
            dir.join("nestegg-restore-staging")
        }
        None => paths.state_dir.join("restore-staging"),
    };
    let parent = target.parent().unwrap_or(Path::new("/"));
    if !parent.is_dir() {
        return Err(RestoreError::BadRequest(format!(
            "{} does not exist; create it first (the restore creates only the last directory)",
            parent.display()
        ))
        .into());
    }
    crate::secrets::create_private_dir(&staging_root)?;
    for warning in crate::staging::cleanup_stale(&staging_root) {
        let _ = writeln!(log, "warning: {warning}");
    }
    check_staging_space(&client, &job, &staging_root, log)?;
    let staging = crate::staging::StagingDir::create(&staging_root)?;
    let data = staging.path().join("data");

    // Progress: size of the staging copy while the client restores.
    let done = Arc::new(AtomicBool::new(false));
    let monitor = {
        let done = done.clone();
        let data = data.clone();
        let restored = progress.restored_bytes.clone();
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                restored.store(dir_size(&data), Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        })
    };
    let result = client
        .restore(
            &RestoreRequest {
                snapshot: job.snapshot,
                archive: job.archive,
                target: &data,
                patterns: job.patterns,
                key: job.key,
            },
            log,
        )
        .and_then(|()| {
            progress
                .restored_bytes
                .store(dir_size(&data), Ordering::Relaxed);
            crate::identity::copy_tree_as(user, &data, target)
        });
    done.store(true, Ordering::Relaxed);
    let _ = monitor.join();
    if let Err(e) = staging.remove() {
        let _ = writeln!(log, "warning: {e:#}");
    }
    result
}

/// What an unprivileged user may see and restore: the part of an archive
/// that corresponds to their home directory.
#[derive(Debug, Clone, PartialEq)]
pub struct UserScope {
    /// Archive-internal path of the home directory, e.g. `/alice`.
    pub prefix: String,
}

/// Map a user's home directory onto an archive of this configuration:
/// `home = /home/alice`, source `home.pxar` → `/home` gives `/alice`.
pub fn user_scope(config: &Config, archive: &str, home: &Path) -> Option<UserScope> {
    let name = archive.strip_suffix(".pxar").unwrap_or(archive);
    let source_path = if config.backup.system && name == crate::layout::SYSTEM_ARCHIVE {
        // The full system backup is `/`; it is not in the configured sources.
        PathBuf::from("/")
    } else {
        let source = config.backup.sources.iter().find(|s| s.archive == name)?;
        std::fs::canonicalize(&source.path).unwrap_or_else(|_| source.path.clone())
    };
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    if config.backup.system && name == crate::layout::SYSTEM_ARCHIVE {
        // Service accounts have homes like `/`, `/var/backups` or `/run/...`:
        // in the full system backup that would expose system files.
        let first = home
            .components()
            .nth(1)
            .map(|c| c.as_os_str().to_string_lossy().into_owned());
        let system_dirs = [
            "bin",
            "boot",
            "dev",
            "etc",
            "lib",
            "lib32",
            "lib64",
            "nonexistent",
            "proc",
            "run",
            "sbin",
            "sys",
            "tmp",
            "usr",
            "var",
        ];
        if first.is_none_or(|f| system_dirs.contains(&f.as_str())) {
            return None;
        }
    }
    let rel = home.strip_prefix(&source_path).ok()?;
    if rel.as_os_str().is_empty() {
        // The archive is the home directory itself (e.g. source `/home/alice`).
        return Some(UserScope { prefix: "/".into() });
    }
    Some(UserScope {
        prefix: format!("/{}", rel.to_str()?),
    })
}

/// Normalise an archive-internal path to `/a/b` form; `..` is rejected.
pub fn normalize_archive_path(path: &str) -> Result<String> {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(RestoreError::BadRequest("paths must not contain '..'".into()).into());
            }
            p => parts.push(p),
        }
    }
    Ok(format!("/{}", parts.join("/")))
}

impl UserScope {
    /// The path is the home directory or inside it.
    pub fn contains(&self, path: &str) -> bool {
        self.prefix == "/" || path == self.prefix || path.starts_with(&format!("{}/", self.prefix))
    }

    /// The path is an ancestor of the home directory (shown, but only the
    /// branch towards the home directory).
    pub fn is_ancestor(&self, path: &str) -> bool {
        self.prefix != "/" && (path == "/" || self.prefix.starts_with(&format!("{path}/")))
    }
}

/// Patterns for `proxmox-backup-client restore --pattern` that select the
/// given archive-internal paths and everything below them.
pub fn patterns_for(paths: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for p in paths {
        let rel = p.trim_start_matches('/');
        if rel.is_empty() {
            return Vec::new(); // whole archive
        }
        out.push(rel.to_string());
        out.push(format!("{rel}/**"));
    }
    out
}

/// Read the key file named by the user (for `--keyfile`).
pub fn read_user_key(path: &Path, sudo: Option<&UserIdentity>) -> Result<crate::secrets::Secret> {
    let bytes = match sudo {
        Some(user) => crate::identity::read_file_as(user, path)?,
        None => std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?,
    };
    Ok(crate::secrets::Secret::new(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_restore_needs_space_for_both_copies() {
        let size = 100 * 1024 * 1024 * 1024u64;
        let same = space_requirements(size, 1, 1);
        assert_eq!(same.len(), 1);
        assert!(same[0].1 >= 2 * size);
        let split = space_requirements(size, 1, 2);
        assert_eq!(split.len(), 2);
        assert!(
            split
                .iter()
                .all(|(_, need)| *need >= size && *need < 2 * size)
        );
    }

    #[test]
    fn subvolumes_sharing_a_pool_count_as_one() {
        assert!(same_storage((1, 100, 50), (2, 100, 50)));
        assert!(same_storage((1, 100, 50), (1, 200, 10)));
        assert!(!same_storage((1, 100, 50), (2, 100, 49)));
    }

    #[test]
    fn restore_targets_are_normalised() {
        assert!(absolute_target(Path::new("/a/../b")).is_err());
        assert!(absolute_target(Path::new("rel")).unwrap().is_absolute());
        assert!(absolute_target(Path::new("/")).is_err());
    }

    #[test]
    fn user_scope_mapping() {
        let mut c = crate::config::example("pbs.example.com", "backups", "backup@pbs!desktop");
        c.backup.sources = vec![crate::config::Source {
            archive: "home".into(),
            path: "/nonexistent-home-root".into(),
            optional: false,
            require_mount: false,
        }];
        let scope = user_scope(&c, "home.pxar", Path::new("/nonexistent-home-root/alice")).unwrap();
        assert_eq!(scope.prefix, "/alice");
        assert!(scope.contains("/alice"));
        assert!(scope.contains("/alice/Documents"));
        assert!(!scope.contains("/alicex"));
        assert!(!scope.contains("/bob"));
        assert!(scope.is_ancestor("/"));
        assert!(!scope.is_ancestor("/bob"));
        assert!(user_scope(&c, "etc.pxar", Path::new("/nonexistent-home-root/alice")).is_none());
        assert!(user_scope(&c, "home.pxar", Path::new("/elsewhere/alice")).is_none());
        assert!(user_scope(&c, "system.pxar", Path::new("/nonexistent-home-root/alice")).is_none());
        let mut sys = c.clone();
        sys.backup.system = true;
        let scope = user_scope(
            &sys,
            "system.pxar",
            Path::new("/nonexistent-home-root/alice"),
        )
        .unwrap();
        assert_eq!(scope.prefix, "/nonexistent-home-root/alice");
        assert!(!scope.contains("/etc"));
        assert!(scope.is_ancestor("/nonexistent-home-root"));
        for home in ["/", "/var/backups", "/run/systemd", "/etc"] {
            assert!(
                user_scope(&sys, "system.pxar", Path::new(home)).is_none(),
                "{home}"
            );
        }
    }

    #[test]
    fn archive_paths_and_patterns() {
        assert_eq!(
            normalize_archive_path("alice//Documents/").unwrap(),
            "/alice/Documents"
        );
        assert_eq!(normalize_archive_path("/").unwrap(), "/");
        assert!(normalize_archive_path("/alice/../bob").is_err());
        assert_eq!(
            patterns_for(&["/alice/Documents".into()]),
            vec![
                "alice/Documents".to_string(),
                "alice/Documents/**".to_string()
            ]
        );
        assert!(patterns_for(&["/".into()]).is_empty());
    }
}
