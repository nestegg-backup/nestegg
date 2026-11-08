//! Mount checks for backup sources.
//!
//! A data disk that failed to mount leaves an empty directory behind. Backing
//! that up would look like a successful (but empty) backup, so sources are
//! checked against `/etc/fstab` and the current mount table. Mount points
//! below a source are not crossed by `proxmox-backup-client`; those that are
//! not covered by another source or `include_dev` produce a warning.

use std::path::{Path, PathBuf};

/// File systems that hold real data (pseudo, overlay and image file systems
/// such as snap squashfs are ignored).
const DATA_FS: &[&str] = &[
    "ext2", "ext3", "ext4", "xfs", "btrfs", "zfs", "vfat", "exfat", "ntfs", "ntfs3", "fuseblk",
    "f2fs", "bcachefs", "jfs", "reiserfs", "nilfs2", "hfsplus",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Mount {
    pub point: PathBuf,
    pub fstype: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FstabEntry {
    pub point: PathBuf,
    pub fstype: String,
    pub noauto: bool,
}

#[derive(Debug, Default, PartialEq)]
pub struct SourceCheck {
    /// Problems that must fail the run.
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

fn unescape(field: &str) -> String {
    // mountinfo and fstab encode space, tab, newline and backslash as \ooo.
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
            let v = u8::from_str_radix(&field[i + 1..i + 4], 8).unwrap_or(b'?');
            out.push(v);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let point = left.split_whitespace().nth(4)?;
            let fstype = right.split_whitespace().next()?;
            Some(Mount {
                point: PathBuf::from(unescape(point)),
                fstype: fstype.to_string(),
            })
        })
        .collect()
}

pub fn parse_fstab(text: &str) -> Vec<FstabEntry> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 4 || !fields[1].starts_with('/') || fields[2] == "swap" {
                return None;
            }
            Some(FstabEntry {
                point: PathBuf::from(unescape(fields[1])),
                fstype: fields[2].to_string(),
                noauto: fields[3].split(',').any(|o| o == "noauto"),
            })
        })
        .collect()
}

/// Read the mount table and fstab. The mount table must be readable; a
/// missing fstab counts as empty.
pub fn load() -> anyhow::Result<(Vec<Mount>, Vec<FstabEntry>)> {
    use anyhow::Context;
    let path = |var: &str, default: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(default))
    };
    let mountinfo = path("NESTEGG_MOUNTINFO", "/proc/self/mountinfo");
    let mounts = std::fs::read_to_string(&mountinfo)
        .with_context(|| format!("cannot read mount table {}", mountinfo.display()))?;
    let fstab_path = path("NESTEGG_FSTAB", "/etc/fstab");
    let fstab = match std::fs::read_to_string(&fstab_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", fstab_path.display())),
    };
    Ok((parse_mountinfo(&mounts), parse_fstab(&fstab)))
}

/// A real mount at `point`. An autofs trigger alone does not count: the
/// actual file system appears as a second mount on the same point.
fn is_mounted(mounts: &[Mount], point: &Path) -> bool {
    mounts
        .iter()
        .any(|m| m.point == point && m.fstype != "autofs")
}

/// Check one source (pass the canonical path).
/// - `require_mount`: the source itself must be a mounted file system.
/// - every fstab file system at or above the source (except `/`) must be
///   mounted, also `noauto` ones: otherwise the source is just a directory
///   on the wrong disk.
/// - auto-mounted fstab file systems below the source must be mounted;
///   `noauto` ones only warn.
/// - mounted data file systems below the source that are not `covered`
///   (another source or `include_dev`) produce a warning.
pub fn check_source(
    source: &Path,
    require_mount: bool,
    covered: &[PathBuf],
    mounts: &[Mount],
    fstab: &[FstabEntry],
) -> SourceCheck {
    let mut check = SourceCheck::default();
    if require_mount && !is_mounted(mounts, source) {
        check.errors.push(format!(
            "{} is not a mounted file system (require_mount = true)",
            source.display()
        ));
    }
    for entry in fstab {
        if entry.point == Path::new("/") || is_mounted(mounts, &entry.point) {
            continue;
        }
        let governs = source.starts_with(&entry.point);
        let below = !governs && entry.point.starts_with(source);
        if governs {
            check.errors.push(format!(
                "{} ({}) holds {} but is not mounted; refusing to back up the empty mount point",
                entry.point.display(),
                entry.fstype,
                source.display()
            ));
        } else if below && entry.noauto {
            check.warnings.push(format!(
                "{} ({}) is listed in /etc/fstab with noauto and is not mounted",
                entry.point.display(),
                entry.fstype
            ));
        } else if below {
            check.errors.push(format!(
                "{} ({}) is listed in /etc/fstab but not mounted; refusing to back up an incomplete source",
                entry.point.display(),
                entry.fstype
            ));
        }
    }
    for m in mounts {
        if m.point != source
            && m.point.starts_with(source)
            && DATA_FS.contains(&m.fstype.as_str())
            && !covered.iter().any(|c| c == &m.point)
        {
            check.warnings.push(format!(
                "{} ({}) is a separate file system below {} and is NOT backed up; \
                 add it as a source or to include_dev",
                m.point.display(),
                m.fstype,
                source.display()
            ));
        }
    }
    check
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
23 22 259:3 / /home rw,relatime shared:2 - ext4 /dev/nvme0n1p3 rw
24 22 0:5 / /proc rw - proc proc rw
25 23 259:4 / /home/alice/Media\\040Disk rw - ext4 /dev/sdb1 rw
26 22 7:1 / /snap/core/1 ro - squashfs /dev/loop1 ro
";

    const FSTAB: &str = "\
# comment
UUID=a / ext4 defaults 0 1
UUID=b /home ext4 defaults 0 2
UUID=c /srv/data xfs defaults 0 2
UUID=d /mnt/usb ext4 noauto,user 0 0
UUID=e none swap sw 0 0
";

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn parsing() {
        let m = parse_mountinfo(MOUNTINFO);
        assert_eq!(m.len(), 5);
        assert_eq!(m[3].point, p("/home/alice/Media Disk"));
        let f = parse_fstab(FSTAB);
        assert_eq!(f.len(), 4);
        assert!(f[3].noauto);
    }

    #[test]
    fn mounted_home_is_fine_but_submount_warns() {
        let c = check_source(
            &p("/home"),
            true,
            &[],
            &parse_mountinfo(MOUNTINFO),
            &parse_fstab(FSTAB),
        );
        assert!(c.errors.is_empty(), "{c:?}");
        assert_eq!(c.warnings.len(), 1);
        assert!(c.warnings[0].contains("Media Disk"));
        let covered = check_source(
            &p("/home"),
            true,
            &[p("/home/alice/Media Disk")],
            &parse_mountinfo(MOUNTINFO),
            &parse_fstab(FSTAB),
        );
        assert!(covered.warnings.is_empty());
    }

    #[test]
    fn unmounted_fstab_entry_fails() {
        let c = check_source(
            &p("/srv/data"),
            false,
            &[],
            &parse_mountinfo(MOUNTINFO),
            &parse_fstab(FSTAB),
        );
        assert_eq!(c.errors.len(), 1);
        assert!(c.errors[0].contains("not mounted"));
    }

    #[test]
    fn unmounted_home_fails() {
        let mounts: Vec<Mount> = parse_mountinfo(MOUNTINFO)
            .into_iter()
            .filter(|m| m.point != p("/home") && !m.point.starts_with("/home"))
            .collect();
        let c = check_source(&p("/home"), false, &[], &mounts, &parse_fstab(FSTAB));
        assert_eq!(c.errors.len(), 1);
    }

    #[test]
    fn missing_parent_mount_fails_for_subdirectory_source() {
        let c = check_source(
            &p("/srv/data/Documents"),
            false,
            &[],
            &parse_mountinfo(MOUNTINFO),
            &parse_fstab(FSTAB),
        );
        assert_eq!(c.errors.len(), 1, "{c:?}");
    }

    #[test]
    fn noauto_parent_of_source_fails() {
        let c = check_source(
            &p("/mnt/usb/backup"),
            false,
            &[],
            &parse_mountinfo(MOUNTINFO),
            &parse_fstab(FSTAB),
        );
        assert_eq!(c.errors.len(), 1, "{c:?}");
    }

    #[test]
    fn autofs_placeholder_is_not_mounted() {
        let info = "22 1 259:2 / / rw - ext4 /dev/sda1 rw\n30 22 0:40 / /srv/data rw - autofs systemd-1 rw\n";
        let c = check_source(
            &p("/srv/data"),
            true,
            &[],
            &parse_mountinfo(info),
            &parse_fstab(FSTAB),
        );
        assert_eq!(c.errors.len(), 2, "{c:?}");
        let mounted = format!("{info}31 30 8:17 / /srv/data rw - xfs /dev/sdb1 rw\n");
        let c = check_source(
            &p("/srv/data"),
            true,
            &[],
            &parse_mountinfo(&mounted),
            &parse_fstab(FSTAB),
        );
        assert!(c.errors.is_empty(), "{c:?}");
    }

    #[test]
    fn require_mount_without_fstab() {
        let c = check_source(&p("/data"), true, &[], &parse_mountinfo(MOUNTINFO), &[]);
        assert_eq!(c.errors.len(), 1);
    }

    #[test]
    fn noauto_below_source_only_warns_and_pseudo_fs_ignored() {
        let c = check_source(
            &p("/mnt"),
            false,
            &[],
            &parse_mountinfo(MOUNTINFO),
            &parse_fstab(FSTAB),
        );
        assert!(c.errors.is_empty());
        assert_eq!(c.warnings.len(), 1);
        let root = check_source(
            &p("/"),
            false,
            &[],
            &parse_mountinfo(MOUNTINFO),
            &parse_fstab(FSTAB),
        );
        // /srv/data (auto, below "/") is not mounted: the source is incomplete.
        assert_eq!(root.errors.len(), 1, "{root:?}");
        assert!(root.errors[0].contains("/srv/data"));
        assert!(
            root.warnings
                .iter()
                .all(|w| !w.contains("/snap") && !w.contains("/proc"))
        );
    }
}
