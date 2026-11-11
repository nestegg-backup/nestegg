//! Back up a powered-off Debian-family computer from the booted stick.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Datelike, Utc};
use clap::Args;

use super::offline_storage::{Sources, text_root, valid_id};
use super::{Session, ask, ask_secret, ask_yes, choose, work_dir};
use crate::client::{ArchiveSpec, BackupSpec, snapshot_epoch, snapshot_path};
use crate::secrets::{self, Secret};

#[derive(Args)]
pub struct BackupArgs {
    /// Whole source disk. It is never written to.
    #[arg(long)]
    pub disk: String,
    /// Root filesystem device when there is more than one installed system.
    #[arg(long)]
    pub root_device: Option<String>,
    /// Confirm the computer's backup ID (from its config or hostname).
    #[arg(long)]
    pub backup_id: String,
    #[arg(long)]
    pub luks_passphrase_file: Option<PathBuf>,
    /// Exported or saved paper key instead of the stick's protected copy.
    #[arg(long)]
    pub keyfile: Option<PathBuf>,
    #[arg(long)]
    pub key_password_file: Option<PathBuf>,
    /// Generate a new key and protect its stick copy with this password.
    #[arg(long, conflicts_with = "keyfile")]
    pub new_key_password_file: Option<PathBuf>,
    /// Confirm the current UTC clock, or set an explicit RFC3339 UTC time.
    #[arg(long, conflicts_with = "time")]
    pub confirm_clock: bool,
    #[arg(long)]
    pub time: Option<String>,
}

fn secret_file(path: &Path) -> Result<Secret> {
    let bytes = fs::read(path)?;
    let mut bytes = bytes;
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    ensure!(!bytes.is_empty(), "empty password file");
    Ok(Secret::new(bytes))
}

pub fn suggested_id(root: &Path) -> Result<String> {
    let hostname = text_root(root, "/etc/hostname")?.trim().to_owned();
    let id = match text_root(root, "/etc/nestegg/config.toml") {
        Ok(text) => {
            let config =
                crate::config::Config::parse(&text).context("offline nestegg config is invalid")?;
            config.backup_id.unwrap_or(hostname)
        }
        Err(_) => hostname,
    };
    ensure!(
        valid_id(&id),
        "computer has no valid backup ID; supply a distinct name"
    );
    Ok(id)
}

/// All offline runs ask for clock confirmation, even when monotonicity alone
/// looks fine: without network time a future RTC could destroy retention.
pub fn clock_problem(now: DateTime<Utc>, newest: Option<i64>) -> Option<&'static str> {
    if now.year() < 2026 || now.year() > 2100 {
        Some("the live clock is outside the supported calendar range")
    } else if newest.is_some_and(|last| now.timestamp() <= last) {
        Some("the clock is not later than this computer's newest backup")
    } else {
        None
    }
}

fn set_clock(time: &str) -> Result<()> {
    let time = DateTime::parse_from_rfc3339(time)
        .context("use an RFC3339 time, e.g. 2026-11-11T12:00:00Z")?
        .with_timezone(&Utc);
    ensure!(
        clock_problem(time, None).is_none(),
        "time is outside the supported calendar range"
    );
    crate::stick::capture(Command::new("date").args(["--utc", "--set", &time.to_rfc3339()]))?;
    // Do not write the computer's hardware clock.
    Ok(())
}

fn source(disk: &str, password: Option<&Path>) -> Result<Sources> {
    let disks = super::list_disks()?;
    let disk = disks
        .iter()
        .find(|d| d.path == disk)
        .context("source is not a whole disk of this computer")?;
    Sources::open(disk, |dev| match password {
        Some(path) => secret_file(path),
        None => ask_secret(&format!("LUKS passphrase for {}", super::printable(dev))),
    })
}

fn recovery_copy(session: &Session, id: &str, key: &Secret, password: &Secret) -> Result<()> {
    let base = session
        .stick_mount()
        .context("no stick")?
        .join("nestegg-keys");
    if !base.exists() {
        fs::create_dir(&base)?;
    }
    let path = super::cli::stick_key_path(session, id)?;
    crate::stick::key::save_copy(&path, key, password, &work_dir())
}

fn new_key() -> Result<Secret> {
    secrets::create_private_dir(&work_dir())?;
    let dir = tempfile::Builder::new()
        .prefix("new-key-")
        .tempdir_in(work_dir())?;
    let path = dir.path().join("key.json");
    crate::client::local::create_key(&path)?;
    Ok(Secret::new(fs::read(path)?))
}

pub fn run(args: BackupArgs) -> Result<u8> {
    ensure!(
        nix::unistd::Uid::effective().is_root(),
        "offline backup requires root"
    );
    ensure!(valid_id(&args.backup_id), "invalid backup ID");
    let medium = super::source::BootMedium::detect()?;
    super::source::require_ram_workspace()?;
    let session = Session::from_stick(medium.open()?)?;
    crate::cancel::install_handler();
    let mut sources = source(&args.disk, args.luks_passphrase_file.as_deref())?;
    let candidates = sources.candidates()?;
    let selected = match &args.root_device {
        Some(path) => candidates.iter().find(|(d, _)| &d.path == path),
        None if candidates.len() == 1 => candidates.first(),
        _ => None,
    }
    .context("select exactly one installed root with --root-device")?;
    sources.mount_system(selected.0.clone(), selected.1.clone())?;
    let password;
    let key = if let Some(path) = &args.new_key_password_file {
        password = Some(secret_file(path)?);
        new_key()?
    } else {
        password = args
            .key_password_file
            .as_deref()
            .map(secret_file)
            .transpose()?;
        match &args.keyfile {
            Some(path) => Secret::new(super::read_key(path)?.json.into_bytes()),
            None => crate::stick::key::open_copy(
                &super::cli::stick_key_path(&session, &args.backup_id)?,
                password
                    .as_ref()
                    .context("--key-password-file or --keyfile required")?,
                &work_dir(),
            )?,
        }
    };
    session.set_key(Some(&key))?;
    session.client().check_group_key(&args.backup_id)?;
    if args.new_key_password_file.is_some() {
        recovery_copy(&session, &args.backup_id, &key, password.as_ref().unwrap())?;
    }
    if let Some(time) = &args.time {
        set_clock(time)?;
    }
    ensure!(
        args.confirm_clock || args.time.is_some(),
        "confirm the live clock with --confirm-clock or --time"
    );
    let result = perform(&session, sources, &args.backup_id);
    let closed = session.close();
    let code = result?;
    closed?;
    eprintln!("Stick flushed and unmounted; safe to unplug.");
    Ok(code)
}

pub fn wizard() -> Result<u8> {
    let medium = super::source::BootMedium::detect()?;
    super::source::require_ram_workspace()?;
    let session = Session::from_stick(medium.open()?)?;
    crate::cancel::install_handler();
    eprintln!(
        "Back up this computer: source disks remain read-only. Journal recovery uses RAM only."
    );
    let disks: Vec<_> = super::list_disks()?
        .into_iter()
        .filter(|d| !d.in_use)
        .collect();
    ensure!(!disks.is_empty(), "no unused source disk found");
    let labels: Vec<_> = disks
        .iter()
        .map(|d| {
            format!(
                "{}  {}  {}",
                d.path,
                super::plan::human(d.size_bytes),
                d.model
            )
        })
        .collect();
    let i = choose("Computer disk to back up", &labels, None)?;
    let mut sources = Sources::open(&disks[i], |dev| {
        ask_secret(&format!("LUKS passphrase for {dev}"))
    })?;
    let candidates = sources.candidates()?;
    ensure!(
        !candidates.is_empty(),
        "no supported installed system found"
    );
    let labels: Vec<_> = candidates
        .iter()
        .map(|(d, sv)| {
            format!(
                "{} {}{}",
                d.path,
                d.fstype.as_deref().unwrap_or_default(),
                sv.as_deref().unwrap_or_default()
            )
        })
        .collect();
    let i = if labels.len() == 1 {
        0
    } else {
        choose("Installed system", &labels, None)?
    };
    sources.mount_system(candidates[i].0.clone(), candidates[i].1.clone())?;
    let suggested = suggested_id(&sources.root)?;
    let id = ask(
        "Confirm computer backup ID (use a distinct name for each computer)",
        Some(&suggested),
    )?;
    ensure!(valid_id(&id), "invalid backup ID");
    let choice = choose(
        "Encryption key",
        &[
            "Existing key: protected stick copy or paper key".into(),
            "Create a new key for this computer".into(),
        ],
        Some(0),
    )?;
    let key = if choice == 0 {
        super::cli::choose_session_key(&session, &[], &id)?
            .context("offline backups must be encrypted")?
    } else {
        let key = new_key()?;
        session.set_key(Some(&key))?;
        session.client().check_group_key(&id)?;
        let password = crate::stick::key::password(None, true)?;
        recovery_copy(&session, &id, &key, &password)?;
        if ask_yes(
            "Show the paper key now so you can write it down and keep it elsewhere",
            true,
        )? {
            let path = session.store_dir.join(secrets::ENCRYPTION_KEY);
            let paper = crate::client::local::paperkey(&path, "text")?;
            eprintln!("{}", String::from_utf8_lossy(&paper));
            ensure!(
                ask_yes("Have you saved the paper key somewhere safe", false)?,
                "save the key before continuing"
            );
        }
        key
    };
    session.set_key(Some(&key))?;
    session.client().check_group_key(&id)?;
    let snapshots = session.client().list_snapshots(&id)?;
    let newest = snapshots
        .iter()
        .filter_map(|s| snapshot_epoch(&s.snapshot))
        .max();
    loop {
        let now = Utc::now();
        eprintln!("Current live clock: {}", now.to_rfc3339());
        eprintln!(
            "Newest backup of this computer: {}",
            newest
                .and_then(|t| DateTime::from_timestamp(t, 0))
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "none".into())
        );
        if let Some(problem) = clock_problem(now, newest) {
            eprintln!("Clock correction required: {problem}");
        } else if ask_yes("Is the current UTC date and time correct", false)? {
            break;
        }
        let time = ask("Correct UTC date and time (RFC3339)", None)?;
        if let Err(e) = set_clock(&time) {
            eprintln!("{}", super::printable_block(&format!("{e:#}")));
        }
    }
    ensure!(
        ask_yes("Start the encrypted backup to this stick", false)?,
        "backup cancelled"
    );
    let result = perform(&session, sources, &id);
    let closed = session.close();
    let code = result?;
    closed?;
    eprintln!("Stick flushed and unmounted; safe to unplug.");
    Ok(code)
}

fn perform(session: &Session, sources: Sources, id: &str) -> Result<u8> {
    let layout = sources.layout()?;
    ensure!(
        layout.bootloader == "grub",
        "offline restore currently requires a GRUB system"
    );
    // Enforce the same restorable-layout contract before uploading anything.
    super::plan::make_plan(
        &layout,
        &super::plan::Target {
            disk: "/dev/target".into(),
            size_bytes: 1 << 60,
            uefi: layout.boot_mode == "uefi",
            archive_bytes: None,
        },
        false,
    )?;
    let scratch = tempfile::Builder::new()
        .prefix("offline-backup-")
        .tempdir_in(work_dir())?;
    let inventory = scratch.path().join("inventory");
    super::offline_inventory::generate(
        &sources.root,
        &layout,
        &inventory,
        &sources.inventory_tree()?,
    )?;
    super::exec::preflight(
        &super::plan::make_plan(
            &layout,
            &super::plan::Target {
                disk: "/dev/target".into(),
                size_bytes: 1 << 60,
                uefi: super::booted_uefi(),
                archive_bytes: None,
            },
            false,
        )?,
        Some(&fs::read_to_string(inventory.join("dpkg-selections.txt"))?),
    )?;
    let client = session.client();
    client.check_group_key(id)?;
    ensure!(
        client.available_bytes()? >= 256 * 1024 * 1024,
        "stick full: less than 256 MiB available; retention will not be expanded"
    );
    let snapshots = client.list_snapshots(id)?;
    let now = Utc::now();
    let newest = snapshots
        .iter()
        .filter_map(|s| snapshot_epoch(&s.snapshot))
        .max();
    ensure!(
        clock_problem(now, newest).is_none(),
        "correct the live clock before backing up"
    );
    let snapshot = snapshot_path(id, now.timestamp());
    let meta = crate::metadata::BackupMetadata {
        format: crate::metadata::FORMAT,
        nestegg_version: env!("CARGO_PKG_VERSION").into(),
        backup_id: id.into(),
        hostname: layout.hostname.clone(),
        backup_time: now.timestamp(),
        snapshot: snapshot.clone(),
        archive_sources: BTreeMap::from([("system.pxar".into(), "/".into())]),
        sources: vec!["/".into()],
        system_mode: true,
        incomplete: Vec::new(),
    }
    .to_bytes();
    let meta_path = scratch.path().join("meta.json");
    secrets::write_private_file(&meta_path, &meta)?;
    let archives = vec![
        ArchiveSpec {
            name: "system".into(),
            path: sources.root.clone(),
        },
        ArchiveSpec {
            name: "inventory".into(),
            path: inventory.clone(),
        },
    ];
    let spec = BackupSpec {
        backup_id: id.into(),
        namespace: None,
        backup_time: now.timestamp(),
        archives: archives.clone(),
        excludes: crate::layout::SYSTEM_EXCLUDES
            .iter()
            .map(|s| s.to_string())
            .chain(
                crate::layout::intentionally_skipped(&layout)
                    .iter()
                    .map(|p| format!("{}/*", p.display())),
            )
            .collect(),
        include_dev: layout
            .mounts
            .iter()
            .filter(|m| layout.role(m) == crate::layout::MountRole::System)
            .map(|m| sources.root.join(m.point.trim_start_matches('/')))
            .chain(
                layout
                    .nested_subvolumes
                    .iter()
                    .map(|p| sources.root.join(p.trim_start_matches('/'))),
            )
            .collect(),
        encrypt: true,
        change_detection_mode: None,
        rate_limit: None,
        metadata: Some(meta_path),
    };
    client
        .backup(&spec, &mut std::io::stderr(), &mut |_| {})
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    sources.check_snapshots()?;
    let verify_dir = scratch.path().join("read-back");
    fs::create_dir(&verify_dir)?;
    let expected = crate::verify::Expected {
        snapshot: &snapshot,
        archives: archives.iter().map(ArchiveSpec::file_name).collect(),
        encrypted: true,
        metadata: &meta,
        inventory: Some(&inventory),
    };
    match crate::verify::read_back(
        &client,
        &expected,
        &verify_dir,
        &mut |s| eprintln!("{}", super::printable_block(s)),
        &crate::cancel::requested,
    ) {
        crate::verify::Outcome::Verified => {}
        other => bail!("offline backup read-back failed: {other:?}; no pruning was done"),
    }
    let pbs = session.local.as_ref().context("no local PBS")?.pbs();
    // A live session has no durable local monthly timestamp: force a complete
    // verification, which is stronger than phase two's monthly minimum.
    pbs.maintenance(&["verify", "stick", "--ignore-verified", "false"])?;
    let verified = client.list_snapshots(id)?;
    ensure!(
        verified
            .iter()
            .any(|s| s.snapshot == snapshot && s.verified.as_deref() == Some("ok")),
        "PBS did not verify this snapshot; no pruning was done"
    );
    ensure!(
        crate::verify::failed_snapshots(&verified).is_empty(),
        "stick contains damaged snapshots; no pruning was done"
    );
    for group in client.list_groups()? {
        let snapshots =
            client.list_group_snapshots(&format!("{}/{}", group.backup_type, group.backup_id))?;
        ensure!(
            crate::verify::failed_snapshots(&snapshots).is_empty(),
            "stick contains damaged snapshots; no pruning was done"
        );
    }
    client.check_group_key(id)?;
    let retention = text_root(&sources.root, "/etc/nestegg/config.toml")
        .ok()
        .map(|text| crate::config::Config::parse(&text))
        .transpose()?
        .and_then(|config| config.stick.map(|s| s.retention))
        .unwrap_or_default();
    client.prune_stick(id, &retention)?;
    pbs.maintenance(&["garbage-collection", "start", "stick"])?;
    sources.close()?;
    eprintln!("Offline backup verified: {snapshot}");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clock_checks_monotonicity_and_bad_rtc() {
        let now = DateTime::parse_from_rfc3339("2026-11-11T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(clock_problem(now, None).is_none());
        assert!(clock_problem(now, Some(now.timestamp())).is_some());
        assert!(clock_problem(now, Some(now.timestamp() + 1)).is_some());
        assert!(clock_problem(now, Some(now.timestamp() - 1)).is_none());
        assert!(clock_problem(DateTime::from_timestamp(0, 0).unwrap(), None).is_some());
    }
    #[test]
    fn computer_id_comes_from_offline_config_then_hostname() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("etc/nestegg")).unwrap();
        fs::write(d.path().join("etc/hostname"), "offline-desktop\n").unwrap();
        assert_eq!(suggested_id(d.path()).unwrap(), "offline-desktop");
        fs::write(d.path().join("etc/nestegg/config.toml"), "backup_id = 'configured-desktop'\n[stick]\ndata_uuids = ['01234567-89ab-cdef-0123-456789abcdef']\n").unwrap();
        assert_eq!(suggested_id(d.path()).unwrap(), "configured-desktop");
        fs::write(d.path().join("etc/nestegg/config.toml"), "invalid = true").unwrap();
        assert!(suggested_id(d.path()).is_err());
    }
}
