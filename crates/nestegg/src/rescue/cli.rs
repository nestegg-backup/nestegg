//! `nestegg rescue` commands: the interactive wizard of the rescue stick,
//! a non-interactive restore for automation, and `export-settings` for the
//! normal system.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use super::exec::{self, Job, Runner};
use super::guard;
use super::plan::{Target, human, make_plan};
use super::{
    Disk, FoundSettings, Inventory, Protection, Session, Settings, ask, ask_secret, ask_yes,
    booted_uefi, choose, fetch_fingerprint, list_disks, paperkey, protection_label, scan_media,
    work_dir,
};
use crate::client::RestoreKey;
use crate::secrets::Secret;

/// Printed when the key check passed.
const KEY_FITS: &str = "The key fits this backup: its signature was checked with the key, and \
                        everything that is restored is encrypted with it.";

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
    /// Exported encryption key or saved paper key (omit for unencrypted
    /// backups).
    #[arg(long)]
    pub keyfile: Option<PathBuf>,
    /// With --keyfile: also restore a backup that is not encrypted and
    /// signed with that key (refused otherwise: anyone with write access to
    /// the backup server could have made or changed such a backup).
    #[arg(long)]
    pub allow_unprotected_backup: bool,
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
    /// Allow erasing a disk that is not empty: it carries a file system,
    /// RAID/LVM/ZFS/LUKS signature or partitions, e.g. a data disk of the
    /// backed-up computer (refused otherwise).
    #[arg(long)]
    pub erase_data_disk: bool,
}

pub fn run(cmd: Option<RescueCmd>) -> Result<u8> {
    match cmd.unwrap_or(RescueCmd::Wizard) {
        RescueCmd::Wizard => wizard(),
        RescueCmd::Restore(args) => restore(*args),
        RescueCmd::Disks => {
            for d in list_disks()? {
                let a = guard::assess(&d, &[], &[]);
                println!("{}", disk_entry(&d, &a, None, ""));
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

/// One entry of the disk list: path, size, model, serial, a short verdict
/// and, for a disk that is not empty, what was found on it.
fn disk_entry(d: &Disk, a: &guard::Assessment, fits: Option<bool>, backup_time: &str) -> String {
    let mut marks = Vec::new();
    if d.in_use {
        marks.push("in use");
    }
    if a.of_backed_up_computer() {
        marks.push("HOLDS DATA OF THE BACKED-UP COMPUTER");
    } else if a.holds_data() {
        marks.push("NOT EMPTY");
    } else {
        marks.push("empty");
    }
    if fits == Some(false) {
        marks.push("too small");
    }
    let mut s = format!(
        "{:<16} {:>10}  {:<6} {}{}  [{}]",
        d.path,
        human(d.size_bytes),
        d.transport,
        d.model,
        if d.serial.is_empty() {
            String::new()
        } else {
            format!("  serial {}", d.serial)
        },
        marks.join(", ")
    );
    for l in a.lines(backup_time) {
        s.push_str(&format!("\n        - {l}"));
    }
    s
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
    let backup_time = snaps
        .iter()
        .find(|s| s.snapshot == snapshot)
        .map_or(snapshot.clone(), |s| s.time.clone());
    // The key is checked before any disk is looked at.
    eprintln!("Checking the key against {snapshot} ...");
    let protection = session
        .check_key(&snapshot)
        .map_err(|e| anyhow::anyhow!("{e:#}\nNothing was changed."))?;
    match &protection {
        Protection::KeyMissing => bail!(
            "{snapshot} is encrypted: give its key with --keyfile (an exported key file or a \
             saved paper key). Nothing was changed."
        ),
        Protection::Encrypted => eprintln!("{KEY_FITS}"),
        p => {
            if let Some(concern) = p.concern() {
                if !a.allow_unprotected_backup {
                    bail!(
                        "{snapshot}: {concern}\nNothing was changed. Only if you know that \
                         this backup was made this way, restore it with \
                         --allow-unprotected-backup."
                    );
                }
                eprintln!(
                    "Warning: {snapshot}: {concern} Restoring it anyway \
                     (--allow-unprotected-backup)."
                );
            }
        }
    }
    let restore_key = protection.restore_key(session.key());
    eprintln!("Reading the disk layout of {snapshot} ...");
    let inventory = session.inventory(&snapshot, restore_key.duplicate())?;
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
    let assessment = guard::assess(disk, &inventory.recorded_ids(), &[]);
    run_restore(
        &session,
        &snapshot,
        &backup_time,
        archive_bytes,
        &inventory,
        &restore_key,
        disk,
        &assessment,
        luks.as_ref(),
        a.dry_run,
        &mut |disk, found| {
            if found.holds_data() && !a.erase_data_disk {
                bail!(
                    "{} is not empty ({}); nothing was changed. \
                     Use --erase-data-disk if it really should be erased.",
                    disk.path,
                    found.lines(&backup_time).join("; ")
                );
            }
            Ok(true)
        },
    )
}

/// An exported key file or a saved paper key (text or html).
fn read_key_file(path: &Path) -> Result<Secret> {
    let key = super::read_key(path)?;
    if let Some(note) = &key.note {
        eprintln!("{}: {note}", path.display());
    }
    Ok(Secret::new(key.json.into_bytes()))
}

/// Does this error from reading a backup come from the encryption key
/// (wrong, missing, or not usable)? "error decoding base64 data" is what
/// the client says about a damaged `data` value.
fn is_key_problem(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}").to_lowercase();
    ["key", "decrypt", "signature", "base64"]
        .iter()
        .any(|w| m.contains(w))
}

/// A certificate fingerprint read from a settings file, with the file it
/// came from.
struct SavedFingerprint<'a> {
    fingerprint: &'a str,
    origin: &'a str,
}

fn same_fingerprint(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Decide whether to trust the certificate the server presented.
/// Returns the fingerprint to pin, or `None` to rely on the certificate
/// authorities of the system. Trusting needs an explicit yes: pressing Enter
/// must never trust a certificate nobody has compared. A fingerprint from a
/// settings file is no exception (any attached drive can carry one); only a
/// certificate the user already trusted in this session is taken again
/// without a question.
fn decide_fingerprint(
    fp: &str,
    saved: Option<SavedFingerprint<'_>>,
    trusted: Option<&str>,
    yes_no: &mut dyn FnMut(&str, bool) -> Result<bool>,
) -> Result<Option<String>> {
    if trusted.is_some_and(|t| same_fingerprint(t, fp)) {
        eprintln!("(this is the certificate you trusted before)");
        return Ok(Some(fp.to_string()));
    }
    match &saved {
        Some(s) if same_fingerprint(s.fingerprint, fp) => {
            eprintln!(
                "It matches the fingerprint saved in {}. Trust it only if that file is yours.",
                s.origin
            );
        }
        Some(s) => {
            eprintln!();
            eprintln!(
                "!!! WARNING: this is NOT the certificate fingerprint saved in {}:",
                s.origin
            );
            eprintln!("!!!   saved:  {}", s.fingerprint);
            eprintln!("!!!   server: {fp}");
            eprintln!(
                "!!! Either the certificate of the PBS was replaced, or this is not your PBS."
            );
            eprintln!(
                "Compare it with the fingerprint on the PBS dashboard (\"Show Fingerprint\")."
            );
        }
        None => {
            eprintln!(
                "Compare it with the fingerprint on the PBS dashboard (\"Show Fingerprint\")."
            );
        }
    }
    eprintln!("Answer no only if the PBS has a certificate from a trusted authority.");
    if yes_no("Trust this certificate?", false)? {
        Ok(Some(fp.to_string()))
    } else {
        Ok(None)
    }
}

/// Which of the settings files found to use; `None`: enter everything.
fn choose_settings(found: &[FoundSettings]) -> Result<Option<&FoundSettings>> {
    match found {
        [] => Ok(None),
        [one] => {
            eprintln!("Found settings in {}.", one.origin);
            Ok(Some(one))
        }
        many => {
            eprintln!("Found {} settings files:", many.len());
            let mut items: Vec<String> = many
                .iter()
                .map(|f| {
                    format!(
                        "{}  (server {}, datastore {}{})",
                        f.origin,
                        f.settings.server,
                        f.settings.datastore,
                        f.settings
                            .backup_id
                            .as_deref()
                            .map(|b| format!(", computer {b}"))
                            .unwrap_or_default()
                    )
                })
                .collect();
            items.push("none: enter everything by hand".into());
            let i = choose("Which settings", &items, Some(0))?;
            Ok(many.get(i))
        }
    }
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

fn target(disk: &Disk, uefi: bool, archive_bytes: Option<u64>) -> Target {
    Target {
        disk: disk.path.clone(),
        size_bytes: disk.size_bytes,
        uefi,
        archive_bytes,
    }
}

/// Shared by the wizard and the non-interactive command.
#[allow(clippy::too_many_arguments)]
fn run_restore(
    session: &Session,
    snapshot: &str,
    backup_time: &str,
    archive_bytes: Option<u64>,
    inventory: &Inventory,
    restore_key: &RestoreKey,
    disk: &Disk,
    assessment: &guard::Assessment,
    luks: Option<&Secret>,
    dry_run: bool,
    // (disk, what would be destroyed on it) → go ahead?
    confirm: &mut dyn FnMut(&Disk, &guard::Assessment) -> Result<bool>,
) -> Result<u8> {
    let layout = &inventory.layout;
    let selections = &inventory.selections;
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
        layout,
        &target(disk, booted_uefi(), archive_bytes),
        luks.is_some(),
    )?;
    let mut warnings = exec::preflight(&plan, selections.as_deref())?;
    exec::check_disk_free(&plan.disk)?;
    let contents = exec::disk_contents(&plan.disk);
    // Something of the backed-up computer (a surviving /home or data disk,
    // a RAID member, ...) must never be overwritten by accident.
    for f in assessment.findings.iter().filter(|f| {
        matches!(
            f.scope,
            Some(guard::Scope::Data) | Some(guard::Scope::System)
        )
    }) {
        warnings.push(format!(
            "THIS DISK HOLDS {} OF THE BACKED-UP COMPUTER ({}). Restoring erases it.",
            f.recorded.join(", "),
            f.device
        ));
    }
    eprintln!();
    eprintln!("{}", plan.describe());
    if !contents.trim().is_empty() {
        eprintln!("Current content of {} (will be erased):", plan.disk);
        eprintln!("{}", contents.trim_end());
    }
    if assessment.holds_data() {
        eprintln!("{}", guard::warning(disk, assessment, backup_time));
    }
    for w in &warnings {
        eprintln!("Warning: {w}");
    }
    if dry_run {
        eprintln!("Dry run: nothing was changed.");
        return Ok(0);
    }
    if !confirm(disk, assessment)? {
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
        layout,
        client: &client,
        snapshot,
        key: restore_key,
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

    eprintln!("Looking for settings and keys on the attached drives ...");
    let media = scan_media();
    for note in &media.notes {
        eprintln!("{note}");
    }
    let keys = media.keys.clone();
    let chosen = choose_settings(&media.settings)?;
    let mut settings = match chosen {
        Some(f) => f.settings.clone(),
        None => Settings {
            port: 8007,
            ..Default::default()
        },
    };
    // A certificate fingerprint from a settings file is only a suggestion;
    // `trusted` is what the user confirmed in this session.
    let file_fingerprint = chosen.and_then(|f| {
        f.settings
            .fingerprint
            .clone()
            .filter(|fp| !fp.is_empty())
            .map(|fp| (fp, f.origin.clone()))
    });
    let mut trusted: Option<String> = None;
    settings.fingerprint = None;

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
                let saved =
                    file_fingerprint
                        .as_ref()
                        .map(|(fingerprint, origin)| SavedFingerprint {
                            fingerprint,
                            origin,
                        });
                trusted = decide_fingerprint(&fp, saved, trusted.as_deref(), &mut ask_yes)?;
            }
            Err(e) => eprintln!("Could not read the certificate: {e:#}"),
        }
        // Only a confirmed fingerprint is ever pinned (never one taken
        // unconfirmed from a settings file).
        settings.fingerprint = trusted.clone();
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
                eprintln!("Login or listing failed: {e:#}");
                if settings.fingerprint.is_none() {
                    eprintln!(
                        "(No certificate was trusted. If the PBS uses its own certificate, \
                         answer yes to \"Trust this certificate?\" next time.)"
                    );
                }
                eprintln!();
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
        let i = choose("Which computer", &machines, Some(default))?;
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
            let label = protection_label(s);
            format!(
                "{}  {}{}{}",
                s.time,
                s.size_bytes.map(human).unwrap_or_default(),
                if label.is_empty() {
                    String::new()
                } else {
                    format!("  {label}")
                },
                if s.verified.as_deref() == Some("ok") {
                    "  verified"
                } else {
                    ""
                }
            )
        })
        .collect();
    // The key is checked against the chosen backup right here, before any
    // disk is looked at, let alone changed: the manifest is read with the
    // key (its signature is verified) and every archive that will be
    // restored must be encrypted. Then the disk layout is read.
    let (snapshot, backup_time, archive_bytes, inventory, restore_key) = 'backup: loop {
        eprintln!("Backups of {backup_id} (newest first):");
        let i = choose("Which backup", &items, Some(0))?;
        let snapshot = snaps[i].snapshot.clone();
        let backup_time = snaps[i].time.clone();
        let archive_bytes = system_archive_bytes(&snaps, &snapshot);
        loop {
            eprintln!("Reading the disk layout of {snapshot} (this checks the key) ...");
            let failure = match prepare_backup(&session, &snapshot, &mut ask_yes) {
                Ok((inventory, restore_key)) => {
                    break 'backup (snapshot, backup_time, archive_bytes, inventory, restore_key);
                }
                Err(f) => f,
            };
            eprintln!();
            let default = match &failure {
                Failure::Key(e) => {
                    eprintln!("{e:#}");
                    eprintln!("The encryption key does not fit this backup (or none was given).");
                    0
                }
                Failure::Declined => {
                    eprintln!("This backup is not restored.");
                    1
                }
                Failure::Other(e) => {
                    eprintln!("{e:#}");
                    eprintln!("The backup could not be read.");
                    2
                }
            };
            eprintln!("Nothing was changed.");
            let options = [
                "Enter or select another key".to_string(),
                "Choose another backup".to_string(),
                "Try again with the same key".to_string(),
                "Quit without changing anything".to_string(),
            ];
            match choose("What now", &options, Some(default))? {
                0 => {
                    let key = choose_key(&keys)?;
                    session.set_key(key.as_ref())?;
                }
                1 => continue 'backup,
                2 => {}
                _ => {
                    eprintln!("Stopped, nothing was changed.");
                    return Ok(1);
                }
            }
        }
    };
    let recorded = inventory.recorded_ids();

    eprintln!();
    eprintln!("Step 3/4: target disk (it will be ERASED)");
    let disks: Vec<Disk> = list_disks()?.into_iter().filter(|d| !d.in_use).collect();
    if disks.is_empty() {
        bail!("no usable disk found (the disk of the rescue stick is not offered)");
    }
    let uefi = booted_uefi();
    // Problems no disk size can fix (unsupported layout or boot loader, ...)
    // are reported now, instead of listing every disk as too small.
    let unlimited = Target {
        size_bytes: 1 << 60,
        ..target(&disks[0], uefi, archive_bytes)
    };
    make_plan(&inventory.layout, &unlimited, false)
        .context("this backup cannot be restored onto any disk")?;
    let rated: Vec<(guard::Assessment, bool)> = disks
        .iter()
        .map(|d| {
            let fits = make_plan(&inventory.layout, &target(d, uefi, archive_bytes), false).is_ok();
            (guard::assess(d, &recorded, &media.devices), fits)
        })
        .collect();
    let items: Vec<String> = disks
        .iter()
        .zip(&rated)
        .map(|(d, (a, fits))| disk_entry(d, a, Some(*fits), &backup_time))
        .collect();
    let default = guard::preselect(
        &rated
            .iter()
            .map(|(a, fits)| guard::DiskChoice {
                holds_data: a.holds_data(),
                fits: *fits,
            })
            .collect::<Vec<_>>(),
    );
    if default.is_none() {
        eprintln!("No disk is pre-selected: check the list and type the number of the disk.");
    }
    let (disk, assessment) = loop {
        let d = choose("Which disk", &items, default)?;
        let (a, _) = &rated[d];
        if !a.holds_data() {
            break (disks[d].clone(), a.clone());
        }
        eprintln!();
        eprintln!("{}", guard::warning(&disks[d], a, &backup_time));
        if ask_yes("Use this disk anyway?", false)? {
            break (disks[d].clone(), a.clone());
        }
        eprintln!();
    };

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
        &backup_time,
        archive_bytes,
        &inventory,
        &restore_key,
        &disk,
        &assessment,
        luks.as_ref(),
        false,
        &mut |disk, a| {
            let typed = ask(
                &format!("Type '{name}' to erase {} and start the restore", disk.path),
                Some(""),
            )?;
            if typed != name {
                return Ok(false);
            }
            if !a.holds_data() {
                return Ok(true);
            }
            // Second, explicit confirmation for a disk that is not empty.
            let phrase = guard::erase_phrase(disk);
            let typed = ask(
                &format!(
                    "{} ({}) is NOT empty, see the list above. Type '{phrase}' to erase it anyway",
                    disk.path,
                    guard::identity(disk)
                ),
                Some(""),
            )?;
            Ok(typed == phrase)
        },
    )?;
    if code == 0 && ask_yes("Remove the rescue stick and restart now?", true)? {
        let _ = std::process::Command::new("systemctl")
            .arg("reboot")
            .status();
    }
    Ok(code)
}

/// Why the chosen backup is not restored (yet).
enum Failure {
    /// The key does not open it, or none was given for an encrypted backup.
    Key(anyhow::Error),
    /// Not protected by the key, and the user did not accept that.
    Declined,
    Other(anyhow::Error),
}

/// Check the key against `snapshot` (see [`Session::check_key`]), ask before
/// accepting a backup the key does not protect, and read the disk layout.
fn prepare_backup(
    session: &Session,
    snapshot: &str,
    yes_no: &mut dyn FnMut(&str, bool) -> Result<bool>,
) -> std::result::Result<(Inventory, RestoreKey), Failure> {
    let classify = |e: anyhow::Error| {
        if is_key_problem(&e) {
            Failure::Key(e)
        } else {
            Failure::Other(e)
        }
    };
    let protection = session.check_key(snapshot).map_err(classify)?;
    if !accept_protection(&protection, yes_no).map_err(Failure::Other)? {
        return Err(if protection == Protection::KeyMissing {
            Failure::Key(anyhow::anyhow!(
                "this backup is encrypted, but no encryption key was given"
            ))
        } else {
            Failure::Declined
        });
    }
    let restore_key = protection.restore_key(session.key());
    let inventory = session
        .inventory(snapshot, restore_key.duplicate())
        .map_err(classify)?;
    Ok((inventory, restore_key))
}

/// Tell what the key check found; `true` to go on. A backup the key does
/// not protect needs an explicit yes (Enter means no).
fn accept_protection(
    p: &Protection,
    yes_no: &mut dyn FnMut(&str, bool) -> Result<bool>,
) -> Result<bool> {
    match p {
        Protection::Plain => Ok(true),
        Protection::Encrypted => {
            eprintln!("{KEY_FITS}");
            Ok(true)
        }
        Protection::KeyMissing => Ok(false),
        other => {
            let concern = other.concern().unwrap_or_default();
            eprintln!();
            eprintln!("!!! WARNING: {concern}");
            eprintln!(
                "!!! Restore it only if you know that this backup was made this way. Your key \
                 cannot vouch for it."
            );
            yes_no("Restore this backup anyway?", false)
        }
    }
}

/// Where the encryption key comes from: a key file or saved paper key on
/// attached media or at a given path, or the paper key typed by hand.
fn choose_key(found: &[PathBuf]) -> Result<Option<Secret>> {
    let mut default = found
        .first()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "type".into());
    if !found.is_empty() {
        eprintln!("Key files and paper keys found on attached media:");
        for k in found {
            eprintln!("  {}", k.display());
        }
    }
    loop {
        eprintln!(
            "Encryption key: the path of a key file or a saved paper key, \
             'type' to type the paper key by hand, '-' if the backup is not encrypted."
        );
        let answer = ask("Encryption key", Some(&default))?;
        if answer == "-" {
            return Ok(None);
        }
        if answer.eq_ignore_ascii_case("type") {
            if let Some(k) = type_key()? {
                return Ok(Some(k));
            }
            continue;
        }
        match read_key_file(Path::new(&answer)) {
            Ok(k) => return Ok(Some(k)),
            Err(e) => {
                eprintln!("{e:#}");
                default = "type".into();
            }
        }
    }
}

/// Ask for the `data` line of a printed paper key. `None`: back to the
/// choice of the key source.
fn type_key() -> Result<Option<Secret>> {
    eprintln!();
    eprintln!("Type the paper key. Only the \"data\" line of it is needed:");
    eprintln!("    \"data\": \"RVhBTVBMRS1LRVktTk9ULUEtUkVBTC1LRVktMDEyMzQ=\",");
    eprintln!(
        "Type the {} characters between the quotes, as one line. Upper and lower case",
        paperkey::DATA_CHARS
    );
    eprintln!(
        "matter, so do + / and the = at the end. Mind l (small L), I (capital i), 1, O and 0."
    );
    eprintln!(
        "Quotes, commas and blanks are ignored. The text stays visible so you can compare it."
    );
    eprintln!("This console uses a US keyboard layout: on other keyboards + / = and quotes are on");
    eprintln!("different keys. To change it, leave with Ctrl+C and run (example: German):");
    eprintln!(
        "    sed -i 's/^XKBLAYOUT=.*/XKBLAYOUT=\"de\"/' /etc/default/keyboard; setupcon -k --force"
    );
    loop {
        let line = ask("Key (empty line = back)", Some(""))?;
        if line.is_empty() {
            return Ok(None);
        }
        match paperkey::from_typed(&line) {
            Ok(k) => {
                eprintln!("Key accepted. It is checked against the backup you choose next.");
                return Ok(Some(Secret::new(k.json.into_bytes())));
            }
            Err(e) => eprintln!("{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved<'a>(fingerprint: &'a str) -> Option<SavedFingerprint<'a>> {
        Some(SavedFingerprint {
            fingerprint,
            origin: "/media/usb/nestegg-rescue.toml",
        })
    }

    #[test]
    fn enter_does_not_trust_a_certificate() {
        // Pressing Enter gives the default of the question.
        let mut enter = |_: &str, default_yes: bool| Ok(default_yes);
        assert_eq!(
            decide_fingerprint("aa:bb", None, None, &mut enter).unwrap(),
            None
        );
        assert_eq!(
            decide_fingerprint("aa:bb", saved("cc:dd"), None, &mut enter).unwrap(),
            None
        );
        // Not even when it matches a settings file: any attached drive can
        // carry one.
        assert_eq!(
            decide_fingerprint("aa:bb", saved("AA:BB"), None, &mut enter).unwrap(),
            None
        );
    }

    #[test]
    fn explicit_yes_trusts_and_a_trusted_certificate_is_not_asked_again() {
        let mut asked = 0;
        let mut yes = |_: &str, _: bool| {
            asked += 1;
            Ok(true)
        };
        for s in [None, saved("aa:bb"), saved("cc:dd")] {
            assert_eq!(
                decide_fingerprint("aa:bb", s, None, &mut yes)
                    .unwrap()
                    .as_deref(),
                Some("aa:bb")
            );
        }
        assert_eq!(asked, 3, "a settings file must not spare the question");
        let mut never = |_: &str, _: bool| -> Result<bool> { panic!("must not ask") };
        assert_eq!(
            decide_fingerprint("aa:bb", saved("aa:bb"), Some("aa:bb"), &mut never)
                .unwrap()
                .as_deref(),
            Some("aa:bb")
        );
        // Trusted before, but the server now shows another certificate.
        let mut no = |_: &str, _: bool| Ok(false);
        assert_eq!(
            decide_fingerprint("ee:ff", None, Some("aa:bb"), &mut no).unwrap(),
            None
        );
    }

    #[test]
    fn an_unprotected_backup_needs_an_explicit_yes() {
        let mut enter = |_: &str, default_yes: bool| Ok(default_yes);
        let mut yes = |_: &str, _: bool| Ok(true);
        let mut never = |_: &str, _: bool| -> Result<bool> { panic!("must not ask") };
        for p in [
            Protection::Unsigned,
            Protection::NotEncrypted(vec![("system.pxar".into(), "sign-only".into())]),
        ] {
            assert!(!accept_protection(&p, &mut enter).unwrap(), "{p:?}");
            assert!(accept_protection(&p, &mut yes).unwrap(), "{p:?}");
        }
        assert!(accept_protection(&Protection::Encrypted, &mut never).unwrap());
        // Without a key, an unencrypted backup is restored as before.
        assert!(accept_protection(&Protection::Plain, &mut never).unwrap());
        assert!(!accept_protection(&Protection::KeyMissing, &mut never).unwrap());
    }

    #[test]
    fn key_problems_are_recognised() {
        for m in [
            "wrong key - unable to verify signature since manifest's key e4:84 does not match provided key 87:2f",
            "missing key - manifest was created with key e4:84",
            "this archive is encrypted or signed; pass the exported key with --keyfile",
            "error decoding base64 data at line 1 column 143",
            "KeyConfig contains wrong fingerprint 00:11:22:33, contained key has fingerprint d7:99",
            "manifest has no signature, but an encryption key is set - refusing to trust a \
             potentially tampered, unsigned backup",
        ] {
            assert!(is_key_problem(&anyhow::anyhow!("{m}")), "{m}");
        }
        let wrapped =
            anyhow::anyhow!("wrong key").context("cannot read the inventory of this snapshot");
        assert!(is_key_problem(&wrapped));
        assert!(!is_key_problem(&anyhow::anyhow!("connection refused")));
        assert!(!is_key_problem(&anyhow::anyhow!(
            "this snapshot has no disk layout (made by an older nestegg)"
        )));
    }

    #[test]
    fn key_file_reader_takes_paper_keys_and_refuses_protected_keys() {
        let dir = tempfile::tempdir().unwrap();
        let data = "RVhBTVBMRS1LRVktTk9ULUEtUkVBTC1LRVktMDEyMzQ=";
        let paper = dir.path().join("paper.txt");
        fs::write(
            &paper,
            format!(
                "{}\n{{\"kdf\":null,\"created\":\"a\",\"modified\":\"b\",\"data\":\"{data}\"}}\n{}\n",
                paperkey::BEGIN,
                paperkey::END
            ),
        )
        .unwrap();
        let secret = read_key_file(&paper).unwrap();
        assert!(String::from_utf8_lossy(secret.as_bytes()).contains(data));
        let protected = dir.path().join("protected.json");
        fs::write(
            &protected,
            format!(
                r#"{{"kdf":{{"Scrypt":{{"n":1}}}},"created":"a","modified":"b","data":"{data}"}}"#
            ),
        )
        .unwrap();
        let e = read_key_file(&protected).unwrap_err().to_string();
        assert!(e.contains("protected with a password"), "{e}");
        let e = read_key_file(&dir.path().join("missing"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("cannot read"), "{e}");
    }
}
