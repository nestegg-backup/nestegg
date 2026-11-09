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

    /// Read `layout.json`, the package list and the storage records of a
    /// snapshot.
    pub fn inventory(&self, snapshot: &str) -> Result<Inventory> {
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
                    key: RestoreKey::Auto,
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

/// Key files and settings found on attached media.
#[derive(Default)]
pub struct MediaScan {
    pub keys: Vec<PathBuf>,
    pub settings: Option<(PathBuf, Settings)>,
    /// Devices (e.g. `/dev/sdb1`) on which a key or settings file was found;
    /// such a disk is never pre-selected as restore target.
    pub devices: Vec<String>,
}

/// Look for exported key files and settings on attached partitions. A
/// partition that is not mounted is mounted read-only WITHOUT journal
/// replay (so nothing is written to it), searched, the files found are
/// copied into the work directory, and it is unmounted again (so the disk
/// stays selectable as restore target).
pub fn scan_media() -> MediaScan {
    let mut scan = MediaScan::default();
    let Ok(out) = std::process::Command::new("lsblk")
        .args(["-J", "-l", "-o", "PATH,KNAME,TYPE,FSTYPE,MOUNTPOINTS"])
        .output()
    else {
        return scan;
    };
    let MediaScan {
        keys,
        settings,
        devices,
    } = &mut scan;
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
        let before = (keys.len(), settings.is_some());
        if let Some(m) = mounted.first() {
            search_media(Path::new(m), None, keys, settings);
            if (keys.len(), settings.is_some()) != before {
                devices.push(dev);
            }
            continue;
        }
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
            search_media(&point, Some(&copies), keys, settings);
            let _ = std::process::Command::new("umount").arg(&point).status();
            if (keys.len(), settings.is_some()) != before {
                devices.push(dev);
            }
        }
        let _ = fs::remove_dir(&point);
    }
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

/// Collect settings and key files below `root`; with `copy_to`, copy them
/// there first (the medium is unmounted afterwards).
fn search_media(
    root: &Path,
    copy_to: Option<&Path>,
    keys: &mut Vec<PathBuf>,
    settings: &mut Option<(PathBuf, Settings)>,
) {
    let keep = |path: &Path| -> Option<PathBuf> {
        let Some(dir) = copy_to else {
            return Some(path.to_path_buf());
        };
        let rel = path.strip_prefix(root).ok()?;
        let flat = rel.to_string_lossy().replace('/', "_");
        fs::create_dir_all(dir).ok()?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).ok()?;
        let dest = dir.join(flat);
        fs::copy(path, &dest).ok()?;
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o600)).ok()?;
        Some(dest)
    };
    for path in walk(root, 2) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if name == SETTINGS_FILE {
            if let Ok(Ok(s)) = fs::read_to_string(&path).map(|t| toml::from_str::<Settings>(&t))
                && let Some(kept) = keep(&path)
            {
                *settings = Some((kept, s));
            }
        } else if key_candidate_name(&name)
            && looks_like_key(&path)
            && let Some(kept) = keep(&path)
        {
            keys.push(kept);
        }
    }
}

fn walk(dir: &Path, depth: u32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_file() => out.push(p),
            Ok(t) if t.is_dir() && depth > 0 => out.extend(walk(&p, depth - 1)),
            _ => {}
        }
    }
    out
}

/// Largest file looked at as a key: a paper key as html carries a QR code
/// image and is about 180 KB.
const MAX_KEY_FILE_BYTES: u64 = 1024 * 1024;

/// File names worth opening: exported keys (`.json`) and paper keys saved
/// as text or html under whatever name (`nestegg key paperkey --output`).
fn key_candidate_name(lowercase_name: &str) -> bool {
    match lowercase_name.rsplit_once('.') {
        None => true,
        Some((_, ext)) => matches!(
            ext,
            "json" | "txt" | "text" | "html" | "htm" | "md" | "key" | "asc"
        ),
    }
}

/// Read a key from an exported key file (JSON) or a saved paper key (text
/// or html). Only keys without password are supported.
pub fn read_key(path: &Path) -> Result<paperkey::Key> {
    let meta = fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    if !meta.is_file() || meta.len() > MAX_KEY_FILE_BYTES {
        bail!("{} is not a key file", path.display());
    }
    let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    paperkey::from_file_text(&text).map_err(|e| match e {
        paperkey::KeyTextError::NotAKey(m) => {
            anyhow::anyhow!("{} is not a key file or paper key: {m}", path.display())
        }
        other => anyhow::anyhow!("{}: {other}", path.display()),
    })
}

/// A key file or a saved paper key (a password-protected key counts: it is
/// offered and then explained).
pub fn looks_like_key(path: &Path) -> bool {
    match read_key(path) {
        Ok(_) => true,
        Err(e) => e.to_string().contains("protected with a password"),
    }
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
            "paperkey",
            "notes.md",
        ] {
            assert!(key_candidate_name(n), "{n}");
        }
        for n in ["photo.jpg", "backup.tar.gz", "movie.mkv"] {
            assert!(!key_candidate_name(n), "{n}");
        }
    }
}
