use super::state::{Package, Transaction};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

pub fn add(state_dir: &Path, path: &Path) -> Result<()> {
    let bytes = crate::restore::read_small_regular_file(path, 512 * 1024 * 1024)?;
    let dir = state_dir.join("packages");
    crate::secrets::create_private_dir(&dir)?;
    if nix::unistd::Uid::effective().is_root() {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(state_dir)?;
        nix::unistd::chown(
            &dir,
            Some(nix::unistd::Uid::from_raw(meta.uid())),
            Some(nix::unistd::Gid::from_raw(meta.gid())),
        )?;
    }
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let file = dir.join(format!("{digest}.deb"));
    // Inspect the copied bytes, never a replaceable source path after hashing.
    super::state::atomic(&file, &bytes)?;
    let field = |name: &str| -> Result<String> {
        let out = Command::new("dpkg-deb")
            .args(["--field"])
            .arg(&file)
            .arg(name)
            .output()?;
        ensure!(out.status.success(), "not a Debian package");
        Ok(String::from_utf8(out.stdout)?.trim().to_string())
    };
    ensure!(
        field("Package")? == "nestegg",
        "package must be named nestegg"
    );
    let version = field("Version")?;
    let architecture = field("Architecture")?;
    ensure!(
        architecture == "amd64",
        "stage 1 supports amd64 packages only"
    );
    ensure!(!version.is_empty(), "package version is missing");
    let mut t = Transaction::open(state_dir)?;
    t.state.package = Some(Package {
        version,
        architecture,
        sha256: digest,
        file,
    });
    t.save().context("publish package metadata")?;
    eprintln!("hub package published");
    Ok(())
}
