//! Bare-metal restore from the rescue stick (`nestegg rescue`).
//!
//! The rescue system is a small live Linux with nestegg and the official
//! `proxmox-backup-client`. It connects to the PBS, reads `layout.json` and
//! the package list from `inventory.pxar`, partitions an empty disk,
//! restores `system.pxar`, adapts fstab/crypttab and installs GRUB.
//! See `docs/rescue.md`.

pub mod cli;
pub mod exec;
pub mod fixup;
pub mod guard;
pub mod paperkey;
pub mod plan;

use std::fs;
use std::io::{BufRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::client::{Client, RestoreKey, RestoreRequest, SnapshotInfo};
use crate::config::{self, Config, WakeConfig};
use crate::layout::{LAYOUT_FILE, Layout, SYSTEM_ARCHIVE};
use crate::secrets::{self, Secret, SecretStore};

/// Working directory of the rescue session (tmpfs on the live system).
pub fn work_dir() -> PathBuf {
    std::env::var_os("NESTEGG_RESCUE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/nestegg-rescue"))
}

/// Connection settings without secrets. Written by `nestegg rescue
/// export-settings` on the normal system, read by the wizard if found next
/// to the key file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub server: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub datastore: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub auth_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_mac: Option<String>,
}

fn default_port() -> u16 {
    8007
}

pub const SETTINGS_FILE: &str = "nestegg-rescue.toml";

impl Settings {
    pub fn from_config(c: &Config) -> Settings {
        Settings {
            server: c.pbs.server.clone(),
            port: c.pbs.port,
            datastore: c.pbs.datastore.clone(),
            namespace: c.pbs.namespace.clone(),
            auth_id: c.pbs.auth_id.clone(),
            fingerprint: c.pbs.fingerprint.clone(),
            backup_id: Some(c.effective_backup_id()),
            wake_mac: c.wake.as_ref().map(|w| w.mac.clone()),
        }
    }

    pub fn to_config(&self) -> Result<Config> {
        let mut c = config::example(&self.server, &self.datastore, &self.auth_id);
        c.pbs.port = self.port;
        c.pbs.namespace = self.namespace.clone().filter(|n| !n.is_empty());
        c.pbs.fingerprint = self.fingerprint.clone().filter(|f| !f.is_empty());
        c.backup_id = self.backup_id.clone();
        if let Some(mac) = self.wake_mac.as_ref().filter(|m| !m.is_empty()) {
            c.wake = Some(WakeConfig {
                mac: mac.clone(),
                broadcast: std::net::Ipv4Addr::BROADCAST,
                port: 9,
                timeout_seconds: 300,
                enabled: true,
            });
        }
        c.validate()?;
        Ok(c)
    }
}

/// A connection to the PBS with secrets kept in the rescue work directory.
pub struct Session {
    pub config: Config,
    pub store_dir: PathBuf,
}

impl Session {
    pub fn new(settings: &Settings, password: &Secret, key: Option<&Secret>) -> Result<Session> {
        let config = settings.to_config()?;
        let dir = work_dir();
        secrets::create_private_dir(&dir)?;
        let store_dir = dir.join("credentials");
        secrets::create_private_dir(&store_dir)?;
        secrets::write_private_file(&store_dir.join(secrets::PASSWORD), password.as_bytes())?;
        let key_path = store_dir.join(secrets::ENCRYPTION_KEY);
        match key {
            Some(k) => secrets::write_private_file(&key_path, k.as_bytes())?,
            None => {
                let _ = fs::remove_file(&key_path);
            }
        }
        Ok(Session { config, store_dir })
    }

    /// Replace the key used for this session (after a key was rejected).
    pub fn set_key(&self, key: Option<&Secret>) -> Result<()> {
        let key_path = self.store_dir.join(secrets::ENCRYPTION_KEY);
        match key {
            Some(k) => secrets::write_private_file(&key_path, k.as_bytes()),
            None => {
                let _ = fs::remove_file(&key_path);
                Ok(())
            }
        }
    }

    pub fn client(&self) -> Client<'_> {
        Client::new(&self.config, SecretStore::new(&self.store_dir))
    }

    /// Backup groups (`host/<id>`) that contain a full system backup.
    pub fn machines(&self) -> Result<Vec<String>> {
        let groups = self.client().list_groups()?;
        let mut ids: Vec<String> = groups
            .into_iter()
            .filter(|g| g.backup_type == "host")
            .map(|g| g.backup_id)
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// Snapshots of a machine that can be restored onto an empty disk,
    /// newest first.
    pub fn system_snapshots(&self, backup_id: &str) -> Result<Vec<SnapshotInfo>> {
        let snaps = self
            .client()
            .list_group_snapshots(&format!("host/{backup_id}"))?;
        Ok(snaps.into_iter().filter(has_system_archive).collect())
    }

    /// The key of this session, if one was given.
    pub fn key(&self) -> Option<Secret> {
        SecretStore::new(&self.store_dir)
            .load(secrets::ENCRYPTION_KEY)
            .ok()
    }

    /// Check the key of this session against `snapshot` before anything is
    /// read from it. With a key, the manifest is read with that key (the
    /// client verifies its signature, so a wrong key fails here) and the
    /// crypt mode of every archive the restore reads is taken from that
    /// verified manifest. See [`Protection`].
    pub fn check_key(&self, snapshot: &str) -> Result<Protection> {
        let client = self.client();
        let signed = crate::client::listed_as_signed(&client.snapshot_files(snapshot)?);
        let Some(key) = self.key() else {
            return Ok(if signed {
                Protection::KeyMissing
            } else {
                Protection::Plain
            });
        };
        if !signed {
            return Ok(Protection::Unsigned);
        }
        match client.read_manifest_with_key(snapshot, &key) {
            Ok(manifest) => protection_from_manifest(&manifest, RESTORED_ARCHIVES),
            // The server listed signed files, but the manifest has no
            // signature: the same as not signed at all.
            Err(e) if format!("{e:#}").contains("manifest has no signature") => {
                Ok(Protection::Unsigned)
            }
            Err(e) => Err(e.context(format!("the key does not open {snapshot}"))),
        }
    }

    /// Read `layout.json`, the package list and the storage records of a
    /// snapshot. `key` comes from [`Protection::restore_key`].
    pub fn inventory(&self, snapshot: &str, key: RestoreKey) -> Result<Inventory> {
        let base = work_dir().join("inventory");
        let _ = fs::remove_dir_all(&base);
        let mut sink = std::io::sink();
        self.client()
            .restore(
                &RestoreRequest {
                    snapshot,
                    archive: "inventory.pxar",
                    target: &base,
                    patterns: &[],
                    key,
                },
                &mut sink,
            )
            .context("cannot read the inventory of this snapshot")?;
        let layout: Layout = serde_json::from_slice(
            &fs::read(base.join(LAYOUT_FILE))
                .context("this snapshot has no disk layout (made by an older nestegg)")?,
        )
        .context("invalid layout.json")?;
        let read = |name: &str| fs::read_to_string(base.join(name)).ok();
        Ok(Inventory {
            layout,
            selections: read("dpkg-selections.txt"),
            fstab: read("fstab"),
            crypttab: read("crypttab"),
            lsblk: read("lsblk.json"),
        })
    }
}

/// What the rescue system reads from a snapshot's `inventory.pxar`.
pub struct Inventory {
    pub layout: Layout,
    /// `dpkg --get-selections` of the backed-up system.
    pub selections: Option<String>,
    /// `/etc/fstab`, `/etc/crypttab` and `lsblk --json` of the backed-up
    /// system: used to recognise its surviving disks.
    pub fstab: Option<String>,
    pub crypttab: Option<String>,
    pub lsblk: Option<String>,
}

impl Inventory {
    pub fn recorded_ids(&self) -> Vec<guard::Recorded> {
        guard::recorded_ids(
            &self.layout,
            self.fstab.as_deref(),
            self.crypttab.as_deref(),
            self.lsblk.as_deref(),
        )
    }
}

/// The archives a restore reads: all of them must be protected by the key.
pub const RESTORED_ARCHIVES: &[&str] = &["inventory.pxar", "system.pxar"];

/// How the key relates to a snapshot (see [`Session::check_key`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protection {
    /// No key was given and the backup is neither signed nor encrypted:
    /// restored as it is (as before keys were checked).
    Plain,
    /// No key was given, but the backup is signed or encrypted.
    KeyMissing,
    /// The manifest's signature was verified with the key and every archive
    /// the restore reads is encrypted: the key fits.
    Encrypted,
    /// A key was given, but the backup is neither signed nor encrypted. It
    /// is not protected by the key: anyone with write access to the backup
    /// server could have made or changed it.
    Unsigned,
    /// The signature was verified with the key, but these archives
    /// (name, crypt mode) are not encrypted.
    NotEncrypted(Vec<(String, String)>),
}

impl Protection {
    /// What is wrong with restoring this backup with a key at hand, or
    /// `None` if nothing is. Restoring despite it needs an explicit yes.
    pub fn concern(&self) -> Option<String> {
        match self {
            Protection::Plain | Protection::Encrypted => None,
            Protection::KeyMissing => {
                Some("this backup is encrypted, but no encryption key was given.".into())
            }
            Protection::Unsigned => Some(
                "this backup is NOT encrypted and NOT signed, so it is not protected by your \
                 key: anyone with access to the backup server could have changed it."
                    .into(),
            ),
            Protection::NotEncrypted(list) => Some(format!(
                "this backup is signed with your key (checked), but {} {} not encrypted. \
                 nestegg encrypts every archive when a key is set, so this backup was made \
                 in another way.",
                list.iter()
                    .map(|(a, m)| format!("{a} ({m})"))
                    .collect::<Vec<_>>()
                    .join(", "),
                if list.len() == 1 { "is" } else { "are" }
            )),
        }
    }

    /// How to read the archives once this protection was accepted.
    pub fn restore_key(&self, key: Option<Secret>) -> RestoreKey {
        match (self, key) {
            // Always pass the key: the client then refuses any manifest
            // that is not signed with it.
            (Protection::Encrypted, Some(k)) => RestoreKey::File(k),
            // Signed, some archives not encrypted: per archive, as usual.
            (Protection::NotEncrypted(_), _) => RestoreKey::Auto,
            _ => RestoreKey::Without,
        }
    }
}

/// Decide from a manifest that the client verified with the key.
pub fn protection_from_manifest(
    manifest: &serde_json::Value,
    archives: &[&str],
) -> Result<Protection> {
    let files = manifest["files"].to_string();
    let mut weak = Vec::new();
    for archive in archives {
        let modes = crate::client::crypt_modes_for_archive(&files, archive);
        if modes.is_empty() {
            bail!("{archive} is missing from this backup");
        }
        // The weakest file of the archive counts (split archives have two).
        if let Some(m) = modes.iter().find(|m| *m != "encrypt") {
            weak.push((archive.to_string(), m.clone()));
        }
    }
    Ok(if weak.is_empty() {
        Protection::Encrypted
    } else {
        Protection::NotEncrypted(weak)
    })
}

/// Short label for the list of backups, from the crypt modes the server
/// lists for the archives a restore reads (not verified yet).
pub fn protection_label(s: &SnapshotInfo) -> &'static str {
    let modes: Vec<&str> = s
        .crypt_modes
        .iter()
        .filter(|(f, _)| {
            RESTORED_ARCHIVES
                .iter()
                .any(|a| f.starts_with(&format!("{}.", a.trim_end_matches(".pxar"))))
        })
        .map(|(_, m)| m.as_str())
        .collect();
    if modes.is_empty() {
        ""
    } else if modes.iter().all(|m| *m == "encrypt") {
        "encrypted"
    } else if modes.iter().all(|m| *m == "none") {
        "NOT ENCRYPTED"
    } else if modes.iter().all(|m| *m == "sign-only") {
        "signed, not encrypted"
    } else {
        "partly encrypted"
    }
}

pub fn has_system_archive(s: &SnapshotInfo) -> bool {
    s.archives
        .iter()
        .any(|a| a.starts_with(&format!("{SYSTEM_ARCHIVE}.")))
}

// ---------------------------------------------------------------------------
// Disks

#[derive(Debug, Clone, PartialEq)]
pub struct Disk {
    pub path: String,
    pub size_bytes: u64,
    pub model: String,
    pub serial: String,
    pub transport: String,
    /// Holds the running rescue system or something mounted.
    pub in_use: bool,
    /// The disk itself and every device on it (partitions, assembled RAID,
    /// open LUKS mappings, ...), each once.
    pub items: Vec<guard::BlockItem>,
}

/// Smallest disk offered as restore target (smaller ones cannot hold a
/// system anyway; the plan checks the real space requirement later).
const MIN_TARGET_BYTES: u64 = 1024 * 1024 * 1024;

pub fn parse_disks(lsblk_json: &str, busy: &[String]) -> Result<Vec<Disk>> {
    let v: serde_json::Value =
        serde_json::from_str(lsblk_json).context("unexpected lsblk output")?;
    let mut out = Vec::new();
    for d in v["blockdevices"]
        .as_array()
        .context("lsblk: no blockdevices")?
    {
        if d["type"].as_str() != Some("disk") {
            continue;
        }
        let path = d["path"].as_str().unwrap_or_default().to_string();
        // Floppy drives (QEMU and some firmware show one) and other tiny
        // devices are never a sensible restore target.
        if path.starts_with("/dev/zram")
            || path.starts_with("/dev/loop")
            || path.starts_with("/dev/fd")
            || path.is_empty()
        {
            continue;
        }
        let size = d["size"]
            .as_u64()
            .or_else(|| d["size"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0);
        if size < MIN_TARGET_BYTES || d["ro"].as_bool() == Some(true) {
            continue;
        }
        let mut in_use = busy.iter().any(|b| b == &path);
        let mut items: Vec<guard::BlockItem> = Vec::new();
        let mut stack: Vec<&serde_json::Value> = vec![d];
        while let Some(n) = stack.pop() {
            let s = |k: &str| n[k].as_str().unwrap_or_default().trim().to_string();
            let item_path = s("path");
            if !items.iter().any(|i| i.path == item_path) {
                items.push(guard::BlockItem {
                    path: item_path,
                    kind: s("type"),
                    fstype: s("fstype"),
                    uuid: s("uuid"),
                    partuuid: s("partuuid"),
                    label: s("label"),
                });
            }
            if let Some(points) = n["mountpoints"].as_array()
                && points
                    .iter()
                    .any(|p| p.as_str().is_some_and(|s| !s.is_empty()))
            {
                in_use = true;
            }
            if let Some(children) = n["children"].as_array() {
                // Reversed so that partitions come out in disk order.
                stack.extend(children.iter().rev());
            }
        }
        out.push(Disk {
            path,
            size_bytes: size,
            model: d["model"].as_str().unwrap_or("").trim().to_string(),
            serial: d["serial"].as_str().unwrap_or("").trim().to_string(),
            transport: d["tran"].as_str().unwrap_or("").to_string(),
            in_use,
            items,
        });
    }
    Ok(out)
}

pub fn list_disks() -> Result<Vec<Disk>> {
    // NAME must be among the columns: without it lsblk prints a flat list
    // instead of a tree, and partitions would not appear below their disk.
    let out = std::process::Command::new("lsblk")
        .args([
            "-J",
            "-b",
            "-o",
            "NAME,PATH,TYPE,SIZE,MODEL,SERIAL,TRAN,RO,MOUNTPOINTS,FSTYPE,UUID,PARTUUID,LABEL",
        ])
        .output()
        .context("cannot run lsblk")?;
    if !out.status.success() {
        bail!("lsblk failed");
    }
    parse_disks(&String::from_utf8_lossy(&out.stdout), &[])
}

pub fn booted_uefi() -> bool {
    Path::new("/sys/firmware/efi").exists()
}

/// SHA-256 fingerprint of the server certificate (shown for confirmation).
pub fn fetch_fingerprint(server: &str, port: u16) -> Result<String> {
    let connect = format!("{server}:{port}");
    // `timeout`: s_client waits forever for a host that drops packets.
    let s_client = std::process::Command::new("timeout")
        .args([
            "15",
            "openssl",
            "s_client",
            "-connect",
            &connect,
            "-servername",
            server,
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .context("cannot run openssl")?;
    let mut x509 = std::process::Command::new("openssl")
        .args(["x509", "-noout", "-fingerprint", "-sha256"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    x509.stdin
        .take()
        .context("no stdin")?
        .write_all(&s_client.stdout)?;
    let out = x509.wait_with_output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let fp = text
        .split_once('=')
        .map(|(_, v)| v.trim().to_lowercase())
        .filter(|v| v.len() == 95)
        .with_context(|| format!("no TLS certificate received from {connect}"))?;
    Ok(fp)
}

/// A settings file found on attached media.
#[derive(Debug, Clone)]
pub struct FoundSettings {
    /// Where it was found, for the user: a path, or the path on a device
    /// that is not mounted (`/nestegg-rescue.toml on /dev/sdb1`).
    pub origin: String,
    pub settings: Settings,
}

/// Key files and settings found on attached media.
#[derive(Default)]
pub struct MediaScan {
    pub keys: Vec<PathBuf>,
    /// Every settings file found, sorted by origin.
    pub settings: Vec<FoundSettings>,
    /// Devices (e.g. `/dev/sdb1`) on which a key or settings file was found;
    /// such a disk is never pre-selected as restore target.
    pub devices: Vec<String>,
    /// Partitions on which the search stopped at a limit.
    pub notes: Vec<String>,
}

/// Look for exported key files and settings on attached partitions. A
/// partition that is not mounted is mounted read-only WITHOUT journal
/// replay (so nothing is written to it), searched, the files found are
/// copied into the work directory, and it is unmounted again (so the disk
/// stays selectable as restore target). The search per partition is
/// limited (see [`Budget`]).
pub fn scan_media() -> MediaScan {
    let mut scan = MediaScan::default();
    let Ok(out) = std::process::Command::new("lsblk")
        .args(["-J", "-l", "-o", "PATH,KNAME,TYPE,FSTYPE,MOUNTPOINTS"])
        .output()
    else {
        return scan;
    };
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    for d in v["blockdevices"].as_array().cloned().unwrap_or_default() {
        let fstype = d["fstype"].as_str().unwrap_or("");
        let Some(options) = no_write_mount_options(fstype) else {
            continue;
        };
        let mounted: Vec<String> = d["mountpoints"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let dev = d["path"].as_str().unwrap_or("").to_string();
        let before = (scan.keys.len(), scan.settings.len());
        let mut budget = Budget::default();
        if let Some(m) = mounted.first() {
            search_media(Path::new(m), None, &dev, &mut budget, &mut scan);
        } else {
            let kname = d["kname"].as_str().unwrap_or("x");
            let point = work_dir().join("media").join(kname);
            if fs::create_dir_all(&point).is_err() {
                continue;
            }
            let ok = std::process::Command::new("mount")
                .args(["-o", options])
                .arg(&dev)
                .arg(&point)
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                let copies = work_dir().join("found").join(kname);
                search_media(&point, Some(&copies), &dev, &mut budget, &mut scan);
                let _ = std::process::Command::new("umount").arg(&point).status();
            }
            let _ = fs::remove_dir(&point);
        }
        if (scan.keys.len(), scan.settings.len()) != before {
            scan.devices.push(dev.clone());
        }
        if let Some(limit) = budget.exhausted {
            scan.notes.push(format!(
                "Stopped looking on {dev} after {limit}; give a key or settings file \
                 there by its path."
            ));
        }
    }
    scan.settings.sort_by(|a, b| a.origin.cmp(&b.origin));
    scan
}

/// Read-only mount options that also skip journal/log replay, or `None` for
/// file systems that are not searched.
fn no_write_mount_options(fstype: &str) -> Option<&'static str> {
    Some(match fstype {
        "ext2" | "ext3" | "ext4" => "ro,noload",
        "xfs" => "ro,norecovery",
        "btrfs" => "ro,rescue=nologreplay",
        "vfat" | "exfat" | "ntfs" | "ntfs3" => "ro",
        _ => return None,
    })
}

/// Limits of the search on one partition: it runs before the first
/// question, on every attached disk, so a big data disk must not hold it up.
#[derive(Debug, Default)]
pub struct Budget {
    entries: usize,
    files: usize,
    bytes: u64,
    /// Which limit was reached, if one was.
    pub exhausted: Option<&'static str>,
}

/// Directory entries looked at per partition.
const MAX_ENTRIES: usize = 5000;
/// Files opened per partition.
const MAX_FILES: usize = 64;
/// Bytes read per partition (a paper key as html is about 100-180 KB).
const MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Largest settings file read.
const MAX_SETTINGS_BYTES: u64 = 64 * 1024;

impl Budget {
    /// Count one directory entry; `false` once the limit is reached.
    fn entry(&mut self) -> bool {
        self.entries += 1;
        if self.entries > MAX_ENTRIES {
            self.exhausted.get_or_insert("5000 directory entries");
            return false;
        }
        true
    }

    /// Reserve reading a file of `len` bytes; `false` if that exceeds a limit.
    fn read(&mut self, len: u64) -> bool {
        if self.files >= MAX_FILES {
            self.exhausted.get_or_insert("64 files");
            return false;
        }
        if self.bytes + len > MAX_BYTES {
            self.exhausted.get_or_insert("8 MiB read");
            return false;
        }
        self.files += 1;
        self.bytes += len;
        true
    }
}

/// Collect settings and key files below `root` (on `dev`); with `copy_to`,
/// copy the keys there (the medium is unmounted afterwards).
fn search_media(
    root: &Path,
    copy_to: Option<&Path>,
    dev: &str,
    budget: &mut Budget,
    scan: &mut MediaScan,
) {
    let origin = |path: &Path| -> String {
        match (copy_to, path.strip_prefix(root)) {
            (Some(_), Ok(rel)) => format!("/{} on {dev}", rel.display()),
            _ => path.display().to_string(),
        }
    };
    for (path, len) in walk(root, 2, budget) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if name == SETTINGS_FILE {
            if len > MAX_SETTINGS_BYTES || !budget.read(len) {
                continue;
            }
            if let Ok(Ok(s)) = fs::read_to_string(&path).map(|t| toml::from_str::<Settings>(&t)) {
                scan.settings.push(FoundSettings {
                    origin: origin(&path),
                    settings: s,
                });
            }
            continue;
        }
        if !key_candidate_name(&name) || len > MAX_KEY_FILE_BYTES || !budget.read(len) {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        if !might_be_key(&bytes) || !key_or_protected(key_from_bytes(&path, &bytes)) {
            continue;
        }
        let kept = match copy_to {
            None => Some(path.clone()),
            Some(dir) => keep_copy(root, &path, dir, &bytes),
        };
        if let Some(k) = kept {
            scan.keys.push(k);
        }
    }
}

/// Copy a key file found on a medium that is unmounted afterwards.
fn keep_copy(root: &Path, path: &Path, dir: &Path, bytes: &[u8]) -> Option<PathBuf> {
    let rel = path.strip_prefix(root).ok()?;
    let flat = rel.to_string_lossy().replace('/', "_");
    fs::create_dir_all(dir).ok()?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).ok()?;
    let dest = dir.join(flat);
    secrets::write_private_file(&dest, bytes).ok()?;
    Some(dest)
}

/// Regular files below `dir`, at most `depth` directory levels down,
/// breadth first (keys usually lie at the top), with their sizes.
fn walk(dir: &Path, depth: u32, budget: &mut Budget) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut level = vec![dir.to_path_buf()];
    for d in 0..=depth {
        let mut next = Vec::new();
        for dir in level {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                if !budget.entry() {
                    return out;
                }
                match e.file_type() {
                    Ok(t) if t.is_file() => {
                        out.push((e.path(), e.metadata().map(|m| m.len()).unwrap_or(u64::MAX)))
                    }
                    Ok(t) if t.is_dir() && d < depth => next.push(e.path()),
                    _ => {}
                }
            }
        }
        level = next;
    }
    out
}

/// Largest file looked at as a key: a paper key as html carries a QR code
/// image and is about 100-180 KB.
const MAX_KEY_FILE_BYTES: u64 = 1024 * 1024;

/// File names worth opening: exported keys (`.json`) and paper keys saved
/// as text or html (`nestegg key paperkey --output`). Files without an
/// extension are not opened (there are too many of them on a system disk);
/// such a key can still be given by its path.
fn key_candidate_name(lowercase_name: &str) -> bool {
    match lowercase_name.rsplit_once('.') {
        None => false,
        Some((_, ext)) => matches!(
            ext,
            "json" | "txt" | "text" | "html" | "htm" | "md" | "key" | "asc"
        ),
    }
}

/// Cheap test before parsing: a paper key has the BEGIN line, an exported
/// key file a `"kdf"` field.
fn might_be_key(bytes: &[u8]) -> bool {
    let contains = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    contains(paperkey::BEGIN.as_bytes()) || contains(b"\"kdf\"")
}

/// Read a key from an exported key file (JSON) or a saved paper key (text
/// or html). Only keys without password are supported.
pub fn read_key(path: &Path) -> Result<paperkey::Key> {
    let meta = fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    if !meta.is_file() || meta.len() > MAX_KEY_FILE_BYTES {
        bail!("{} is not a key file", path.display());
    }
    let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    key_from_bytes(path, &bytes)
}

fn key_from_bytes(path: &Path, bytes: &[u8]) -> Result<paperkey::Key> {
    let text = String::from_utf8_lossy(bytes);
    paperkey::from_file_text(&text).map_err(|e| match e {
        paperkey::KeyTextError::NotAKey(m) => {
            anyhow::anyhow!("{} is not a key file or paper key: {m}", path.display())
        }
        other => anyhow::anyhow!("{}: {other}", path.display()),
    })
}

/// A key, or a password-protected key (it is offered and then explained).
fn key_or_protected(r: Result<paperkey::Key>) -> bool {
    match r {
        Ok(_) => true,
        Err(e) => e.to_string().contains("protected with a password"),
    }
}

/// A key file or a saved paper key (a password-protected key counts: it is
/// offered and then explained).
pub fn looks_like_key(path: &Path) -> bool {
    key_or_protected(read_key(path))
}

// ---------------------------------------------------------------------------
// Prompts

pub fn ask(prompt: &str, default: Option<&str>) -> Result<String> {
    loop {
        match default {
            Some(d) if !d.is_empty() => eprint!("{prompt} [{d}]: "),
            _ => eprint!("{prompt}: "),
        }
        std::io::stderr().flush()?;
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line)? == 0 {
            bail!("input closed");
        }
        let v = line.trim();
        if !v.is_empty() {
            return Ok(v.to_string());
        }
        if let Some(d) = default {
            return Ok(d.to_string());
        }
    }
}

pub fn ask_yes(prompt: &str, default_yes: bool) -> Result<bool> {
    let d = if default_yes { "Y/n" } else { "y/N" };
    let v = ask(&format!("{prompt} ({d})"), Some(""))?;
    Ok(interpret_yes(&v, default_yes))
}

/// An empty answer means the default; everything that is not a clear yes is
/// a no.
pub fn interpret_yes(answer: &str, default_yes: bool) -> bool {
    match answer.trim().to_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" | "j" | "ja" => true,
        _ => false,
    }
}

pub fn ask_secret(prompt: &str) -> Result<Secret> {
    use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
    let stdin = std::io::stdin();
    let tty = nix::unistd::isatty(&stdin).unwrap_or(false);
    // Switch echo off BEFORE showing the prompt: input typed (or pasted)
    // as soon as the prompt appears must never be echoed.
    let saved = if tty {
        let original = tcgetattr(&stdin)?;
        let mut silent = original.clone();
        silent.local_flags.remove(LocalFlags::ECHO);
        tcsetattr(&stdin, SetArg::TCSANOW, &silent)?;
        Some(original)
    } else {
        None
    };
    eprint!("{prompt}: ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    let r = stdin.lock().read_line(&mut line);
    if let Some(original) = saved {
        let _ = tcsetattr(&stdin, SetArg::TCSANOW, &original);
        eprintln!();
    }
    let read = r?;
    let v = line.trim_end_matches(['\n', '\r']).to_string();
    if v.is_empty() {
        if read == 0 {
            bail!("input closed");
        }
        // Just Enter: ask again instead of ending the wizard.
        eprintln!("Please enter a value.");
        return ask_secret(prompt);
    }
    Ok(Secret::new(v.into_bytes()))
}

/// Let the user pick one entry (1-based numbers). `default` is 0-based;
/// without one the user has to type a number.
pub fn choose(prompt: &str, items: &[String], default: Option<usize>) -> Result<usize> {
    for (i, item) in items.iter().enumerate() {
        eprintln!("  {:>2}) {item}", i + 1);
    }
    let default = default.map(|d| (d + 1).to_string());
    loop {
        let v = ask(prompt, default.as_deref())?;
        match v.parse::<usize>() {
            Ok(n) if n >= 1 && n <= items.len() => return Ok(n - 1),
            _ => eprintln!("Please enter a number between 1 and {}.", items.len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip() {
        let s = Settings {
            server: "pbs.example.com".into(),
            port: 8007,
            datastore: "backups".into(),
            namespace: Some("desktops".into()),
            auth_id: "backup@pbs!desktop".into(),
            fingerprint: None,
            backup_id: Some("desktop".into()),
            wake_mac: Some("00:00:5e:00:53:01".into()),
        };
        let text = toml::to_string(&s).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), s);
        let c = s.to_config().unwrap();
        assert_eq!(
            c.repository(),
            "backup@pbs!desktop@pbs.example.com:8007:backups"
        );
        assert!(c.wake.is_some());
    }

    #[test]
    fn disks() {
        let json = r#"{"blockdevices":[
 {"path":"/dev/sda","type":"disk","size":16008609792,"model":"USB Stick","tran":"usb","ro":false,"mountpoints":[null],
  "children":[{"path":"/dev/sda1","type":"part","size":1,"mountpoints":["/run/live/medium"]}]},
 {"path":"/dev/nvme0n1","type":"disk","size":512110190592,"model":"Samsung SSD 980 ","tran":"nvme","ro":false,"mountpoints":[null]},
 {"path":"/dev/sr0","type":"rom","size":1073741312,"model":"DVD","tran":"sata","ro":false,"mountpoints":[null]},
 {"path":"/dev/zram0","type":"disk","size":8000000,"model":null,"tran":null,"ro":false,"mountpoints":["[SWAP]"]},
 {"path":"/dev/fd0","type":"disk","size":4096,"model":null,"tran":null,"ro":false,"mountpoints":[null]},
 {"path":"/dev/sdb","type":"disk","size":16777216,"model":"Tiny","tran":"usb","ro":false,"mountpoints":[null]}
]}"#;
        let d = parse_disks(json, &[]).unwrap();
        assert_eq!(d.len(), 2);
        assert!(d[0].in_use);
        assert!(!d[1].in_use);
        assert_eq!(d[1].model, "Samsung SSD 980");
        assert_eq!(d[0].items.len(), 2);
        assert_eq!(d[1].items.len(), 1);
    }

    #[test]
    fn disk_items_with_signatures() {
        let json = r#"{"blockdevices":[
 {"path":"/dev/sdb","type":"disk","size":4000787030016,"model":"WDC WD40EFRX","serial":"WD-0001","tran":"sata","ro":false,"mountpoints":[null],"fstype":null,"uuid":null,"partuuid":null,"label":null,
  "children":[
   {"path":"/dev/sdb1","type":"part","size":1,"mountpoints":[null],"fstype":"linux_raid_member","uuid":"r1","partuuid":"p1","label":"desktop:0",
    "children":[{"path":"/dev/md127","type":"raid1","size":1,"mountpoints":[null],"fstype":"ext4","uuid":"e1","label":null}]},
   {"path":"/dev/sdb2","type":"part","size":1,"mountpoints":[null],"fstype":"crypto_LUKS","uuid":"l1","partuuid":"p2","label":null}]}
]}"#;
        let d = parse_disks(json, &[]).unwrap();
        assert_eq!(d[0].serial, "WD-0001");
        let paths: Vec<&str> = d[0].items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["/dev/sdb", "/dev/sdb1", "/dev/md127", "/dev/sdb2"]);
        assert_eq!(d[0].items[1].fstype, "linux_raid_member");
        assert_eq!(d[0].items[1].partuuid, "p1");
        assert_eq!(d[0].items[3].uuid, "l1");
        assert!(!d[0].in_use);
    }

    #[test]
    fn yes_no_answers() {
        assert!(!interpret_yes("", false));
        assert!(interpret_yes("", true));
        for yes in ["y", "Y", "yes", "YES", " ja ", "j"] {
            assert!(interpret_yes(yes, false), "{yes}");
        }
        for no in ["n", "no", "nein", "maybe", "yy", "ok", "1"] {
            assert!(!interpret_yes(no, true), "{no}");
        }
    }

    #[test]
    fn key_detection() {
        let dir = tempfile::tempdir().unwrap();
        let k = dir.path().join("key.json");
        fs::write(
            &k,
            r#"{"kdf":null,"created":"x","modified":"x","data":"abc"}"#,
        )
        .unwrap();
        let other = dir.path().join("other.json");
        fs::write(&other, r#"{"a":1}"#).unwrap();
        assert!(!looks_like_key(&k), "data is not a valid key value");
        assert!(!looks_like_key(&other));
        let data = "RVhBTVBMRS1LRVktTk9ULUEtUkVBTC1LRVktMDEyMzQ=";
        fs::write(
            &k,
            format!(r#"{{"kdf":null,"created":"x","modified":"x","data":"{data}"}}"#),
        )
        .unwrap();
        assert!(looks_like_key(&k));
        let paper = dir.path().join("paper.txt");
        fs::write(
            &paper,
            format!(
                "header\n{}\n{{\"kdf\":null,\"created\":\"x\",\"modified\":\"x\",\"data\":\"{data}\"}}\n{}\nQR\n",
                paperkey::BEGIN,
                paperkey::END
            ),
        )
        .unwrap();
        assert!(looks_like_key(&paper));
        let protected = dir.path().join("protected.json");
        fs::write(
            &protected,
            format!(
                r#"{{"kdf":{{"Scrypt":{{"n":1}}}},"created":"x","modified":"x","data":"{data}"}}"#
            ),
        )
        .unwrap();
        assert!(looks_like_key(&protected));
        assert!(
            read_key(&protected)
                .unwrap_err()
                .to_string()
                .contains("password")
        );
    }

    #[test]
    fn key_file_names() {
        for n in [
            "key.json",
            "paperkey.txt",
            "paper.html",
            "paper.htm",
            "notes.md",
        ] {
            assert!(key_candidate_name(n), "{n}");
        }
        // Files without extension are not opened any more.
        for n in [
            "photo.jpg",
            "backup.tar.gz",
            "movie.mkv",
            "paperkey",
            "vmlinuz",
        ] {
            assert!(!key_candidate_name(n), "{n}");
        }
    }

    const DATA: &str = "RVhBTVBMRS1LRVktTk9ULUEtUkVBTC1LRVktMDEyMzQ=";

    fn key_json() -> String {
        format!(r#"{{"kdf":null,"created":"x","modified":"x","data":"{DATA}"}}"#)
    }

    #[test]
    fn media_search_finds_keys_and_every_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let settings = "server = \"pbs.example.com\"\ndatastore = \"store\"\nauth_id = \"backup@pbs!desktop\"\n";
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::write(root.join(SETTINGS_FILE), settings).unwrap();
        fs::write(root.join("a").join(SETTINGS_FILE), settings).unwrap();
        fs::write(root.join("key.json"), key_json()).unwrap();
        fs::write(
            root.join("a/b/paper.txt"),
            format!("{}\n{}\n{}\n", paperkey::BEGIN, key_json(), paperkey::END),
        )
        .unwrap();
        // Too deep, no extension, not a key:
        fs::write(root.join("a/b/c/deep.json"), key_json()).unwrap();
        fs::write(root.join("keyfile"), key_json()).unwrap();
        fs::write(root.join("notes.txt"), "shopping list").unwrap();
        let mut scan = MediaScan::default();
        let mut budget = Budget::default();
        search_media(root, None, "/dev/sdz1", &mut budget, &mut scan);
        let names: Vec<String> = scan
            .keys
            .iter()
            .map(|k| k.strip_prefix(root).unwrap().display().to_string())
            .collect();
        assert_eq!(names, ["key.json", "a/b/paper.txt"], "breadth first");
        assert_eq!(scan.settings.len(), 2);
        assert!(
            scan.settings
                .iter()
                .any(|f| f.origin == root.join(SETTINGS_FILE).display().to_string())
        );
        assert_eq!(budget.exhausted, None);
        // On a medium that is unmounted afterwards: copies, and origins
        // that name the device.
        let copies = dir.path().join("copies");
        let mut scan = MediaScan::default();
        search_media(
            root,
            Some(&copies),
            "/dev/sdz1",
            &mut Budget::default(),
            &mut scan,
        );
        assert!(scan.keys.iter().all(|k| k.starts_with(&copies)));
        assert!(
            scan.settings
                .iter()
                .any(|f| f.origin == "/a/nestegg-rescue.toml on /dev/sdz1"),
            "{:?}",
            scan.settings
        );
    }

    #[test]
    fn media_search_is_limited() {
        let dir = tempfile::tempdir().unwrap();
        // Many candidate files that are not keys: only MAX_FILES are opened.
        for i in 0..(MAX_FILES + 20) {
            fs::write(dir.path().join(format!("{i:03}.txt")), "x").unwrap();
        }
        fs::write(dir.path().join("zzz-key.json"), key_json()).unwrap();
        let mut scan = MediaScan::default();
        let mut budget = Budget::default();
        search_media(dir.path(), None, "/dev/sdz1", &mut budget, &mut scan);
        assert_eq!(budget.exhausted, Some("64 files"));
        assert!(scan.keys.is_empty());
        // Bytes: big files exhaust the byte budget.
        let mut b = Budget::default();
        assert!(b.read(MAX_BYTES - 10));
        assert!(!b.read(11));
        assert_eq!(b.exhausted, Some("8 MiB read"));
        // Directory entries.
        let mut b = Budget::default();
        assert!((0..MAX_ENTRIES).all(|_| b.entry()));
        assert!(!b.entry());
    }

    #[test]
    fn cheap_precheck_before_parsing() {
        assert!(might_be_key(key_json().as_bytes()));
        assert!(might_be_key(format!("x {} y", paperkey::BEGIN).as_bytes()));
        assert!(!might_be_key(b"{\"data\": \"abc\"}"));
        assert!(!might_be_key(b""));
    }

    fn manifest(modes: &[(&str, &str)]) -> serde_json::Value {
        serde_json::json!({
            "files": modes
                .iter()
                .map(|(f, m)| serde_json::json!({"filename": f, "crypt-mode": m, "size": 1}))
                .collect::<Vec<_>>()
        })
    }

    #[test]
    fn every_restored_archive_must_be_encrypted() {
        let all = manifest(&[
            ("inventory.pxar.didx", "encrypt"),
            ("system.pxar.didx", "encrypt"),
            ("catalog.pcat1.didx", "encrypt"),
        ]);
        assert_eq!(
            protection_from_manifest(&all, RESTORED_ARCHIVES).unwrap(),
            Protection::Encrypted
        );
        // Inventory encrypted, system not: found before the disk step.
        let mixed = manifest(&[
            ("inventory.pxar.didx", "encrypt"),
            ("system.pxar.didx", "none"),
        ]);
        assert_eq!(
            protection_from_manifest(&mixed, RESTORED_ARCHIVES).unwrap(),
            Protection::NotEncrypted(vec![("system.pxar".into(), "none".into())])
        );
        let signed = manifest(&[
            ("inventory.pxar.didx", "sign-only"),
            ("system.pxar.didx", "sign-only"),
        ]);
        let p = protection_from_manifest(&signed, RESTORED_ARCHIVES).unwrap();
        let concern = p.concern().unwrap();
        assert!(
            concern.contains("signed with your key") && concern.contains("system.pxar (sign-only)"),
            "{concern}"
        );
        // A split archive counts with its weakest part.
        let split = manifest(&[
            ("inventory.pxar.didx", "encrypt"),
            ("system.mpxar.didx", "encrypt"),
            ("system.ppxar.didx", "sign-only"),
        ]);
        assert!(matches!(
            protection_from_manifest(&split, RESTORED_ARCHIVES).unwrap(),
            Protection::NotEncrypted(_)
        ));
        let missing = manifest(&[("inventory.pxar.didx", "encrypt")]);
        let e = protection_from_manifest(&missing, RESTORED_ARCHIVES).unwrap_err();
        assert!(e.to_string().contains("system.pxar is missing"), "{e}");
    }

    #[test]
    fn how_archives_are_read_after_the_check() {
        let key = || Some(Secret::new(b"k".to_vec()));
        assert!(matches!(
            Protection::Encrypted.restore_key(key()),
            RestoreKey::File(_)
        ));
        assert!(matches!(
            Protection::Unsigned.restore_key(key()),
            RestoreKey::Without
        ));
        assert!(matches!(
            Protection::Plain.restore_key(None),
            RestoreKey::Without
        ));
        assert!(matches!(
            Protection::NotEncrypted(vec![]).restore_key(key()),
            RestoreKey::Auto
        ));
        let c = Protection::Unsigned.concern().unwrap();
        assert!(c.contains("not protected by your key") && c.contains("could have changed it"));
        assert_eq!(Protection::Plain.concern(), None);
        assert_eq!(Protection::Encrypted.concern(), None);
    }

    #[test]
    fn backup_list_says_whether_a_backup_is_encrypted() {
        let snap = |modes: &[(&str, &str)]| SnapshotInfo {
            time: String::new(),
            snapshot: String::new(),
            size_bytes: None,
            archives: vec![],
            encrypted: None,
            verified: None,
            protected: false,
            comment: None,
            archive_bytes: vec![],
            crypt_modes: modes
                .iter()
                .map(|(f, m)| (f.to_string(), m.to_string()))
                .collect(),
        };
        let encrypted = [
            ("inventory.pxar.didx", "encrypt"),
            ("system.pxar.didx", "encrypt"),
            ("index.json.blob", "sign-only"),
        ];
        assert_eq!(protection_label(&snap(&encrypted)), "encrypted");
        let none = [
            ("inventory.pxar.didx", "none"),
            ("system.pxar.didx", "none"),
        ];
        assert_eq!(protection_label(&snap(&none)), "NOT ENCRYPTED");
        let signed = [
            ("inventory.pxar.didx", "sign-only"),
            ("system.pxar.didx", "sign-only"),
        ];
        assert_eq!(protection_label(&snap(&signed)), "signed, not encrypted");
        let mixed = [
            ("inventory.pxar.didx", "encrypt"),
            ("system.pxar.didx", "none"),
        ];
        assert_eq!(protection_label(&snap(&mixed)), "partly encrypted");
        assert_eq!(protection_label(&snap(&[])), "");
    }
}
