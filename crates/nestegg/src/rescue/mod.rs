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

    /// Read `layout.json` and the package list of a snapshot.
    pub fn inventory(&self, snapshot: &str) -> Result<(Layout, Option<String>)> {
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
        let selections = fs::read_to_string(base.join("dpkg-selections.txt")).ok();
        Ok((layout, selections))
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
    pub transport: String,
    /// Holds the running rescue system or something mounted.
    pub in_use: bool,
}

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
        if path.starts_with("/dev/zram") || path.starts_with("/dev/loop") || path.is_empty() {
            continue;
        }
        let size = d["size"]
            .as_u64()
            .or_else(|| d["size"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0);
        if size == 0 || d["ro"].as_bool() == Some(true) {
            continue;
        }
        let mut in_use = busy.iter().any(|b| b == &path);
        let mut stack: Vec<&serde_json::Value> = vec![d];
        while let Some(n) = stack.pop() {
            if let Some(points) = n["mountpoints"].as_array()
                && points
                    .iter()
                    .any(|p| p.as_str().is_some_and(|s| !s.is_empty()))
            {
                in_use = true;
            }
            if let Some(children) = n["children"].as_array() {
                stack.extend(children.iter());
            }
        }
        out.push(Disk {
            path,
            size_bytes: size,
            model: d["model"].as_str().unwrap_or("").trim().to_string(),
            transport: d["tran"].as_str().unwrap_or("").to_string(),
            in_use,
        });
    }
    Ok(out)
}

pub fn list_disks() -> Result<Vec<Disk>> {
    let out = std::process::Command::new("lsblk")
        .args(["-J", "-b", "-o", "PATH,TYPE,SIZE,MODEL,TRAN,RO,MOUNTPOINTS"])
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

/// Look for exported key files and settings on attached partitions. A
/// partition that is not mounted is mounted read-only WITHOUT journal
/// replay (so nothing is written to it), searched, the files found are
/// copied into the work directory, and it is unmounted again (so the disk
/// stays selectable as restore target).
pub fn scan_media() -> (Vec<PathBuf>, Option<(PathBuf, Settings)>) {
    let mut keys = Vec::new();
    let mut settings = None;
    let Ok(out) = std::process::Command::new("lsblk")
        .args(["-J", "-l", "-o", "PATH,KNAME,TYPE,FSTYPE,MOUNTPOINTS"])
        .output()
    else {
        return (keys, settings);
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
        if let Some(m) = mounted.first() {
            search_media(Path::new(m), None, &mut keys, &mut settings);
            continue;
        }
        let kname = d["kname"].as_str().unwrap_or("x");
        let point = work_dir().join("media").join(kname);
        if fs::create_dir_all(&point).is_err() {
            continue;
        }
        let ok = std::process::Command::new("mount")
            .args(["-o", options])
            .arg(d["path"].as_str().unwrap_or(""))
            .arg(&point)
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            let copies = work_dir().join("found").join(kname);
            search_media(&point, Some(&copies), &mut keys, &mut settings);
            let _ = std::process::Command::new("umount").arg(&point).status();
        }
        let _ = fs::remove_dir(&point);
    }
    (keys, settings)
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
        } else if name.ends_with(".json")
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

/// A `proxmox-backup-client` key file (JSON with `kdf` and `data`).
pub fn looks_like_key(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if meta.len() > 64 * 1024 {
        return false;
    }
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .is_some_and(|v| v.get("kdf").is_some() && v.get("data").is_some())
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
    Ok(match v.to_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" | "j" | "ja" => true,
        _ => false,
    })
}

pub fn ask_secret(prompt: &str) -> Result<Secret> {
    use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
    let stdin = std::io::stdin();
    let tty = nix::unistd::isatty(&stdin).unwrap_or(false);
    eprint!("{prompt}: ");
    std::io::stderr().flush()?;
    let saved = if tty {
        let original = tcgetattr(&stdin)?;
        let mut silent = original.clone();
        silent.local_flags.remove(LocalFlags::ECHO);
        tcsetattr(&stdin, SetArg::TCSANOW, &silent)?;
        Some(original)
    } else {
        None
    };
    let mut line = String::new();
    let r = stdin.lock().read_line(&mut line);
    if let Some(original) = saved {
        let _ = tcsetattr(&stdin, SetArg::TCSANOW, &original);
        eprintln!();
    }
    r?;
    let v = line.trim_end_matches(['\n', '\r']).to_string();
    if v.is_empty() {
        bail!("empty input");
    }
    Ok(Secret::new(v.into_bytes()))
}

/// Let the user pick one entry (1-based numbers). `default` is 0-based.
pub fn choose(prompt: &str, items: &[String], default: usize) -> Result<usize> {
    for (i, item) in items.iter().enumerate() {
        eprintln!("  {:>2}) {item}", i + 1);
    }
    loop {
        let v = ask(prompt, Some(&(default + 1).to_string()))?;
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
 {"path":"/dev/zram0","type":"disk","size":8000000,"model":null,"tran":null,"ro":false,"mountpoints":["[SWAP]"]}
]}"#;
        let d = parse_disks(json, &[]).unwrap();
        assert_eq!(d.len(), 2);
        assert!(d[0].in_use);
        assert!(!d[1].in_use);
        assert_eq!(d[1].model, "Samsung SSD 980");
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
        assert!(looks_like_key(&k));
        assert!(!looks_like_key(&other));
    }
}
