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
    let mut t = Transaction::open(&config.state_dir)?;
    ensure!(
        t.state.master.public_key_pem.is_some(),
        "no master key exists; complete hub init first"
    );
    if t.state.master.proven_at.is_some() {
        println!("Master key copy is already proven.");
        return Ok(());
    }
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
    if let Some(file) = file {
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
                t.state.master.private_digest.as_deref().unwrap_or("")
            ),
            "copy does not contain this household's master key"
        );
        if crate::keyproof::locate(&real).needs_statement() && !args.accept_local_copy {
            ensure!(
                prompt(
                    "Type yes to state that another copy exists outside the hub's disk (or use --accept-local-copy)"
                )? == "yes",
                "copy outside the hub was not confirmed"
            );
        }
    } else if let Some(code) = code {
        let code = crate::keyproof::normalize_check_code(&code).context("not a check code")?;
        ensure!(
            crypto::matches(
                &code,
                t.state.master.paper_code_digest.as_deref().unwrap_or("")
            ),
            "check code does not belong to this master key"
        );
        if let Some(file) = &t.state.master.paper_file {
            match std::fs::metadata(file) {
                Ok(_) => ensure!(
                    !crate::keyproof::locate(file).needs_statement(),
                    "paper key file still exists on this disk; print and delete it or move it to removable media; alternatively prove the file with --file and --accept-local-copy"
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => {
                    return Err(e).context("cannot verify that the paper key file was removed");
                }
            }
        }
        if !args.printed {
            ensure!(
                prompt(
                    "Type yes to state that the sheet is printed and stored away from the hub (or use --printed)"
                )? == "yes",
                "printed copy not confirmed"
            );
        }
    } else {
        anyhow::bail!("proof required");
    }
    let private = config.state_dir.join("master/master-private.pem");
    match std::fs::remove_file(&private) {
        Ok(()) => std::fs::File::open(private.parent().context("master directory")?)?.sync_all()?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e).context("remove hub's private master key"),
    }
    t.state.master.proven_at = Some(state::now());
    t.state.master.private_digest = None;
    t.state.master.paper_code_digest = None;
    t.save()?;
    println!(
        "Master key copy proven; the private part was removed from the hub. Unplug removable media and keep it away from this machine."
    );
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
}
