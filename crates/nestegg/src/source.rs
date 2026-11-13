//! Pin backup roots before recording permissions or starting the client.
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use nix::fcntl::{OFlag, open, openat};
use nix::sys::stat::Mode;

/// The recorded path is derived from this handle; the client inherits the handle.
#[derive(Debug)]
pub struct SourceDirectory {
    pub directory: File,
    pub path: PathBuf,
}

impl SourceDirectory {
    pub fn open(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            bail!("backup source must be absolute");
        }
        // Configuration may name symlinks, as before. Once resolved, refuse
        // any symlink introduced before or during the component-wise open.
        let canonical = std::fs::canonicalize(path).context("cannot resolve backup source")?;
        Self::open_resolved(&canonical)
    }

    fn open_resolved(path: &Path) -> Result<Self> {
        Self::from_directory(Self::open_without_symlinks(path)?)
    }

    fn open_without_symlinks(path: &Path) -> Result<File> {
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let mut fd = open("/", flags, Mode::empty())?;
        for component in path.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => {
                    fd = openat(&fd, name, flags, Mode::empty()).with_context(|| {
                        format!("cannot open source {} without symlinks", path.display())
                    })?;
                }
                _ => bail!("backup source must not contain parent traversal"),
            }
        }
        Ok(File::from(fd))
    }

    fn from_directory(directory: File) -> Result<Self> {
        let path = std::fs::read_link(format!("/proc/self/fd/{}", directory.as_raw_fd()))
            .context("cannot derive source path from directory handle")?;
        if !path.is_absolute() || nix::sys::stat::fstat(&directory)?.st_nlink == 0 {
            bail!("source directory was removed while opening it");
        }
        if path.to_str().is_none() {
            bail!("source path from directory handle is not valid UTF-8");
        }
        Self::verify_path(&directory, &path)?;
        Ok(Self { directory, path })
    }

    fn verify_path(directory: &File, path: &Path) -> Result<()> {
        // A detached mount can give /proc/self/fd a misleading absolute path.
        // Re-open only to verify its identity; the client still uses directory.
        let reopened = Self::open_without_symlinks(path)
            .context("cannot verify source path from directory handle")?;
        let pinned = nix::sys::stat::fstat(directory)?;
        let resolved = nix::sys::stat::fstat(&reopened)?;
        if (pinned.st_dev, pinned.st_ino) != (resolved.st_dev, resolved.st_ino) {
            bail!("source path no longer identifies the opened directory");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn swap_after_recording_cannot_change_the_inherited_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Documents");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(source.join("report"), "own files").unwrap();
        std::fs::write(outside.join("secret"), "other files").unwrap();
        let pinned = SourceDirectory::open(&source).unwrap();
        assert_eq!(pinned.path, source);
        std::fs::rename(&source, dir.path().join("moved")).unwrap();
        symlink(&outside, &source).unwrap();
        let mut fds = crate::secrets::FdPassing::new();
        let fd = fds.inherit_directory(&pinned.directory).unwrap();
        let mut cmd = std::process::Command::new("ls");
        cmd.arg(format!("/proc/self/fd/{fd}/"));
        fds.apply(&mut cmd);
        let output = cmd.output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "report\n");
        assert_eq!(SourceDirectory::open(&source).unwrap().path, outside);
        assert!(SourceDirectory::open(&source.join("subdirectory")).is_err());
    }

    #[test]
    fn configured_symlinks_in_leaf_and_parent_components_are_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let actual = dir.path().join("actual");
        std::fs::create_dir_all(actual.join("Documents")).unwrap();
        let link = dir.path().join("configured");
        symlink(&actual, &link).unwrap();
        for suffix in ["", "Documents"] {
            assert_eq!(
                SourceDirectory::open(&link.join(suffix)).unwrap().path,
                actual.join(suffix)
            );
        }
    }

    #[test]
    fn symlink_introduced_after_canonicalization_is_refused() {
        for leaf in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source");
            let outside = dir.path().join("outside");
            std::fs::create_dir_all(source.join("Documents")).unwrap();
            std::fs::create_dir_all(outside.join("Documents")).unwrap();
            let path = if leaf {
                source.clone()
            } else {
                source.join("Documents")
            };
            let canonical = std::fs::canonicalize(path).unwrap();
            std::fs::rename(&source, dir.path().join("moved")).unwrap();
            symlink(&outside, &source).unwrap();
            assert!(SourceDirectory::open_resolved(&canonical).is_err());
        }
    }

    #[test]
    fn recorded_path_must_identify_the_same_directory_without_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Documents");
        let other = dir.path().join("other");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&other).unwrap();
        let directory = SourceDirectory::open_without_symlinks(&source).unwrap();
        SourceDirectory::verify_path(&directory, &source).unwrap();
        let error = SourceDirectory::verify_path(&directory, &other).unwrap_err();
        assert!(
            error.to_string().contains("no longer identifies"),
            "{error:#}"
        );
        std::fs::rename(&source, dir.path().join("moved")).unwrap();
        assert!(SourceDirectory::verify_path(&directory, &source).is_err());
        symlink(dir.path().join("moved"), &source).unwrap();
        assert!(SourceDirectory::verify_path(&directory, &source).is_err());
    }

    #[test]
    #[ignore = "requires root and a private mount namespace (unshare)"]
    fn detached_mount_path_is_refused() {
        use std::process::Command;
        const TEST: &str = "source::tests::detached_mount_path_is_refused";
        if std::env::var_os("NESTEGG_TEST_PRIVATE_MOUNT_NS").is_none() {
            let status = Command::new("unshare")
                .args(["--mount", "--propagation", "private"])
                .arg(std::env::current_exe().unwrap())
                .args([TEST, "--exact", "--ignored", "--nocapture"])
                .env("NESTEGG_TEST_PRIVATE_MOUNT_NS", "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Command::new("mount")
                .args(["-t", "tmpfs", "tmpfs"])
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let source = dir.path().join("Documents");
        std::fs::create_dir(&source).unwrap();
        let directory = SourceDirectory::open_without_symlinks(&source).unwrap();
        assert!(
            Command::new("umount")
                .arg("-l")
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let recorded =
            std::fs::read_link(format!("/proc/self/fd/{}", directory.as_raw_fd())).unwrap();
        assert_eq!(recorded, Path::new("/Documents"));
        let error = SourceDirectory::from_directory(directory).unwrap_err();
        assert!(format!("{error:#}").contains("source path"), "{error:#}");
    }

    #[test]
    fn an_existing_directory_named_deleted_is_not_a_deleted_handle() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Documents (deleted)");
        std::fs::create_dir(&source).unwrap();
        assert_eq!(SourceDirectory::open(&source).unwrap().path, source);
    }

    #[test]
    fn rejects_missing_and_relative_sources_and_accepts_root() {
        assert!(SourceDirectory::open(Path::new("/this-source-does-not-exist")).is_err());
        assert!(SourceDirectory::open(Path::new("home/alice")).is_err());
        assert_eq!(
            SourceDirectory::open(Path::new("/etc/../etc"))
                .unwrap()
                .path,
            Path::new("/etc")
        );
        assert_eq!(
            SourceDirectory::open(Path::new("/")).unwrap().path,
            Path::new("/")
        );
    }
}
