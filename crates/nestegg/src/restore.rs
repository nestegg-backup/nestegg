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
    #[error("{message}")]
    ScopeForbidden {
        reason: ForbiddenReason,
        message: String,
    },
    #[error("{0}")]
    BadRequest(String),
}

/// Stable, source-independent reasons for a refused restore scope.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForbiddenReason {
    RestoreNoHome,
    RestoreSourceUnverified,
    RestoreOutsideHome,
}

pub fn scope_for_source(source: &Path, bound: bool, user: &UserIdentity) -> Result<UserScope> {
    let home = std::fs::canonicalize(&user.home).ok();
    if !bound
        && home
            .as_ref()
            .is_some_and(|home| source != home && source.starts_with(home))
    {
        return Err(RestoreError::ScopeForbidden {
            reason: ForbiddenReason::RestoreSourceUnverified,
            message: "this archive needs handle-bound source metadata; ask an administrator to restore it with sudo nestegg restore".into(),
        }.into());
    }
    user_scope_with_binding(source, &user.home, user.uid, bound).ok_or_else(|| {
        RestoreError::ScopeForbidden {
            reason: ForbiddenReason::RestoreNoHome,
            message: "this archive does not contain your home directory".into(),
        }
        .into()
    })
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
    let client = Client::new(
        config,
        crate::secrets::SecretStore::new(&paths.credentials_dir),
    );
    execute_with_client(paths, &client, job, requester, progress, log)
}

/// The same permission and staging rules for an isolated stick client.
pub fn execute_with_client(
    paths: &Paths,
    client: &Client<'_>,
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
    let exists = match requester {
        Requester::User(user) => crate::identity::exists_as(user, target)?,
        Requester::Root { .. } => std::fs::symlink_metadata(target).is_ok(),
    };
    if exists {
        return Err(RestoreError::TargetExists(target.to_path_buf()).into());
    }
    let patterns = crate::client::user_restore_patterns(job.archive, target, job.patterns);
    let direct = matches!(requester, Requester::Root { .. }) && root_controlled_parent(target)?;
    if direct {
        client.restore(
            &RestoreRequest {
                snapshot: job.snapshot,
                archive: job.archive,
                target,
                patterns: &patterns,
                key: job.key,
            },
            log,
        )?;
        if !job.patterns.is_empty() && std::fs::symlink_metadata(target).is_err() {
            bail!("no files matched the selected paths");
        }
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
    if !crate::identity::is_dir_as(user, parent)? {
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
    check_staging_space(client, &job, &staging_root, log)?;
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
                patterns: &patterns,
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
    /// Archive-internal path of the allowed subtree, e.g. `/alice`.
    /// `/` also covers an archive sourced from a directory below the home.
    pub prefix: String,
}

/// Map a user's home directory onto an archive: `home = /home/alice` and
/// the archive's source path `/home` give `/alice`. A source inside the
/// home (e.g. `/home/alice/Documents`) grants the archive root `/`.
/// `source_path` must
/// describe the snapshot itself, not the current configuration (which may
/// have changed since), and must come from a source the user cannot
/// change: the snapshot's metadata blob or the local run record, never the
/// snapshot notes (decision 0016).
pub fn user_scope(source_path: &Path, home: &Path, uid: u32) -> Option<UserScope> {
    user_scope_with_binding(source_path, home, uid, true)
}

fn user_scope_with_binding(
    source_path: &Path,
    home: &Path,
    uid: u32,
    bound: bool,
) -> Option<UserScope> {
    use std::os::unix::fs::MetadataExt;
    // The home directory must be a real directory owned by the caller and
    // not the root directory (system accounts).
    let home = std::fs::canonicalize(home).ok()?;
    if home == Path::new("/") {
        return None;
    }
    let meta = std::fs::metadata(&home).ok()?;
    if !meta.is_dir() || meta.uid() != uid {
        return None;
    }
    if source_path == Path::new("/") {
        // Full system backup: service accounts have homes like
        // `/var/backups` or `/run/...`, which would expose system files.
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
    // Compare the recorded path lexically: the source may no longer exist.
    // Never resolve it against today's file system or accept traversal.
    if !source_path.is_absolute()
        || source_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    if source_path.starts_with(&home) {
        // Enforce binding against this final home resolution too. A changed
        // home symlink between the explanatory check and here cannot grant
        // an unbound descendant archive.
        if !bound && source_path != home {
            return None;
        }
        return Some(UserScope { prefix: "/".into() });
    }
    let rel = home.strip_prefix(source_path).ok()?;
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

/// Escape glob metacharacters so a path matches only itself.
fn escape_glob(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Patterns for `proxmox-backup-client restore --pattern` that select the
/// given archive-internal paths and everything below them. Patterns are
/// anchored at the archive root (leading `/`) and glob characters in names
/// are escaped: an unanchored `alice` would match every directory named
/// `alice` anywhere in the archive.
pub fn patterns_for(paths: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for p in paths {
        let rel = p.trim_start_matches('/');
        if rel.is_empty() {
            return Vec::new(); // whole archive
        }
        let escaped = escape_glob(rel);
        out.push(format!("/{escaped}"));
        out.push(format!("/{escaped}/**"));
    }
    out
}

/// Largest file accepted as a key file or saved paper key. A key file has
/// a few hundred bytes; an html paper key with its QR code some 10 KiB.
pub const MAX_KEY_FILE_BYTES: u64 = 1024 * 1024;

/// Read a small regular file named by a user. Refuses anything else: a
/// FIFO or terminal would block forever, a device such as `/dev/zero` or a
/// whole disk would be read into memory without end.
pub fn read_small_regular_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    // O_NONBLOCK: opening a FIFO without a writer returns at once.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOCTTY)
        .open(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("cannot read {}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    if meta.len() > max_bytes {
        bail!(
            "{} is too large ({} bytes; a key file has less than {max_bytes})",
            path.display(),
            meta.len()
        );
    }
    let mut data = Vec::new();
    // The limit again while reading: the file may grow after the check.
    file.take(max_bytes + 1)
        .read_to_end(&mut data)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if data.len() as u64 > max_bytes {
        bail!("{} is too large for a key file", path.display());
    }
    Ok(data)
}

/// Read the key file named by the user (for `--keyfile`).
pub fn read_user_key(path: &Path, sudo: Option<&UserIdentity>) -> Result<crate::secrets::Secret> {
    let bytes = match sudo {
        Some(user) => crate::identity::read_file_as(user, path)?,
        None => read_small_regular_file(path, MAX_KEY_FILE_BYTES)?,
    };
    Ok(crate::secrets::Secret::new(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_home_link_cannot_turn_an_unbound_source_into_a_descendant_scope() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("alice");
        let source = home.join("Documents");
        std::fs::create_dir_all(&source).unwrap();
        let link = dir.path().join("home-link");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        let uid = nix::unistd::geteuid().as_raw();
        assert!(user_scope_with_binding(&source, &link, uid, false).is_some());
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&home, &link).unwrap();
        assert!(user_scope_with_binding(&source, &link, uid, false).is_none());
        assert_eq!(
            user_scope_with_binding(&source, &link, uid, true)
                .unwrap()
                .prefix,
            "/"
        );
    }

    #[test]
    fn key_files_must_be_small_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("key.json");
        std::fs::write(&ok, b"{}").unwrap();
        assert_eq!(read_small_regular_file(&ok, 16).unwrap(), b"{}");

        let big = dir.path().join("big");
        std::fs::write(&big, vec![0u8; 17]).unwrap();
        let err = read_small_regular_file(&big, 16).unwrap_err().to_string();
        assert!(err.contains("too large"), "{err}");

        // A FIFO without a writer must not block.
        let fifo = dir.path().join("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
        let err = read_small_regular_file(&fifo, 16).unwrap_err().to_string();
        assert!(err.contains("not a regular file"), "{err}");

        let err = read_small_regular_file(Path::new("/dev/zero"), 16)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a regular file"), "{err}");
        let err = read_small_regular_file(dir.path(), 16)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a regular file"), "{err}");
    }

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
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let homes = std::fs::canonicalize(root.path()).unwrap();
        let alice = homes.join("alice");
        std::fs::create_dir(&alice).unwrap();
        let uid = std::fs::metadata(&alice).unwrap().uid();
        let scope = user_scope(&homes, &alice, uid).unwrap();
        assert_eq!(scope.prefix, "/alice");
        // Not owned by the caller, root directory, outside the source.
        assert!(user_scope(&homes, &alice, uid + 1).is_none());
        assert!(user_scope(Path::new("/"), Path::new("/"), 0).is_none());
        assert!(user_scope(Path::new("/elsewhere"), &alice, uid).is_none());
        assert!(scope.contains("/alice"));
        assert!(scope.contains("/alice/Documents"));
        assert!(!scope.contains("/alicex"));
        assert!(!scope.contains("/bob"));
        assert!(scope.is_ancestor("/"));
        assert!(!scope.is_ancestor("/bob"));
        // Full system backup (source "/"): homes map onto their absolute
        // path; system directories are refused.
        // (The test home lives below /tmp, a system directory: refused.)
        assert!(user_scope(Path::new("/"), &alice, uid).is_none());
        let sys = UserScope {
            prefix: "/home/alice".into(),
        };
        assert!(!sys.contains("/etc"));
        for home in ["/var", "/run", "/etc", "/tmp"] {
            assert!(
                user_scope(Path::new("/"), Path::new(home), 0).is_none(),
                "{home}"
            );
        }
    }

    #[test]
    fn archives_sourced_inside_home_start_at_the_archive_root() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("alice");
        std::fs::create_dir(&home).unwrap();
        let uid = std::fs::metadata(&home).unwrap().uid();
        // No current source directory is needed: metadata describes the past.
        for source in [
            home.clone(),
            home.join("Documents"),
            home.join("Documents/work"),
        ] {
            let scope = user_scope(&source, &home, uid).unwrap();
            assert_eq!(scope.prefix, "/");
            assert!(scope.contains("/"));
            assert!(scope.contains("/report.txt"));
            assert!(!scope.is_ancestor("/"));
            assert!(patterns_for(&[scope.prefix]).is_empty());
            assert!(user_scope(&source, &home, uid + 1).is_none());
        }
        for source in [
            root.path().join("alicex/Documents"),
            root.path().join("bob/Documents"),
            home.join("../bob"),
            home.join("Documents/../../bob"),
            PathBuf::from("relative"),
        ] {
            assert!(user_scope(&source, &home, uid).is_none(), "{source:?}");
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
                "/alice/Documents".to_string(),
                "/alice/Documents/**".to_string()
            ]
        );
        assert_eq!(
            patterns_for(&["/alice/[ab]*?.txt".into()]),
            vec![
                r"/alice/\[ab\]\*\?.txt".to_string(),
                r"/alice/\[ab\]\*\?.txt/**".to_string()
            ]
        );
        assert!(patterns_for(&["/".into()]).is_empty());
    }
}
