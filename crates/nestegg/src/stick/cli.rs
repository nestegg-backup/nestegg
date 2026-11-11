//! Root-only stick preparation; no backups are started in phase one.
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Subcommand;

use super::media::{self, OwnedLoop, VerifiedImage};
use super::{capture, private_dir};
use crate::config::{Config, Paths, StickConfig};

#[derive(Subcommand)]
pub enum StickCmd {
    /// Erase a whole disk after typing its name; make a BIOS/UEFI backup stick.
    Create {
        disk: PathBuf,
        /// Trusted release ISO with an adjacent .sha256 file.
        #[arg(long)]
        image: PathBuf,
        /// Test harness only: disk is a disposable regular image file.
        #[arg(long, hide = true)]
        test_image: bool,
    },
    /// Write the inactive live slot and switch boot configuration last.
    Update {
        /// Whole disk; omitted only when exactly one registered disk is present.
        disk: Option<PathBuf>,
        #[arg(long)]
        image: PathBuf,
        #[arg(long, hide = true)]
        test_image: bool,
    },
    /// Show format, data UUID and active live slot.
    Info {
        disk: Option<PathBuf>,
        #[arg(long, hide = true)]
        test_image: bool,
    },
    /// Register data UUID and install the desktop PBS from a trusted release ISO.
    Add {
        disk: Option<PathBuf>,
        #[arg(long)]
        image: PathBuf,
        #[arg(long, hide = true)]
        test_image: bool,
    },
}

pub fn run(command: StickCmd, paths: &Paths) -> Result<u8> {
    if !nix::unistd::Uid::effective().is_root() {
        bail!("stick commands need root; run sudo nestegg stick …");
    }
    let (action, disk, image, test_image) = match command {
        StickCmd::Create {
            disk,
            image,
            test_image,
        } => ("create", Some(disk), Some(image), test_image),
        StickCmd::Update {
            disk,
            image,
            test_image,
        } => ("update", disk, Some(image), test_image),
        StickCmd::Info { disk, test_image } => ("info", disk, None, test_image),
        StickCmd::Add {
            disk,
            image,
            test_image,
        } => ("add", disk, Some(image), test_image),
    };
    // Serialize media operations and cache/registration updates on this desktop.
    fs::create_dir_all(&paths.runtime_dir)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(paths.runtime_dir.join("stick.lock"))?;
    let _lock = nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockExclusiveNonblock)
        .map_err(|(_, e)| anyhow::anyhow!("another stick operation is running: {e}"))?;
    let disk = match disk {
        Some(d) => d,
        None => registered_disk(paths)?,
    };
    let owned = if test_image {
        Some(OwnedLoop::open(&disk)?)
    } else {
        None
    };
    let target = owned.as_ref().map(OwnedLoop::path).unwrap_or(&disk);
    let disk = media::inspect(target, owned.is_some())?;
    let image = image.as_deref().map(VerifiedImage::open).transpose()?;
    let cache = paths.state_dir.join("stick");
    if image.is_some() {
        private_dir(&cache)?;
    }
    if action == "create" {
        media::confirm(&disk)?;
    }
    // Check registration document before writing/installing anything for add.
    if action == "add" {
        let _ = registration_document(&paths.config_file)?;
    }
    let info = media::operate(action, &disk, image.as_ref(), &cache)?;
    if action == "add" {
        register(&paths.config_file, &info.data_uuid)?;
    }
    println!("{}", serde_json::to_string_pretty(&info)?);
    Ok(0)
}

fn registered_disk(paths: &Paths) -> Result<PathBuf> {
    let doc = registration_document(&paths.config_file)?;
    let uuids = doc
        .get("stick")
        .and_then(|s| s.get("data_uuids"))
        .and_then(|v| v.as_array())
        .context("no registered stick; specify its whole disk explicitly")?;
    let mut disks = Vec::new();
    for uuid in uuids.iter().filter_map(|v| v.as_str()) {
        let part = PathBuf::from(format!("/dev/disk/by-uuid/{}", uuid.to_ascii_lowercase()));
        if part.exists() {
            let parent = capture(Command::new("lsblk").args(["-n", "-o", "PKNAME"]).arg(part))?;
            if !parent.is_empty() && !parent.contains('\n') {
                disks.push(PathBuf::from(format!("/dev/{parent}")));
            }
        }
    }
    disks.sort();
    disks.dedup();
    if disks.len() != 1 {
        bail!("expected exactly one attached registered stick; specify its whole disk explicitly");
    }
    Ok(disks.remove(0))
}

/// During phase one a registration-only document is valid for stick commands.
/// The existing PBS pipeline still requires [pbs]; target support is phase two.
fn registration_document(path: &Path) -> Result<toml_edit::DocumentMut> {
    let text = if path.exists() {
        fs::read_to_string(path)?
    } else {
        String::new()
    };
    let doc: toml_edit::DocumentMut = text.parse()?;
    if doc.contains_key("pbs") {
        Config::parse(&text)?;
    } else {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Registration {
            stick: Option<StickConfig>,
        }
        let r: Registration = toml::from_str(&text)?;
        if let Some(s) = r.stick {
            s.validate()?;
        }
    }
    Ok(doc)
}
fn register(path: &Path, uuid: &str) -> Result<()> {
    let mut doc = registration_document(path)?;
    let mut values: Vec<String> = doc
        .get("stick")
        .and_then(|s| s.get("data_uuids"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if !values.iter().any(|v| v.eq_ignore_ascii_case(uuid)) {
        values.push(uuid.to_owned());
    }
    let mut array = toml_edit::Array::new();
    for value in &values {
        array.push(value.as_str());
    }
    doc["stick"]["data_uuids"] = toml_edit::value(array);
    fs::create_dir_all(path.parent().context("config parent missing")?)?;
    let text = doc.to_string();
    if path.exists() {
        crate::settings::write_atomically(path, &text)
    } else {
        crate::settings::write_file_atomic(path, &text, 0o600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registration_preserves_permissions_and_refuses_config_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[pbs]\nserver='pbs.example.com'\ndatastore='backups'\nauth_id='backup@pbs!desktop'\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        register(&path, "01234567-89ab-cdef-0123-456789abcdef").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
        let before = fs::read(&path).unwrap();
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(register(&link, "11111111-2222-3333-4444-555555555555").is_err());
        assert!(link.is_symlink());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn registration_preserves_pbs_and_comments_and_rejects_unknowns() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.toml");
        let old = "# keep this\n[pbs]\nserver='pbs.example.com'\ndatastore='backups'\nauth_id='backup@pbs!desktop'\n";
        fs::write(&path, old).unwrap();
        register(&path, "01234567-89ab-cdef-0123-456789abcdef").unwrap();
        register(&path, "01234567-89ab-cdef-0123-456789abcdef").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this"));
        assert_eq!(
            Config::parse(&text).unwrap().pbs,
            Config::parse(old).unwrap().pbs
        );
        assert_eq!(
            Config::parse(&text)
                .unwrap()
                .stick
                .unwrap()
                .data_uuids
                .len(),
            1
        );
        fs::write(
            &path,
            "[stick]\ndata_uuids=['01234567-89ab-cdef-0123-456789abcdef']\nsecret='bad'\n",
        )
        .unwrap();
        assert!(registration_document(&path).is_err());
    }
    #[test]
    fn registration_only_document() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.toml");
        register(&path, "01234567-89ab-cdef-0123-456789abcdef").unwrap();
        assert!(registration_document(&path).is_ok());
    }
}
