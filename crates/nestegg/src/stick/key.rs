//! Password-protected copies are made and opened only by the official client.
use std::fs;
use std::io::{BufRead, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::{Config, Paths};
use crate::secrets::{self, Secret, SecretStore};
use crate::state::Store;
use anyhow::{Context, Result, bail};

pub fn strong_password(password: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(password) else {
        return false;
    };
    if text.chars().any(char::is_control) || text.len() > 512 {
        return false;
    }
    let distinct = text.chars().collect::<std::collections::HashSet<_>>().len();
    let classes = [
        text.chars().any(char::is_lowercase),
        text.chars().any(char::is_uppercase),
        text.chars().any(|c| c.is_ascii_digit()),
        text.chars().any(|c| !c.is_alphanumeric()),
    ]
    .into_iter()
    .filter(|b| *b)
    .count();
    let words: Vec<_> = text.split_whitespace().collect();
    let distinct_words = words
        .iter()
        .map(|w| w.to_lowercase())
        .collect::<std::collections::HashSet<_>>()
        .len();
    distinct >= 8
        && ((text.chars().count() >= 16 && classes >= 3)
            || (text.chars().count() >= 24
                && distinct_words >= 5
                && words.iter().all(|w| w.chars().count() >= 3)))
}

pub fn password(fd: Option<i32>, confirm: bool) -> Result<Secret> {
    if let Some(fd) = fd {
        anyhow::ensure!(fd >= 0, "invalid password descriptor");
        let copy = nix::fcntl::fcntl(
            unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
            nix::fcntl::FcntlArg::F_DUPFD_CLOEXEC(64),
        )?;
        let file = fs::File::from(unsafe { OwnedFd::from_raw_fd(copy) });
        let mut bytes = Vec::new();
        std::io::Read::take(std::io::BufReader::new(file), 514).read_until(b'\n', &mut bytes)?;
        while bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        anyhow::ensure!(bytes.len() <= 512, "password is too long");
        return Ok(Secret::new(bytes));
    }
    use nix::sys::termios::{self, LocalFlags, SetArg};
    let mut tty = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("a terminal or --password-fd is required")?;
    let original = termios::tcgetattr(&tty)?;
    let mut hidden = original.clone();
    hidden.local_flags.remove(LocalFlags::ECHO);
    struct Restore<'a>(&'a fs::File, termios::Termios);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            let _ = termios::tcsetattr(self.0, SetArg::TCSANOW, &self.1);
        }
    }
    let restore_file = tty.try_clone()?;
    let _restore = Restore(&restore_file, original);
    termios::tcsetattr(&tty, SetArg::TCSANOW, &hidden)?;
    let mut read = |prompt: &str| -> Result<Secret> {
        tty.write_all(prompt.as_bytes())?;
        tty.flush()?;
        let mut bytes = Vec::new();
        std::io::Read::take(std::io::BufReader::new(&tty), 514).read_until(b'\n', &mut bytes)?;
        tty.write_all(b"\n")?;
        anyhow::ensure!(
            bytes.last() == Some(&b'\n') && bytes.len() <= 513,
            "password is too long"
        );
        bytes.pop();
        Ok(Secret::new(bytes))
    };
    let secret = read("Stick key password: ")?;
    if confirm {
        let again = read("Repeat password: ")?;
        anyhow::ensure!(
            secret.as_bytes() == again.as_bytes(),
            "passwords do not match"
        );
    }
    Ok(secret)
}

fn isolated_python() -> Command {
    let mut cmd = Command::new("/usr/bin/python3");
    cmd.arg("-I").current_dir("/");
    // -I also ignores these for Python itself. Remove them so the official
    // client (and any helpers it starts) cannot inherit Python injection.
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"PYTHON") {
            cmd.env_remove(name);
        }
    }
    cmd
}

fn change(path: &Path, password: &Secret, kdf: &str) -> Result<()> {
    let mut cmd = isolated_python();
    cmd.args(["-c", include_str!("key_pty.py")])
        .arg(crate::client::client_binary())
        .arg(path)
        .arg(kdf);
    for name in [
        "PBS_ENCRYPTION_PASSWORD",
        "PBS_ENCRYPTION_PASSWORD_FD",
        "PBS_ENCRYPTION_PASSWORD_FILE",
        "PBS_ENCRYPTION_PASSWORD_CMD",
    ] {
        cmd.env_remove(name);
    }
    let mut fds = secrets::FdPassing::new();
    fds.add(password, 10)?;
    fds.apply(&mut cmd);
    let status = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    anyhow::ensure!(
        status.success(),
        "official client could not change/open the key with this password"
    );
    Ok(())
}

fn read_protected(path: &Path) -> Result<Secret> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    anyhow::ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= crate::restore::MAX_KEY_FILE_BYTES,
        "stick key must be a bounded regular file"
    );
    let mut bytes = Vec::new();
    file.take(crate::restore::MAX_KEY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= crate::restore::MAX_KEY_FILE_BYTES,
        "stick key is too large"
    );
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        value["kdf"].get("Scrypt").is_some() || value["kdf"].get("scrypt").is_some(),
        "stick key copy is not protected with scrypt"
    );
    Ok(Secret::new(bytes))
}

/// Open only a scrypt-protected copy. Plaintext exists in private RAM scratch
/// and is removed on every return path, including a wrong password.
pub fn open_copy(path: &Path, password: &Secret, scratch: &Path) -> Result<Secret> {
    secrets::create_private_dir(scratch)?;
    let work = tempfile::Builder::new()
        .prefix("open-key-")
        .tempdir_in(scratch)?;
    let plain = work.path().join("key.json");
    let encrypted = read_protected(path)?;
    secrets::write_private_file(&plain, encrypted.as_bytes())?;
    change(&plain, password, "none")?;
    let key = Secret::new(fs::read(&plain)?);
    crate::keyproof::KeyMaterial::from_copy(key.as_bytes())?;
    Ok(key)
}

/// Create a protected recovery copy and independently reopen the bytes from
/// the medium. Existing copies must be opened and match before replacement.
pub fn save_copy(path: &Path, key: &Secret, password: &Secret, scratch: &Path) -> Result<()> {
    anyhow::ensure!(
        strong_password(password.as_bytes()),
        "use 16 characters from three classes or five distinct words totalling 24 characters"
    );
    if fs::symlink_metadata(path).is_ok() {
        let previous = open_copy(path, password, scratch)?;
        anyhow::ensure!(
            crate::keyproof::KeyMaterial::from_copy(previous.as_bytes())?
                == crate::keyproof::KeyMaterial::from_copy(key.as_bytes())?,
            "existing recovery copy belongs to another key"
        );
    }
    secrets::create_private_dir(scratch)?;
    let work = tempfile::Builder::new()
        .prefix("protect-key-")
        .tempdir_in(scratch)?;
    let plain = work.path().join("key.json");
    secrets::write_private_file(&plain, key.as_bytes())?;
    change(&plain, password, "scrypt")?;
    let protected = read_protected(&plain)?;
    crate::settings::write_file_atomic(path, std::str::from_utf8(protected.as_bytes())?, 0o600)?;
    let reopened = open_copy(path, password, scratch)?;
    anyhow::ensure!(
        crate::keyproof::KeyMaterial::from_copy(reopened.as_bytes())?
            == crate::keyproof::KeyMaterial::from_copy(key.as_bytes())?,
        "recovery copy read-back differs"
    );
    Ok(())
}

pub fn run(paths: &Paths, uuid: &str, check_only: bool, password_fd: Option<i32>) -> Result<u8> {
    anyhow::ensure!(
        nix::unistd::Uid::effective().is_root(),
        "stick key commands require root"
    );
    let config = Config::load(&paths.config_file)?;
    let password = password(password_fd, !check_only)?;
    if !check_only && !strong_password(password.as_bytes()) {
        bail!(
            "use at least 16 characters from three character classes, or five distinct words totalling at least 24 characters"
        );
    }
    let _lock = crate::run::RunLock::try_acquire(paths)?
        .context("a backup is running; try again afterwards")?;
    let uuid =
        super::target::attached(&config, Some(uuid))?.context("registered stick not plugged in")?;
    super::target::isolate_mounts()?;
    let session =
        super::target::Session::open(paths, &uuid).map_err(|e| anyhow::anyhow!("{}", e.message))?;
    let key = SecretStore::new(&paths.credentials_dir).load(secrets::ENCRYPTION_KEY)?;
    secrets::create_private_dir(&paths.tmp_dir())?;
    let work = tempfile::Builder::new()
        .prefix("stick-key-")
        .tempdir_in(paths.tmp_dir())?;
    let plain = work.path().join("key.json");
    let keys = session.mount.join("nestegg-keys");
    if !keys.exists() {
        fs::create_dir(&keys)?;
    }
    let meta = fs::symlink_metadata(&keys)?;
    anyhow::ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "stick key directory must be a real directory"
    );
    let copy = keys.join(format!("{}.json", config.effective_backup_id()));
    if !check_only && copy.exists() {
        let previous = read_protected(&copy)?;
        secrets::write_private_file(&plain, previous.as_bytes())?;
        change(&plain, &password, "none")?;
        let opened = Secret::new(fs::read(&plain)?);
        anyhow::ensure!(
            crate::keyproof::KeyMaterial::from_copy(opened.as_bytes())?
                == crate::keyproof::KeyMaterial::from_copy(key.as_bytes())?,
            "this stick key copy belongs to another key; choose a distinct backup ID instead of overwriting its recovery key"
        );
    }
    if !check_only {
        secrets::write_private_file(&plain, key.as_bytes())?;
        change(&plain, &password, "scrypt")?;
        let encrypted = Secret::new(fs::read(&plain)?);
        let value: serde_json::Value = serde_json::from_slice(encrypted.as_bytes())?;
        anyhow::ensure!(
            value["kdf"].get("Scrypt").is_some() || value["kdf"].get("scrypt").is_some(),
            "official client did not create a scrypt key"
        );
        crate::settings::write_file_atomic(
            &copy,
            std::str::from_utf8(encrypted.as_bytes())?,
            0o600,
        )?;
    }
    // Read the actual medium copy back and open it independently. A digest of
    // the encrypted envelope cannot prove that the password recovers the key.
    let encrypted = read_protected(&copy)?;
    secrets::write_private_file(&plain, encrypted.as_bytes())?;
    change(&plain, &password, "none")?;
    let opened = Secret::new(fs::read(&plain)?);
    let original = crate::keyproof::KeyMaterial::from_copy(key.as_bytes())?;
    anyhow::ensure!(
        crate::keyproof::KeyMaterial::from_copy(opened.as_bytes())? == original,
        "stick copy contains another encryption key"
    );
    session.close()?;
    let store = Store::new(paths);
    let mut state = store.load_state()?;
    let proof = state.stick_media.entry(uuid).or_default();
    proof.key_digest = Some(secrets::digest(&key));
    proof.password_checked_at = Some(chrono::Utc::now());
    store.save_state(&state)?;
    store.clear_failures()?;
    println!(
        "Stick key copy opened and matched; unplug the stick and keep it safely away from this computer."
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protected_copy_refuses_plain_keys_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copy.json");
        fs::write(&path, br#"{"kdf":"none","data":"fixture"}"#).unwrap();
        assert!(read_protected(&path).is_err());
        fs::write(&path, br#"{"kdf":{"Scrypt":{}},"data":"fixture"}"#).unwrap();
        assert!(read_protected(&path).is_ok());
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_protected(&link).is_err());
        assert!(read_protected(dir.path()).is_err());
    }
    #[test]
    fn python_imports_ignore_untrusted_directory_and_environment() {
        let dir = tempfile::tempdir().unwrap();
        for module in ["pty", "signal", "termios"] {
            fs::write(
                dir.path().join(format!("{module}.py")),
                "raise RuntimeError('injected')",
            )
            .unwrap();
        }
        let mut cmd = isolated_python();
        assert_eq!(cmd.get_program(), "/usr/bin/python3");
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/")));
        let output = cmd
            .current_dir(dir.path())
            .env("PYTHONPATH", dir.path())
            .env("PYTHONHOME", dir.path())
            .args(["-c", "import pty, signal, termios; print('isolated')"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"isolated\n");
    }

    #[test]
    fn password_policy() {
        for weak in [
            "",
            "password",
            "aaaaaaaaaaaaaaaaaaaaaaaa",
            "Password123!",
            "word word word word word word",
        ] {
            assert!(!strong_password(weak.as_bytes()));
        }
        assert!(strong_password(b"Orchid!72-Copper#41"));
        assert!(strong_password(b"orchard river copper lantern meadow"));
        assert!(!strong_password(b"Orchid!72-Copper#41\n"));
    }
}
