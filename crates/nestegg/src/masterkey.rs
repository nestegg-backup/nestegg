//! Proof of the household master key, independent of the computer's own copy.
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use x509_cert::der::{DecodePem, Encode};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Proof {
    pub fingerprint: String,
    pub proven_at: DateTime<Utc>,
}

pub fn fingerprint(pem: &[u8]) -> Result<String> {
    let key = x509_cert::spki::SubjectPublicKeyInfoOwned::from_pem(pem)
        .context("not a PEM public key")?;
    // The official client accepts RSA master keys only.
    if key.algorithm.oid.to_string() != "1.2.840.113549.1.1.1" {
        bail!("the master public key is not an RSA key");
    }
    Ok(Sha256::digest(key.to_der()?)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":"))
}

pub fn covered(
    config: &crate::config::Config,
    state: &crate::state::State,
) -> Result<Option<Proof>> {
    if config.hub.is_none() {
        return Ok(None);
    }
    let proof = state.master_key.as_ref().context(
        "the joined hub has no recorded master-key proof; join again or prove your own key copy",
    )?;
    let path = config.backup.master_public_key.as_deref().context("the hub master public key is not configured; join the hub again or prove your own key copy")?;
    check_file(path, &proof.fingerprint)?;
    Ok(Some(proof.clone()))
}

/// Check the exact loaded key, rather than trusting the state's cached digest.
pub fn require_coverage(
    config: &crate::config::Config,
    state: &crate::state::State,
    loaded_digest: &str,
    stick_proof: bool,
) -> Result<()> {
    if state.key_exported_digest.as_deref() == Some(loaded_digest) || stick_proof {
        return Ok(());
    }
    if covered(config, state)?.is_some() {
        return Ok(());
    }
    bail!(
        "the encryption key has not been exported yet; run `nestegg key export <file>` (and move it to a USB stick) or `nestegg key paperkey --format html --output key.html` (print it, then delete the file), then prove it with `nestegg key confirm-exported --file <copy>` or `--paper-code <code> --printed`"
    );
}

pub fn check_file(path: &Path, expected: &str) -> Result<()> {
    let pem = std::fs::read(path).with_context(|| format!("the master public key file {} is missing or cannot be read; restore the hub's public key or prove your own key copy", path.display()))?;
    let actual = fingerprint(&pem).context(
        "the master public key file is damaged; restore it from the hub or prove your own key copy",
    )?;
    if actual != expected {
        bail!(
            "the master public key fingerprint has changed (expected {expected}, found {actual}); restore the hub's public key or prove your own key copy"
        );
    }
    Ok(())
}

/// Recover using only the PBS token and the administrator's private master
/// key. All crypto and password prompting stay in the official client.
pub fn recover(
    paths: &crate::config::Paths,
    master: &Path,
    output: &Path,
    snapshot: Option<&str>,
) -> Result<()> {
    use crate::client::{self, Client};
    use crate::secrets::{self, Secret, SecretStore};
    use std::process::{Command, Stdio};
    let config = crate::config::Config::load(&paths.config_file)?;
    let client = Client::new(&config, SecretStore::new(&paths.credentials_dir));
    let id = config.effective_backup_id();
    let snapshot = match snapshot {
        Some(time) => {
            let parsed = DateTime::parse_from_rfc3339(time).context("--snapshot must be a snapshot time in RFC 3339 format, for example 2026-11-14T12:00:00Z")?;
            client::snapshot_path(&id, parsed.timestamp())
        }
        None => client::last_finished_snapshot(
            &client.list_snapshots_bounded(&id, client::readback_list_timeout())?,
        )
        .context("this computer has no finished snapshot on the backup server")?
        .snapshot
        .clone(),
    };
    let absolute = |path: &Path| -> Result<std::path::PathBuf> {
        Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        })
    };
    let output = absolute(output)?;
    let master = absolute(master)?;
    let user = crate::identity::UserIdentity::from_sudo()?;
    if user
        .as_ref()
        .map(|u| crate::identity::exists_as(u, &output))
        .transpose()?
        .unwrap_or_else(|| output.symlink_metadata().is_ok())
    {
        bail!(
            "{} already exists; choose a new --output file (nothing was overwritten)",
            output.display()
        );
    }
    // Read the master key with the caller's rights. The client opens a
    // private copy, so it cannot follow an arbitrary user-controlled path
    // with root's privileges. Its password comes directly from the terminal.
    let master_bytes = Secret::new(match &user {
        Some(user) => crate::identity::read_file_as(user, &master)?,
        None => {
            crate::restore::read_small_regular_file(&master, crate::restore::MAX_KEY_FILE_BYTES)?
        }
    });
    let work = crate::staging::StagingDir::create(&paths.tmp_dir().join("master-recovery"))?;
    let private = work.path().join("master-private.pem");
    secrets::create_new_private_file(&private, master_bytes.as_bytes())?;
    let blob = work.path().join("rsa-encrypted.key");
    client.fetch_master_key_blob(&snapshot, &blob)?;
    let recovered = work.path().join("recovered.json");
    let status = Command::new(client::client_binary())
        .args(["key", "import-with-master-key"])
        .arg(&recovered)
        .arg("--master-keyfile")
        .arg(&private)
        .arg("--encrypted-keyfile")
        .arg(&blob)
        .args(["--kdf", "none"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("cannot run proxmox-backup-client key import-with-master-key")?;
    if !status.success() {
        bail!(
            "key recovery failed; check the master key and its password. No output file was written"
        );
    }
    let info = client::local::key_info(&recovered)?;
    info.require_unprotected()?;
    // Verify against the signed manifest before handing out the recovered key.
    let key = Secret::new(std::fs::read(&recovered)?);
    if !client.open_snapshot_with_key(&snapshot, &key)? {
        bail!("the snapshot is not signed; the recovered key could not be verified");
    }
    match &user {
        Some(user) => crate::identity::write_new_file_as(user, &output, key.as_bytes())?,
        None => secrets::create_new_private_file(&output, key.as_bytes())?,
    }
    println!(
        "Recovered key from {snapshot} (fingerprint {}). Written to {}. Import it with: sudo nestegg key import <recovered-file>",
        info.fingerprint,
        output.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const PEM: &[u8] = include_bytes!("../tests/fixtures/master-public.fixture");

    #[test]
    fn gate_combines_own_proof_and_valid_master_proof_but_never_trusts_missing_or_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master-public.pem");
        let mut cfg = crate::config::example("pbs.example.com", "backups", "backup@pbs!desktop");
        let mut state = crate::state::State::default();
        assert!(require_coverage(&cfg, &state, "current-key", false).is_err());
        state.key_digest = Some("current-key".into());
        state.key_exported_digest = Some("current-key".into());
        assert!(require_coverage(&cfg, &state, "current-key", false).is_ok());
        assert!(require_coverage(&cfg, &state, "replaced-key", false).is_err());
        state.key_exported_digest = None;
        state.master_key = Some(Proof {
            fingerprint: fingerprint(PEM).unwrap(),
            proven_at: "2026-01-01T12:00:00Z".parse().unwrap(),
        });
        cfg.backup.master_public_key = Some(path.clone());
        std::fs::write(&path, PEM).unwrap();
        // A standalone public key alone does not establish the hub's proof.
        assert!(covered(&cfg, &state).unwrap().is_none());
        assert!(require_coverage(&cfg, &state, "current-key", false).is_err());
        cfg.hub = Some(crate::config::HubConfig {
            name: "Home".into(),
            url: "https://hub.example.com:8443".into(),
            pin: crate::pinned::pin_der(b"fixture"),
            machine_id: "fixture-machine".into(),
        });
        assert!(require_coverage(&cfg, &state, "current-key", false).is_ok());
        assert!(covered(&cfg, &state).unwrap().is_some());
        assert!(!state.key_exported());
        state.master_key.as_mut().unwrap().fingerprint = "changed".into();
        let error = require_coverage(&cfg, &state, "current-key", false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("fingerprint has changed"), "{error}");
        // An independently proven own copy remains valid.
        state.key_exported_digest = Some("current-key".into());
        assert!(require_coverage(&cfg, &state, "current-key", false).is_ok());
        state.key_exported_digest = None;
        std::fs::remove_file(&path).unwrap();
        let error = require_coverage(&cfg, &state, "current-key", false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing or cannot be read"));
        state.key_exported_digest = Some("current-key".into());
        assert!(require_coverage(&cfg, &state, "current-key", false).is_ok());
        state.key_exported_digest = None;
        state.master_key = None;
        assert!(require_coverage(&cfg, &state, "current-key", false).is_err());
        assert!(require_coverage(&cfg, &state, "current-key", true).is_ok());
        assert!(fingerprint(b"not a PEM key").is_err());
    }
}
