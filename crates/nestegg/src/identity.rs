//! Acting as the user who invoked `sudo`.
//!
//! nestegg never switches identity inside its own process. Work that must
//! happen with a user's permissions (writing an exported key into the user's
//! directory, reading a key file the user names, restoring into a directory
//! the user controls) runs in a child process that drops to the user's uid,
//! primary gid and supplementary groups before `exec`. The kernel then
//! applies exactly that user's permissions to every path lookup.

use std::ffi::CString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct UserIdentity {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    pub home: PathBuf,
}

impl UserIdentity {
    /// The account that invoked sudo (`SUDO_UID`), if any and not root.
    pub fn from_sudo() -> Result<Option<UserIdentity>> {
        let Some(uid) = std::env::var("SUDO_UID")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
        else {
            return Ok(None);
        };
        if uid == 0 {
            return Ok(None);
        }
        let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))?
            .with_context(|| format!("SUDO_UID {uid} does not exist"))?;
        let name = CString::new(user.name.clone())?;
        let groups = nix::unistd::getgrouplist(&name, user.gid)
            .context("cannot determine the user's groups")?
            .into_iter()
            .map(|g| g.as_raw())
            .collect();
        Ok(Some(UserIdentity {
            uid,
            gid: user.gid.as_raw(),
            groups,
            home: user.dir,
        }))
    }

    /// Make `cmd` drop to this identity in the child, after any earlier
    /// `pre_exec` hooks (e.g. file descriptor passing) have run.
    pub fn apply(&self, cmd: &mut Command) {
        use std::os::unix::process::CommandExt;
        let uid = self.uid;
        let gid = self.gid;
        let groups: Vec<nix::libc::gid_t> = self.groups.clone();
        cmd.env("HOME", &self.home).current_dir("/");
        // SAFETY: only async-signal-safe system calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if nix::libc::setgroups(groups.len(), groups.as_ptr()) != 0
                    || nix::libc::setgid(gid) != 0
                    || nix::libc::setuid(uid) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                // Refuse to continue if root could be regained.
                if nix::libc::setuid(0) == 0 {
                    return Err(std::io::Error::from_raw_os_error(nix::libc::EPERM));
                }
                Ok(())
            });
        }
    }
}

/// Run a hidden `nestegg` helper subcommand as `user`.
fn helper(user: &UserIdentity, args: &[&std::ffi::OsStr], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let exe = std::env::current_exe().context("cannot locate nestegg binary")?;
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    user.apply(&mut cmd);
    let mut child = cmd.spawn().context("cannot start helper process")?;
    if let Some(data) = input {
        let mut stdin = child.stdin.take().context("no stdin")?;
        stdin.write_all(data)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "{}",
            String::from_utf8_lossy(&output.stderr)
                .trim()
                .trim_start_matches("error: ")
        );
    }
    Ok(output.stdout)
}

/// Create a new mode-0600 file as `user` (fails if anything exists there).
pub fn write_new_file_as(user: &UserIdentity, path: &Path, data: &[u8]) -> Result<()> {
    helper(
        user,
        &["__write-new-file".as_ref(), path.as_os_str()],
        Some(data),
    )
    .map(|_| ())
}

/// Read a file with `user`'s permissions.
pub fn read_file_as(user: &UserIdentity, path: &Path) -> Result<Vec<u8>> {
    helper(user, &["__read-file".as_ref(), path.as_os_str()], None)
}

/// Copy the directory tree `source` (readable by root) into the new
/// directory `target` with `user`'s identity: root packs the tree with tar,
/// a tar process running as the user unpacks it. Ownership, ACLs, extended
/// attributes and file capabilities are not transferred.
pub fn copy_tree_as(user: &UserIdentity, source: &Path, target: &Path) -> Result<()> {
    let mkdir = {
        let mut cmd = Command::new("mkdir");
        cmd.arg("--")
            .arg(target)
            .stdin(Stdio::null())
            .stderr(Stdio::piped());
        user.apply(&mut cmd);
        cmd.output().context("cannot run mkdir")?
    };
    if !mkdir.status.success() {
        bail!(
            "cannot create {}: {}",
            target.display(),
            String::from_utf8_lossy(&mkdir.stderr).trim()
        );
    }
    let mut pack = Command::new("tar")
        .arg("-C")
        .arg(source)
        .args(["-c", "-f", "-", "--numeric-owner", "."])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot run tar")?;
    let pack_out = pack.stdout.take().context("no tar output")?;
    let mut unpack = Command::new("tar");
    unpack
        .arg("-C")
        .arg(target)
        .args([
            "-x",
            "-f",
            "-",
            "--no-same-owner",
            "--delay-directory-restore",
        ])
        .stdin(Stdio::from(pack_out))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    user.apply(&mut unpack);
    let unpacked = unpack.output().context("cannot run tar as the user")?;
    let packed = pack.wait_with_output()?;
    if !packed.status.success() {
        bail!(
            "packing the restored files failed: {}",
            String::from_utf8_lossy(&packed.stderr).trim()
        );
    }
    if !unpacked.status.success() {
        bail!(
            "copying into {} failed: {}",
            target.display(),
            String::from_utf8_lossy(&unpacked.stderr).trim()
        );
    }
    Ok(())
}
