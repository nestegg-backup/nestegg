//! `nestegg rescue` commands: the interactive wizard of the rescue stick,
//! a non-interactive restore for automation, and `export-settings` for the
//! normal system.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use super::exec::{self, Job, Runner};
use super::plan::{Target, human, make_plan};
use super::{
    Disk, Session, Settings, ask, ask_secret, ask_yes, booted_uefi, choose, fetch_fingerprint,
    list_disks, scan_media, work_dir,
};
use crate::secrets::Secret;

#[derive(Subcommand)]
pub enum RescueCmd {
    /// Interactive restore onto an empty disk (default on the rescue stick).
    Wizard,
    /// Non-interactive restore onto an empty disk (for automation).
    Restore(Box<RestoreArgs>),
    /// List the disks of this computer.
    Disks,
    /// Write the connection settings (no secrets) for the rescue stick to a
    /// file, e.g. next to the exported key on a USB stick.
    ExportSettings { file: PathBuf },
}

#[derive(Args)]
pub struct RestoreArgs {
    /// Settings file (see `export-settings`); individual options override it.
    #[arg(long)]
    pub settings: Option<PathBuf>,
    #[arg(long)]
    pub server: Option<String>,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub datastore: Option<String>,
    #[arg(long)]
    pub namespace: Option<String>,
    #[arg(long)]
    pub auth_id: Option<String>,
    #[arg(long)]
    pub fingerprint: Option<String>,
    /// Machine (backup ID) to restore.
    #[arg(long)]
    pub backup_id: Option<String>,
    #[arg(long)]
    pub wake_mac: Option<String>,
    /// File with the PBS password or API token secret.
    #[arg(long)]
    pub password_file: PathBuf,
    /// Exported encryption key (omit for unencrypted backups).
    #[arg(long)]
    pub keyfile: Option<PathBuf>,
    /// Snapshot path or `latest`.
    #[arg(long, default_value = "latest")]
    pub snapshot: String,
    /// Target disk, e.g. /dev/nvme0n1. ALL DATA ON IT IS ERASED.
    #[arg(long)]
    pub disk: String,
    /// Must repeat the target disk to confirm that it may be erased.
    #[arg(long)]
    pub wipe: String,
    /// Encrypt the new root file system (LUKS2) with the passphrase in this file.
    #[arg(long)]
    pub encrypt_passphrase_file: Option<PathBuf>,
    /// Only show the plan, change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Allow erasing a disk that holds a data disk of the backed-up computer
    /// (refused otherwise).
    #[arg(long)]
    pub erase_data_disk: bool,
}

pub fn run(cmd: Option<RescueCmd>) -> Result<u8> {
    match cmd.unwrap_or(RescueCmd::Wizard) {
        RescueCmd::Wizard => wizard(),
        RescueCmd::Restore(args) => restore(*args),
        RescueCmd::Disks => {
            for d in list_disks()? {
                println!("{}", disk_line(&d));
            }
            Ok(0)
        }
        RescueCmd::ExportSettings { file } => {
            let paths = crate::config::Paths::from_env();
            let config = crate::config::Config::load(&paths.config_file)?;
            let text = toml::to_string_pretty(&Settings::from_config(&config))?;
            if file.exists() {
                bail!("{} already exists", file.display());
            }
            fs::write(&file, format!(
                "# nestegg rescue settings (no secrets). Keep it next to the exported key.\n{text}"
            ))
            .with_context(|| format!("cannot write {}", file.display()))?;
            println!("Wrote {}", file.display());
            Ok(0)
        }
    }
}

fn disk_line(d: &Disk) -> String {
    format!(
        "{:<16} {:>10}  {:<6} {}{}",
        d.path,
        human(d.size_bytes),
        d.transport,
        d.model,
        if d.in_use { "  (in use)" } else { "" }
    )
}

fn require_root() -> Result<()> {
    if !nix::unistd::geteuid().is_root() {
        bail!("the rescue restore must run as root");
    }
    Ok(())
}

fn read_secret_file(path: &Path) -> Result<Secret> {
    let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let text = String::from_utf8(bytes).context("secret file is not text")?;
    let v = text.trim_end_matches(['\n', '\r']).to_string();
    if v.is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(Secret::new(v.into_bytes()))
}

fn ensure_reachable(session: &Session) -> Result<()> {
    match crate::wake::ensure_available(&session.config, |e| eprintln!("{e}"))? {
        crate::wake::Availability::Unreachable => bail!(
            "the PBS {}:{} does not answer",
            session.config.pbs.server,
            session.config.pbs.port
        ),
        _ => Ok(()),
    }
}

fn restore(a: RestoreArgs) -> Result<u8> {
    let mut s = match &a.settings {
        Some(p) => toml::from_str::<Settings>(
            &fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))?,
        )?,
        None => Settings {
            port: 8007,
            ..Default::default()
        },
    };
    if let Some(v) = a.server {
        s.server = v;
    }
    if let Some(v) = a.port {
        s.port = v;
    }
    if let Some(v) = a.datastore {
        s.datastore = v;
    }
    if a.namespace.is_some() {
        s.namespace = a.namespace;
    }
    if let Some(v) = a.auth_id {
        s.auth_id = v;
    }
    if a.fingerprint.is_some() {
        s.fingerprint = a.fingerprint;
    }
    if a.backup_id.is_some() {
        s.backup_id = a.backup_id;
    }
    if a.wake_mac.is_some() {
        s.wake_mac = a.wake_mac;
    }
    let backup_id = s.backup_id.clone().context("--backup-id is required")?;
    if a.wipe != a.disk {
        bail!("--wipe must repeat the target disk ({})", a.disk);
    }
    if !a.dry_run {
        require_root()?;
    }
    let password = read_secret_file(&a.password_file)?;
    let key = a.keyfile.as_deref().map(read_key_file).transpose()?;
    let luks = a
        .encrypt_passphrase_file
        .as_deref()
        .map(read_secret_file)
        .transpose()?;
    let session = Session::new(&s, &password, key.as_ref())?;
    ensure_reachable(&session)?;
    let snaps = session.system_snapshots(&backup_id)?;
    let snapshot = if a.snapshot == "latest" {
        snaps
            .first()
            .with_context(|| format!("no full system backup of '{backup_id}' found"))?
            .snapshot
            .clone()
    } else {
        a.snapshot.clone()
    };
    let archive_bytes = system_archive_bytes(&snaps, &snapshot);
    let disks = list_disks()?;
    let disk = disks
        .iter()
        .find(|d| d.path == a.disk)
        .with_context(|| format!("{} is not a disk of this computer", a.disk))?;
    if disk.in_use {
        bail!(
            "{} is in use (mounted or holds the rescue system)",
            disk.path
        );
    }
    run_restore(
        &session,
        &snapshot,
        archive_bytes,
        disk,
        luks.as_ref(),
        a.dry_run,
        &mut |disk, data_disk| {
            if data_disk && !a.erase_data_disk {
                bail!(
                    "{disk} holds a data disk of the backed-up computer; nothing was changed. \
                     Use --erase-data-disk if it really should be erased."
                );
            }
            Ok(true)
        },
    )
}

fn read_key_file(path: &Path) -> Result<Secret> {
    if !super::looks_like_key(path) {
        bail!("{} is not a proxmox-backup-client key file", path.display());
    }
    Ok(Secret::new(fs::read(path)?))
}

fn system_archive_bytes(snaps: &[crate::client::SnapshotInfo], snapshot: &str) -> Option<u64> {
    let name = format!("{}.pxar.didx", crate::layout::SYSTEM_ARCHIVE);
    snaps
        .iter()
        .find(|s| s.snapshot == snapshot)?
        .archive_bytes
        .iter()
        .find(|(f, _)| *f == name)
        .map(|(_, b)| *b)
}

/// Shared by the wizard and the non-interactive command.
fn run_restore(
    session: &Session,
    snapshot: &str,
    archive_bytes: Option<u64>,
    disk: &Disk,
    luks: Option<&Secret>,
    dry_run: bool,
    // (disk, it holds a data disk of the backed-up computer) → go ahead?
    confirm: &mut dyn FnMut(&str, bool) -> Result<bool>,
) -> Result<u8> {
    eprintln!("Reading the disk layout of {snapshot} ...");
    let (layout, selections) = session.inventory(snapshot)?;
    eprintln!(
        "Backed-up system: {} ({}), {} boot, {}",
        if layout.os_name.is_empty() {
            &layout.os_id
        } else {
            &layout.os_name
        },
        layout.hostname,
        layout.boot_mode.to_uppercase(),
        human(layout.used_bytes())
    );
    let plan = make_plan(
        &layout,
        &Target {
            disk: disk.path.clone(),
            size_bytes: disk.size_bytes,
            uefi: booted_uefi(),
            archive_bytes,
        },
        luks.is_some(),
    )?;
    let mut warnings = exec::preflight(&plan, selections.as_deref())?;
    exec::check_disk_free(&plan.disk)?;
    let (contents, uuids) = exec::disk_contents(&plan.disk);
    // A file system of the backed-up computer that is NOT part of the
    // system (a data disk) must never be overwritten by accident.
    // Encrypted data disks show only their LUKS UUID while locked.
    let mut data_disk = false;
    for m in &layout.mounts {
        if layout.role(m) == crate::layout::MountRole::Other
            && [&m.uuid, &m.crypt_uuid]
                .into_iter()
                .flatten()
                .any(|u| uuids.contains(u))
        {
            data_disk = true;
            warnings.push(format!(
                "THIS DISK HOLDS {} OF THE BACKED-UP COMPUTER (a data disk that is not part of \
                 the system backup). Restoring erases it.",
                m.point
            ));
        }
    }
    eprintln!();
    eprintln!("{}", plan.describe());
    if !contents.trim().is_empty() {
        eprintln!("Current content of {} (will be erased):", plan.disk);
        eprintln!("{}", contents.trim_end());
    }
    for w in &warnings {
        eprintln!("Warning: {w}");
    }
    if dry_run {
        eprintln!("Dry run: nothing was changed.");
        return Ok(0);
    }
    if !confirm(&plan.disk, data_disk)? {
        eprintln!("Cancelled, nothing was changed.");
        return Ok(1);
    }
    let log_path = work_dir().join("restore.log");
    let log = fs::File::create(&log_path)
        .with_context(|| format!("cannot create {}", log_path.display()))?;
    let mut runner = Runner::new(Box::new(log));
    let client = session.client();
    let job = Job {
        plan: &plan,
        layout: &layout,
        client: &client,
        snapshot,
        luks_passphrase: luks,
        dpkg_selections: selections.as_deref(),
    };
    match exec::execute(&job, &mut runner, &mut |m| eprintln!("{m}")) {
        Ok(report) => {
            eprintln!();
            for n in &report.notes {
                eprintln!("  - {n}");
            }
            for w in warnings.iter().chain(&report.warnings) {
                eprintln!("  Warning: {w}");
            }
            eprintln!();
            eprintln!(
                "Restore finished. Report inside the restored system: \
                 /var/lib/nestegg/rescue-report.txt"
            );
            eprintln!("Log of all commands: {}", log_path.display());
            Ok(0)
        }
        Err(e) => {
            eprintln!();
            eprintln!("Restore FAILED: {e:#}");
            eprintln!("Details: {}", log_path.display());
            Ok(1)
        }
    }
}

fn show_network() {
    let out = std::process::Command::new("ip")
        .args(["-brief", "address", "show", "scope", "global"])
        .output();
    let text = out
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    if text.trim().is_empty() {
        eprintln!("No network connection yet. Plug in a cable, or run `nmtui` for Wi-Fi.");
    } else {
        eprintln!("Network:\n{}", text.trim_end());
    }
}

fn wizard() -> Result<u8> {
    require_root()?;
    eprintln!();
    eprintln!("nestegg rescue: restore a computer from its backup");
    eprintln!("====================================================");
    eprintln!("This erases the disk you choose and puts the backed-up system on it.");
    eprintln!("Nothing is changed before you confirm the final plan.");
    eprintln!();
    show_network();
    eprintln!();

    let (keys, found) = scan_media();
    let mut settings = match found {
        Some((path, s)) => {
            eprintln!("Found settings in {}.", path.display());
            s
        }
        None => Settings {
            port: 8007,
            ..Default::default()
        },
    };

    // Connection, repeated until the PBS answers.
    let (session, backup_id) = loop {
        eprintln!("Step 1/4: backup server");
        settings.server = ask("PBS address (host name or IP)", Some(&settings.server))?;
        settings.port = ask("Port", Some(&settings.port.to_string()))?
            .parse()
            .unwrap_or(8007);
        settings.datastore = ask("Datastore", Some(&settings.datastore))?;
        let ns = ask(
            "Namespace (empty = none)",
            Some(settings.namespace.as_deref().unwrap_or("-")),
        )?;
        settings.namespace = (ns != "-" && !ns.is_empty()).then_some(ns);
        settings.auth_id = ask(
            "User or API token (user@realm!token)",
            Some(&settings.auth_id),
        )?;
        let mac = ask(
            "MAC address for Wake-on-LAN (empty = none)",
            Some(settings.wake_mac.as_deref().unwrap_or("-")),
        )?;
        settings.wake_mac = (mac != "-" && !mac.is_empty()).then_some(mac);
        // Wake a sleeping PBS before asking it for its certificate.
        if let Ok(c) = settings.to_config()
            && let Err(e) = crate::wake::ensure_available(&c, |ev| eprintln!("{ev}"))
        {
            eprintln!("Wake-on-LAN failed: {e:#}");
        }
        match fetch_fingerprint(&settings.server, settings.port) {
            Ok(fp) => {
                eprintln!("Certificate fingerprint of the server:\n  {fp}");
                if settings.fingerprint.as_deref() == Some(fp.as_str()) {
                    eprintln!("(matches the saved settings)");
                } else if ask_yes(
                    "Does it match the fingerprint on the PBS dashboard? Answer no if the PBS has a trusted certificate",
                    true,
                )? {
                    settings.fingerprint = Some(fp);
                } else {
                    settings.fingerprint = None;
                }
            }
            Err(e) => eprintln!("Could not read the certificate: {e:#}"),
        }
        let password = ask_secret("Password or API token secret")?;
        let key = choose_key(&keys)?;
        let session = match Session::new(&settings, &password, key.as_ref()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Invalid settings: {e:#}\n");
                continue;
            }
        };
        if let Err(e) = ensure_reachable(&session) {
            eprintln!("{e:#}\n");
            continue;
        }
        let machines = match session.machines() {
            Ok(m) if !m.is_empty() => m,
            Ok(_) => {
                eprintln!("No backups found with these settings.\n");
                continue;
            }
            Err(e) => {
                eprintln!("Login or listing failed: {e:#}\n");
                continue;
            }
        };
        eprintln!();
        eprintln!("Step 2/4: computer");
        let default = settings
            .backup_id
            .as_ref()
            .and_then(|id| machines.iter().position(|m| m == id))
            .unwrap_or(0);
        let i = choose("Which computer", &machines, default)?;
        break (session, machines[i].clone());
    };

    let snaps = session.system_snapshots(&backup_id)?;
    if snaps.is_empty() {
        bail!(
            "'{backup_id}' has no full system backup. Enable `system = true` in its nestegg \
             configuration; until then single files can be restored with `nestegg restore`."
        );
    }
    let items: Vec<String> = snaps
        .iter()
        .take(15)
        .map(|s| {
            format!(
                "{}  {}{}",
                s.time,
                s.size_bytes.map(human).unwrap_or_default(),
                if s.verified.as_deref() == Some("ok") {
                    "  verified"
                } else {
                    ""
                }
            )
        })
        .collect();
    eprintln!("Backups of {backup_id} (newest first):");
    let i = choose("Which backup", &items, 0)?;
    let snapshot = snaps[i].snapshot.clone();
    let archive_bytes = system_archive_bytes(&snaps, &snapshot);

    eprintln!();
    eprintln!("Step 3/4: target disk (it will be ERASED)");
    let disks: Vec<Disk> = list_disks()?.into_iter().filter(|d| !d.in_use).collect();
    if disks.is_empty() {
        bail!("no usable disk found (the disk of the rescue stick is not offered)");
    }
    let items: Vec<String> = disks.iter().map(disk_line).collect();
    let d = choose("Which disk", &items, 0)?;
    let disk = disks[d].clone();

    eprintln!();
    eprintln!("Step 4/4: encryption");
    let luks = if ask_yes(
        "Encrypt the restored disk (passphrase at every start)?",
        false,
    )? {
        loop {
            let a = ask_secret("New disk passphrase")?;
            let b = ask_secret("Repeat the passphrase")?;
            if a.as_bytes() == b.as_bytes() {
                break Some(a);
            }
            eprintln!("The passphrases differ, please try again.");
        }
    } else {
        None
    };
    let name = disk.path.trim_start_matches("/dev/").to_string();
    let code = run_restore(
        &session,
        &snapshot,
        archive_bytes,
        &disk,
        luks.as_ref(),
        false,
        &mut |_, _| {
            let typed = ask(
                &format!("Type '{name}' to erase {} and start the restore", disk.path),
                Some(""),
            )?;
            Ok(typed == name)
        },
    )?;
    if code == 0 && ask_yes("Remove the rescue stick and restart now?", true)? {
        let _ = std::process::Command::new("systemctl")
            .arg("reboot")
            .status();
    }
    Ok(code)
}

fn choose_key(found: &[PathBuf]) -> Result<Option<Secret>> {
    let mut default = found
        .first()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "-".into());
    if !found.is_empty() {
        eprintln!("Key files found on attached media:");
        for k in found {
            eprintln!("  {}", k.display());
        }
    }
    loop {
        let path = ask(
            "Exported encryption key file ('-' = backup is not encrypted)",
            Some(&default),
        )?;
        if path == "-" {
            return Ok(None);
        }
        match read_key_file(Path::new(&path)) {
            Ok(k) => return Ok(Some(k)),
            Err(e) => {
                eprintln!("{e:#}");
                default = "-".into();
            }
        }
    }
}
