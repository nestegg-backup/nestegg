//! Proof that the user really holds a copy of the encryption key.
//!
//! Backups are worthless without the key, so `nestegg key confirm-exported`
//! does not take the user's word: it needs the copy itself (a key file or a
//! saved paper key) or the check code printed on the paper key. This module
//! holds the pure parts: the check code, the paper key header, extracting a
//! key from a copy, comparing key material and deciding whether a copy lies
//! on a disk that belongs to this computer. See
//! `docs/decisions/0018-key-export-must-be-proven.md`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::layout::{Layout, MountRole};
use crate::state::PaperKeyFile;

/// The block `proxmox-backup-client key paperkey` writes around the key.
/// Everything we add to a paper key stays outside these two lines, because
/// readers of paper keys (the rescue stick) only look between them.
pub const BEGIN_MARKER: &str = "-----BEGIN PROXMOX BACKUP KEY-----";
pub const END_MARKER: &str = "-----END PROXMOX BACKUP KEY-----";

const CHECK_PREFIX: &str = "nestegg-paper-check:";

/// Check code of a key: the first 8 hex digits (upper case) of
/// `SHA-256("nestegg-paper-check:" + data)`, shown as `XXXX-XXXX`. `data` is
/// the `data` field of the key file. The code only ever appears on the paper
/// key itself, so typing it proves that the user holds that sheet.
pub fn check_code(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CHECK_PREFIX.as_bytes());
    hasher.update(data.as_bytes());
    let hex: String = hasher
        .finalize()
        .iter()
        .take(4)
        .map(|b| format!("{b:02X}"))
        .collect();
    format!("{}-{}", &hex[..4], &hex[4..])
}

/// Bring user input into the `XXXX-XXXX` form. Accepts lower case, spaces
/// and a missing hyphen; anything else is `None`.
pub fn normalize_check_code(input: &str) -> Option<String> {
    let digits: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect::<String>()
        .to_ascii_uppercase();
    if digits.len() != 8 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("{}-{}", &digits[..4], &digits[4..]))
}

/// The part of a key that decides whether it opens the backups: the key
/// material (`data`) and how it is protected (`kdf`). Timestamps, the
/// fingerprint and the layout of the file do not matter, so a pretty-printed
/// paper key equals the compact key file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMaterial {
    data: String,
    kdf: Value,
}

impl KeyMaterial {
    /// From the JSON of a key file.
    pub fn from_key_json(json: &str) -> Result<KeyMaterial> {
        let value: Value = serde_json::from_str(json).context("this is not a key (not JSON)")?;
        let data = value["data"]
            .as_str()
            .filter(|d| !d.is_empty())
            .context("this is not a key (no `data` field)")?;
        Ok(KeyMaterial {
            data: data.to_string(),
            kdf: value["kdf"].clone(),
        })
    }

    /// From a copy the user holds: a key file, or a paper key saved as text
    /// or HTML (the JSON between the BEGIN and END lines).
    pub fn from_copy(bytes: &[u8]) -> Result<KeyMaterial> {
        KeyMaterial::from_key_json(&key_json_of_copy(bytes)?)
    }

    /// Check code printed on the paper key of this key.
    pub fn check_code(&self) -> String {
        check_code(&self.data)
    }
}

/// The key JSON inside a copy of the key: the whole file for a key file, the
/// block between the BEGIN and END lines for a saved paper key. The result
/// is checked to be a key.
pub fn key_json_of_copy(bytes: &[u8]) -> Result<String> {
    let text = String::from_utf8_lossy(bytes);
    let json = match extract_key_block(&text) {
        Some(json) => json,
        None if text.contains(BEGIN_MARKER) => {
            bail!("this paper key has no {END_MARKER} line")
        }
        None => text.trim().to_string(),
    };
    KeyMaterial::from_key_json(&json)
        .map_err(|e| anyhow::anyhow!("no key found in this file ({e:#})"))?;
    Ok(json)
}

/// The text between the BEGIN and END lines, with HTML entities resolved
/// (a paper key saved as HTML). `None` if either marker is missing.
pub fn extract_key_block(text: &str) -> Option<String> {
    let start = text.find(BEGIN_MARKER)? + BEGIN_MARKER.len();
    let end = start + text[start..].find(END_MARKER)?;
    Some(unescape_html(text[start..end].trim()))
}

fn unescape_html(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#x22;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// What the header at the top of a paper key says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaperHeader {
    pub backup_id: String,
    pub hostname: String,
    /// `YYYY-MM-DD`.
    pub date: String,
    pub server: String,
    pub datastore: String,
    pub fingerprint: String,
    pub check_code: String,
}

impl PaperHeader {
    fn computer(&self) -> String {
        if self.backup_id == self.hostname {
            self.backup_id.clone()
        } else {
            format!("{} (host name {})", self.backup_id, self.hostname)
        }
    }

    fn rows(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Computer", self.computer()),
            ("Date", self.date.clone()),
            ("PBS server", self.server.clone()),
            ("Datastore", self.datastore.clone()),
            ("Key fingerprint", self.fingerprint.clone()),
            ("Check code", self.check_code.clone()),
        ]
    }
}

const HOW_TO: [&str; 3] = [
    "To restore, save everything between the BEGIN and END lines below as a text file \
     (or scan the QR code and save its text), for example as key.json.",
    "Then give that file to the restore: `nestegg restore ... --keyfile key.json`, \
     or to the rescue stick when it asks for the key. The rescue stick also lets you \
     type the \"data\" line by hand.",
    "Keep this sheet away from this computer and delete the file it was printed from. \
     Typing the check code with `nestegg key confirm-exported --paper-code <code>` states \
     that the sheet is printed and stored there.",
];

/// Header in front of a text paper key: plain lines, then a blank line.
pub fn text_header(header: &PaperHeader) -> String {
    let mut out = String::from("nestegg paper key\n=================\n\n");
    for (label, value) in header.rows() {
        out.push_str(&format!("{:<18}{}\n", format!("{label}:"), value));
    }
    out.push('\n');
    for (i, line) in HOW_TO.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", i + 1, line));
    }
    out.push('\n');
    out
}

/// Header for the start of the body of an HTML paper key.
pub fn html_header(header: &PaperHeader) -> String {
    let mut out = String::from(
        "<div style=\"page-break-inside: avoid; font-family: sans-serif; margin-bottom: 1.5em\">\n\
         <h1>nestegg paper key</h1>\n<table style=\"border-collapse: collapse\">\n",
    );
    for (label, value) in header.rows() {
        let value = escape_html(&value);
        let value = if label == "Check code" {
            format!("<b style=\"font-size: 140%; letter-spacing: 0.1em\">{value}</b>")
        } else {
            value
        };
        out.push_str(&format!(
            "<tr><td style=\"padding: 0 1em 0 0\">{}:</td><td>{value}</td></tr>\n",
            escape_html(label)
        ));
    }
    out.push_str("</table>\n<ol>\n");
    for line in HOW_TO {
        out.push_str(&format!("<li>{}</li>\n", escape_html(line)));
    }
    out.push_str("</ol>\n</div>\n");
    out
}

/// Our header followed by the unmodified text output of the client.
pub fn compose_text(header: &PaperHeader, client_output: &[u8]) -> Vec<u8> {
    let mut out = text_header(header).into_bytes();
    out.extend_from_slice(client_output);
    out
}

/// The client's HTML with our header block inserted at the start of `<body>`.
pub fn compose_html(header: &PaperHeader, client_output: &[u8]) -> Result<Vec<u8>> {
    let html = String::from_utf8_lossy(client_output);
    let body = html
        .find("<body")
        .and_then(|at| html[at..].find('>').map(|close| at + close + 1))
        .context("unexpected paper key output of proxmox-backup-client (no <body>)")?;
    let mut out = String::with_capacity(html.len() + 2048);
    out.push_str(&html[..body]);
    out.push('\n');
    out.push_str(&html_header(header));
    out.push_str(&html[body..]);
    Ok(out.into_bytes())
}

/// Whether `key paperkey` may write to stdout: a QR code scrolling past in
/// a terminal is not a backup of the key.
pub fn paperkey_stdout_allowed(has_output_file: bool, stdout_is_terminal: bool) -> bool {
    has_output_file || !stdout_is_terminal
}

/// What the device behind a file looks like, one entry per layer
/// (partition, encryption layer, disk).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceFacts {
    pub removable: bool,
    pub hotplug: bool,
    /// `usb`, `sata`, `nvme`, ... as `lsblk` reports it.
    pub transport: Option<String>,
    /// `disk`, `part`, `crypt`, `lvm`, `raid1`, `loop`, ... (`lsblk` TYPE).
    pub kind: Option<String>,
}

impl DeviceFacts {
    fn is_external(&self) -> bool {
        let transport = self.transport.as_deref().unwrap_or("");
        // Internal buses report "hotplug" when the controller supports
        // hot-plugging, which says nothing about the disk being removable.
        let internal_bus = matches!(transport, "sata" | "ata" | "nvme" | "sas" | "mmc");
        self.removable || matches!(transport, "usb" | "ieee1394") || (self.hotplug && !internal_bus)
    }
}

/// Where a copy of the key lies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// On a removable or hot-plugged medium (USB stick, card).
    Removable,
    /// On a disk that belongs to this computer.
    Internal,
    /// Not decidable (RAM disk, network share, overlay file system, no
    /// `findmnt`/`lsblk`). Treated like [`Location::Internal`]: the user
    /// has to state that another copy exists elsewhere.
    Unknown(String),
    /// Inside what the backup contains (a configured source, or a file
    /// system that goes into `system.pxar`), whatever the device is. A copy
    /// there exists only together with the data it protects, and inside the
    /// backup it is encrypted with the key it contains. Treated like
    /// [`Location::Internal`].
    InBackup(String),
}

/// Whether `key import` may record the key as exported. Importing the key
/// that is already stored proves nothing new (it could be the file `key
/// export` just wrote): only a confirmation given for it before stays. A
/// new key counts when the file comes from a removable medium outside the
/// backup; everything else needs `key confirm-exported`.
pub fn import_counts_as_exported(
    same_key: bool,
    same_key_was_confirmed: bool,
    location: &Location,
) -> bool {
    if same_key {
        same_key_was_confirmed
    } else {
        !location.needs_statement()
    }
}

impl Location {
    /// A copy here can be lost together with this computer (or is not known
    /// to be safe), so it needs the user's statement.
    pub fn needs_statement(&self) -> bool {
        !matches!(self, Location::Removable)
    }

    /// Where the file is, as a phrase that follows "the file": `lies on ...`.
    pub fn describe(&self) -> String {
        match self {
            Location::Removable => "lies on a removable medium".into(),
            Location::Internal => "lies on a disk of this computer".into(),
            Location::Unknown(why) => {
                format!("lies in a place that nestegg cannot classify ({why})")
            }
            Location::InBackup(why) => format!("lies {why}"),
        }
    }
}

/// The system file systems of a [`Layout`], split by whether their content
/// goes into `system.pxar`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemMounts {
    /// Mount points of the root file system, the separate system file
    /// systems and bind mounts of them.
    pub inside: Vec<PathBuf>,
    /// Every other mount point (data disks, removable media, network, ...).
    pub outside: Vec<PathBuf>,
}

impl SystemMounts {
    pub fn from_layout(layout: &Layout) -> SystemMounts {
        let mut out = SystemMounts::default();
        for m in &layout.mounts {
            let inside = matches!(layout.role(m), MountRole::Root | MountRole::System)
                || crate::layout::system_bind(layout, m);
            let point = PathBuf::from(&m.point);
            if inside {
                out.inside.push(point);
            } else {
                out.outside.push(point);
            }
        }
        for point in &layout.all_points {
            let point = PathBuf::from(point);
            if !out.inside.contains(&point) && !out.outside.contains(&point) {
                out.outside.push(point);
            }
        }
        out
    }

    /// The mount point of the system file system `path` lies on, if its
    /// content goes into `system.pxar`: the deepest mount point above the
    /// path decides.
    fn system_mount_of(&self, path: &Path) -> Option<&Path> {
        fn deepest<'a>(points: &'a [PathBuf], path: &Path) -> Option<&'a PathBuf> {
            points
                .iter()
                .filter(|p| path.starts_with(p))
                .max_by_key(|p| p.components().count())
        }
        let inside = deepest(&self.inside, path)?;
        match deepest(&self.outside, path) {
            Some(outside) if outside.components().count() > inside.components().count() => None,
            _ => Some(inside.as_path()),
        }
    }
}

/// What the backup contains, as far as it matters for a copy of the key:
/// the configured sources, and in system mode the system file systems.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackupScope {
    /// Canonical paths of `backup.sources` and `backup.include_dev`.
    pub roots: Vec<PathBuf>,
    /// System mode: the mounts of the running system.
    pub system: Option<SystemMounts>,
    /// Set when the scope could not be determined (unreadable configuration,
    /// no disk layout in system mode): a copy on removable media can then
    /// not be told apart from one inside the backup.
    pub unknown: Option<String>,
    /// Every mount point of the running system with the device number of its
    /// file system (the mount table, in its order). The backup client stays
    /// on the file system of a source: another file system mounted below it
    /// is not part of the backup unless it is a source or listed in
    /// `include_dev` itself. It tells file systems apart by the device
    /// number, so a second mount of the source's own file system (a bind
    /// mount) IS backed up. `None` when the table could not be read; every
    /// path below a source then counts as inside (the safe side).
    pub mount_points: Option<Vec<(PathBuf, String)>>,
}

impl BackupScope {
    /// The same scope, knowing the mounts of the running system (mount
    /// point, device number), in the order of the mount table.
    pub fn with_mount_points(mut self, points: Vec<(PathBuf, String)>) -> BackupScope {
        self.mount_points = Some(points);
        self
    }

    /// Whether another file system is mounted between `root` (a source or an
    /// `include_dev` path) and `path`: then `path` is not on the file system
    /// of `root`, and the backup client skips it. A mount point that is
    /// `root` itself is the file system of `root`; a path that is itself a
    /// source or `include_dev` entry is a root of its own. A mount of the
    /// same device (a bind mount of the source's own file system) is not a
    /// boundary for the client. If the device of `root` is not known, nothing
    /// counts as crossed (the safe side: inside the backup).
    fn crosses_mount(&self, root: &Path, path: &Path) -> bool {
        let Some(points) = self.mount_points.as_ref() else {
            return false;
        };
        // The device a path is on: the deepest mount point at or above it;
        // of several mounts on the same point the last one is on top.
        let device_at = |p: &Path| {
            points
                .iter()
                .enumerate()
                .filter(|(_, (m, _))| p.starts_with(m))
                .max_by_key(|(i, (m, _))| (m.components().count(), *i))
                .map(|(_, (_, device))| device.as_str())
        };
        let Some(root_device) = device_at(root) else {
            return false;
        };
        points.iter().any(|(m, _)| {
            m != root
                && m.starts_with(root)
                && path.starts_with(m)
                && device_at(m) != Some(root_device)
        })
    }

    /// The scope of a configuration, with the paths resolved on this system.
    /// `layout` is the disk layout and is only needed in system mode.
    pub fn for_config(config: &crate::config::Config, layout: Option<&Layout>) -> BackupScope {
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let roots = config
            .backup
            .sources
            .iter()
            .map(|s| canonical(&s.path))
            .chain(config.backup.include_dev.iter().map(|p| canonical(p)))
            .collect();
        let mut scope = BackupScope {
            roots,
            ..BackupScope::default()
        };
        if config.backup.system {
            match layout {
                Some(layout) => scope.system = Some(SystemMounts::from_layout(layout)),
                None => scope.unknown = Some("the disk layout cannot be read".into()),
            }
        }
        scope
    }

    /// Why `path` (canonical) lies inside the backup, if it does.
    pub fn contains(&self, path: &Path) -> Option<String> {
        if let Some(root) = self
            .roots
            .iter()
            .find(|r| path.starts_with(r) && !self.crosses_mount(r, path))
        {
            return Some(format!(
                "inside {}, which is part of the backup, so the copy exists only together \
                 with the data it protects",
                root.display()
            ));
        }
        let system = self.system.as_ref()?;
        let point = system.system_mount_of(path)?;
        Some(format!(
            "on the file system mounted at {}, which goes into the system backup, so the \
             copy exists only together with the data it protects",
            point.display()
        ))
    }
}

/// Fold the backup scope into the location found for the device: a path
/// inside the backup counts as on this computer whatever the device is.
/// `real` is the canonical path of the file.
pub fn apply_scope(device: Location, real: &Path, scope: &BackupScope) -> Location {
    if let Some(why) = scope.contains(real) {
        return Location::InBackup(why);
    }
    match (&device, &scope.unknown) {
        (Location::Removable, Some(why)) => Location::Unknown(format!(
            "it could not be checked whether the file lies inside the backup: {why}"
        )),
        _ => device,
    }
}

/// [`locate`] for a copy of the key, with the backup scope applied. The path
/// is resolved first, so a link on a USB stick to a file on the disk of this
/// computer does not count as removable.
pub fn locate_in(path: &Path, scope: &BackupScope) -> Location {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    apply_scope(locate(&real), &real, scope)
}

/// Decide from the layers behind a file. Only the whole disks at the bottom
/// count, and ALL of them must be external: a volume group, RAID or btrfs
/// that spans the internal disk and a USB disk may keep the file on the
/// internal one. (A LUKS volume on a USB stick has only the stick below it.)
pub fn classify_devices(chain: &[DeviceFacts]) -> Location {
    if chain.is_empty() {
        return Location::Unknown("the file is not on a block device".into());
    }
    let disks: Vec<&DeviceFacts> = chain
        .iter()
        .filter(|d| d.kind.as_deref() == Some("disk"))
        .collect();
    if disks.is_empty() {
        Location::Unknown("no physical disk found behind the file".into())
    } else if disks.iter().all(|d| d.is_external()) {
        Location::Removable
    } else {
        Location::Internal
    }
}

/// Parse `lsblk -P -o RM,HOTPLUG,TRAN,TYPE` (lines of `KEY="value"` pairs).
pub fn parse_lsblk_pairs(output: &str) -> Vec<DeviceFacts> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut facts = DeviceFacts::default();
            for (key, value) in lsblk_pairs(line) {
                match key.as_str() {
                    "RM" => facts.removable = value == "1" || value == "true",
                    "HOTPLUG" => facts.hotplug = value == "1" || value == "true",
                    "TRAN" if !value.is_empty() => facts.transport = Some(value),
                    "TYPE" if !value.is_empty() => facts.kind = Some(value),
                    _ => {}
                }
            }
            facts
        })
        .collect()
}

fn lsblk_pairs(line: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut rest = line.trim();
    while let Some(eq) = rest.find("=\"") {
        let key = rest[..eq].trim().to_string();
        let value_start = eq + 2;
        let Some(len) = rest[value_start..].find('"') else {
            break;
        };
        pairs.push((key, rest[value_start..value_start + len].to_string()));
        rest = rest[value_start + len + 1..].trim_start();
    }
    pairs
}

/// The block device from `findmnt -n -o SOURCE`: `/dev/sda2`, and for a
/// btrfs subvolume `/dev/sda2[/@home]` without the subvolume.
pub fn parse_findmnt_source(output: &str) -> Option<String> {
    let line = output.lines().next()?.trim();
    let device = line.split('[').next()?.trim();
    device.starts_with("/dev/").then(|| device.to_string())
}

/// Find out where `path` lies. Never fails: what cannot be found out is
/// [`Location::Unknown`].
pub fn locate(path: &Path) -> Location {
    let source = Command::new("findmnt")
        .args(["-n", "-o", "SOURCE", "-T"])
        .arg(path)
        .stdin(Stdio::null())
        .output();
    let source = match source {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(_) => return Location::Unknown("findmnt does not know the file".into()),
        Err(e) => return Location::Unknown(format!("cannot run findmnt: {e}")),
    };
    let Some(device) = parse_findmnt_source(&source) else {
        return Location::Unknown(format!(
            "the file is on {}, which is not a disk",
            source.trim()
        ));
    };
    // `-s` adds the devices below (encryption layers, partitions' disks).
    let chain = Command::new("lsblk")
        .args(["-s", "-n", "-P", "-o", "RM,HOTPLUG,TRAN,TYPE"])
        .arg(&device)
        .stdin(Stdio::null())
        .output();
    match chain {
        Ok(o) if o.status.success() => {
            classify_devices(&parse_lsblk_pairs(&String::from_utf8_lossy(&o.stdout)))
        }
        Ok(_) => Location::Unknown(format!("lsblk does not know {device}")),
        Err(e) => Location::Unknown(format!("cannot run lsblk: {e}")),
    }
}

/// What `confirm-exported --file` does about a copy on this computer's disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalCopy {
    /// Removable medium, or the user already stated it with
    /// `--accept-local-copy`.
    Accept,
    /// A terminal is available: explain and ask for `yes`.
    Ask,
    /// No terminal and no `--accept-local-copy`.
    Refuse,
}

pub fn decide_local_copy(location: &Location, accept_flag: bool, terminal: bool) -> LocalCopy {
    if !location.needs_statement() || accept_flag {
        LocalCopy::Accept
    } else if terminal {
        LocalCopy::Ask
    } else {
        LocalCopy::Refuse
    }
}

/// What `confirm-exported --paper-code` does about the statement that the
/// sheet is printed and stored away from this computer. A printed sheet
/// cannot be proven, so the user has to say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Statement {
    /// Given with `--printed`.
    Given,
    /// A terminal is available: explain and ask for `yes`.
    Ask,
    /// No terminal and no `--printed`: nothing is recorded.
    Missing,
}

pub fn decide_paper_statement(printed_flag: bool, terminal: bool) -> Statement {
    if printed_flag {
        Statement::Given
    } else if terminal {
        Statement::Ask
    } else {
        Statement::Missing
    }
}

/// The paper key files remembered for the key with `key_digest` that still
/// exist, with the place they lie on. `exists` and `locate` are injected.
pub fn existing_paperkey_files(
    files: &[PaperKeyFile],
    key_digest: &str,
    exists: impl Fn(&Path) -> bool,
    locate: impl Fn(&Path) -> Location,
) -> Vec<(PathBuf, Location)> {
    files
        .iter()
        .filter(|f| f.key_digest == key_digest && exists(&f.path))
        .map(|f| (f.path.clone(), locate(&f.path)))
        .collect()
}

/// The paper key files that stand in the way of `--paper-code`: they still
/// exist on this computer's disk (or inside the backup), so reading the code
/// off the screen would be possible.
pub fn paperkey_files_in_the_way(existing: &[(PathBuf, Location)]) -> Vec<&Path> {
    existing
        .iter()
        .filter(|(_, location)| location.needs_statement())
        .map(|(path, _)| path.as_path())
        .collect()
}

/// Quote a path for the shell if it needs it (for a command shown to the
/// user).
fn shell_quote(path: &Path) -> String {
    let text = path.display().to_string();
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+,:@%".contains(c));
    if plain {
        text
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// The refusal of `confirm-exported --paper-code` while remembered paper key
/// files lie on this computer (`in_the_way` is not empty). Names the way out
/// for a file that cannot be deleted: it is a copy of the key, so `--file`
/// with the statement accepts it.
pub fn paperkey_in_the_way_message(in_the_way: &[&Path]) -> String {
    let first = in_the_way.first().copied().unwrap_or(Path::new(""));
    let more = match in_the_way.len() {
        0 | 1 => String::new(),
        n => format!(" (and {} more)", n - 1),
    };
    format!(
        "the paper key file {}{more} still exists on this computer, so the check code could \
         have been read from the screen. Print it, then delete the file or move it to a USB \
         stick, and run this again. If it cannot be deleted (read-only file system, no \
         permission), it is a copy of the key: confirm with `sudo nestegg key confirm-exported \
         --file {} --accept-local-copy` instead (you then state that another copy exists \
         outside this computer). Nothing was recorded.",
        first.display(),
        shell_quote(first)
    )
}

/// The refusal of `key paperkey` when as many paper key files as can be
/// remembered still exist. Forgetting one that exists would let
/// `confirm-exported --paper-code` miss it, so none is forgotten.
pub fn paperkey_limit_message(files: &[PaperKeyFile]) -> String {
    let mut text = format!(
        "{} paper key files written earlier still exist, and nestegg cannot keep track of more \
         than {}. Print the one you need, delete the others (or move them to a USB stick), \
         then run this again. Nothing was written. The files:",
        files.len(),
        crate::state::MAX_PAPERKEY_FILES
    );
    for f in files {
        text.push_str("\n  ");
        text.push_str(&f.path.display().to_string());
    }
    text
}

/// Whether `key paperkey` has to refuse a paper key file at `target`: as many
/// files as can be remembered still exist, and `target` is not one of them. A
/// target that is already in the list is not a new entry (the shell may have
/// emptied it by a redirect, so refusing then would only lose the file), so it
/// is allowed and its entry stays.
pub fn paperkey_files_full_for(files: &[PaperKeyFile], target: &Path) -> bool {
    files.len() >= crate::state::MAX_PAPERKEY_FILES && !files.iter().any(|f| f.path == target)
}

/// How long `key paperkey` waits for the run lock to record a paper key that
/// went into a pipe. A backup can hold it for hours, so the wait is short.
pub const PAPERKEY_RECORD_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Record that the paper key for the key with `digest` went out through
/// standard output (a pipe): `key_last_exported_digest` is set. Called only
/// after the whole output was written and flushed without error. The run lock
/// is taken only now and only for this short update (waiting at most `wait`),
/// the state is read again and the stored key must still be the one that was
/// printed. `Err` says in words why nothing was recorded.
pub fn record_piped_paperkey(
    paths: &crate::config::Paths,
    wait: std::time::Duration,
    digest: &str,
) -> std::result::Result<(), String> {
    let deadline = std::time::Instant::now() + wait;
    let _lock = loop {
        match crate::run::RunLock::try_acquire(paths) {
            Ok(Some(lock)) => break lock,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Ok(None) => {
                return Err(format!(
                    "a backup was running for more than {} seconds",
                    wait.as_secs()
                ));
            }
            Err(e) => return Err(format!("the run lock cannot be taken: {e:#}")),
        }
    };
    let secrets_store = crate::secrets::SecretStore::new(&paths.credentials_dir);
    let current = secrets_store
        .load(crate::secrets::ENCRYPTION_KEY)
        .map_err(|e| format!("the stored key cannot be read again: {e:#}"))?;
    if crate::secrets::digest(&current) != digest {
        return Err(
            "the stored key changed after the paper key was made, so this paper \
                    key is for a key that is not used any more"
                .to_string(),
        );
    }
    let store = crate::state::Store::new(paths);
    let mut state = store
        .load_state()
        .map_err(|e| format!("the state cannot be read: {e:#}"))?;
    state.key_last_exported_digest = Some(digest.to_string());
    store
        .save_state(&state)
        .map_err(|e| format!("the state cannot be saved: {e:#}"))
}

/// Where standard output of `key paperkey` goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdoutTarget {
    /// A terminal, a pipe, a socket, a device such as `/dev/null`, or
    /// something not known: nothing stays on the disk that could be
    /// remembered.
    Other,
    /// A regular file (`nestegg key paperkey > key.txt`) with this path.
    File(PathBuf),
    /// A regular file whose path cannot be kept in the state (not UTF-8, or
    /// not known). The shell has already created it, so this has to be found
    /// out before the key is written. The text says why.
    Unstorable(String),
}

/// The target of standard output of this process.
pub fn stdout_target() -> StdoutTarget {
    stdout_target_at(Path::new("/proc/self/fd/1"))
}

/// [`stdout_target`] for the file descriptor link `fd` (`/proc/self/fd/1`).
pub fn stdout_target_at(fd: &Path) -> StdoutTarget {
    // `metadata` follows the link to the open file itself: a regular file
    // is the only kind that leaves something on the disk.
    match std::fs::metadata(fd) {
        Ok(meta) if meta.is_file() => {}
        _ => return StdoutTarget::Other,
    }
    let path = match std::fs::read_link(fd) {
        Ok(path) => path,
        Err(e) => {
            return StdoutTarget::Unstorable(format!(
                "the name of the file it is redirected to cannot be found out ({e})"
            ));
        }
    };
    // A deleted file shows as "... (deleted)" and has no path any more:
    // nothing stays on the disk.
    if !path.is_absolute() || !path.exists() {
        return StdoutTarget::Other;
    }
    if path.to_str().is_none() {
        return StdoutTarget::Unstorable(
            "the name of the file it is redirected to contains characters that are not valid \
             UTF-8, so it could not be remembered"
                .into(),
        );
    }
    StdoutTarget::File(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = r#"{"kdf":null,"created":"2026-11-09T20:54:47+00:00","modified":"2026-11-09T20:54:47+00:00","data":"TkVTVEVHRy1FWEFNUExFLUtFWS1OT1QtUkVBTC0wMDE=","fingerprint":"ff:ed:94"}"#;

    fn pretty(key: &str) -> String {
        let v: Value = serde_json::from_str(key).unwrap();
        serde_json::to_string_pretty(&v).unwrap()
    }

    fn header() -> PaperHeader {
        PaperHeader {
            backup_id: "desktop".into(),
            hostname: "alice-pc".into(),
            date: "2026-11-09".into(),
            server: "pbs.example.com:8007".into(),
            datastore: "store1".into(),
            fingerprint: "ff:ed:94".into(),
            check_code: "ABCD-0123".into(),
        }
    }

    #[test]
    fn import_only_confirms_a_new_key_from_removable_media() {
        let stick = Location::Removable;
        let disk = Location::Internal;
        let in_backup = Location::InBackup("inside /srv".into());
        let unknown = Location::Unknown("no findmnt".into());
        // A new key: only a file on removable media outside the backup.
        assert!(import_counts_as_exported(false, false, &stick));
        for place in [&disk, &in_backup, &unknown] {
            assert!(!import_counts_as_exported(false, false, place), "{place:?}");
        }
        // The key that is already stored: the place proves nothing (it may
        // be the file `key export` just wrote, even on a stick); only an
        // earlier confirmation stays.
        for place in [&stick, &disk, &in_backup, &unknown] {
            assert!(!import_counts_as_exported(true, false, place), "{place:?}");
            assert!(import_counts_as_exported(true, true, place), "{place:?}");
        }
        // A new key never inherits the old key's confirmation.
        assert!(!import_counts_as_exported(false, true, &disk));
    }

    #[test]
    fn check_code_is_the_start_of_a_prefixed_sha256() {
        // sha256("nestegg-paper-check:abc")
        let mut hasher = Sha256::new();
        hasher.update(b"nestegg-paper-check:abc");
        let hex: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect();
        assert_eq!(check_code("abc"), format!("{}-{}", &hex[..4], &hex[4..8]));
        let code = check_code("abc");
        assert_eq!(code.len(), 9);
        assert_eq!(code, code.to_ascii_uppercase());
        assert_ne!(check_code("abc"), check_code("abd"));
        // Known answer, so the format cannot drift unnoticed.
        assert_eq!(check_code(""), {
            let h = Sha256::digest(b"nestegg-paper-check:");
            format!("{:02X}{:02X}-{:02X}{:02X}", h[0], h[1], h[2], h[3])
        });
    }

    #[test]
    fn check_code_input_is_forgiving_but_strict_about_the_digits() {
        assert_eq!(normalize_check_code("abcd-0123").unwrap(), "ABCD-0123");
        assert_eq!(normalize_check_code(" ab cd 0123\n").unwrap(), "ABCD-0123");
        assert_eq!(normalize_check_code("ABCD0123").unwrap(), "ABCD-0123");
        assert!(normalize_check_code("ABCD-012").is_none());
        assert!(normalize_check_code("ABCD-01234").is_none());
        assert!(normalize_check_code("ABCD-012G").is_none());
        assert!(normalize_check_code("").is_none());
    }

    #[test]
    fn key_material_ignores_layout_and_timestamps() {
        let compact = KeyMaterial::from_key_json(KEY).unwrap();
        let spaced = KeyMaterial::from_key_json(&pretty(KEY)).unwrap();
        assert_eq!(compact, spaced);
        let touched = KEY.replace("2026-11-09T20:54:47+00:00", "2027-01-01T00:00:00+00:00");
        assert_eq!(compact, KeyMaterial::from_key_json(&touched).unwrap());
        assert_eq!(compact.check_code(), spaced.check_code());
    }

    #[test]
    fn key_material_differs_for_other_data_or_protection() {
        let key = KeyMaterial::from_key_json(KEY).unwrap();
        let other_data = KEY.replace("TkVTVEVHRy1F", "TkVTVEVHRy1G");
        assert_ne!(key, KeyMaterial::from_key_json(&other_data).unwrap());
        let protected = KEY.replace(
            r#""kdf":null"#,
            r#""kdf":{"Scrypt":{"n":65536,"r":8,"p":1,"salt":"AA=="}}"#,
        );
        assert_ne!(key, KeyMaterial::from_key_json(&protected).unwrap());
        // A missing kdf is the same as null.
        let no_kdf = KEY.replace(r#""kdf":null,"#, "");
        assert_eq!(key, KeyMaterial::from_key_json(&no_kdf).unwrap());
    }

    #[test]
    fn things_that_are_not_keys_are_rejected() {
        assert!(KeyMaterial::from_key_json("not json").is_err());
        assert!(KeyMaterial::from_key_json(r#"{"fp":"aa"}"#).is_err());
        assert!(KeyMaterial::from_key_json(r#"{"data":""}"#).is_err());
        assert!(KeyMaterial::from_copy(b"just some text").is_err());
        let torn = format!("{BEGIN_MARKER}\n{KEY}\n");
        let err = KeyMaterial::from_copy(torn.as_bytes()).unwrap_err();
        assert!(format!("{err:#}").contains("END"), "{err:#}");
        let damaged = format!("{BEGIN_MARKER}\n{{\"data\": \n{END_MARKER}\n");
        assert!(KeyMaterial::from_copy(damaged.as_bytes()).is_err());
    }

    #[test]
    fn key_is_found_in_a_key_file_and_in_text_and_html_paper_keys() {
        let expect = KeyMaterial::from_key_json(KEY).unwrap();
        // Key file, with and without trailing newline.
        assert_eq!(KeyMaterial::from_copy(KEY.as_bytes()).unwrap(), expect);
        assert_eq!(
            KeyMaterial::from_copy(format!("{KEY}\n").as_bytes()).unwrap(),
            expect
        );
        // Text paper key as the client writes it, behind our header.
        let client_text = format!(
            "{BEGIN_MARKER}\n{}\n{END_MARKER}\n\u{2588}\u{2580}\u{2580} qr code\n",
            pretty(KEY)
        );
        let text = compose_text(&header(), client_text.as_bytes());
        assert_eq!(KeyMaterial::from_copy(&text).unwrap(), expect);
        // HTML paper key.
        let client_html = format!(
            "<!DOCTYPE html>\n<html>\n<head><title>Proxmox Backup Paperkey</title></head>\n<body>\n\
             <div style=\"page-break-inside: avoid\">\n<p>\n{BEGIN_MARKER}\n{}\n{END_MARKER}\n</p>\n\
             <center><img src=\"data:image/svg+xml;base64,AAAA\"></center>\n</div>\n</body>\n</html>\n",
            pretty(KEY)
        );
        let html = compose_html(&header(), client_html.as_bytes()).unwrap();
        assert_eq!(KeyMaterial::from_copy(&html).unwrap(), expect);
        // Entities, as after saving the page from a browser.
        let escaped = client_html.replace('"', "&quot;");
        let escaped = escaped.replace("<div style=&quot;", "<div style=\"");
        assert_eq!(KeyMaterial::from_copy(escaped.as_bytes()).unwrap(), expect);
    }

    #[test]
    fn text_header_stays_outside_the_key_block_and_shows_the_facts() {
        let client_text = format!("{BEGIN_MARKER}\n{}\n{END_MARKER}\nQR\n", pretty(KEY));
        let out = String::from_utf8(compose_text(&header(), client_text.as_bytes())).unwrap();
        let begin = out.find(BEGIN_MARKER).unwrap();
        let head = &out[..begin];
        for needle in [
            "nestegg paper key",
            "desktop (host name alice-pc)",
            "2026-11-09",
            "pbs.example.com:8007",
            "store1",
            "ff:ed:94",
            "ABCD-0123",
        ] {
            assert!(head.contains(needle), "{needle} missing in\n{head}");
        }
        // Exactly one block, and the client output is untouched after our header.
        assert_eq!(out.matches(BEGIN_MARKER).count(), 1);
        assert!(out.ends_with(&client_text));
        assert_eq!(
            head.lines()
                .filter(|l| l.starts_with(|c: char| c.is_ascii_digit()))
                .count(),
            3
        );
    }

    #[test]
    fn html_header_is_inserted_at_the_start_of_the_body() {
        let client = "<html><head></head><body class=\"x\">\n<p>\nBLOCK\n</p>\n</body></html>\n";
        let out = String::from_utf8(compose_html(&header(), client.as_bytes()).unwrap()).unwrap();
        let body = out.find("<body class=\"x\">").unwrap();
        let title = out.find("nestegg paper key").unwrap();
        let block = out.find("BLOCK").unwrap();
        assert!(body < title && title < block);
        assert!(out.contains("ABCD-0123"));
        assert!(out.ends_with("</body></html>\n"));
        assert!(compose_html(&header(), b"no markup here").is_err());
    }

    #[test]
    fn html_header_escapes_what_it_shows() {
        let mut h = header();
        h.backup_id = "<script>".into();
        h.hostname = "<script>".into();
        let html = html_header(&h);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn paper_key_on_a_terminal_needs_an_output_file() {
        assert!(!paperkey_stdout_allowed(false, true));
        assert!(paperkey_stdout_allowed(true, true));
        assert!(paperkey_stdout_allowed(false, false));
        assert!(paperkey_stdout_allowed(true, false));
    }

    fn layer(removable: bool, hotplug: bool, tran: Option<&str>, kind: &str) -> DeviceFacts {
        DeviceFacts {
            removable,
            hotplug,
            transport: tran.map(String::from),
            kind: Some(kind.into()),
        }
    }

    fn disk(removable: bool, hotplug: bool, tran: Option<&str>) -> DeviceFacts {
        layer(removable, hotplug, tran, "disk")
    }

    #[test]
    fn location_follows_removable_hotplug_and_transport() {
        let internal = [
            layer(false, false, Some("nvme"), "part"),
            disk(false, false, Some("nvme")),
        ];
        assert_eq!(classify_devices(&internal), Location::Internal);
        // SATA controllers often flag every port as hot-plug.
        let sata = [disk(false, true, Some("sata"))];
        assert_eq!(classify_devices(&sata), Location::Internal);
        let stick = [
            layer(true, true, Some("usb"), "part"),
            disk(true, true, Some("usb")),
        ];
        assert_eq!(classify_devices(&stick), Location::Removable);
        let usb_disk = [disk(false, false, Some("usb"))];
        assert_eq!(classify_devices(&usb_disk), Location::Removable);
        let card = [disk(true, false, None)];
        assert_eq!(classify_devices(&card), Location::Removable);
        let hotplug_only = [disk(false, true, None)];
        assert_eq!(classify_devices(&hotplug_only), Location::Removable);
        // LUKS on a stick: the layer above is plain, the disk below is USB.
        let luks_on_stick = [
            layer(false, false, None, "crypt"),
            layer(true, true, Some("usb"), "part"),
            disk(true, true, Some("usb")),
        ];
        assert_eq!(classify_devices(&luks_on_stick), Location::Removable);
        assert!(matches!(classify_devices(&[]), Location::Unknown(_)));
    }

    #[test]
    fn a_volume_spanning_internal_and_usb_disks_is_not_removable() {
        // LVM volume group with one physical volume on the internal NVMe and
        // one on a USB disk: the file may lie on either.
        let spanning = [
            layer(false, false, None, "lvm"),
            layer(false, false, Some("nvme"), "part"),
            disk(false, false, Some("nvme")),
            layer(true, true, Some("usb"), "part"),
            disk(true, true, Some("usb")),
        ];
        assert_eq!(classify_devices(&spanning), Location::Internal);
        // A removable-looking layer above an internal disk does not count.
        let odd = [
            layer(true, true, Some("usb"), "part"),
            disk(false, false, Some("nvme")),
        ];
        assert_eq!(classify_devices(&odd), Location::Internal);
        // No whole disk in the chain (loop device, image file): unknown.
        let image = [layer(false, false, None, "loop")];
        assert!(matches!(classify_devices(&image), Location::Unknown(_)));
        assert!(classify_devices(&image).needs_statement());
    }

    #[test]
    fn lsblk_and_findmnt_output_is_parsed() {
        let out = "RM=\"0\" HOTPLUG=\"0\" TRAN=\"\" TYPE=\"crypt\"\nRM=\"1\" HOTPLUG=\"1\" TRAN=\"usb\" TYPE=\"disk\"\n";
        let chain = parse_lsblk_pairs(out);
        assert_eq!(
            chain,
            vec![
                layer(false, false, None, "crypt"),
                disk(true, true, Some("usb"))
            ]
        );
        assert_eq!(classify_devices(&chain), Location::Removable);
        assert_eq!(
            parse_lsblk_pairs("RM=\"0\" HOTPLUG=\"0\" TRAN=\"sata\" TYPE=\"disk\"\n"),
            vec![disk(false, false, Some("sata"))]
        );
        assert!(parse_lsblk_pairs("\n").is_empty());
        assert_eq!(parse_findmnt_source("/dev/sda2\n").unwrap(), "/dev/sda2");
        assert_eq!(
            parse_findmnt_source("/dev/nvme0n1p2[/@home]\n").unwrap(),
            "/dev/nvme0n1p2"
        );
        assert!(parse_findmnt_source("overlay\n").is_none());
        assert!(parse_findmnt_source("tmpfs\n").is_none());
        assert!(parse_findmnt_source("").is_none());
    }

    #[test]
    fn a_copy_on_this_computers_disk_needs_the_users_statement() {
        let internal = Location::Internal;
        let unknown = Location::Unknown("overlay".into());
        let stick = Location::Removable;
        assert_eq!(decide_local_copy(&stick, false, false), LocalCopy::Accept);
        assert_eq!(decide_local_copy(&stick, false, true), LocalCopy::Accept);
        assert_eq!(
            decide_local_copy(&internal, false, false),
            LocalCopy::Refuse
        );
        assert_eq!(decide_local_copy(&internal, false, true), LocalCopy::Ask);
        assert_eq!(decide_local_copy(&internal, true, false), LocalCopy::Accept);
        assert_eq!(decide_local_copy(&internal, true, true), LocalCopy::Accept);
        assert_eq!(decide_local_copy(&unknown, false, false), LocalCopy::Refuse);
        assert_eq!(decide_local_copy(&unknown, false, true), LocalCopy::Ask);
        assert_eq!(decide_local_copy(&unknown, true, false), LocalCopy::Accept);
    }

    // ---- decision "paper code and removable are not proof enough" ----

    #[test]
    fn the_paper_code_needs_the_users_statement() {
        assert_eq!(decide_paper_statement(true, false), Statement::Given);
        assert_eq!(decide_paper_statement(true, true), Statement::Given);
        assert_eq!(decide_paper_statement(false, true), Statement::Ask);
        // No terminal and no flag: nothing is recorded.
        assert_eq!(decide_paper_statement(false, false), Statement::Missing);
    }

    fn remembered(path: &str, digest: &str) -> PaperKeyFile {
        PaperKeyFile {
            path: PathBuf::from(path),
            key_digest: digest.into(),
        }
    }

    #[test]
    fn a_remembered_paper_key_file_on_this_computer_is_in_the_way() {
        let files = [remembered("/home/alice/key.html", "d1")];
        let there = |_: &Path| true;
        let internal = |_: &Path| Location::Internal;
        let existing = existing_paperkey_files(&files, "d1", there, internal);
        assert_eq!(existing.len(), 1);
        assert_eq!(
            paperkey_files_in_the_way(&existing),
            vec![Path::new("/home/alice/key.html")]
        );
        // Not classifiable counts as on this computer, so does a place inside
        // the backup.
        for location in [
            Location::Unknown("overlay".into()),
            Location::InBackup("inside /srv".into()),
        ] {
            let existing = existing_paperkey_files(&files, "d1", there, |_| location.clone());
            assert_eq!(
                paperkey_files_in_the_way(&existing).len(),
                1,
                "{location:?}"
            );
        }
    }

    #[test]
    fn a_paper_key_file_that_is_gone_on_a_stick_or_for_another_key_is_no_obstacle() {
        let files = [remembered("/media/alice/STICK/key.html", "d1")];
        // On removable media: found, but not in the way.
        let existing = existing_paperkey_files(&files, "d1", |_| true, |_| Location::Removable);
        assert_eq!(existing.len(), 1);
        assert!(paperkey_files_in_the_way(&existing).is_empty());
        // Deleted (or the stick is not plugged in).
        let existing = existing_paperkey_files(&files, "d1", |_| false, |_| Location::Internal);
        assert!(existing.is_empty());
        assert!(paperkey_files_in_the_way(&existing).is_empty());
        // Made from an older key: the key was replaced since.
        let existing = existing_paperkey_files(&files, "d2", |_| true, |_| Location::Internal);
        assert!(existing.is_empty());
        // Nothing remembered (output piped to a printer).
        assert!(existing_paperkey_files(&[], "d1", |_| true, |_| Location::Internal).is_empty());
    }

    #[test]
    fn paper_key_files_are_remembered_per_key_without_duplicates() {
        let mut state = crate::state::State::default();
        state.remember_paperkey_file(PathBuf::from("/a/key.html"), "d1");
        state.remember_paperkey_file(PathBuf::from("/a/key.html"), "d1");
        state.remember_paperkey_file(PathBuf::from("/a/key.txt"), "d1");
        assert_eq!(state.paperkey_files.len(), 2);
        // A new key forgets the files of the old one.
        state.remember_paperkey_file(PathBuf::from("/b/key.html"), "d2");
        assert_eq!(state.paperkey_files, vec![remembered("/b/key.html", "d2")]);
        // No entry for the current key is ever dropped to make room ...
        for i in 0..80 {
            state.remember_paperkey_file(PathBuf::from(format!("/c/{i}.html")), "d2");
        }
        assert_eq!(state.paperkey_files.len(), 81);
        // It survives a round trip through the state file.
        let json = serde_json::to_string(&state).unwrap();
        let back: crate::state::State = serde_json::from_str(&json).unwrap();
        assert_eq!(back.paperkey_files, state.paperkey_files);
        // An old state file without the field still loads.
        let old: crate::state::State = serde_json::from_str("{}").unwrap();
        assert!(old.paperkey_files.is_empty());
    }

    #[test]
    fn a_full_list_forgets_only_what_is_gone_and_then_refuses() {
        use crate::state::MAX_PAPERKEY_FILES;
        let mut state = crate::state::State::default();
        for i in 0..MAX_PAPERKEY_FILES {
            state.remember_paperkey_file(PathBuf::from(format!("/c/{i}.html")), "d1");
        }
        // Everything still exists: no room (the oldest is not pushed out).
        state.prune_paperkey_files("d1", |_| true);
        assert!(state.paperkey_files_full());
        assert_eq!(state.paperkey_files.len(), MAX_PAPERKEY_FILES);
        assert_eq!(state.paperkey_files[0].path, Path::new("/c/0.html"));
        let message = paperkey_limit_message(&state.paperkey_files);
        assert!(message.contains("delete the others"), "{message}");
        assert!(message.contains("Nothing was written"), "{message}");
        assert!(message.contains("/c/0.html") && message.contains("/c/49.html"));
        // Files that are gone, or made from another key, make room.
        state.prune_paperkey_files("d1", |p| p != Path::new("/c/7.html"));
        assert!(!state.paperkey_files_full());
        assert_eq!(state.paperkey_files.len(), MAX_PAPERKEY_FILES - 1);
        state.prune_paperkey_files("d2", |_| true);
        assert!(state.paperkey_files.is_empty());
    }

    #[test]
    fn only_a_clear_not_there_means_a_remembered_file_is_gone() {
        use crate::state::paperkey_file_exists;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("key.html");
        assert!(!paperkey_file_exists(&file));
        std::fs::write(&file, "x").unwrap();
        assert!(paperkey_file_exists(&file));
        // A dangling link is still an entry on the disk.
        let link = dir.path().join("link.html");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &link).unwrap();
        assert!(paperkey_file_exists(&link));
        // The parent became a file: the path is gone.
        assert!(!paperkey_file_exists(&file.join("below")));
    }

    #[test]
    fn the_refusal_names_the_way_out_for_a_file_that_cannot_be_deleted() {
        let message = paperkey_in_the_way_message(&[Path::new("/home/alice/key.html")]);
        assert!(
            message.contains("/home/alice/key.html still exists"),
            "{message}"
        );
        assert!(message.contains("delete the file or move it to a USB stick"));
        assert!(message.contains("cannot be deleted"), "{message}");
        assert!(
            message.contains("confirm-exported --file /home/alice/key.html --accept-local-copy"),
            "{message}"
        );
        assert!(message.contains("Nothing was recorded"), "{message}");
        // Several files: the first is named, the others counted; a path with
        // a space is quoted so the command can be pasted.
        let message = paperkey_in_the_way_message(&[
            Path::new("/home/alice/My Keys/key.html"),
            Path::new("/b"),
        ]);
        assert!(message.contains("(and 1 more)"), "{message}");
        assert!(
            message.contains("--file '/home/alice/My Keys/key.html' --accept-local-copy"),
            "{message}"
        );
        assert_eq!(shell_quote(Path::new("/a/it's")), "'/a/it'\\''s'");
    }

    #[test]
    fn stdout_that_is_a_regular_file_is_found_out_and_nothing_else_is() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let fd_path = |f: &dyn AsRawFd| PathBuf::from(format!("/proc/self/fd/{}", f.as_raw_fd()));
        // A file: its path.
        let path = dir.path().join("redirected.txt");
        let file = std::fs::File::create(&path).unwrap();
        assert_eq!(
            stdout_target_at(&fd_path(&file)),
            StdoutTarget::File(std::fs::canonicalize(&path).unwrap())
        );
        // /dev/null and a pipe leave nothing to remember: the key is written.
        let null = std::fs::File::create("/dev/null").unwrap();
        assert_eq!(stdout_target_at(&fd_path(&null)), StdoutTarget::Other);
        let (read, write) = nix::unistd::pipe().unwrap();
        assert_eq!(stdout_target_at(&fd_path(&read)), StdoutTarget::Other);
        assert_eq!(stdout_target_at(&fd_path(&write)), StdoutTarget::Other);
        // A directory, a closed descriptor and a path that is not there.
        let dir_fd = std::fs::File::open(dir.path()).unwrap();
        assert_eq!(stdout_target_at(&fd_path(&dir_fd)), StdoutTarget::Other);
        assert_eq!(
            stdout_target_at(Path::new("/proc/self/fd/9999")),
            StdoutTarget::Other
        );
        // A deleted file has no path any more.
        let gone = dir.path().join("gone.txt");
        let file = std::fs::File::create(&gone).unwrap();
        std::fs::remove_file(&gone).unwrap();
        assert_eq!(stdout_target_at(&fd_path(&file)), StdoutTarget::Other);
        // A name that is not UTF-8 cannot be remembered: refused up front.
        use std::os::unix::ffi::OsStrExt;
        let odd = dir
            .path()
            .join(std::ffi::OsStr::from_bytes(b"paper-\xff-key.txt"));
        let file = std::fs::File::create(&odd).unwrap();
        match stdout_target_at(&fd_path(&file)) {
            StdoutTarget::Unstorable(why) => assert!(why.contains("UTF-8"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    fn scope_with(roots: &[&str], system: Option<SystemMounts>) -> BackupScope {
        BackupScope {
            roots: roots.iter().map(PathBuf::from).collect(),
            system,
            unknown: None,
            mount_points: None,
        }
    }

    /// Mount points, each a file system of its own (its own device number).
    fn mounted(points: &[&str]) -> Vec<(PathBuf, String)> {
        points
            .iter()
            .enumerate()
            .map(|(i, p)| (PathBuf::from(p), format!("8:{i}")))
            .collect()
    }

    #[test]
    fn a_bind_mount_of_the_same_file_system_below_a_source_is_inside_the_backup() {
        // The client tells file systems apart by the device number: a second
        // mount of the source's own file system below the source is backed
        // up with it, so a copy of the key there is inside the backup.
        let dev = |p: &str, d: &str| (PathBuf::from(p), d.to_string());
        let scope = scope_with(&["/home"], None).with_mount_points(vec![
            dev("/", "8:1"),
            dev("/home", "8:2"),
            dev("/home/alice/bind", "8:2"),
            dev("/home/alice/STICK", "8:17"),
        ]);
        assert!(
            scope
                .contains(Path::new("/home/alice/bind/key.json"))
                .is_some()
        );
        assert!(
            scope
                .contains(Path::new("/home/alice/STICK/key.json"))
                .is_none()
        );
        // Another file system mounted inside the bind mount is outside again.
        let nested = scope_with(&["/home"], None).with_mount_points(vec![
            dev("/", "8:1"),
            dev("/home/alice/bind", "8:1"),
            dev("/home/alice/bind/usb", "8:17"),
        ]);
        assert!(nested.contains(Path::new("/home/alice/bind/k")).is_some());
        assert!(
            nested
                .contains(Path::new("/home/alice/bind/usb/k"))
                .is_none()
        );
        // Behind another file system the same device does not come back in:
        // the client never gets there.
        let behind = scope_with(&["/home"], None).with_mount_points(vec![
            dev("/", "8:1"),
            dev("/home/alice/STICK", "8:17"),
            dev("/home/alice/STICK/back", "8:1"),
        ]);
        assert!(
            behind
                .contains(Path::new("/home/alice/STICK/back/k"))
                .is_none()
        );
        // Mounted over: the mount on top decides.
        let over = scope_with(&["/home"], None).with_mount_points(vec![
            dev("/", "8:1"),
            dev("/home/alice/m", "8:1"),
            dev("/home/alice/m", "8:17"),
        ]);
        assert!(over.contains(Path::new("/home/alice/m/k")).is_none());
        // The device of the source is not known: inside (the safe side).
        let unknown =
            scope_with(&["/home"], None).with_mount_points(vec![dev("/home/alice/STICK", "8:17")]);
        assert!(unknown.contains(Path::new("/home/alice/STICK/k")).is_some());
    }

    #[test]
    fn a_stick_mounted_below_a_source_is_outside_the_backup() {
        // The client stays on the file system of the source: /home/alice/STICK
        // is a file system of its own and is not backed up with /home.
        let scope = scope_with(&["/home"], None).with_mount_points(mounted(&[
            "/",
            "/home/alice/STICK",
            "/boot/efi",
        ]));
        let key = Path::new("/home/alice/STICK/key.json");
        assert_eq!(scope.contains(key), None);
        assert_eq!(
            apply_scope(Location::Removable, key, &scope),
            Location::Removable
        );
        // The rest of /home is inside, also next to the mount point.
        assert!(scope.contains(Path::new("/home/alice/key.json")).is_some());
        assert!(scope.contains(Path::new("/home/alice/STICK2/k")).is_some());
        // A mount point above the source does not matter: same file system.
        let above = scope_with(&["/home/alice"], None).with_mount_points(mounted(&["/", "/home"]));
        assert!(above.contains(Path::new("/home/alice/k")).is_some());
        // Neither does a mount point that is the source itself.
        let itself =
            scope_with(&["/mnt/data"], None).with_mount_points(mounted(&["/", "/mnt/data"]));
        assert!(itself.contains(Path::new("/mnt/data/k")).is_some());
        // A stick below the source that is not removable still needs the
        // statement, as any disk of this computer.
        assert!(apply_scope(Location::Internal, key, &scope).needs_statement());
    }

    #[test]
    fn a_file_system_below_a_source_is_inside_when_it_is_a_source_or_in_include_dev() {
        // /mnt/data is a source of its own, /srv/disk is in include_dev
        // (both are roots); /home is another source with a mount below it.
        let scope =
            scope_with(&["/home", "/home/alice/data", "/srv/disk"], None).with_mount_points(
                mounted(&["/", "/home/alice/data", "/home/alice/STICK", "/srv/disk"]),
            );
        for inside in [
            "/home/alice/data/k",
            "/home/alice/data/deep/k",
            "/srv/disk/k",
        ] {
            assert!(scope.contains(Path::new(inside)).is_some(), "{inside}");
        }
        assert!(scope.contains(Path::new("/home/alice/STICK/k")).is_none());
        // A mount below the data disk is again its own file system.
        let nested = scope_with(&["/home/alice/data"], None).with_mount_points(mounted(&[
            "/",
            "/home/alice/data",
            "/home/alice/data/usb",
        ]));
        assert!(
            nested
                .contains(Path::new("/home/alice/data/usb/k"))
                .is_none()
        );
        assert!(nested.contains(Path::new("/home/alice/data/k")).is_some());
        // Component-wise, not by prefix.
        let by_name =
            scope_with(&["/home"], None).with_mount_points(mounted(&["/", "/home/alice/US"]));
        assert!(by_name.contains(Path::new("/home/alice/USB/k")).is_some());
    }

    #[test]
    fn without_a_readable_mount_table_everything_below_a_source_counts_as_inside() {
        let scope = scope_with(&["/home"], None);
        assert!(scope.mount_points.is_none());
        assert!(scope.contains(Path::new("/home/alice/STICK/k")).is_some());
        // The system backup still follows its own rule (the layout).
        let layout = crate::layout::tests::btrfs_layout();
        let system = scope_with(&[], Some(SystemMounts::from_layout(&layout)))
            .with_mount_points(mounted(&["/", "/home"]));
        assert!(system.contains(Path::new("/home/alice/k")).is_some());
        assert!(system.contains(Path::new("/media/alice/USB/k")).is_none());
    }

    #[test]
    fn a_path_inside_a_backup_source_is_on_this_computer_whatever_the_device_is() {
        // An external disk that is itself a source (mounted below /mnt).
        let scope = scope_with(&["/home", "/mnt/data"], None);
        for path in [
            "/mnt/data/keys/key.json",
            "/mnt/data",
            "/home/alice/key.json",
        ] {
            let found = apply_scope(Location::Removable, Path::new(path), &scope);
            assert!(matches!(found, Location::InBackup(_)), "{path}: {found:?}");
            assert!(found.needs_statement(), "{path}");
        }
        // Next to a source, but not inside it (component-wise, not by prefix).
        for path in [
            "/mnt/data2/key.json",
            "/mnt/key.json",
            "/media/alice/STICK/key.json",
        ] {
            assert_eq!(
                apply_scope(Location::Removable, Path::new(path), &scope),
                Location::Removable,
                "{path}"
            );
        }
        // What the device says is kept when the path is outside the backup.
        assert_eq!(
            apply_scope(Location::Internal, Path::new("/srv/key.json"), &scope),
            Location::Internal
        );
        let message = Location::InBackup("inside /mnt/data, which is part of the backup".into());
        assert!(message.describe().starts_with("lies inside /mnt/data"));
    }

    #[test]
    fn the_system_backup_covers_the_system_file_systems_but_not_other_mounts() {
        let layout = crate::layout::tests::btrfs_layout();
        let system = SystemMounts::from_layout(&layout);
        let scope = scope_with(&[], Some(system));
        let inside = |p: &str| scope.contains(Path::new(p)).is_some();
        // Root, /home (a system mount) and the EFI partition are in system.pxar.
        assert!(inside("/etc/key.json"));
        assert!(inside("/home/alice/key.json"));
        assert!(inside("/boot/efi/key.json"));
        // A data disk, a stick and the snapshots are mounts of their own.
        assert!(!inside("/srv/data/key.json"));
        assert!(!inside("/media/alice/USB/key.json"));
        assert!(!inside("/.snapshots/1/key.json"));
        // A mount below a system mount is its own file system as well.
        let mut layout = layout;
        layout.all_points = vec!["/home/alice/nfs".into()];
        let scope = scope_with(&[], Some(SystemMounts::from_layout(&layout)));
        assert!(
            scope
                .contains(Path::new("/home/alice/nfs/key.json"))
                .is_none()
        );
        assert!(scope.contains(Path::new("/home/alice/key.json")).is_some());
        // Without system mode only the sources count.
        assert!(
            scope_with(&[], None)
                .contains(Path::new("/etc/key.json"))
                .is_none()
        );
        // The reason names the mount point.
        assert!(
            scope
                .contains(Path::new("/home/alice/key.json"))
                .unwrap()
                .contains("/home")
        );
    }

    #[test]
    fn an_unknown_scope_makes_a_removable_copy_uncertain() {
        let scope = BackupScope {
            unknown: Some("the configuration cannot be read".into()),
            ..BackupScope::default()
        };
        let found = apply_scope(
            Location::Removable,
            Path::new("/media/alice/STICK/k"),
            &scope,
        );
        assert!(matches!(found, Location::Unknown(_)));
        assert!(found.needs_statement());
        // Internal stays internal.
        assert_eq!(
            apply_scope(Location::Internal, Path::new("/srv/k"), &scope),
            Location::Internal
        );
    }

    #[test]
    fn the_scope_comes_from_the_configuration_and_a_link_is_resolved_first() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("data");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(source.join("keys")).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let config = crate::config::Config::parse(&format!(
            "[pbs]\nserver = \"pbs.example.com\"\ndatastore = \"backups\"\n\
             auth_id = \"backup@pbs!desktop\"\n[backup]\n\
             sources = [{{ archive = \"data\", path = \"{}\" }}]\n",
            source.display()
        ))
        .unwrap();
        let scope = BackupScope::for_config(&config, None);
        assert_eq!(scope.roots, vec![std::fs::canonicalize(&source).unwrap()]);
        assert!(scope.system.is_none() && scope.unknown.is_none());

        // A file in the source counts, one beside it does not ...
        let inside = source.join("keys/key.json");
        std::fs::write(&inside, "{}").unwrap();
        let beside = elsewhere.join("key.json");
        std::fs::write(&beside, "{}").unwrap();
        assert!(matches!(locate_in(&inside, &scope), Location::InBackup(_)));
        assert!(!matches!(locate_in(&beside, &scope), Location::InBackup(_)));
        // ... and a link beside the source to a file inside it counts too.
        let link = elsewhere.join("link.json");
        std::os::unix::fs::symlink(&inside, &link).unwrap();
        assert!(matches!(locate_in(&link, &scope), Location::InBackup(_)));

        // System mode without a layout: the scope is unknown.
        let system = crate::config::Config::parse(
            "[pbs]\nserver = \"pbs.example.com\"\ndatastore = \"backups\"\n\
             auth_id = \"backup@pbs!desktop\"\n[backup]\nsystem = true\n",
        )
        .unwrap();
        assert!(BackupScope::for_config(&system, None).unknown.is_some());
        let layout = crate::layout::tests::btrfs_layout();
        let with_layout = BackupScope::for_config(&system, Some(&layout));
        assert!(with_layout.system.is_some() && with_layout.unknown.is_none());
    }

    // --- recording a paper key that went into a pipe ----------------------

    fn paperkey_paths(dir: &tempfile::TempDir) -> crate::config::Paths {
        use std::os::unix::fs::PermissionsExt;
        let root = dir.path();
        for d in ["cred", "state", "run"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::set_permissions(root.join("cred"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let key = root.join("cred/encryption-key");
        std::fs::write(&key, KEY).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        crate::config::Paths {
            config_file: root.join("config.toml"),
            credentials_dir: root.join("cred"),
            state_dir: root.join("state"),
            runtime_dir: root.join("run"),
            systemctl: PathBuf::from("systemctl"),
        }
    }

    fn key_digest() -> String {
        crate::secrets::digest(&crate::secrets::Secret::new(KEY.as_bytes().to_vec()))
    }

    #[test]
    fn a_piped_paper_key_is_recorded_under_the_lock_for_the_key_that_was_printed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paperkey_paths(&dir);
        let wait = std::time::Duration::from_millis(300);
        record_piped_paperkey(&paths, wait, &key_digest()).unwrap();
        let state = crate::state::Store::new(&paths).load_state().unwrap();
        assert_eq!(state.key_last_exported_digest, Some(key_digest()));
        // The lock is let go again.
        assert!(crate::run::RunLock::try_acquire(&paths).unwrap().is_some());
    }

    #[test]
    fn a_piped_paper_key_is_not_recorded_while_a_backup_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paperkey_paths(&dir);
        let held = crate::run::RunLock::try_acquire(&paths).unwrap().unwrap();
        let start = std::time::Instant::now();
        let err =
            record_piped_paperkey(&paths, std::time::Duration::from_millis(400), &key_digest())
                .unwrap_err();
        assert!(err.contains("backup was running"), "{err}");
        assert!(start.elapsed() >= std::time::Duration::from_millis(400));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "bounded"
        );
        let state = crate::state::Store::new(&paths).load_state().unwrap();
        assert_eq!(state.key_last_exported_digest, None);
        drop(held);
    }

    #[test]
    fn a_piped_paper_key_of_a_replaced_key_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paperkey_paths(&dir);
        let err = record_piped_paperkey(
            &paths,
            std::time::Duration::from_millis(300),
            "digest-of-the-old-key",
        )
        .unwrap_err();
        assert!(err.contains("key changed"), "{err}");
        let state = crate::state::Store::new(&paths).load_state().unwrap();
        assert_eq!(state.key_last_exported_digest, None);
    }

    #[test]
    fn a_file_that_is_already_remembered_is_no_new_entry_for_the_limit() {
        let files: Vec<PaperKeyFile> = (0..crate::state::MAX_PAPERKEY_FILES)
            .map(|i| remembered(&format!("/c/{i}.html"), "d1"))
            .collect();
        // Full: another file is refused, one that is in the list is not.
        assert!(paperkey_files_full_for(&files, Path::new("/c/new.html")));
        assert!(!paperkey_files_full_for(&files, Path::new("/c/7.html")));
        // Not full: nothing is refused.
        assert!(!paperkey_files_full_for(
            &files[1..],
            Path::new("/c/new.html")
        ));
    }
}
