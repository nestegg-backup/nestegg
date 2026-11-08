//! Private staging directories for restores into user-owned targets.
//!
//! Every staging directory `<root>/<id>` has a lock file `<root>/<id>.lock`
//! that the owning restore holds (exclusive `flock`) for its whole lifetime.
//! The lock file is created and locked *before* the directory. Cleanup of
//! leftovers from interrupted restores only deletes a directory whose lock
//! it can acquire itself, so a running restore is never touched, however
//! old its files look (restored files carry the backup's timestamps).

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg};

pub struct StagingDir {
    dir: PathBuf,
    lock_path: PathBuf,
    lock: Option<Flock<File>>,
}

impl StagingDir {
    /// Create a fresh staging directory below `root` (created with mode 0700).
    pub fn create(root: &Path) -> Result<StagingDir> {
        crate::secrets::create_private_dir(root)?;
        let id = format!("{:016x}", rand::random::<u64>());
        let lock_path = root.join(format!("{id}.lock"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .with_context(|| format!("cannot create {}", lock_path.display()))?;
        let lock = Flock::lock(file, FlockArg::LockExclusive)
            .map_err(|(_, e)| e)
            .context("cannot lock staging directory")?;
        let dir = root.join(&id);
        fs::create_dir(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        Ok(StagingDir {
            dir,
            lock_path,
            lock: Some(lock),
        })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Remove the directory, then the lock file, then release the lock.
    pub fn remove(mut self) -> Result<()> {
        self.cleanup()
    }

    fn cleanup(&mut self) -> Result<()> {
        if self.lock.is_none() {
            return Ok(());
        }
        let result = match fs::remove_dir_all(&self.dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("cannot remove {}", self.dir.display()))
            }
            _ => Ok(()),
        };
        if result.is_ok() {
            let _ = fs::remove_file(&self.lock_path);
        }
        self.lock = None;
        result
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if let Err(e) = self.cleanup() {
            eprintln!("warning: {e:#}");
        }
    }
}

/// Remove staging directories of restores that are no longer running.
/// Returns warnings for entries that could not be removed.
pub fn cleanup_stale(root: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return warnings;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let lock_path = root.join(format!("{name}.lock"));
        // A missing lock file means the owner is gone (it is created first
        // and removed last); creating it here makes the check uniform.
        let Ok(file) = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
        else {
            continue;
        };
        let Ok(_lock) = Flock::lock(file, FlockArg::LockExclusiveNonblock) else {
            continue; // still in use
        };
        match fs::remove_dir_all(&path) {
            Ok(()) => {
                let _ = fs::remove_file(&lock_path);
            }
            Err(e) => warnings.push(format!(
                "could not remove stale staging {}: {e}",
                path.display()
            )),
        }
    }
    // Orphaned lock files without a directory.
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "lock")
                && !path.with_extension("").exists()
                && let Ok(file) = OpenOptions::new().write(true).open(&path)
                && Flock::lock(file, FlockArg::LockExclusiveNonblock).is_ok()
            {
                let _ = fs::remove_file(&path);
            }
        }
    }
    warnings
}

/// Remove a directory tree that belongs to the caller, also if it contains
/// read-only directories: directories get `u+rwx` before they are emptied.
/// Symlinks are removed, never followed; other file systems are not entered.
pub fn remove_tree(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("cannot stat {}", path.display()))?;
    remove_tree_inner(path, meta.dev())
}

fn remove_tree_inner(path: &Path, dev: u64) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return fs::remove_file(path).with_context(|| format!("cannot remove {}", path.display()));
    }
    if meta.dev() != dev {
        anyhow::bail!(
            "{} is on another file system; not removing it",
            path.display()
        );
    }
    fs::set_permissions(
        path,
        fs::Permissions::from_mode((meta.mode() & 0o7777) | 0o700),
    )
    .with_context(|| format!("cannot make {} writable", path.display()))?;
    for entry in fs::read_dir(path)? {
        remove_tree_inner(&entry?.path(), dev)?;
    }
    fs::remove_dir(path).with_context(|| format!("cannot remove {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_restore_survives_cleanup_and_stale_one_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let active = StagingDir::create(root.path()).unwrap();
        fs::write(active.path().join("file"), "x").unwrap();
        // Simulate restored, very old timestamps on the active directory.
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        File::open(active.path())
            .unwrap()
            .set_modified(old)
            .unwrap();

        // A leftover of an interrupted restore: directory without a holder.
        let stale = root.path().join("deadbeefdeadbeef");
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("file"), "x").unwrap();
        // Another leftover whose lock file was never created.
        let stale2 = root.path().join("cafecafecafecafe");
        fs::create_dir(&stale2).unwrap();

        let warnings = cleanup_stale(root.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(
            active.path().join("file").exists(),
            "active staging must survive"
        );
        assert!(!stale.exists());
        assert!(!stale2.exists());

        let path = active.path().to_path_buf();
        active.remove().unwrap();
        assert!(!path.exists());
        assert_eq!(
            fs::read_dir(root.path()).unwrap().count(),
            0,
            "lock files removed"
        );
    }

    #[test]
    fn remove_tree_handles_read_only_directories_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, "keep").unwrap();
        let tree = root.path().join("tree");
        fs::create_dir_all(tree.join("readonly/sub")).unwrap();
        fs::write(tree.join("readonly/sub/data"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("readonly/link")).unwrap();
        fs::set_permissions(tree.join("readonly/sub"), fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(tree.join("readonly"), fs::Permissions::from_mode(0o555)).unwrap();
        remove_tree(&tree).unwrap();
        assert!(!tree.exists());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
    }
}
