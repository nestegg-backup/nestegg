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
//!
//! `.cred` files are sealed to the TPM (when there is one) and the host key,
//! but deliberately not to any PCR: a Secure Boot or firmware change must not
//! lock nestegg out of its own key (decision 0008). A credential that cannot
//! be decrypted any more is reported as [`Unreadable`], whose message tells
//! the user how to store it again.

use std::ffi::OsString;
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
/// Secret of the read-only overview token (optional).
pub const OVERVIEW_PASSWORD: &str = "overview-password";

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

/// A stored `.cred` file exists but cannot be decrypted on this computer,
/// typically after a TPM or mainboard change or a restore onto other
/// hardware. The message names the command that stores the secret again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    pub name: String,
    pub path: PathBuf,
    pub detail: String,
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot decrypt {} ({}); it is bound to this computer's TPM and host key. \
             Store it again with: {}",
            self.path.display(),
            self.detail,
            restore_command(&self.name)
        )
    }
}

impl std::error::Error for Unreadable {}

/// The command that stores the secret `name` again.
pub fn restore_command(name: &str) -> &'static str {
    match name {
        ENCRYPTION_KEY => "sudo nestegg key import <exported key file>",
        OVERVIEW_PASSWORD => "sudo nestegg credentials set-password --overview",
        _ => "sudo nestegg credentials set-password",
    }
}

/// The [`Unreadable`] error somewhere in the chain of `e`, if any.
pub fn find_unreadable(e: &anyhow::Error) -> Option<&Unreadable> {
    e.chain().find_map(|c| c.downcast_ref::<Unreadable>())
}

/// Which key `systemd-creds encrypt` seals a credential with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealWith {
    /// systemd's default `auto`: TPM2 and host key when a TPM2 is present,
    /// host key only otherwise. No PCR binding (`--tpm2-pcrs=`).
    Auto,
    /// Host key only (`/var/lib/systemd/credential.secret`), used when a
    /// TPM is present but not usable.
    Host,
}

/// Arguments for `systemd-creds encrypt` that read the secret from stdin
/// and write `target`. Only options that exist since systemd 250, the first
/// release with `systemd-creds`.
pub fn encrypt_args(name: &str, seal: SealWith, target: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["encrypt".into(), format!("--name={name}").into()];
    match seal {
        // An empty list binds to no PCR at all. Without it systemd binds to
        // PCR 7 (Secure Boot state), which changes with firmware updates.
        SealWith::Auto => args.push("--tpm2-pcrs=".into()),
        SealWith::Host => args.push("--with-key=host".into()),
    }
    args.push("-".into());
    args.push(target.as_os_str().to_owned());
    args
}

pub struct SecretStore {
    dir: PathBuf,
    creds_binary: PathBuf,
}

impl SecretStore {
    pub fn new(dir: &Path) -> SecretStore {
        SecretStore {
            dir: dir.to_path_buf(),
            creds_binary: std::env::var_os("NESTEGG_SYSTEMD_CREDS")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("systemd-creds")),
        }
    }

    /// Use another `systemd-creds` binary (tests).
    pub fn with_creds_binary(mut self, binary: &Path) -> SecretStore {
        self.creds_binary = binary.to_path_buf();
        self
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
            let unreadable = |detail: String| Unreadable {
                name: name.to_string(),
                path: cred.clone(),
                detail,
            };
            let output = Command::new(&self.creds_binary)
                .args(["decrypt", &format!("--name={name}")])
                .arg(&cred)
                .arg("-")
                .stdin(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .with_context(|| {
                    format!(
                        "cannot run {} to decrypt {}",
                        self.creds_binary.display(),
                        cred.display()
                    )
                })?;
            if !output.status.success() {
                return Err(unreadable(last_stderr_line(&output.stderr)).into());
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
        let encrypted = self
            .encrypt(name, secret, &cred, SealWith::Auto)
            .or_else(|e| {
                // A TPM that exists but does not work makes `auto` fail; the
                // host key still keeps the secret encrypted at rest.
                self.encrypt(name, secret, &cred, SealWith::Host)
                    .map(|()| {
                        eprintln!(
                            "note: the TPM could not be used ({e:#}); {name} is encrypted with \
                             the host key only"
                        )
                    })
                    .map_err(|_| e)
            });
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

    fn encrypt(&self, name: &str, secret: &Secret, target: &Path, seal: SealWith) -> Result<()> {
        let tmp = target.with_extension("cred.tmp");
        // "Text file busy" is a short-lived condition (the program file is
        // still open for writing somewhere, e.g. during a package upgrade or
        // in tests that have just written a stand-in): wait it out instead
        // of treating it as "this way of sealing does not work".
        let mut attempt = 0;
        let mut child = loop {
            let spawned = Command::new(&self.creds_binary)
                .args(encrypt_args(name, seal, &tmp))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn();
            match spawned {
                Err(e) if e.raw_os_error() == Some(nix::libc::ETXTBSY) && attempt < 20 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                other => break other.context("cannot run systemd-creds")?,
            }
        };
        child
            .stdin
            .take()
            .context("no stdin")?
            .write_all(secret.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            let _ = fs::remove_file(&tmp);
            bail!("{}", last_stderr_line(&output.stderr));
        }
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp, target)?;
        Ok(())
    }
}

fn credentials_directory_path(name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("CREDENTIALS_DIRECTORY")?;
    let path = Path::new(&dir).join(format!("nestegg.{name}"));
    path.exists().then_some(path)
}

/// The last non-empty line of a tool's stderr: systemd-creds prints TPM
/// library noise first and its own summary last.
fn last_stderr_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("systemd-creds failed without a message")
        .to_string()
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

fn random_suffix() -> String {
    format!("{:016x}", rand::random::<u64>())
}

/// Create `path` exclusively (`O_CREAT|O_EXCL|O_NOFOLLOW`, mode 0600) and
/// write `bytes`. Fails if anything exists at `path`, including a symlink.
pub fn create_new_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    // The umask can only remove bits; make the mode explicit on the open fd.
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Atomically replace `path` with a mode-0600 file. The temporary file has a
/// random name and is created exclusively, so a pre-placed file or symlink
/// cannot redirect the write. Meant for directories controlled by the caller
/// (credentials, state, runtime directories).
pub fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("path without parent")?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid file name")?;
    let tmp = dir.join(format!(".{name}.{}.tmp", random_suffix()));
    if let Err(e) = create_new_private_file(&tmp, bytes) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// SHA-256 of a secret, used to bind "key exported" to one specific key.
pub fn digest(secret: &Secret) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Source descriptors are moved to this number or higher; targets must be
/// below it.
const FIRST_SOURCE_FD: i32 = 64;

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
        if !(3..FIRST_SOURCE_FD).contains(&target_fd) {
            bail!("target descriptor {target_fd} out of range");
        }
        let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
        let mut file = fs::File::from(write);
        file.write_all(secret.as_bytes())?;
        drop(file);
        // Keep the source descriptor above every possible target number, so
        // one dup2 can never overwrite the source of another mapping.
        let high = nix::fcntl::fcntl(
            &read,
            nix::fcntl::FcntlArg::F_DUPFD_CLOEXEC(FIRST_SOURCE_FD),
        )?;
        drop(read);
        // SAFETY: `high` is a freshly duplicated descriptor owned by nobody else.
        let read = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(high) };
        self.entries.push((read, target_fd));
        Ok(target_fd)
    }

    /// Inherit a pinned backup root above all fixed secret descriptor targets.
    pub fn inherit_directory(&mut self, directory: &fs::File) -> Result<i32> {
        let high = nix::fcntl::fcntl(
            directory,
            nix::fcntl::FcntlArg::F_DUPFD_CLOEXEC(FIRST_SOURCE_FD),
        )?;
        // SAFETY: fcntl returned a new descriptor owned by this value.
        let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(high) };
        self.entries.push((fd, high));
        Ok(high)
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
    fn fd_mapping_does_not_clobber_sources() {
        // Occupy low descriptors so pipes would naturally land on 10/11.
        let mut fds = FdPassing::new();
        fds.add(&Secret::new(b"PASSWORD".to_vec()), 10).unwrap();
        fds.add(&Secret::new(b"KEY".to_vec()), 11).unwrap();
        for (src, _) in &fds.entries {
            assert!(src.as_raw_fd() >= FIRST_SOURCE_FD);
        }
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg("echo $(cat <&10) $(cat <&11)");
        fds.apply(&mut cmd);
        let out = cmd.output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "PASSWORD KEY");
    }

    #[test]
    fn create_new_refuses_existing_files_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "keep").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(create_new_private_file(&link, b"x").is_err());
        assert!(create_new_private_file(&victim, b"x").is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        let fresh = dir.path().join("fresh");
        create_new_private_file(&fresh, b"x").unwrap();
        assert_eq!(fs::metadata(&fresh).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn atomic_write_ignores_planted_tmp_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "keep").unwrap();
        // The old implementation wrote through `<name>.tmp`.
        std::os::unix::fs::symlink(&victim, dir.path().join("secret.tmp")).unwrap();
        write_private_file(&dir.path().join("secret"), b"new").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(fs::read(dir.path().join("secret")).unwrap(), b"new");
        assert_eq!(
            fs::metadata(dir.path().join("secret")).unwrap().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn digest_is_stable() {
        let a = digest(&Secret::new(b"k".to_vec()));
        assert_eq!(a.len(), 64);
        assert_eq!(a, digest(&Secret::new(b"k".to_vec())));
        assert_ne!(a, digest(&Secret::new(b"l".to_vec())));
    }

    #[test]
    fn missing_secret_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::new(dir.path());
        let err = store.load(ENCRYPTION_KEY).unwrap_err().to_string();
        assert!(err.contains("not set up"));
    }

    /// Fake `systemd-creds`: logs its arguments next to itself, "seals" by
    /// prefixing `SEALED:`, fails the TPM path when `tpm-broken` exists and
    /// fails decryption like a changed TPM for content `FOREIGN`.
    const FAKE_CREDS: &str = r#"#!/bin/bash
here=$(dirname "$0")
echo "$*" >> "$here/args.log"
case "$1" in
  encrypt)
    if [ -e "$here/tpm-broken" ] && [ "$3" = "--tpm2-pcrs=" ]; then
      echo "ERROR:tcti:Failed to open specified TCTI device file /dev/tpmrm0" >&2
      echo "Failed to create TPM2 context: State not recoverable" >&2
      exit 1
    fi
    { printf 'SEALED:'; cat; } > "${@: -1}"
    ;;
  decrypt)
    content=$(cat "$3")
    if [ "$content" = "FOREIGN" ]; then
      echo "WARNING:esys:Esys_Load_Finish() Received TPM Error" >&2
      echo "Failed to unseal secret using TPM2: State not recoverable" >&2
      exit 1
    fi
    printf '%s' "${content#SEALED:}"
    ;;
esac
"#;

    fn fake_creds(dir: &Path) -> PathBuf {
        let path = dir.join("systemd-creds");
        fs::write(&path, FAKE_CREDS).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn encrypt_args_bind_to_no_pcr() {
        let target = Path::new("/etc/nestegg/credentials/password.cred.tmp");
        let auto: Vec<String> = encrypt_args(PASSWORD, SealWith::Auto, target)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        assert_eq!(
            auto,
            [
                "encrypt",
                "--name=password",
                "--tpm2-pcrs=",
                "-",
                "/etc/nestegg/credentials/password.cred.tmp"
            ]
        );
        let host = encrypt_args(ENCRYPTION_KEY, SealWith::Host, target);
        assert!(host.iter().any(|a| a == "--with-key=host"));
        assert!(!host.iter().any(|a| a == "--tpm2-pcrs="));
        // Never an option newer than systemd 250.
        for a in auto.iter().chain(["--with-key=host".to_string()].iter()) {
            assert!(!a.starts_with("--tpm2-public-key"), "{a}");
        }
    }

    #[test]
    fn store_uses_systemd_creds_without_pcrs() {
        let dir = tempfile::tempdir().unwrap();
        let creds = fake_creds(dir.path());
        let store = SecretStore::new(&dir.path().join("cred")).with_creds_binary(&creds);
        let storage = store
            .store(PASSWORD, &Secret::new(b"tok".to_vec()), false)
            .unwrap();
        assert_eq!(storage, Storage::SystemdCreds);
        let log = fs::read_to_string(dir.path().join("args.log")).unwrap();
        assert!(
            log.starts_with("encrypt --name=password --tpm2-pcrs= - "),
            "{log}"
        );
        let cred = dir.path().join("cred/password.cred");
        assert_eq!(fs::metadata(&cred).unwrap().mode() & 0o777, 0o600);
        assert_eq!(store.load(PASSWORD).unwrap().as_bytes(), b"tok");
    }

    #[test]
    fn a_busy_program_file_is_waited_out() {
        // While the program file is open for writing, starting it fails with
        // "Text file busy" (as during a package upgrade). That is waited out;
        // it must not count as "systemd-creds does not work" and lead to the
        // fallback or to an error.
        let dir = tempfile::tempdir().unwrap();
        let creds = fake_creds(dir.path());
        let writer = fs::OpenOptions::new().append(true).open(&creds).unwrap();
        // Really busy now: a start attempt fails with that error.
        let busy = Command::new(&creds).arg("--version").output().unwrap_err();
        assert_eq!(busy.raw_os_error(), Some(nix::libc::ETXTBSY), "{busy}");
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            drop(writer);
        });
        let store = SecretStore::new(&dir.path().join("cred")).with_creds_binary(&creds);
        let storage = store
            .store(PASSWORD, &Secret::new(b"tok".to_vec()), false)
            .unwrap();
        release.join().unwrap();
        assert_eq!(storage, Storage::SystemdCreds);
        // The first way of sealing worked: no fallback to the host key.
        let log = fs::read_to_string(dir.path().join("args.log")).unwrap();
        assert!(!log.contains("--with-key=host"), "{log}");
        assert_eq!(store.load(PASSWORD).unwrap().as_bytes(), b"tok");
    }

    #[test]
    fn store_falls_back_to_host_key_when_the_tpm_fails() {
        let dir = tempfile::tempdir().unwrap();
        let creds = fake_creds(dir.path());
        fs::write(dir.path().join("tpm-broken"), "").unwrap();
        let store = SecretStore::new(&dir.path().join("cred")).with_creds_binary(&creds);
        let storage = store
            .store(ENCRYPTION_KEY, &Secret::new(b"{}".to_vec()), false)
            .unwrap();
        assert_eq!(storage, Storage::SystemdCreds);
        let log = fs::read_to_string(dir.path().join("args.log")).unwrap();
        assert!(log.contains("--with-key=host"), "{log}");
        assert!(!dir.path().join("cred/encryption-key.cred.tmp").exists());
        assert_eq!(store.load(ENCRYPTION_KEY).unwrap().as_bytes(), b"{}");
    }

    #[test]
    fn store_without_systemd_creds_needs_allow_plain() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::new(&dir.path().join("cred"))
            .with_creds_binary(&dir.path().join("does-not-exist"));
        let secret = Secret::new(b"tok".to_vec());
        assert!(store.store(PASSWORD, &secret, false).is_err());
        assert_eq!(
            store.store(PASSWORD, &secret, true).unwrap(),
            Storage::PlainFile
        );
        assert_eq!(store.load(PASSWORD).unwrap().as_bytes(), b"tok");
    }

    #[test]
    fn undecryptable_credential_is_unreadable_with_a_fix() {
        let dir = tempfile::tempdir().unwrap();
        let creds = fake_creds(dir.path());
        let cred_dir = dir.path().join("cred");
        create_private_dir(&cred_dir).unwrap();
        write_private_file(&cred_dir.join("encryption-key.cred"), b"FOREIGN").unwrap();
        write_private_file(&cred_dir.join("password.cred"), b"FOREIGN").unwrap();
        let store = SecretStore::new(&cred_dir).with_creds_binary(&creds);

        let err = store.load(ENCRYPTION_KEY).unwrap_err();
        let unreadable = find_unreadable(&err).expect("typed error");
        assert_eq!(unreadable.name, ENCRYPTION_KEY);
        // Only systemd's own summary line, not the TPM library noise.
        assert_eq!(
            unreadable.detail,
            "Failed to unseal secret using TPM2: State not recoverable"
        );
        let msg = format!("{err:#}");
        assert!(
            msg.contains("sudo nestegg key import <exported key file>"),
            "{msg}"
        );

        let err = store.load(PASSWORD).unwrap_err().context("loading");
        assert!(find_unreadable(&err).is_some(), "found through context");
        assert!(format!("{err:#}").contains("sudo nestegg credentials set-password"));

        // A missing systemd-creds is a setup problem, not an unreadable secret.
        let store = SecretStore::new(&cred_dir).with_creds_binary(&dir.path().join("missing"));
        let err = store.load(PASSWORD).unwrap_err();
        assert!(find_unreadable(&err).is_none());
        assert!(format!("{err:#}").contains("cannot run"), "{err:#}");

        // Other failures are not.
        let empty = SecretStore::new(dir.path()).with_creds_binary(&creds);
        assert!(find_unreadable(&empty.load(OVERVIEW_PASSWORD).unwrap_err()).is_none());
    }

    #[test]
    fn restore_commands_name_the_right_fix() {
        assert_eq!(
            restore_command(OVERVIEW_PASSWORD),
            "sudo nestegg credentials set-password --overview"
        );
        assert_eq!(
            restore_command(PASSWORD),
            "sudo nestegg credentials set-password"
        );
        assert!(restore_command(ENCRYPTION_KEY).contains("key import"));
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
