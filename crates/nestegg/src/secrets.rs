//! Secrets: the PBS password / API token secret and the encryption key.
//!
//! Lookup order for a secret named `<name>`:
//! 1. `$CREDENTIALS_DIRECTORY/nestegg.<name>` (systemd service credentials),
//! 2. `<credentials_dir>/<name>.cred`, decrypted with `systemd-creds`,
//! 3. `<credentials_dir>/<name>`, a plain file readable by root only.
//!
//! Secrets are handed to `proxmox-backup-client` through inherited pipe file
//! descriptors (`PBS_PASSWORD_FD`, `--keyfd`), never through arguments,
//! environment values or temporary files.

use std::fs;
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use nix::fcntl::OFlag;

pub const PASSWORD: &str = "password";
pub const ENCRYPTION_KEY: &str = "encryption-key";

/// Secret bytes that are wiped from memory when dropped.
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: Vec<u8>) -> Secret {
        Secret(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            // Volatile write so the compiler does not optimise the wipe away.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    /// Encrypted with `systemd-creds` (bound to the TPM if available).
    SystemdCreds,
    /// Plain file, mode 0600, owned by root.
    PlainFile,
}

pub struct SecretStore {
    dir: PathBuf,
}

impl SecretStore {
    pub fn new(dir: &Path) -> SecretStore {
        SecretStore {
            dir: dir.to_path_buf(),
        }
    }

    pub fn exists(&self, name: &str) -> bool {
        credentials_directory_path(name).is_some()
            || self.dir.join(format!("{name}.cred")).exists()
            || self.dir.join(name).exists()
    }

    pub fn load(&self, name: &str) -> Result<Secret> {
        if let Some(path) = credentials_directory_path(name) {
            return Ok(Secret(
                fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?,
            ));
        }
        let cred = self.dir.join(format!("{name}.cred"));
        if cred.exists() {
            let output = Command::new("systemd-creds")
                .args(["decrypt", &format!("--name={name}")])
                .arg(&cred)
                .arg("-")
                .stdin(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .context("cannot run systemd-creds")?;
            if !output.status.success() {
                bail!(
                    "systemd-creds could not decrypt {}: {}",
                    cred.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            return Ok(Secret(output.stdout));
        }
        let plain = self.dir.join(name);
        if plain.exists() {
            check_private(&plain)?;
            return Ok(Secret(
                fs::read(&plain).with_context(|| format!("cannot read {}", plain.display()))?,
            ));
        }
        bail!(
            "secret '{name}' is not set up (expected {} or {})",
            cred.display(),
            plain.display()
        )
    }

    /// Store a secret, preferring `systemd-creds`. Removes the other variant.
    pub fn store(&self, name: &str, secret: &Secret, allow_plain: bool) -> Result<Storage> {
        create_private_dir(&self.dir)?;
        let cred = self.dir.join(format!("{name}.cred"));
        let plain = self.dir.join(name);
        let encrypted = encrypt_with_systemd_creds(name, secret, &cred);
        match encrypted {
            Ok(()) => {
                let _ = fs::remove_file(&plain);
                Ok(Storage::SystemdCreds)
            }
            Err(e) if allow_plain => {
                eprintln!(
                    "note: systemd-creds unavailable ({e:#}); storing {name} as a root-only file"
                );
                write_private_file(&plain, secret.as_bytes())?;
                let _ = fs::remove_file(&cred);
                Ok(Storage::PlainFile)
            }
            Err(e) => Err(e),
        }
    }
}

fn credentials_directory_path(name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("CREDENTIALS_DIRECTORY")?;
    let path = Path::new(&dir).join(format!("nestegg.{name}"));
    path.exists().then_some(path)
}

fn encrypt_with_systemd_creds(name: &str, secret: &Secret, target: &Path) -> Result<()> {
    let tmp = target.with_extension("cred.tmp");
    let mut child = Command::new("systemd-creds")
        .args(["encrypt", &format!("--name={name}"), "-"])
        .arg(&tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot run systemd-creds")?;
    child
        .stdin
        .take()
        .context("no stdin")?
        .write_all(secret.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let _ = fs::remove_file(&tmp);
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, target)?;
    Ok(())
}

fn check_private(path: &Path) -> Result<()> {
    let meta = fs::metadata(path)?;
    if meta.mode() & 0o077 != 0 {
        bail!(
            "{} is accessible by other users (mode {:o}); run: chmod 600 {}",
            path.display(),
            meta.mode() & 0o777,
            path.display()
        );
    }
    let me = nix::unistd::geteuid().as_raw();
    if meta.uid() != me {
        bail!("{} is not owned by the current user", path.display());
    }
    Ok(())
}

pub fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// A pipe whose read end is inherited by a child process at a fixed file
/// descriptor number. The secret is written before the child starts, so it
/// must fit into the pipe buffer (64 KiB on Linux; secrets are tiny).
pub struct FdPassing {
    entries: Vec<(OwnedFd, i32)>,
}

impl FdPassing {
    pub fn new() -> FdPassing {
        FdPassing {
            entries: Vec::new(),
        }
    }

    /// Prepare `secret` to appear as file descriptor `target_fd` in the
    /// child. Returns `target_fd` for use in arguments or environment.
    pub fn add(&mut self, secret: &Secret, target_fd: i32) -> Result<i32> {
        if secret.as_bytes().len() > 60 * 1024 {
            bail!("secret too large for fd passing");
        }
        let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
        let mut file = fs::File::from(write);
        file.write_all(secret.as_bytes())?;
        drop(file);
        self.entries.push((read, target_fd));
        Ok(target_fd)
    }

    /// Install the descriptors in the child via `pre_exec`.
    pub fn apply(&self, cmd: &mut Command) {
        use std::os::unix::process::CommandExt;
        let mapping: Vec<(i32, i32)> = self
            .entries
            .iter()
            .map(|(fd, target)| (fd.as_raw_fd(), *target))
            .collect();
        // SAFETY: only async-signal-safe libc calls (dup2/fcntl) in the child.
        unsafe {
            cmd.pre_exec(move || {
                for (src, target) in &mapping {
                    if nix::libc::dup2(*src, *target) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // dup2 onto the same number keeps CLOEXEC; clear it.
                    let flags = nix::libc::fcntl(*target, nix::libc::F_GETFD);
                    if flags < 0
                        || nix::libc::fcntl(
                            *target,
                            nix::libc::F_SETFD,
                            flags & !nix::libc::FD_CLOEXEC,
                        ) < 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
}

impl Default for FdPassing {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_secret_roundtrip_and_permission_check() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::new(&dir.path().join("cred"));
        assert!(!store.exists(PASSWORD));
        // Force the plain path by writing directly (systemd-creds may be
        // available on the test machine, but needs root).
        create_private_dir(&dir.path().join("cred")).unwrap();
        write_private_file(&dir.path().join("cred").join(PASSWORD), b"s3cret").unwrap();
        assert!(store.exists(PASSWORD));
        assert_eq!(store.load(PASSWORD).unwrap().as_bytes(), b"s3cret");

        fs::set_permissions(
            dir.path().join("cred").join(PASSWORD),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(store.load(PASSWORD).is_err());
    }

    #[test]
    fn missing_secret_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::new(dir.path());
        let err = store.load(ENCRYPTION_KEY).unwrap_err().to_string();
        assert!(err.contains("not set up"));
    }

    #[test]
    fn debug_does_not_leak() {
        let s = Secret::new(b"hunter2".to_vec());
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
    }

    #[test]
    fn fd_passing_reaches_child() {
        let mut fds = FdPassing::new();
        let secret = Secret::new(b"from-a-pipe".to_vec());
        let fd = fds.add(&secret, 7).unwrap();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("cat <&{fd}"));
        fds.apply(&mut cmd);
        let out = cmd.output().unwrap();
        assert_eq!(out.stdout, b"from-a-pipe");
    }
}
