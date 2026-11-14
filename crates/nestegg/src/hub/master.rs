//! The official client owns RSA generation and the passphrase prompt. nestegg
//! sees only encrypted PEM bytes and the public key, never the passphrase.
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{
    config::Config,
    crypto,
    state::{self, Transaction},
};
use anyhow::{Context, Result, ensure};
use clap::Args;

#[derive(Args)]
pub struct Confirm {
    #[arg(long, conflicts_with = "paper_code")]
    file: Option<PathBuf>,
    #[arg(long, conflicts_with = "file")]
    paper_code: Option<String>,
    #[arg(long, requires = "paper_code")]
    printed: bool,
    #[arg(long, requires = "file")]
    accept_local_copy: bool,
}

fn pem_boundaries() -> (String, String) {
    let label = "ENCRYPTED PRIVATE KEY";
    (
        format!("-----BEGIN {label}-----"),
        format!("-----END {label}-----"),
    )
}

fn pem_data(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes)
        .context("master key copy must be UTF-8 PEM or a saved paper key")?;
    let (begin, end) = pem_boundaries();
    let data = text
        .split_once(begin.as_str())
        .and_then(|(_, s)| s.split_once(end.as_str()))
        .context("copy contains no encrypted master private key")?
        .0;
    // The official HTML paper key puts QR <img> tags between PEM lines.
    // Compare only the visible PEM text, never SVG data attributes.
    let mut in_tag = false;
    let data: String = data
        .chars()
        .filter(|c| {
            if *c == '<' {
                in_tag = true;
                return false;
            }
            if *c == '>' {
                in_tag = false;
                return false;
            }
            !in_tag
        })
        .collect();
    let data: String = data
        .lines()
        .map(|line| match line.split_once(':') {
            Some((number, rest)) if number.trim().bytes().all(|b| b.is_ascii_digit()) => rest,
            _ => line,
        })
        .flat_map(str::chars)
        .filter(|c| !c.is_whitespace())
        .collect();
    ensure!(
        !data.is_empty()
            && data
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)),
        "invalid master private key PEM"
    );
    Ok(data)
}

pub fn create(config: &Config, paper: &Path) -> Result<()> {
    let dir = config.state_dir.join("master");
    crate::secrets::create_private_dir(&dir)?;
    let mut t = Transaction::open(&config.state_dir)?;
    ensure!(
        t.state.master.proven_at.is_none(),
        "master key already proven; refusing to replace it"
    );
    let paper = std::fs::canonicalize(
        paper
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .join(paper.file_name().context("paper key filename")?);
    ensure!(paper.to_str().is_some(), "paper key path must be UTF-8");
    let resumable_paper = t.state.master.private_digest.is_some()
        && t.state.master.paper_file.as_ref() == Some(&paper);
    t.state.master.paper_file = Some(paper.clone());
    t.save()?;
    let private = dir.join("master-private.pem");
    let public = dir.join("master-public.pem");
    if private.exists() || public.exists() {
        ensure!(
            t.state.master.generation_started,
            "master key files already exist but are not recorded as this hub's setup; refusing to replace or adopt them"
        );
        if private.exists() != public.exists() && t.state.master.private_digest.is_none() {
            // No paper copy or public identity can have been handed out yet:
            // both are recorded only after the official client completed.
            for file in [&private, &public] {
                if file.exists() {
                    ensure!(
                        std::fs::symlink_metadata(file)?.is_file(),
                        "incomplete master key path is not a regular file"
                    );
                    std::fs::remove_file(file)?;
                }
            }
        }
    }
    if !private.exists() && !public.exists() {
        t.state.master.generation_started = true;
        t.save()?;
        // Direct inherited controlling terminal. No capturing, proxying or
        // programmatic reading of the administrator's master passphrase.
        let status = Command::new(crate::client::client_binary())
            .args(["key", "create-master-key"])
            .current_dir(&dir)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("run official master-key creation")?;
        ensure!(
            status.success(),
            "master-key creation failed; no key was proven"
        );
    }
    ensure!(
        private.is_file() && public.is_file(),
        "incomplete master-key creation; recover the existing key before continuing"
    );
    let encrypted = crate::secrets::Secret::new(crate::restore::read_small_regular_file(
        &private,
        1024 * 1024,
    )?);
    let data = pem_data(encrypted.as_bytes())?;
    let public_pem = std::fs::read_to_string(&public)?;
    let fingerprint = super::tls::hex_fingerprint(&super::tls::public_der(public_pem.as_bytes())?);
    let check = crate::keyproof::check_code(&data);
    let header = crate::keyproof::PaperHeader {
        backup_id: "household master key".into(),
        hostname: config.name.clone(),
        date: state::now(),
        server: format!("{}:{}", config.pbs.server, config.pbs.port),
        datastore: config.pbs.datastore.clone(),
        fingerprint: fingerprint.clone(),
        check_code: check.clone(),
    };
    let rendered = crate::client::local::paperkey(&private, "html")?;
    let rendered = crate::keyproof::compose_html(&header, &rendered)?;
    let mut rendered = String::from_utf8(rendered)?;
    // Reuse the established escaped header/check-code format, but RSA master
    // recovery differs from restoring with a computer's symmetric key.
    let start = rendered.find("<ol>").context("paper header instructions")?;
    let end = start
        + rendered[start..]
            .find("</ol>")
            .context("paper header instructions")?
        + 5;
    rendered.replace_range(start..end, "<ol><li>Save the encrypted PEM block below as master-private.pem. Keep its passphrase separately.</li><li>Use nestegg key recover with this master key to recover a computer key from a snapshot; see nestegg key recover --help.</li><li>Print and store this sheet away from the hub, then delete the local paper file. Prove it with nestegg hub master-key confirm.</li></ol>");
    let rendered = rendered.into_bytes();
    // Persist the pending identity before writing the copy so an interruption
    // can be resumed without generating a different household key.
    t.state.master.paper_written = false;
    t.state.master.public_key_pem = Some(public_pem);
    t.state.master.fingerprint = Some(fingerprint);
    t.state.master.private_digest = Some(crypto::hash(&data));
    t.state.master.paper_code_digest = Some(crypto::hash(&check));
    t.state.master.paper_file = Some(paper.clone());
    t.save()?;
    if !resumable_paper || !paper.exists() {
        crate::secrets::create_new_private_file(&paper, &rendered)?;
    }
    let back = crate::restore::read_small_regular_file(&paper, 4 * 1024 * 1024)?;
    ensure!(
        crypto::matches(
            &pem_data(&back)?,
            t.state.master.private_digest.as_deref().unwrap_or("")
        ),
        "paper key read-back failed"
    );
    t.state.master.paper_written = true;
    t.save()?;
    println!(
        "Master paper key written and read back. Print it, then delete this file or move it to removable media. Confirm with `nestegg hub master-key confirm` before creating join codes."
    );
    Ok(())
}

fn prompt(text: &str) -> Result<String> {
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("proof needs --file or --paper-code without a terminal")?;
    write!(tty, "{text}: ")?;
    tty.flush()?;
    let mut line = String::new();
    std::io::BufReader::new(tty).read_line(&mut line)?;
    Ok(line.trim().into())
}

pub fn confirm(config: &Config, args: Confirm) -> Result<()> {
    {
        let t = Transaction::open(&config.state_dir)?;
        if t.state.master.public_key_pem.is_none()
            || (!t.state.master.paper_written
                && config.state_dir.join("master/master-private.pem").exists())
        {
            let paper = t.state.master.paper_file.clone();
            drop(t);
            let paper = paper.map(Ok).unwrap_or_else(|| {
                prompt("New file for the master paper key (HTML)").map(PathBuf::from)
            })?;
            create(config, &paper)?;
        }
    }
    let t = Transaction::open(&config.state_dir)?;
    ensure!(
        t.state.master.public_key_pem.is_some(),
        "no master key exists; complete hub init first"
    );
    if t.state.master.proven_at.is_some() {
        println!("Master key copy is already proven.");
        return Ok(());
    }
    let master = t.state.master.clone();
    drop(t);
    let (file, code) = if args.file.is_none() && args.paper_code.is_none() {
        let choice = prompt("Proof: type file or paper")?;
        if choice == "file" {
            (
                Some(PathBuf::from(prompt("Path to the copy outside the hub")?)),
                None,
            )
        } else {
            (None, Some(prompt("Check code from the printed sheet")?))
        }
    } else {
        (args.file, args.paper_code)
    };
    let mut local_copies = Vec::new();
    let mut kept_copies: Vec<PathBuf> = Vec::new();
    let encrypted = if let Some(file) = file {
        let real = std::fs::canonicalize(&file)?;
        ensure!(
            !real.starts_with(std::fs::canonicalize(&config.state_dir)?)
                && !real.starts_with(std::fs::canonicalize(&config.credentials_dir)?),
            "the hub's own stored key is not a copy"
        );
        let bytes = crate::restore::read_small_regular_file(&real, 4 * 1024 * 1024)?;
        ensure!(
            crypto::matches(
                &pem_data(&bytes)?,
                master.private_digest.as_deref().unwrap_or("")
            ),
            "copy does not contain this household's master key"
        );
        let location = crate::keyproof::locate(&real);
        match hub_copy(&real, &location) {
            // On a disk of the hub itself: it has to go, and only after the
            // statement that this is not the only copy.
            HubCopy::OnHub => {
                if !args.accept_local_copy {
                    ensure!(
                        prompt(&format!(
                            "{} {} and will be deleted after the proof. Type yes to state that another copy exists outside the hub's disk (or use --accept-local-copy)",
                            real.display(),
                            location.describe()
                        ))? == "yes",
                        "copy outside the hub was not confirmed"
                    );
                }
                local_copies.push(real.clone());
            }
            // A network share, an overlay, a container without device
            // information: this may be the only copy. Never delete it.
            HubCopy::Undecidable => {
                if !args.accept_local_copy {
                    ensure!(
                        prompt(&format!(
                            "{} {}. Type yes to state that this copy, or another one, is stored outside the hub's disk (or use --accept-local-copy)",
                            real.display(),
                            location.describe()
                        ))? == "yes",
                        "copy outside the hub was not confirmed"
                    );
                }
                kept_copies.push(real.clone());
            }
            HubCopy::Elsewhere => {}
        }
        bytes
    } else if let Some(code) = code {
        let code = crate::keyproof::normalize_check_code(&code).context("not a check code")?;
        ensure!(
            crypto::matches(&code, master.paper_code_digest.as_deref().unwrap_or("")),
            "check code does not belong to this master key"
        );
        if !args.printed {
            ensure!(
                prompt(
                    "Type yes to state that the sheet is printed and stored away from the hub (or use --printed)"
                )? == "yes",
                "printed copy not confirmed"
            );
        }
        let pending = crate::restore::read_small_regular_file(
            &config.state_dir.join("master/master-private.pem"),
            1024 * 1024,
        )?;
        // The key that is decrypted below must be the one whose public half
        // the computers receive, not whatever lies in that file now.
        ensure!(
            crypto::matches(
                &pem_data(&pending)?,
                master.private_digest.as_deref().unwrap_or("")
            ),
            "the pending private key on the hub is not this household's master key; prove with --file and a copy of the sheet"
        );
        pending
    } else {
        anyhow::bail!("proof required");
    };
    // Decrypt before removing any copy or changing the proof. Only OpenSSL
    // reads the freshly entered passphrase, directly from the terminal.
    decrypt_proof(&encrypted)?;
    if let Some(file) = &master.paper_file
        && file.exists()
    {
        match hub_copy(file, &crate::keyproof::locate(file)) {
            HubCopy::OnHub => local_copies.push(file.clone()),
            HubCopy::Undecidable => kept_copies.push(file.clone()),
            HubCopy::Elsewhere => {}
        }
    }
    // Terminal prompts and decryption never hold the hub state lock.
    let mut t = Transaction::open(&config.state_dir)?;
    ensure!(
        t.state.master.private_digest == master.private_digest
            && t.state.master.proven_at.is_none(),
        "master-key setup changed during proof; repeat the confirmation"
    );
    // Include matching copies in the managed master directory, even if an
    // administrator renamed the pending PEM there while resuming setup.
    for entry in std::fs::read_dir(config.state_dir.join("master"))? {
        let path = entry?.path();
        if path == config.state_dir.join("master/master-private.pem") {
            continue;
        }
        let bytes = crate::restore::read_small_regular_file(&path, 4 * 1024 * 1024).with_context(|| format!("cannot check managed master-key file {}; move it away or delete it before proving the copy; no proof was recorded", path.display()))?;
        if let Ok(data) = pem_data(&bytes)
            && crypto::matches(&data, master.private_digest.as_deref().unwrap_or(""))
        {
            local_copies.push(path);
        }
    }
    for file in local_copies {
        remove_copy(&file)?;
    }
    let private = config.state_dir.join("master/master-private.pem");
    remove_copy(&private)?;
    t.state.master.proven_at = Some(state::now());
    t.state.master.private_digest = None;
    t.state.master.paper_code_digest = None;
    t.save()?;
    println!(
        "Master key copy proven; the private part was removed from the hub. Unplug removable media and keep it away from this machine."
    );
    kept_copies.sort();
    kept_copies.dedup();
    for file in kept_copies {
        println!(
            "Not deleted: {} (nestegg cannot tell whether it lies on the hub's own disk). If it does, move it away or delete it yourself; if it is your copy on a network share, keep it.",
            file.display()
        );
    }
    Ok(())
}

/// What the proof does with a file that holds the private master key.
#[derive(Debug, PartialEq, Eq)]
enum HubCopy {
    /// Certainly on a disk of the hub: must not stay (decision 0034).
    OnHub,
    /// Not decidable (network share, overlay, container without device
    /// information). It may be the administrator's only copy, so it is
    /// never deleted; the administrator is told.
    Undecidable,
    /// Removable medium.
    Elsewhere,
}

fn hub_copy(path: &Path, location: &crate::keyproof::Location) -> HubCopy {
    use crate::keyproof::Location;
    match location {
        Location::Internal | Location::InBackup(_) => HubCopy::OnHub,
        Location::Unknown(_) => hub_copy_by_fs(file_system_type(path).as_deref()),
        Location::Removable => HubCopy::Elsewhere,
    }
}

/// A location without device information is still certainly on this machine
/// when the file system lives in its memory or is its container layer.
/// Everything else (network shares, and disk file systems whose device is
/// hidden, as in a container) stays undecidable.
fn hub_copy_by_fs(fs_type: Option<&str>) -> HubCopy {
    match fs_type {
        Some("tmpfs" | "ramfs" | "devtmpfs" | "overlay") => HubCopy::OnHub,
        _ => HubCopy::Undecidable,
    }
}

fn file_system_type(path: &Path) -> Option<String> {
    let out = Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE", "-T"])
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

fn decrypt_proof(encrypted: &[u8]) -> Result<()> {
    let (begin, end) = pem_boundaries();
    let pem = format!("{begin}\n{}\n{end}\n", pem_data(encrypted)?);
    // An anonymous file leaves no private PEM behind after SIGKILL either.
    // Only its descriptor is inherited; stdin remains the administrator's TTY.
    use std::io::Seek;
    let mut copy = tempfile::tempfile()?;
    copy.write_all(pem.as_bytes())?;
    copy.rewind()?;
    let mut fds = crate::secrets::FdPassing::new();
    let fd = fds.inherit_file(&copy)?;
    let mut command = Command::new("openssl");
    command
        .args(["pkey", "-noout", "-in"])
        .arg(format!("/proc/self/fd/{fd}"))
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    fds.apply(&mut command);
    let status = command
        .status()
        .context("run openssl to verify the master-key passphrase")?;
    ensure!(
        status.success(),
        "master-key decryption failed: wrong passphrase or damaged copy; no proof was recorded and no copy was removed"
    );
    Ok(())
}

fn remove_copy(file: &Path) -> Result<()> {
    match std::fs::remove_file(file) {
        Ok(()) => std::fs::File::open(file.parent().context("key directory")?)?.sync_all()?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(_) => anyhow::bail!(
            "private master-key copy still exists at {}; move it away or delete it, then repeat the proof; no proof was recorded",
            file.display()
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn master_pem_proof_accepts_paper_layout_but_not_other_keys() {
        let (begin, end) = pem_boundaries();
        let raw = format!("{begin}\nYWJj\n{end}\n");
        let paper = format!(
            "<html><p>{begin}</p>\n<p>01: YWJj</p>\n<img src=ignored-qr>\n<p>{end}</p></html>"
        );
        assert_eq!(
            pem_data(raw.as_bytes()).unwrap(),
            pem_data(paper.as_bytes()).unwrap()
        );
        assert!(pem_data(b"not a key").is_err());
    }
    // Run only as the PTY child below. The test harness never reads the
    // password; OpenSSL owns the terminal throughout its prompt.
    #[test]
    fn proof_child() {
        let Ok(path) = std::env::var("NESTEGG_PROOF_CONFIG") else {
            return;
        };
        let config: Config = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let paper = std::env::var("NESTEGG_PROOF_PAPER").ok();
        let args = Confirm {
            file: paper
                .is_none()
                .then(|| config.state_dir.join("../copy.html")),
            paper_code: paper,
            printed: true,
            accept_local_copy: true,
        };
        let result = confirm(&config, args);
        assert!(result.is_ok(), "{}", result.unwrap_err());
    }

    fn terminal_proof(config: &Path, answer: &str, code: Option<&str>) -> bool {
        let script = r#"
import os,pty,select,signal,sys,time
pid,fd=pty.fork()
if pid==0:
 os.execv(sys.argv[1],[sys.argv[1],'--exact','hub::master::tests::proof_child','--nocapture'])
pending=b''
deadline=time.monotonic()+30
try:
 while time.monotonic()<deadline:
  if select.select([fd],[],[],.1)[0]:
   try: part=os.read(fd,65536)
   except OSError: break
   if not part: break
   pending=(pending+part)[-65536:]
   if pending.rstrip().endswith(b':') and b'pass phrase' in pending.lower():
    os.write(fd,sys.argv[2].encode()+b'\n'); pending=b''
  done,status=os.waitpid(pid,os.WNOHANG)
  if done:
   pid=None; sys.exit(0 if os.waitstatus_to_exitcode(status)==0 else 1)
 if pid:
  _,status=os.waitpid(pid,0); pid=None
  sys.exit(0 if os.waitstatus_to_exitcode(status)==0 else 1)
finally:
 os.close(fd)
 if pid:
  os.kill(pid,signal.SIGKILL); os.waitpid(pid,0)
"#;
        let mut command = Command::new("python3");
        command
            .args(["-c", script])
            .arg(std::env::current_exe().unwrap())
            .arg(answer)
            .env("NESTEGG_PROOF_CONFIG", config)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(code) = code {
            command.env("NESTEGG_PROOF_PAPER", code);
        }
        command.status().unwrap().success()
    }

    #[test]
    fn real_encrypted_key_requires_fresh_correct_passphrase_and_removes_local_copies() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let master = root.join("state/master");
        std::fs::create_dir_all(&master).unwrap();
        let private = master.join("master-private.pem");
        let mut generate = Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
                "-aes-256-cbc",
                "-pass",
                "stdin",
                "-out",
            ])
            .arg(&private)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        generate
            .stdin
            .take()
            .unwrap()
            .write_all(b"disposable-proof-passphrase\n")
            .unwrap();
        assert!(generate.wait().unwrap().success());
        let mut public = Command::new("openssl")
            .args(["pkey", "-in"])
            .arg(&private)
            .args(["-passin", "stdin", "-pubout", "-out"])
            .arg(master.join("master-public.pem"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        public
            .stdin
            .take()
            .unwrap()
            .write_all(b"disposable-proof-passphrase\n")
            .unwrap();
        assert!(public.wait().unwrap().success());
        let public_pem = std::fs::read_to_string(master.join("master-public.pem")).unwrap();
        let bytes = std::fs::read(&private).unwrap();
        let data = pem_data(&bytes).unwrap();
        let config = Config {
            name: "Home".into(),
            url: "https://hub.example.com:8443".into(),
            listen: "127.0.0.1:8443".into(),
            state_dir: root.join("state"),
            credentials_dir: root.join("credentials"),
            certificate: root.join("tls.crt"),
            private_key: root.join("tls.key"),
            pbs: super::super::config::Pbs {
                server: "pbs.example.com".into(),
                port: 8007,
                datastore: "backups".into(),
                namespace: None,
                user: "backup@pbs".into(),
                fingerprint: None,
            },
            wake: None,
        };
        std::fs::create_dir(&config.credentials_dir).unwrap();
        let config_path = root.join("hub.toml");
        std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
        let paper = root.join("paper.html");
        let copy = root.join("copy.html");
        let code = crate::keyproof::check_code(&data);
        for by_code in [false, true] {
            std::fs::write(&private, &bytes).unwrap();
            std::fs::write(master.join("renamed-copy.pem"), &bytes).unwrap();
            std::fs::write(&paper, &bytes).unwrap();
            std::fs::write(&copy, &bytes).unwrap();
            let mut t = Transaction::open(&config.state_dir).unwrap();
            t.state.master.public_key_pem = Some(public_pem.clone());
            t.state.master.fingerprint =
                Some(crate::masterkey::fingerprint(public_pem.as_bytes()).unwrap());
            t.state.master.paper_written = true;
            t.state.master.private_digest = Some(crypto::hash(&data));
            t.state.master.paper_code_digest = Some(crypto::hash(&code));
            t.state.master.paper_file = Some(paper.clone());
            t.state.master.proven_at = None;
            t.save().unwrap();
            drop(t);
            let before = std::fs::read(config.state_dir.join("state.json")).unwrap();
            let proof_code = by_code.then_some(code.as_str());
            assert!(!terminal_proof(
                &config_path,
                "wrong-disposable-passphrase",
                proof_code
            ));
            assert_eq!(
                before,
                std::fs::read(config.state_dir.join("state.json")).unwrap()
            );
            assert!(private.exists() && paper.exists() && copy.exists());
            // An unremovable recorded path must never publish proof either.
            std::fs::remove_file(&paper).unwrap();
            std::fs::create_dir(&paper).unwrap();
            assert!(!terminal_proof(
                &config_path,
                "disposable-proof-passphrase",
                proof_code
            ));
            assert_eq!(
                before,
                std::fs::read(config.state_dir.join("state.json")).unwrap()
            );
            assert!(private.exists());
            std::fs::remove_dir(&paper).unwrap();
            std::fs::write(&paper, &bytes).unwrap();
            std::fs::write(&copy, &bytes).unwrap();
            assert!(terminal_proof(
                &config_path,
                "disposable-proof-passphrase",
                proof_code
            ));
            assert!(
                !private.exists() && !paper.exists() && !master.join("renamed-copy.pem").exists()
            );
            assert!(by_code || !copy.exists());
            assert!(
                Transaction::open(&config.state_dir)
                    .unwrap()
                    .state
                    .master
                    .proven_at
                    .is_some()
            );
        }
    }
    #[test]
    fn only_copies_certainly_on_the_hub_are_deleted() {
        use crate::keyproof::Location;
        let anywhere = Path::new("/nonexistent/master.html");
        assert_eq!(hub_copy(anywhere, &Location::Internal), HubCopy::OnHub);
        assert_eq!(
            hub_copy(anywhere, &Location::InBackup("inside /home".into())),
            HubCopy::OnHub
        );
        assert_eq!(hub_copy(anywhere, &Location::Removable), HubCopy::Elsewhere);
        // Memory and container layers are this machine.
        for fs in ["tmpfs", "ramfs", "overlay"] {
            assert_eq!(hub_copy_by_fs(Some(fs)), HubCopy::OnHub);
        }
        // A network share may hold the administrator's only copy; a disk
        // file system with a hidden device (container) cannot be told from
        // a mounted stick. Neither is ever deleted.
        for fs in ["nfs4", "cifs", "fuse.sshfs", "9p", "ext4", "zfs"] {
            assert_eq!(hub_copy_by_fs(Some(fs)), HubCopy::Undecidable);
        }
        assert_eq!(hub_copy_by_fs(None), HubCopy::Undecidable);
    }
    #[test]
    fn an_unremovable_copy_names_the_path_and_fails() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("paper.html");
        std::fs::create_dir(&blocked).unwrap();
        let message = remove_copy(&blocked).unwrap_err().to_string();
        assert!(message.contains(blocked.to_str().unwrap()));
        assert!(message.contains("no proof was recorded"));
    }
}
