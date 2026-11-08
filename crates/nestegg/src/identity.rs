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
        let Ok(raw) = std::env::var("SUDO_UID") else {
            return Ok(None);
        };
        let uid: u32 = raw
            .trim()
            .parse()
            .with_context(|| format!("SUDO_UID '{raw}' is not a valid user id"))?;
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
        // SAFETY: no allocation between fork and exec; only plain system
        // calls (setgroups is not on the POSIX async-signal-safe list but is
        // a direct system call wrapper in glibc and musl on Linux).
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

fn run_as(user: &UserIdentity, program: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    user.apply(&mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("cannot run {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Copy the directory tree `source` (readable by root) to the new path
/// `target` with `user`'s identity. Root packs the tree with tar, a tar
/// process running as the user unpacks it into a temporary sibling
/// directory, which is renamed to `target` only after everything succeeded
/// (never replacing an existing path). On failure the partial copy is
/// removed with the user's identity. Ownership, ACLs, extended attributes
/// and file capabilities are not transferred; sparse files stay sparse.
/// Returns tar's warnings (e.g. skipped sockets).
pub fn copy_tree_as(user: &UserIdentity, source: &Path, target: &Path) -> Result<Vec<String>> {
    let parent = target.parent().context("restore target has no parent")?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid restore target name")?;
    let partial = parent.join(format!(
        ".{name}.nestegg-partial-{:08x}",
        rand::random::<u32>()
    ));
    run_as(
        user,
        "mkdir",
        &[
            "-m".as_ref(),
            "0700".as_ref(),
            "--".as_ref(),
            partial.as_os_str(),
        ],
    )?;

    let result = (|| -> Result<Vec<String>> {
        let mut pack = Command::new("tar")
            .arg("-C")
            .arg(source)
            .args(["-c", "-f", "-", "--sparse", "--numeric-owner", "."])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("cannot run tar")?;
        let pack_out = pack.stdout.take().context("no tar output")?;
        let pack_err = pack.stderr.take().context("no tar stderr")?;
        // Drain the packer's stderr concurrently; otherwise a full pipe can
        // block both tar processes.
        let err_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut std::io::BufReader::new(pack_err), &mut buf);
            buf
        });
        let mut unpack = Command::new("tar");
        unpack
            .arg("-C")
            .arg(&partial)
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
        let unpacked = match unpack.output() {
            Ok(o) => o,
            Err(e) => {
                let _ = pack.kill();
                let _ = pack.wait();
                let _ = err_reader.join();
                return Err(e).context("cannot run tar as the user");
            }
        };
        if !unpacked.status.success() {
            let _ = pack.kill();
        }
        let pack_status = pack.wait()?;
        let pack_stderr = err_reader.join().unwrap_or_default();
        if !unpacked.status.success() {
            bail!(
                "copying the restored files failed: {}",
                String::from_utf8_lossy(&unpacked.stderr).trim()
            );
        }
        if !pack_status.success() {
            bail!(
                "packing the restored files failed: {}",
                String::from_utf8_lossy(&pack_stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&pack_stderr)
            .lines()
            .chain(String::from_utf8_lossy(&unpacked.stderr).lines())
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    })();

    let result = result.and_then(|warnings| {
        helper(
            user,
            &[
                "__rename-noreplace".as_ref(),
                partial.as_os_str(),
                target.as_os_str(),
            ],
            None,
        )?;
        Ok(warnings)
    });
    if result.is_err() {
        if let Err(e) = run_as(
            user,
            "rm",
            &[
                "-rf".as_ref(),
                "--one-file-system".as_ref(),
                "--".as_ref(),
                partial.as_os_str(),
            ],
        ) {
            eprintln!(
                "warning: could not remove partial copy {}: {e:#}",
                partial.display()
            );
        }
    }
    result
}
