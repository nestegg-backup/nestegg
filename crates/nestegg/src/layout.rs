//! Disk layout of the running system, recorded with every backup as
//! `layout.json` inside `inventory.pxar`.
//!
//! The rescue system (`nestegg rescue`) reads it to recreate a bootable
//! system on an empty disk: which file systems belong to the system, their
//! types, btrfs subvolumes, swap, boot mode and boot loader. The same
//! selection function decides at backup time which mounts are added to
//! `system.pxar` (`--include-dev`), so backup and restore always agree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const LAYOUT_FILE: &str = "layout.json";
pub const LAYOUT_VERSION: u32 = 1;
/// Archive name (without `.pxar`) of the full system backup.
pub const SYSTEM_ARCHIVE: &str = "system";
/// Start of the note of [`system_mode_config`] when `system.pxar` cannot be
/// made; the run records it as a reason why the backup is incomplete.
pub const SYSTEM_SKIPPED: &str = "full system backup skipped";

/// Ignore missing, malformed and firmware placeholder UUIDs. The root is
/// injected so rescue tests never depend on the host's DMI identity.
pub fn system_uuid(dmi: &Path) -> Option<String> {
    let value = std::fs::read_to_string(dmi.join("product_uuid")).ok()?;
    normalized_system_uuid(&value)
}

pub fn normalized_system_uuid(value: &str) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    (value.len() == 36
        && value.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
        && value.bytes().any(|c| c != b'0' && c != b'-')
        && value.bytes().any(|c| c != b'f' && c != b'-'))
    .then_some(value)
}

/// File system types the rescue system can recreate.
pub const RECREATABLE_FS: &[&str] = &["ext4", "ext3", "ext2", "btrfs", "xfs", "vfat"];

/// Mount points that belong to the operating system when they are separate
/// file systems. Their content goes into `system.pxar`.
const SYSTEM_POINTS: &[&str] = &[
    "/boot",
    "/boot/efi",
    "/efi",
    "/home",
    "/root",
    "/opt",
    "/srv",
    "/usr",
    "/usr/local",
    "/var",
    "/var/cache",
    "/var/lib",
    "/var/log",
];

/// Exclusions added to every backup in system mode. Patterns starting with
/// `/` are anchored at the archive root, i.e. `/` for `system.pxar`.
pub const SYSTEM_EXCLUDES: &[&str] = &[
    "/tmp/*",
    "/var/tmp/*",
    "/var/cache/apt/archives/*.deb",
    "/var/lib/nestegg/restore-staging",
    "/var/lib/systemd/coredump/*",
    "/swapfile",
    "/swap.img",
    "/swap/swapfile",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Layout {
    pub version: u32,
    /// `uefi` or `bios`.
    pub boot_mode: String,
    /// `grub`, `systemd-boot` or `unknown`.
    pub bootloader: String,
    /// `ID` and `VERSION_ID` from os-release.
    pub os_id: String,
    pub os_version: String,
    pub os_name: String,
    pub hostname: String,
    /// DMI product UUID, when available; absent in older inventories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_uuid: Option<String>,
    /// Size of the disk that holds `/`, if known.
    pub root_disk_bytes: Option<u64>,
    pub mounts: Vec<MountRecord>,
    pub swap: Vec<SwapRecord>,
    /// Every mount point of the running system, of any file system type
    /// (NFS, FUSE, autofs, tmpfs, ...). Only used at backup time to decide
    /// whether a configured source lies inside `system.pxar`.
    #[serde(skip)]
    pub all_points: Vec<String>,
    /// btrfs subvolumes inside the system subvolumes that are not mounted
    /// themselves (e.g. `/home/alice/Projects`, `/var/lib/machines`). They
    /// are included in `system.pxar` and restored as normal folders.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nested_subvolumes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MountRecord {
    pub point: String,
    pub fstype: String,
    /// Device as shown in the mount table, e.g. `/dev/nvme0n1p2`.
    pub source: String,
    /// Root of the mount inside the file system (`/@home` for a btrfs
    /// subvolume, `/` otherwise).
    pub fsroot: String,
    pub options: String,
    pub uuid: Option<String>,
    pub size_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    /// The file system lies on a LUKS container (UUID of the container).
    pub crypt_uuid: Option<String>,
    pub on_lvm: bool,
    /// Bind mount of a directory (not a whole file system or a btrfs
    /// subvolume), e.g. `/srv/data` from `/home/alice/data`, or the private
    /// `/tmp` of a systemd service.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bind: bool,
    /// File system options from the mount table (`compress=zstd:3`, ...).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub super_options: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SwapRecord {
    /// `partition` or `file`.
    pub kind: String,
    pub path: String,
    pub size_bytes: u64,
    pub uuid: Option<String>,
    pub crypt_uuid: Option<String>,
}

/// What happens with a mount of the old system when it is restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountRole {
    /// The root file system.
    Root,
    /// Separate system file system; its content is part of `system.pxar`.
    System,
    /// btrfs subvolume on the root device that is not backed up (snapshots,
    /// swap); recreated empty so the fstab entry keeps working.
    EmptySubvolume,
    /// Anything else (data disks, removable media): not part of the system.
    Other,
}

impl Layout {
    pub fn root(&self) -> Option<&MountRecord> {
        self.mounts.iter().find(|m| m.point == "/")
    }

    pub fn role(&self, m: &MountRecord) -> MountRole {
        let Some(root) = self.root() else {
            return MountRole::Other;
        };
        mount_role(m, root)
    }

    /// Mount point of the EFI system partition, if any.
    pub fn efi_point(&self) -> Option<&str> {
        self.mounts
            .iter()
            .find(|m| m.fstype == "vfat" && (m.point == "/boot/efi" || m.point == "/efi"))
            .map(|m| m.point.as_str())
    }

    /// Bytes of data to restore (each file system counted once).
    pub fn used_bytes(&self) -> u64 {
        let mut seen = std::collections::HashSet::new();
        self.mounts
            .iter()
            .filter(|m| matches!(self.role(m), MountRole::Root | MountRole::System))
            .filter(|m| seen.insert(m.uuid.clone().unwrap_or_else(|| m.source.clone())))
            .filter_map(|m| m.used_bytes)
            .sum()
    }
}

fn is_skipped_point(point: &str) -> bool {
    point.contains("/.snapshots")
        || [
            "/tmp",
            "/var/tmp",
            "/timeshift",
            "/timeshift-btrfs",
            "/run",
            "/media",
            "/mnt",
            "/snap",
            "/swap",
            "/proc",
            "/sys",
            "/dev",
        ]
        .iter()
        .any(|p| point == *p || point.starts_with(&format!("{p}/")))
        || point.starts_with("/var/lib/docker")
        || point.starts_with("/var/lib/containers")
}

pub fn mount_role(m: &MountRecord, root: &MountRecord) -> MountRole {
    if m.point == "/" {
        return MountRole::Root;
    }
    if m.bind {
        // The directory is part of another mount; its own fstab line stays.
        return MountRole::Other;
    }
    let same_btrfs = m.fstype == "btrfs" && root.fstype == "btrfs" && m.source == root.source;
    if is_skipped_point(&m.point) {
        return if same_btrfs {
            MountRole::EmptySubvolume
        } else {
            MountRole::Other
        };
    }
    if same_btrfs {
        return MountRole::System;
    }
    if SYSTEM_POINTS.contains(&m.point.as_str()) && RECREATABLE_FS.contains(&m.fstype.as_str()) {
        return MountRole::System;
    }
    MountRole::Other
}

/// Mount points to pass as `--include-dev` for `system.pxar`.
pub fn system_include_dev(layout: &Layout) -> Vec<PathBuf> {
    layout
        .mounts
        .iter()
        .filter(|m| layout.role(m) == MountRole::System)
        .map(|m| PathBuf::from(&m.point))
        .collect()
}

/// Whether an fstab entry belongs to the system backup, i.e. an unmounted
/// file system there would make `system.pxar` incomplete: a system mount
/// point, or an entry on a device of the system (e.g. another subvolume of
/// the root file system at `/var/lib/mysql`).
pub fn is_system_entry(layout: &Layout, spec: &str, point: &Path) -> bool {
    let Some(text) = point.to_str() else {
        return false;
    };
    if SYSTEM_POINTS.contains(&text) {
        return true;
    }
    if is_skipped_point(text) {
        return false;
    }
    layout
        .mounts
        .iter()
        .filter(|m| matches!(layout.role(m), MountRole::Root | MountRole::System))
        .any(|m| {
            m.point == text
                || m.source == spec
                || m.uuid
                    .as_deref()
                    .is_some_and(|u| spec == format!("UUID={u}"))
        })
}

/// Mounts below `/` that are left out of `system.pxar` on purpose (snapshot
/// subvolumes, bind mounts, removable media); no "not backed up" warning.
pub fn intentionally_skipped(layout: &Layout) -> Vec<PathBuf> {
    layout
        .mounts
        .iter()
        .filter(|m| {
            system_bind(layout, m)
                || layout.role(m) == MountRole::EmptySubvolume
                || m.point.starts_with("/media/")
                || m.point.starts_with("/run/")
        })
        .map(|m| PathBuf::from(&m.point))
        .collect()
}

/// A bind mount (or second mount) of a file system that is already part of
/// `system.pxar`: its content is in the archive once, under the original
/// path, so the bind target is excluded.
pub fn system_bind(layout: &Layout, m: &MountRecord) -> bool {
    m.bind
        && layout.mounts.iter().any(|x| {
            !x.bind
                && x.source == m.source
                && matches!(layout.role(x), MountRole::Root | MountRole::System)
        })
}

/// Fixed directory part of an anchored exclude pattern: `/tmp/*` → `tmp`.
fn pattern_dir(pattern: &str) -> &str {
    pattern
        .trim_start_matches('/')
        .split(['*', '?', '['])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/')
}

/// Configuration for one run in system mode: `system.pxar` from `/` with
/// the system mounts included, plus every configured source that is not
/// already part of it (e.g. a data disk). Returns notes for the run log.
/// `exists` tells whether a path exists (used to keep the system
/// exclusions from hiding data in the other archives).
pub fn system_mode_config(
    config: &crate::config::Config,
    layout: &Layout,
    canonicalize: impl Fn(&Path) -> Option<PathBuf>,
    exists: impl Fn(&Path) -> bool,
) -> (crate::config::Config, Vec<String>) {
    let mut notes = Vec::new();
    let mut c = config.clone();
    // The rescue system needs layout.json from the inventory.
    c.backup.inventory = true;
    match layout.root() {
        Some(root) if RECREATABLE_FS.contains(&root.fstype.as_str()) && root.fstype != "vfat" => {}
        other => {
            notes.push(format!(
                "warning: {SYSTEM_SKIPPED}: the root file system ({}) cannot be \
                 recreated by the rescue stick; only the configured folders are backed up",
                other.map_or("unknown", |r| r.fstype.as_str())
            ));
            return (c, notes);
        }
    }
    let covered = |source: &crate::config::Source| -> bool {
        // A source that must be its own mounted file system keeps its own
        // archive and mount check.
        if source.require_mount {
            return false;
        }
        let Some(p) = canonicalize(&source.path) else {
            return false;
        };
        // Below a path that system.pxar excludes (e.g. /var/tmp/builds).
        if SYSTEM_EXCLUDES
            .iter()
            .any(|e| p.starts_with(Path::new("/").join(pattern_dir(e))))
        {
            return false;
        }
        // The deepest mount point of ANY type that contains the path decides
        // (NFS, FUSE or autofs below / must not count as part of /).
        let recorded: Vec<&str> = layout.mounts.iter().map(|m| m.point.as_str()).collect();
        let points: Vec<&str> = if layout.all_points.is_empty() {
            recorded
        } else {
            layout.all_points.iter().map(String::as_str).collect()
        };
        let Some(deepest) = points
            .into_iter()
            .filter(|point| p.starts_with(point))
            .max_by_key(|point| point.len())
        else {
            return false;
        };
        layout
            .mounts
            .iter()
            .rev()
            .find(|m| m.point == deepest)
            .is_some_and(|m| matches!(layout.role(m), MountRole::Root | MountRole::System))
    };
    let mut sources = vec![crate::config::Source {
        archive: SYSTEM_ARCHIVE.into(),
        path: PathBuf::from("/"),
        optional: false,
        require_mount: false,
    }];
    for s in &config.backup.sources {
        if covered(s) {
            notes.push(format!(
                "{} is part of {SYSTEM_ARCHIVE}.pxar (system mode)",
                s.path.display()
            ));
        } else {
            sources.push(s.clone());
        }
    }
    c.backup.sources = sources;
    for dev in system_include_dev(layout) {
        if !c.backup.include_dev.contains(&dev) {
            c.backup.include_dev.push(dev);
        }
    }
    for point in &layout.nested_subvolumes {
        let dev = PathBuf::from(point);
        if !c.backup.include_dev.contains(&dev) {
            notes.push(format!(
                "{point} is a btrfs subvolume; it is part of {SYSTEM_ARCHIVE}.pxar \
                 (restored as a normal folder)"
            ));
            c.backup.include_dev.push(dev);
        }
    }
    // Exclude patterns apply to every archive of the run, anchored at each
    // archive's root. Leave a system exclusion out when another archive has
    // that path, so it never hides data there (system.pxar just gets bigger).
    let others: Vec<PathBuf> = c.backup.sources[1..]
        .iter()
        .filter_map(|s| canonicalize(&s.path))
        .collect();
    let mut excludes: Vec<String> = SYSTEM_EXCLUDES.iter().map(|e| e.to_string()).collect();
    // Bind mounts of system file systems: the content is already in the
    // archive under its original path.
    excludes.extend(
        layout
            .mounts
            .iter()
            .filter(|m| system_bind(layout, m))
            .map(|m| format!("{}/*", m.point)),
    );
    for e in &excludes {
        let fixed = pattern_dir(e);
        if let Some(clash) = others.iter().find(|o| exists(&o.join(fixed))) {
            notes.push(format!(
                "{} also exists in {}; {e} is not excluded from {SYSTEM_ARCHIVE}.pxar",
                clash.join(fixed).display(),
                clash.display()
            ));
            continue;
        }
        if !c.backup.excludes.iter().any(|x| x == e) {
            c.backup.excludes.push(e.clone());
        }
    }
    if layout.bootloader != "grub" {
        notes.push(format!(
            "warning: boot loader '{}' detected; the rescue stick only reinstalls GRUB",
            layout.bootloader
        ));
    }
    (c, notes)
}

// ---------------------------------------------------------------------------
// Capture

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq)]
pub struct MountInfo {
    pub dev: String,
    pub fsroot: String,
    pub point: String,
    pub options: String,
    pub fstype: String,
    pub source: String,
    pub super_options: String,
}

pub fn parse_mountinfo(text: &str) -> Vec<MountInfo> {
    text.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let l: Vec<&str> = left.split_whitespace().collect();
            let r: Vec<&str> = right.split_whitespace().collect();
            if l.len() < 6 || r.len() < 2 {
                return None;
            }
            Some(MountInfo {
                dev: l[2].to_string(),
                fsroot: unescape(l[3]),
                point: unescape(l[4]),
                options: l[5].to_string(),
                fstype: r[0].to_string(),
                source: unescape(r[1]),
                super_options: r.get(2).map(|o| o.to_string()).unwrap_or_default(),
            })
        })
        .collect()
}

fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            out.push(u8::from_str_radix(&field[i + 1..i + 4], 8).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Block devices from `lsblk -J -b -l -o PATH,KNAME,PKNAME,MAJ:MIN,TYPE,SIZE,FSTYPE,UUID`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BlockDev {
    pub path: String,
    pub kname: String,
    pub pkname: Option<String>,
    pub majmin: String,
    pub kind: String,
    pub size: Option<u64>,
    pub fstype: Option<String>,
    pub uuid: Option<String>,
}

pub fn parse_lsblk(json: &str) -> Result<Vec<BlockDev>> {
    let v: Value = serde_json::from_str(json).context("unexpected lsblk output")?;
    let s = |d: &Value, k: &str| d[k].as_str().map(String::from);
    Ok(v["blockdevices"]
        .as_array()
        .context("lsblk: no blockdevices")?
        .iter()
        .map(|d| BlockDev {
            path: s(d, "path").unwrap_or_default(),
            kname: s(d, "kname").unwrap_or_default(),
            pkname: s(d, "pkname"),
            majmin: s(d, "maj:min").unwrap_or_default(),
            kind: s(d, "type").unwrap_or_default(),
            size: d["size"]
                .as_u64()
                .or_else(|| d["size"].as_str().and_then(|x| x.parse().ok())),
            fstype: s(d, "fstype"),
            uuid: s(d, "uuid"),
        })
        .collect())
}

pub fn lsblk() -> Result<Vec<BlockDev>> {
    let out = Command::new("lsblk")
        .args([
            "-J",
            "-b",
            "-l",
            "-o",
            "PATH,KNAME,PKNAME,MAJ:MIN,TYPE,SIZE,FSTYPE,UUID",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("cannot run lsblk")?;
    if !out.status.success() {
        anyhow::bail!("lsblk failed");
    }
    parse_lsblk(&String::from_utf8_lossy(&out.stdout))
}

/// Find the block device for a mount source or device number.
pub fn find_dev<'a>(devs: &'a [BlockDev], source: &str, majmin: &str) -> Option<&'a BlockDev> {
    let canonical = std::fs::canonicalize(source).ok();
    devs.iter()
        .find(|d| d.majmin == majmin)
        .or_else(|| devs.iter().find(|d| d.path == source))
        .or_else(|| {
            let c = canonical.as_ref()?;
            devs.iter()
                .find(|d| std::fs::canonicalize(&d.path).ok().as_ref() == Some(c))
        })
}

/// Walk up the device tree: (LUKS container UUID, on LVM, disk size).
pub fn ancestry(devs: &[BlockDev], dev: &BlockDev) -> (Option<String>, bool, Option<u64>) {
    let by_kname: HashMap<&str, &BlockDev> = devs.iter().map(|d| (d.kname.as_str(), d)).collect();
    let mut crypt = None;
    let mut lvm = false;
    let mut disk = None;
    let mut cur = Some(dev);
    let mut guard = 0;
    while let Some(d) = cur {
        guard += 1;
        if guard > 16 {
            break;
        }
        match d.kind.as_str() {
            "crypt" => {
                if let Some(parent) = d.pkname.as_deref().and_then(|p| by_kname.get(p)) {
                    crypt = parent.uuid.clone();
                }
            }
            "lvm" => lvm = true,
            "disk" => disk = d.size,
            _ => {}
        }
        cur = d.pkname.as_deref().and_then(|p| by_kname.get(p).copied());
    }
    (crypt, lvm, disk)
}

pub fn parse_os_release(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

/// `/proc/swaps` → (kind, path, size in bytes). zram is ignored.
pub fn parse_proc_swaps(text: &str) -> Vec<(String, String, u64)> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 3 || f[0].starts_with("/dev/zram") {
                return None;
            }
            Some((
                f[1].to_string(),
                unescape(f[0]),
                f[2].parse::<u64>().ok()? * 1024,
            ))
        })
        .collect()
}

fn statvfs_sizes(point: &Path) -> (Option<u64>, Option<u64>) {
    match nix::sys::statvfs::statvfs(point) {
        Ok(st) => {
            let frag = st.fragment_size();
            let total = st.blocks() * frag;
            let free = st.blocks_free() * frag;
            (Some(total), Some(total.saturating_sub(free)))
        }
        Err(_) => (None, None),
    }
}

fn detect_bootloader(efi_point: Option<&str>) -> String {
    let has = |p: &str| Path::new(p).exists();
    if let Some(efi) = efi_point
        && Path::new(efi).join("loader/loader.conf").exists()
        && !has("/boot/grub/grub.cfg")
    {
        return "systemd-boot".into();
    }
    if has("/boot/grub/grub.cfg") || has("/boot/grub2/grub.cfg") || has("/usr/sbin/grub-install") {
        return "grub".into();
    }
    "unknown".into()
}

/// Paths of btrfs subvolumes (from `btrfs subvolume list -a /`) that lie
/// inside a mounted system subvolume but are not mounted themselves.
pub fn nested_subvolume_points(
    list: &str,
    mounts: &[MountRecord],
    all_points: &[String],
) -> Vec<String> {
    let Some(root) = mounts.iter().find(|m| m.point == "/") else {
        return Vec::new();
    };
    // Mounted subvolumes of the root device: (path in the file system, point).
    let mounted: Vec<(String, &str)> = mounts
        .iter()
        .filter(|m| m.fstype == "btrfs" && m.source == root.source && !m.bind)
        .filter(|m| matches!(mount_role(m, root), MountRole::Root | MountRole::System))
        .map(|m| (m.fsroot.trim_matches('/').to_string(), m.point.as_str()))
        .collect();
    let mut out = Vec::new();
    for line in list.lines() {
        let Some((_, path)) = line.split_once(" path ") else {
            continue;
        };
        let path = path.trim().trim_start_matches("<FS_TREE>/");
        let best = mounted
            .iter()
            .filter(|(fsroot, _)| {
                fsroot.is_empty() || path == fsroot || path.starts_with(&format!("{fsroot}/"))
            })
            .max_by_key(|(fsroot, _)| fsroot.len());
        let Some((fsroot, point)) = best else {
            continue;
        };
        if path == fsroot {
            continue; // the mounted subvolume itself
        }
        let rest = if fsroot.is_empty() {
            path
        } else {
            &path[fsroot.len() + 1..]
        };
        let full = Path::new(point).join(rest).to_string_lossy().into_owned();
        if all_points.contains(&full) || is_skipped_point(&full) {
            continue;
        }
        out.push(full);
    }
    // Each nested subvolume needs its own --include-dev, also one inside
    // another nested subvolume.
    out.sort();
    out.dedup();
    out
}

/// Record the layout of the running system.
pub fn capture() -> Result<Layout> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .context("cannot read /proc/self/mountinfo")?;
    let devs = lsblk().unwrap_or_default();
    let mut mounts = Vec::new();
    let all = parse_mountinfo(&mountinfo);
    let all_points: Vec<String> = all.iter().map(|m| m.point.clone()).collect();
    for mi in all {
        if !crate::layout::RECREATABLE_FS.contains(&mi.fstype.as_str())
            && !matches!(
                mi.fstype.as_str(),
                "f2fs" | "zfs" | "bcachefs" | "ntfs3" | "exfat"
            )
        {
            continue;
        }
        let dev = find_dev(&devs, &mi.source, &mi.dev);
        let (crypt_uuid, on_lvm, _) = dev.map(|d| ancestry(&devs, d)).unwrap_or_default();
        let (size, used) = statvfs_sizes(Path::new(&mi.point));
        // A btrfs subvolume's top directory has inode 256; any other mount
        // of a sub-directory is a bind mount.
        let bind = (mi.fsroot != "/"
            && !(mi.fstype == "btrfs"
                && std::fs::metadata(&mi.point)
                    .map(|md| std::os::unix::fs::MetadataExt::ino(&md) == 256)
                    .unwrap_or(false)))
            // The same file system or subvolume mounted a second time.
            || mounts.iter().any(|x: &MountRecord| {
                x.source == mi.source && x.fsroot == mi.fsroot && x.point != mi.point
            });
        mounts.push(MountRecord {
            bind,
            super_options: mi.super_options.clone(),
            uuid: dev.and_then(|d| d.uuid.clone()),
            point: mi.point,
            fstype: mi.fstype,
            source: mi.source,
            fsroot: mi.fsroot,
            options: mi.options,
            size_bytes: size,
            used_bytes: used,
            crypt_uuid,
            on_lvm,
        });
    }
    // Keep the last mount per point (over-mounts hide earlier ones).
    let mut dedup: Vec<MountRecord> = Vec::new();
    for m in mounts {
        dedup.retain(|x| x.point != m.point);
        dedup.push(m);
    }
    let root_disk_bytes = dedup.iter().find(|m| m.point == "/").and_then(|m| {
        let dev = find_dev(&devs, &m.source, "")?;
        ancestry(&devs, dev).2
    });
    let swap = std::fs::read_to_string("/proc/swaps")
        .map(|t| parse_proc_swaps(&t))
        .unwrap_or_default()
        .into_iter()
        .map(|(kind, path, size_bytes)| {
            let dev = (kind == "partition")
                .then(|| find_dev(&devs, &path, ""))
                .flatten();
            SwapRecord {
                uuid: dev.and_then(|d| d.uuid.clone()),
                crypt_uuid: dev.and_then(|d| ancestry(&devs, d).0),
                kind,
                path,
                size_bytes,
            }
        })
        .collect();
    let os = std::fs::read_to_string("/etc/os-release")
        .map(|t| parse_os_release(&t))
        .unwrap_or_default();
    let get = |k: &str| os.get(k).cloned().unwrap_or_default();
    let efi_point = dedup
        .iter()
        .find(|m| m.fstype == "vfat" && (m.point == "/boot/efi" || m.point == "/efi"))
        .map(|m| m.point.clone());
    let nested_subvolumes = match dedup.iter().find(|m| m.point == "/") {
        Some(root) if root.fstype == "btrfs" => Command::new("btrfs")
            .args(["subvolume", "list", "-a", "/"])
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                nested_subvolume_points(&String::from_utf8_lossy(&o.stdout), &dedup, &all_points)
            })
            .unwrap_or_default()
            .into_iter()
            .filter(|p| Path::new(p).is_dir())
            .collect(),
        _ => Vec::new(),
    };
    Ok(Layout {
        version: LAYOUT_VERSION,
        boot_mode: if Path::new("/sys/firmware/efi").exists() {
            "uefi".into()
        } else {
            "bios".into()
        },
        bootloader: detect_bootloader(efi_point.as_deref()),
        os_id: get("ID"),
        os_version: get("VERSION_ID"),
        os_name: get("PRETTY_NAME"),
        hostname: crate::config::hostname(),
        system_uuid: system_uuid(Path::new("/sys/class/dmi/id")),
        root_disk_bytes,
        mounts: dedup,
        swap,
        all_points,
        nested_subvolumes,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn rec(point: &str, fstype: &str, source: &str, fsroot: &str, uuid: &str) -> MountRecord {
        MountRecord {
            point: point.into(),
            fstype: fstype.into(),
            source: source.into(),
            fsroot: fsroot.into(),
            options: "rw,relatime".into(),
            uuid: Some(uuid.into()),
            size_bytes: Some(100 << 30),
            used_bytes: Some(10 << 30),
            crypt_uuid: None,
            on_lvm: false,
            bind: false,
            super_options: String::new(),
        }
    }

    #[test]
    fn system_identity_is_optional_and_rejects_firmware_placeholders() {
        let mut old = serde_json::to_value(btrfs_layout()).unwrap();
        old.as_object_mut().unwrap().remove("system_uuid");
        let layout: Layout = serde_json::from_value(old).unwrap();
        assert_eq!(layout.system_uuid, None);
        let dmi = tempfile::tempdir().unwrap();
        for value in [
            "",
            "unknown",
            "00000000-0000-0000-0000-000000000000",
            "FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF",
        ] {
            std::fs::write(dmi.path().join("product_uuid"), value).unwrap();
            assert_eq!(system_uuid(dmi.path()), None);
        }
        std::fs::write(
            dmi.path().join("product_uuid"),
            "12345678-1234-4321-9876-123456789ABC\n",
        )
        .unwrap();
        assert_eq!(
            system_uuid(dmi.path()).as_deref(),
            Some("12345678-1234-4321-9876-123456789abc")
        );
    }

    pub fn btrfs_layout() -> Layout {
        Layout {
            version: 1,
            boot_mode: "uefi".into(),
            bootloader: "grub".into(),
            os_id: "linuxmint".into(),
            os_version: "22.2".into(),
            os_name: "Linux Mint 22.2".into(),
            hostname: "desktop".into(),
            system_uuid: None,
            root_disk_bytes: Some(500 << 30),
            mounts: vec![
                rec("/", "btrfs", "/dev/nvme0n1p2", "/@", "1111"),
                rec("/home", "btrfs", "/dev/nvme0n1p2", "/@home", "1111"),
                rec(
                    "/.snapshots",
                    "btrfs",
                    "/dev/nvme0n1p2",
                    "/@snapshots",
                    "1111",
                ),
                rec("/boot/efi", "vfat", "/dev/nvme0n1p1", "/", "AAAA-BBBB"),
                rec("/srv/data", "ext4", "/dev/sda1", "/", "2222"),
                rec("/media/alice/USB", "vfat", "/dev/sdc1", "/", "CCCC-DDDD"),
            ],
            swap: vec![SwapRecord {
                kind: "partition".into(),
                path: "/dev/nvme0n1p3".into(),
                size_bytes: 8 << 30,
                uuid: Some("3333".into()),
                crypt_uuid: None,
            }],
            all_points: Vec::new(),
            nested_subvolumes: Vec::new(),
        }
    }

    #[test]
    fn roles() {
        let l = btrfs_layout();
        let roles: Vec<(&str, MountRole)> = l
            .mounts
            .iter()
            .map(|m| (m.point.as_str(), l.role(m)))
            .collect();
        assert_eq!(
            roles,
            vec![
                ("/", MountRole::Root),
                ("/home", MountRole::System),
                ("/.snapshots", MountRole::EmptySubvolume),
                ("/boot/efi", MountRole::System),
                ("/srv/data", MountRole::Other),
                ("/media/alice/USB", MountRole::Other),
            ]
        );
        assert_eq!(
            system_include_dev(&l),
            vec![PathBuf::from("/home"), PathBuf::from("/boot/efi")]
        );
        assert_eq!(l.efi_point(), Some("/boot/efi"));
        // btrfs counted once, ESP once.
        assert_eq!(l.used_bytes(), 20 << 30);
    }

    #[test]
    fn separate_home_partition_is_system() {
        let mut l = btrfs_layout();
        l.mounts = vec![
            rec("/", "ext4", "/dev/sda2", "/", "1"),
            rec("/home", "ext4", "/dev/sda3", "/", "2"),
            rec("/srv", "xfs", "/dev/sdb1", "/", "3"),
            rec("/data", "ext4", "/dev/sdb2", "/", "4"),
        ];
        let roles: Vec<MountRole> = l.mounts.iter().map(|m| l.role(m)).collect();
        assert_eq!(
            roles,
            vec![
                MountRole::Root,
                MountRole::System,
                MountRole::System,
                MountRole::Other
            ]
        );
    }

    #[test]
    fn system_mode_replaces_covered_sources() {
        let l = btrfs_layout();
        let mut config = crate::config::example("pbs.example.com", "store", "a@pbs!t");
        config.backup.sources.push(crate::config::Source {
            archive: "data".into(),
            path: "/srv/data".into(),
            optional: false,
            require_mount: true,
        });
        let (c, notes) = system_mode_config(&config, &l, |p| Some(p.to_path_buf()), |_| false);
        let names: Vec<&str> = c
            .backup
            .sources
            .iter()
            .map(|s| s.archive.as_str())
            .collect();
        assert_eq!(names, vec!["system", "data"]);
        assert!(c.backup.include_dev.contains(&PathBuf::from("/boot/efi")));
        assert!(c.backup.excludes.iter().any(|e| e == "/swapfile"));
        assert!(
            notes
                .iter()
                .any(|n| n.contains("/home is part of system.pxar"))
        );
        assert!(!notes.iter().any(|n| n.starts_with("warning")));
        c.validate().unwrap();
    }

    #[test]
    fn system_mode_keeps_foreign_sources_separate() {
        let mut l = btrfs_layout();
        // /srv/share is NFS (not recorded), /srv/bind a bind mount of the root fs.
        let mut bind = rec(
            "/srv/bind",
            "btrfs",
            "/dev/nvme0n1p2",
            "/@home/alice/x",
            "1111",
        );
        bind.bind = true;
        l.mounts.push(bind);
        l.all_points = l.mounts.iter().map(|m| m.point.clone()).collect();
        l.all_points.push("/srv/share".into());
        let source = |archive: &str, path: &str, require_mount| crate::config::Source {
            archive: archive.into(),
            path: path.into(),
            optional: false,
            require_mount,
        };
        let mut config = crate::config::example("pbs.example.com", "store", "a@pbs!t");
        config.backup.sources = vec![
            source("share", "/srv/share/docs", false),
            source("bind", "/srv/bind", false),
            source("musthave", "/home/alice/disk", true),
            source("home", "/home", false),
            source("builds", "/var/tmp/builds", false),
        ];
        let (c, notes) = system_mode_config(
            &config,
            &l,
            |p| Some(p.to_path_buf()),
            |p| p == Path::new("/srv/share/docs/tmp"),
        );
        let names: Vec<&str> = c
            .backup
            .sources
            .iter()
            .map(|s| s.archive.as_str())
            .collect();
        assert_eq!(names, vec!["system", "share", "bind", "musthave", "builds"]);
        assert_eq!(l.role(l.mounts.last().unwrap()), MountRole::Other);
        assert!(!system_include_dev(&l).contains(&PathBuf::from("/srv/bind")));
        // The share has a tmp folder: /tmp/* must not hide it.
        assert!(!c.backup.excludes.iter().any(|e| e == "/tmp/*"));
        assert!(c.backup.excludes.iter().any(|e| e == "/var/tmp/*"));
        assert!(notes.iter().any(|n| n.contains("/tmp/* is not excluded")));
        assert!(is_system_entry(&l, "UUID=9", Path::new("/home")));
        assert!(!is_system_entry(&l, "UUID=9", Path::new("/mnt/usb")));
        // Another subvolume of the root file system.
        assert!(is_system_entry(
            &l,
            "UUID=1111",
            Path::new("/var/lib/mysql")
        ));
        assert!(!is_system_entry(&l, "UUID=1111", Path::new("/.snapshots")));
        assert!(intentionally_skipped(&l).contains(&PathBuf::from("/.snapshots")));
    }

    #[test]
    fn nested_subvolumes_are_found() {
        let l = btrfs_layout();
        let list = "\
ID 256 gen 50 top level 5 path <FS_TREE>/@
ID 257 gen 50 top level 5 path <FS_TREE>/@home
ID 258 gen 50 top level 5 path <FS_TREE>/@snapshots
ID 259 gen 50 top level 257 path <FS_TREE>/@home/alice/Projects
ID 260 gen 50 top level 256 path <FS_TREE>/@/var/lib/machines
ID 261 gen 50 top level 256 path <FS_TREE>/@/var/lib/docker/btrfs/subvolumes/abc
ID 262 gen 50 top level 258 path <FS_TREE>/@snapshots/1/snapshot
ID 263 gen 50 top level 5 path <FS_TREE>/timeshift-btrfs/snapshots/x/@
";
        let points: Vec<String> = l.mounts.iter().map(|m| m.point.clone()).collect();
        assert_eq!(
            nested_subvolume_points(list, &l.mounts, &points),
            vec!["/home/alice/Projects", "/var/lib/machines"]
        );
    }

    #[test]
    fn system_binds_are_excluded_once() {
        let mut l = btrfs_layout();
        let mut bind = rec("/srv/www", "btrfs", "/dev/nvme0n1p2", "/@/var/www", "1111");
        bind.bind = true;
        let mut usb = rec("/srv/usb", "vfat", "/dev/sdc1", "/x", "CCCC-DDDD");
        usb.bind = true;
        l.mounts.extend([bind, usb]);
        let config = crate::config::example("pbs.example.com", "store", "a@pbs!t");
        let (c, _) = system_mode_config(&config, &l, |p| Some(p.to_path_buf()), |_| false);
        assert!(c.backup.excludes.iter().any(|e| e == "/srv/www/*"));
        assert!(!c.backup.excludes.iter().any(|e| e == "/srv/usb/*"));
        let quiet = intentionally_skipped(&l);
        assert!(quiet.contains(&PathBuf::from("/srv/www")));
        assert!(!quiet.contains(&PathBuf::from("/srv/usb")));
    }

    #[test]
    fn system_mode_needs_a_recreatable_root() {
        let mut l = btrfs_layout();
        l.mounts.retain(|m| m.point != "/");
        let config = crate::config::example("pbs.example.com", "store", "a@pbs!t");
        let (c, notes) = system_mode_config(&config, &l, |p| Some(p.to_path_buf()), |_| false);
        assert_eq!(c.backup.sources, config.backup.sources);
        assert!(notes[0].contains("full system backup skipped"));
    }

    #[test]
    fn mountinfo_parsing() {
        let text = "\
29 1 0:26 /@ / rw,noatime shared:1 - btrfs /dev/nvme0n1p2 rw,compress=zstd:3,subvol=/@
30 29 0:26 /@home /home rw,noatime shared:2 - btrfs /dev/nvme0n1p2 rw,subvol=/@home
31 29 259:1 / /boot/efi rw,relatime shared:3 - vfat /dev/nvme0n1p1 rw,fmask=0077
32 29 8:1 / /media/a\\040b rw - ext4 /dev/sdb1 rw
";
        let m = parse_mountinfo(text);
        assert_eq!(m.len(), 4);
        assert_eq!(m[1].fsroot, "/@home");
        assert_eq!(m[2].dev, "259:1");
        assert_eq!(m[3].point, "/media/a b");
    }

    #[test]
    fn lsblk_and_ancestry() {
        let json = r#"{"blockdevices":[
 {"path":"/dev/nvme0n1","kname":"nvme0n1","pkname":null,"maj:min":"259:0","type":"disk","size":512110190592,"fstype":null,"uuid":null},
 {"path":"/dev/nvme0n1p3","kname":"nvme0n1p3","pkname":"nvme0n1","maj:min":"259:3","type":"part","size":500000000000,"fstype":"crypto_LUKS","uuid":"luks-uuid"},
 {"path":"/dev/mapper/vgmint-root","kname":"dm-1","pkname":"dm-0","maj:min":"252:1","type":"lvm","size":400000000000,"fstype":"ext4","uuid":"root-uuid"},
 {"path":"/dev/mapper/nvme0n1p3_crypt","kname":"dm-0","pkname":"nvme0n1p3","maj:min":"252:0","type":"crypt","size":499000000000,"fstype":"LVM2_member","uuid":"pv"}
]}"#;
        let devs = parse_lsblk(json).unwrap();
        let root = find_dev(&devs, "/dev/mapper/vgmint-root", "252:1").unwrap();
        let (crypt, lvm, disk) = ancestry(&devs, root);
        assert_eq!(crypt.as_deref(), Some("luks-uuid"));
        assert!(lvm);
        assert_eq!(disk, Some(512110190592));
    }

    #[test]
    fn swaps() {
        let text = "Filename\tType\tSize\tUsed\tPriority\n/swapfile file 2097148 0 -2\n/dev/zram0 partition 8000000 0 100\n/dev/sda3 partition 1048572 0 -3\n";
        let s = parse_proc_swaps(text);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], ("file".into(), "/swapfile".into(), 2097148 * 1024));
    }

    #[test]
    fn os_release() {
        let o = parse_os_release("ID=linuxmint\nVERSION_ID=\"22.2\"\n");
        assert_eq!(o["VERSION_ID"], "22.2");
    }
}
