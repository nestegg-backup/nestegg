//! Stick preparation, registration and protected recovery-key commands.
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
    /// Copy the machine key to a registered stick with a strong scrypt password.
    KeyCopy {
        #[arg(long)]
        uuid: String,
        #[arg(long)]
        password_fd: Option<i32>,
    },
    /// Open the stick key copy again and renew the password reminder.
    KeyCheck {
        #[arg(long)]
        uuid: String,
        #[arg(long)]
        password_fd: Option<i32>,
    },
    #[command(hide = true)]
    Registered {
        #[arg(long)]
        uuid: Option<String>,
        #[arg(long)]
        parent: Option<PathBuf>,
        #[arg(long)]
        unit: bool,
    },
    /// Erase a whole disk after typing its name; make a BIOS/UEFI backup stick.
    Create {
        disk: PathBuf,
        /// Root-owned local release ISO; checksum is integrity, not authenticity.
        #[arg(long)]
        image: PathBuf,
        /// SHA-256 obtained independently (takes precedence over adjacent .sha256).
        #[arg(long)]
        sha256: Option<String>,
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
        /// SHA-256 obtained independently (takes precedence over adjacent .sha256).
        #[arg(long)]
        sha256: Option<String>,
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
        /// SHA-256 obtained independently (takes precedence over adjacent .sha256).
        #[arg(long)]
        sha256: Option<String>,
        /// Optional name shown instead of the UUID, e.g. "Blue stick" (at most 40 characters).
        #[arg(long)]
        name: Option<String>,
        #[arg(long, hide = true)]
        test_image: bool,
    },
    /// Give a registered stick a name, change it, or remove it with --clear.
    Name {
        #[arg(long)]
        uuid: String,
        /// New name (at most 40 characters).
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        name: Option<String>,
        /// Remove the name; the stick is shown by its UUID again.
        #[arg(long)]
        clear: bool,
    },
}

pub fn run(command: StickCmd, paths: &Paths) -> Result<u8> {
    let command = match command {
        StickCmd::KeyCopy { uuid, password_fd } => {
            return super::key::run(paths, &uuid, false, password_fd);
        }
        StickCmd::KeyCheck { uuid, password_fd } => {
            return super::key::run(paths, &uuid, true, password_fd);
        }
        StickCmd::Registered { uuid, parent, unit } => {
            return Ok(
                if super::target::registered(paths, uuid.as_deref(), parent.as_deref()) {
                    if unit {
                        println!(
                            "{}",
                            uuid.context("--unit needs --uuid")?
                                .to_ascii_lowercase()
                                .replace('-', "\\x2d")
                        );
                    } else {
                        println!("registered");
                    }
                    0
                } else {
                    1
                },
            );
        }
        command => command,
    };
    if !nix::unistd::Uid::effective().is_root() {
        bail!("stick commands need root; run sudo nestegg stick …");
    }
    if let StickCmd::Name { uuid, name, clear } = command {
        let name = if clear { None } else { name };
        set_name(&paths.config_file, &uuid, name.as_deref())?;
        match name {
            Some(name) => println!(
                "stick {} is now named \"{}\"",
                uuid.to_ascii_lowercase(),
                name.trim()
            ),
            None => println!("stick {} has no name now", uuid.to_ascii_lowercase()),
        }
        return Ok(0);
    }
    let mut name = None;
    let (action, disk, image, sha256, test_image) = match command {
        StickCmd::Create {
            disk,
            image,
            sha256,
            test_image,
        } => ("create", Some(disk), Some(image), sha256, test_image),
        StickCmd::Update {
            disk,
            image,
            sha256,
            test_image,
        } => ("update", disk, Some(image), sha256, test_image),
        StickCmd::Info { disk, test_image } => ("info", disk, None, None, test_image),
        StickCmd::Add {
            disk,
            image,
            sha256,
            name: n,
            test_image,
        } => {
            // Refuse a bad name before anything is written to the stick.
            if let Some(n) = &n {
                crate::config::validate_stick_name(n.trim())?;
            }
            name = n;
            ("add", disk, Some(image), sha256, test_image)
        }
        _ => unreachable!("handled above"),
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
    let _signals = media::SignalGuard::install()?;
    let owned = if test_image {
        Some(OwnedLoop::open(&disk)?)
    } else {
        None
    };
    let target = owned.as_ref().map(OwnedLoop::path).unwrap_or(&disk);
    media::settle_udev();
    let disk = media::inspect(target, owned.is_some())?;
    let cache = paths.state_dir.join("stick");
    let image = image
        .as_deref()
        .map(|p| VerifiedImage::open(p, sha256.as_deref(), &cache))
        .transpose()?;
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
    media::check_interrupted()?;
    let info = media::operate(action, &disk, image.as_ref(), &cache)?;
    if action == "add" {
        register(&paths.config_file, &info.data_uuid)?;
        if let Some(name) = &name {
            set_name(&paths.config_file, &info.data_uuid, Some(name))?;
        }
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
    let uuids: Vec<_> = uuids.iter().filter_map(|v| v.as_str()).collect();
    // Query every attached device; /dev/disk/by-uuid hides cloned UUIDs by
    // exposing just one arbitrarily selected symlink target.
    let tree = registered_tree()?;
    let mut disks = registered_candidates(&tree, &uuids)?;
    disks.sort();
    disks.dedup();
    if disks.len() != 1 {
        bail!("expected exactly one attached registered stick; specify its whole disk explicitly");
    }
    Ok(disks.remove(0))
}

pub(super) fn registered_tree() -> Result<serde_json::Value> {
    let mut tree: serde_json::Value =
        serde_json::from_str(&capture(Command::new("lsblk").args([
            "--json",
            "--paths",
            "-o",
            "PATH,TYPE,UUID,PKNAME",
        ]))?)?;
    refresh_partition_uuids(&mut tree["blockdevices"])?;
    Ok(tree)
}

fn refresh_partition_uuids(nodes: &mut serde_json::Value) -> Result<()> {
    if let Some(nodes) = nodes.as_array_mut() {
        for node in nodes {
            if node["type"] == "part" {
                let path = node["path"].as_str().context("partition path missing")?;
                let out = Command::new("blkid")
                    .args(["-p", "-s", "UUID", "-o", "value", "--"])
                    .arg(path)
                    .output()?;
                if !out.status.success() && out.status.code() != Some(2) {
                    bail!("cannot probe registered partition UUID");
                }
                node["uuid"] = String::from_utf8(out.stdout)?.trim().into();
            }
            refresh_partition_uuids(&mut node["children"])?;
        }
    }
    Ok(())
}

pub(super) fn registered_candidates(
    tree: &serde_json::Value,
    uuids: &[&str],
) -> Result<Vec<PathBuf>> {
    fn visit<'a>(items: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
        if let Some(items) = items.as_array() {
            for item in items {
                out.push(item);
                visit(&item["children"], out);
            }
        }
    }
    let mut items = Vec::new();
    visit(&tree["blockdevices"], &mut items);
    let mut disks = Vec::new();
    for uuid in uuids {
        let mut matches: Vec<_> = items
            .iter()
            .filter(|d| {
                d["uuid"]
                    .as_str()
                    .is_some_and(|v| v.eq_ignore_ascii_case(uuid))
            })
            .collect();
        matches.sort_by_key(|d| d["path"].as_str());
        matches.dedup_by_key(|d| d["path"].as_str());
        if matches.len() > 1 {
            bail!(super::target::error(
                crate::state::ErrorKind::StickAmbiguous,
                "cloned data UUID on multiple devices; refusing ambiguous registered stick"
            ));
        }
        if let Some(d) = matches.first() {
            let parent = d["pkname"]
                .as_str()
                .context("registered UUID is not a partition")?;
            if !parent.starts_with("/dev/") {
                bail!("invalid registered partition parent");
            }
            disks.push(PathBuf::from(parent));
        }
    }
    Ok(disks)
}

/// Accept registration-only documents while preserving strict configuration parsing.
/// Registration supplies full-system defaults for a new stick-only setup.
fn registration_document(path: &Path) -> Result<toml_edit::DocumentMut> {
    let text = if path.exists() {
        fs::read_to_string(path)?
    } else {
        String::new()
    };
    validate_document(&text)?;
    Ok(text.parse()?)
}
fn register(path: &Path, uuid: &str) -> Result<()> {
    fs::create_dir_all(path.parent().context("config parent missing")?)?;
    let _lock = crate::settings::config_lock(path)?;
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
    if !doc.contains_key("pbs") && !doc.contains_key("backup") {
        doc["backup"]["system"] = toml_edit::value(true);
    }
    if path.try_exists()? {
        crate::settings::write_atomically(path, &doc.to_string())
    } else {
        let group = nix::unistd::Group::from_name("nestegg")?
            .context("nestegg group missing; install the nestegg package before registering")?;
        crate::settings::write_file_atomic_owned(
            path,
            &doc.to_string(),
            0o640,
            Some((nix::unistd::Uid::from_raw(0), group.gid)),
        )
    }
}

/// Set (`Some`) or remove (`None`) the name of a registered stick in
/// `[stick.names]`, keeping comments and every other setting as they are.
fn set_name(path: &Path, uuid: &str, name: Option<&str>) -> Result<()> {
    let name = name.map(str::trim);
    if let Some(name) = name {
        crate::config::validate_stick_name(name)?;
    }
    let uuid = uuid.trim().to_ascii_lowercase();
    let _lock = crate::settings::config_lock(path)?;
    let mut doc = registration_document(path)?;
    let registered = doc
        .get("stick")
        .and_then(|s| s.get("data_uuids"))
        .and_then(|v| v.as_array())
        .is_some_and(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .any(|v| v.eq_ignore_ascii_case(&uuid))
        });
    if !registered {
        bail!("{uuid} is not a registered stick; register it first with nestegg stick add");
    }
    let stick = doc["stick"]
        .as_table_like_mut()
        .context("[stick] is not a table")?;
    match name {
        Some(name) => {
            if stick.get("names").is_none() {
                stick.insert("names", toml_edit::table());
            }
            let names = stick
                .get_mut("names")
                .and_then(|n| n.as_table_like_mut())
                .context("stick.names is not a table")?;
            names.insert(&uuid, toml_edit::value(name));
        }
        None => {
            if let Some(names) = stick.get_mut("names").and_then(|n| n.as_table_like_mut()) {
                names.remove(&uuid);
                if names.is_empty() {
                    stick.remove("names");
                }
            }
        }
    }
    let text = doc.to_string();
    // The result must be a valid document before it replaces the file.
    validate_document(&text)?;
    crate::settings::write_atomically(path, &text)
}

/// The same strictness as [`registration_document`], for text about to be written.
fn validate_document(text: &str) -> Result<()> {
    let doc: toml_edit::DocumentMut = text.parse()?;
    if doc.contains_key("pbs") || doc.contains_key("backup") || doc.contains_key("backup_id") {
        Config::parse(text)?;
    } else {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Registration {
            stick: Option<StickConfig>,
        }
        let r: Registration = toml::from_str(text)?;
        if let Some(s) = r.stick {
            s.validate()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cloned_uuid_does_not_select_an_arbitrary_symlink() {
        let tree = serde_json::json!({"blockdevices":[
            {"path":"/dev/sdx1", "pkname":"/dev/sdx", "uuid":"sample"},
            {"path":"/dev/sdy1", "pkname":"/dev/sdy", "uuid":"sample"}
        ]});
        assert!(registered_candidates(&tree, &["sample"]).is_err());
    }
    #[test]
    fn registration_preserves_permissions_and_refuses_config_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[pbs]\nserver='pbs.example.com'\ndatastore='backups'\nauth_id='backup@pbs!desktop'\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        register(&path, "01234567-89ab-cdef-0123-456789abcdef").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
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
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let before = fs::metadata(&path).unwrap();
        register(&path, "01234567-89ab-cdef-0123-456789abcdef").unwrap();
        register(&path, "01234567-89ab-cdef-0123-456789abcdef").unwrap();
        let after = fs::metadata(&path).unwrap();
        assert_eq!(
            (before.uid(), before.gid(), before.mode()),
            (after.uid(), after.gid(), after.mode())
        );
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
    fn names_are_set_changed_and_cleared_without_touching_the_rest() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.toml");
        let old = "# keep this\n[pbs]\nserver='pbs.example.com'\ndatastore='backups'\nauth_id='backup@pbs!desktop'\n[backup]\nencryption=true\n";
        fs::write(&path, old).unwrap();
        let uuid = "01234567-89ab-cdef-0123-456789abcdef";
        // Only registered sticks get a name.
        assert!(set_name(&path, uuid, Some("Blue")).is_err());
        register(&path, uuid).unwrap();
        set_name(&path, &uuid.to_ascii_uppercase(), Some("  Blue stick ")).unwrap();
        let read = || Config::load(&path).unwrap().stick.unwrap();
        assert_eq!(read().name_of(uuid), Some("Blue stick"));
        set_name(&path, uuid, Some("Grey")).unwrap();
        assert_eq!(read().name_of(uuid), Some("Grey"));
        // A refused name leaves the file as it was.
        let before = fs::read(&path).unwrap();
        assert!(set_name(&path, uuid, Some("bad\u{7}name")).is_err());
        assert!(set_name(&path, uuid, Some(&"x".repeat(41))).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        set_name(&path, uuid, None).unwrap();
        assert!(read().names.is_empty());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this"));
        assert!(!text.contains("names"), "{text}");
        // Clearing a stick without a name is fine.
        set_name(&path, uuid, None).unwrap();
        // A hand-written [stick] table gets a [stick.names] table.
        fs::write(
            &path,
            format!("[stick]\ndata_uuids = ['{uuid}']\noverdue_after_hours = 48\n"),
        )
        .unwrap();
        set_name(&path, uuid, Some("Office")).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[stick.names]"), "{text}");
        assert!(text.contains("overdue_after_hours = 48"), "{text}");
        assert_eq!(read().name_of(uuid), Some("Office"));
    }

    #[test]
    fn registration_only_document() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.toml");
        fs::write(&path, "").unwrap();
        register(&path, "01234567-89ab-cdef-0123-456789abcdef").unwrap();
        assert!(registration_document(&path).is_ok());
        let config = Config::load(&path).unwrap();
        assert!(config.backup.system);
        assert!(config.pbs.is_none());
    }
}
