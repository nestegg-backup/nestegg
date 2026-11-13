//! Pin backup roots before recording permissions or starting the client.
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use nix::fcntl::{OFlag, open, openat};
use nix::sys::stat::Mode;

/// The path is derived from this handle, never resolved a second time.
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
        let path = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
            .context("cannot derive source path from directory handle")?;
        if !path.is_absolute() || nix::sys::stat::fstat(&fd)?.st_nlink == 0 {
            bail!("source directory was removed while opening it");
        }
        if path.to_str().is_none() {
            bail!("source path from directory handle is not valid UTF-8");
        }
        Ok(Self {
            directory: File::from(fd),
            path,
        })
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
        assert!(SourceDirectory::open(&source).is_err());
        assert!(SourceDirectory::open(&source.join("subdirectory")).is_err());
    }

    #[test]
    fn an_existing_directory_named_deleted_is_not_a_deleted_handle() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Documents (deleted)");
        std::fs::create_dir(&source).unwrap();
        assert_eq!(SourceDirectory::open(&source).unwrap().path, source);
    }

    #[test]
    fn rejects_missing_relative_and_traversing_sources_and_accepts_root() {
        assert!(SourceDirectory::open(Path::new("/this-source-does-not-exist")).is_err());
        assert!(SourceDirectory::open(Path::new("home/alice")).is_err());
        assert!(SourceDirectory::open(Path::new("/home/../etc")).is_err());
        assert_eq!(
            SourceDirectory::open(Path::new("/")).unwrap().path,
            Path::new("/")
        );
    }
}
