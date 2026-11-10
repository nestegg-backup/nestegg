//! Guard against erasing a disk that still holds data.
//!
//! Before the wizard offers a target disk it classifies every disk: does it
//! carry a file system, RAID/LVM/ZFS/LUKS signature or partitions, and does
//! any of it belong to the backed-up computer (a surviving `/home` or data
//! disk, a RAID member, an unmounted fstab entry)? The facts come from the
//! rescue system's `lsblk` and from the snapshot's inventory (`layout.json`,
//! `fstab`, `crypttab`, `lsblk.json`). Pure functions, nothing is executed
//! here. See `docs/rescue.md` and decision 0010.

use serde_json::Value;

use super::Disk;
use super::plan::human;
use crate::layout::{Layout, MountRole};

/// One block device of a disk (the disk itself, a partition, or a device
/// stacked on it such as an assembled RAID or an open LUKS mapping).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BlockItem {
    pub path: String,
    /// lsblk `TYPE`: `disk`, `part`, `crypt`, `lvm`, `raid1`, ...
    pub kind: String,
    /// lsblk `FSTYPE` (blkid signature): `ext4`, `linux_raid_member`, ...
    pub fstype: String,
    pub uuid: String,
    pub partuuid: String,
    pub label: String,
}

/// What a recorded identifier means for the backed-up computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// Seen on the backed-up computer, role unknown.
    Unknown,
    /// Part of the system: its content is in `system.pxar`.
    System,
    /// Not part of the system backup (data disk, removable medium).
    Data,
}

/// A file system, partition or container UUID known from the snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    pub id: String,
    /// Mount point(s) or a short description, e.g. `/home`.
    pub what: String,
    pub scope: Scope,
    /// Lower = more descriptive source (layout < fstab < crypttab < lsblk).
    rank: u8,
}

/// Something found on a disk that would be destroyed by erasing it.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// Device name without `/dev/`, e.g. `sdb1`.
    pub device: String,
    /// What the signature is, e.g. `ext4 file system 'home'`.
    pub signature: String,
    /// Mount points or descriptions on the backed-up computer.
    pub recorded: Vec<String>,
    pub scope: Option<Scope>,
    /// The key or settings file of the wizard was found here.
    pub key_media: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Assessment {
    pub findings: Vec<Finding>,
    /// Partitions without any recognisable signature (unknown or encrypted
    /// content such as VeraCrypt, a BIOS boot partition, ...).
    pub bare_partitions: usize,
}

impl Assessment {
    /// Erasing this disk destroys something: it is not empty.
    pub fn holds_data(&self) -> bool {
        !self.findings.is_empty() || self.bare_partitions > 0
    }

    /// Something on the disk belongs to the backed-up computer.
    pub fn of_backed_up_computer(&self) -> bool {
        self.findings.iter().any(|f| f.scope.is_some())
    }

    /// One line per finding, for the disk list and the confirmation.
    pub fn lines(&self, backup_time: &str) -> Vec<String> {
        let mut out: Vec<String> = self.findings.iter().map(|f| f.line(backup_time)).collect();
        match self.bare_partitions {
            0 => {}
            1 => out.push("1 partition without a recognisable file system".into()),
            n => out.push(format!("{n} partitions without a recognisable file system")),
        }
        out
    }
}

impl Finding {
    fn line(&self, backup_time: &str) -> String {
        let mut s = format!("{}: {}", self.device, self.signature);
        if self.key_media {
            s.push_str(", holds the key or settings file found by the wizard");
        }
        let what = self.recorded.join(", ");
        match self.scope {
            None => {}
            Some(Scope::Data) => s.push_str(&format!(
                " = {what} of the backed-up computer, NOT part of the system backup \
                 (lost unless it was backed up separately)"
            )),
            Some(Scope::System) => s.push_str(&format!(
                " = {what} of the backed-up computer (comes back as of {backup_time}; \
                 anything written later is lost)"
            )),
            Some(Scope::Unknown) => {
                s.push_str(&format!("; recorded on the backed-up computer as {what}"))
            }
        }
        s
    }
}

fn short(path: &str) -> &str {
    path.strip_prefix("/dev/").unwrap_or(path)
}

/// Human description of a blkid signature, `None` for no signature.
fn describe(item: &BlockItem) -> Option<String> {
    let label = if item.label.is_empty() {
        String::new()
    } else {
        format!(" '{}'", item.label)
    };
    Some(match item.fstype.as_str() {
        "" => return None,
        "linux_raid_member" => format!("member of a software RAID (md){label}"),
        "zfs_member" => format!("member of the ZFS pool{label}"),
        "crypto_LUKS" => format!("encrypted LUKS container{label}"),
        "LVM2_member" => "LVM physical volume".into(),
        "swap" => format!("swap space{label}"),
        "BitLocker" => "BitLocker-encrypted volume".into(),
        f if f.ends_with("_raid_member") => format!("member of a RAID ({f})"),
        f => format!("{f} file system{label}"),
    })
}

fn same_id(a: &str, b: &str) -> bool {
    !a.is_empty() && a.eq_ignore_ascii_case(b)
}

/// Classify a disk. `recorded` comes from the snapshot (may be empty, e.g.
/// for `nestegg rescue disks`), `key_media` are the devices on which the
/// wizard found the key or settings file.
pub fn assess(disk: &Disk, recorded: &[Recorded], key_media: &[String]) -> Assessment {
    let mut a = Assessment::default();
    for item in &disk.items {
        let Some(signature) = describe(item) else {
            // A partition table without partitions is empty; a partition
            // without a signature is not (it may be encrypted or unknown).
            if item.kind == "part" {
                a.bare_partitions += 1;
            }
            continue;
        };
        let matches: Vec<&Recorded> = recorded
            .iter()
            .filter(|r| same_id(&item.uuid, &r.id) || same_id(&item.partuuid, &r.id))
            .collect();
        let best = matches.iter().map(|r| r.rank).min();
        let mut what: Vec<String> = Vec::new();
        let mut scope = None;
        for r in matches.iter().filter(|r| Some(r.rank) == best) {
            if !what.contains(&r.what) {
                what.push(r.what.clone());
            }
            scope = scope.max(Some(r.scope));
        }
        a.findings.push(Finding {
            device: short(&item.path).to_string(),
            signature,
            recorded: what,
            scope,
            key_media: key_media.iter().any(|k| k == &item.path),
        });
    }
    a
}

fn scope_of_point(layout: &Layout, point: &str) -> Scope {
    match layout.mounts.iter().find(|m| m.point == point && !m.bind) {
        Some(m) => match layout.role(m) {
            MountRole::Other => Scope::Data,
            _ => Scope::System,
        },
        None => Scope::Unknown,
    }
}

fn spec_id(spec: &str) -> Option<&str> {
    [
        "UUID=",
        "PARTUUID=",
        "/dev/disk/by-uuid/",
        "/dev/disk/by-partuuid/",
    ]
    .iter()
    .find_map(|p| spec.strip_prefix(p))
    .map(|v| v.trim_matches('"'))
    .filter(|v| !v.is_empty())
}

/// Identifiers of the backed-up computer's storage, from the snapshot's
/// inventory: mounted file systems and swap (`layout.json`), fstab and
/// crypttab entries (also unmounted or `noauto` ones), and every device of
/// its `lsblk` listing (RAID members, ZFS members, LVM, ntfs-3g, ...).
pub fn recorded_ids(
    layout: &Layout,
    fstab: Option<&str>,
    crypttab: Option<&str>,
    lsblk_json: Option<&str>,
) -> Vec<Recorded> {
    let mut out = Vec::new();
    let mut push = |id: &str, what: String, scope: Scope, rank: u8| {
        if !id.is_empty() {
            out.push(Recorded {
                id: id.to_string(),
                what,
                scope,
                rank,
            });
        }
    };
    for m in layout.mounts.iter().filter(|m| !m.bind) {
        let scope = match layout.role(m) {
            MountRole::Other => Scope::Data,
            _ => Scope::System,
        };
        for id in [&m.uuid, &m.crypt_uuid].into_iter().flatten() {
            push(id, m.point.clone(), scope, 0);
        }
    }
    for s in &layout.swap {
        for id in [&s.uuid, &s.crypt_uuid].into_iter().flatten() {
            push(id, "swap".into(), Scope::System, 0);
        }
    }
    for e in crate::mounts::parse_fstab(fstab.unwrap_or_default()) {
        if let Some(id) = spec_id(&e.spec) {
            let scope = if crate::layout::is_system_entry(layout, &e.spec, &e.point) {
                Scope::System
            } else {
                Scope::Data
            };
            push(id, e.point.display().to_string(), scope, 1);
        }
    }
    for line in crypttab.unwrap_or_default().lines().map(str::trim) {
        if line.starts_with('#') {
            continue;
        }
        let mut f = line.split_whitespace();
        if let (Some(name), Some(spec)) = (f.next(), f.next())
            && let Some(id) = spec_id(spec)
        {
            push(
                id,
                format!("encrypted volume '{name}' (crypttab)"),
                Scope::Unknown,
                2,
            );
        }
    }
    if let Some(v) = lsblk_json.and_then(|t| serde_json::from_str::<Value>(t).ok())
        && let Some(devs) = v["blockdevices"].as_array()
    {
        for d in devs {
            recorded_lsblk(layout, d, &mut push);
        }
    }
    out
}

/// Mount points of a node of `lsblk --json` and everything stacked on it.
fn points_below(n: &Value) -> Vec<String> {
    let mut out: Vec<String> = match (n["mountpoints"].as_array(), n["mountpoint"].as_str()) {
        (Some(a), _) => a
            .iter()
            .filter_map(|p| p.as_str())
            .filter(|p| p.starts_with('/'))
            .map(String::from)
            .collect(),
        (None, Some(p)) if p.starts_with('/') => vec![p.to_string()],
        _ => Vec::new(),
    };
    for c in n["children"].as_array().into_iter().flatten() {
        for p in points_below(c) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

fn recorded_lsblk(layout: &Layout, n: &Value, push: &mut impl FnMut(&str, String, Scope, u8)) {
    let s = |k: &str| n[k].as_str().unwrap_or_default();
    if !s("fstype").is_empty() || !s("partuuid").is_empty() {
        let points = points_below(n);
        let (what, scope) = if points.is_empty() {
            let what = match s("fstype") {
                "" => format!("partition {}", s("name")),
                f => format!("{f} on {}", s("name")),
            };
            (what, Scope::Unknown)
        } else {
            let scope = points
                .iter()
                .map(|p| scope_of_point(layout, p))
                .max()
                .unwrap_or(Scope::Unknown);
            (points.join(", "), scope)
        };
        push(s("uuid"), what.clone(), scope, 3);
        push(s("partuuid"), what, scope, 3);
    }
    for c in n["children"].as_array().into_iter().flatten() {
        recorded_lsblk(layout, c, push);
    }
}

/// A disk offered as restore target, reduced to what pre-selection needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiskChoice {
    pub holds_data: bool,
    /// Big enough for the restore plan.
    pub fits: bool,
}

/// The disk to pre-select in the wizard: only when exactly one disk is empty
/// and big enough. Otherwise the user must type a number.
pub fn preselect(choices: &[DiskChoice]) -> Option<usize> {
    let mut usable = choices
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.holds_data && c.fits)
        .map(|(i, _)| i);
    match (usable.next(), usable.next()) {
        (Some(i), None) => Some(i),
        _ => None,
    }
}

/// What the user must type to erase a disk that holds data.
pub fn erase_phrase(disk: &Disk) -> String {
    format!("ERASE {}", short(&disk.path))
}

/// Model, size, serial and path of a disk, so the user can tell it apart
/// from a disk of the same size.
pub fn identity(disk: &Disk) -> String {
    let mut parts = Vec::new();
    if !disk.model.is_empty() {
        parts.push(disk.model.clone());
    }
    parts.push(human(disk.size_bytes));
    if !disk.serial.is_empty() {
        parts.push(format!("serial {}", disk.serial));
    }
    parts.push(disk.path.clone());
    parts.join(", ")
}

/// The warning shown before a disk that holds data may be erased.
pub fn warning(disk: &Disk, a: &Assessment, backup_time: &str) -> String {
    let mut s = format!(
        "{} IS NOT EMPTY{}:\n  {}\n",
        disk.path,
        if a.of_backed_up_computer() {
            " AND HOLDS DATA OF THE BACKED-UP COMPUTER"
        } else {
            ""
        },
        identity(disk)
    );
    for l in a.lines(backup_time) {
        s.push_str(&format!("  - {l}\n"));
    }
    s.push_str("Restoring onto this disk destroys all of it.");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::tests::{btrfs_layout, rec};

    fn item(path: &str, kind: &str, fstype: &str, uuid: &str, label: &str) -> BlockItem {
        BlockItem {
            path: path.into(),
            kind: kind.into(),
            fstype: fstype.into(),
            uuid: uuid.into(),
            partuuid: String::new(),
            label: label.into(),
        }
    }

    fn disk(path: &str, items: Vec<BlockItem>) -> Disk {
        let mut all = vec![item(path, "disk", "", "", "")];
        all.extend(items);
        Disk {
            path: path.into(),
            size_bytes: 4 << 40,
            model: "WDC WD40EFRX".into(),
            serial: "WD-0001".into(),
            transport: "sata".into(),
            in_use: false,
            items: all,
        }
    }

    /// btrfs system on nvme0n1, separate ext4 /home on sdb1, data disk at
    /// /srv/data, an unmounted fstab entry, md RAID and ZFS in lsblk.json.
    fn recorded() -> Vec<Recorded> {
        let mut l = btrfs_layout();
        l.mounts.retain(|m| m.point != "/home");
        l.mounts
            .push(rec("/home", "ext4", "/dev/sdb1", "/", "home-uuid"));
        let fstab = "UUID=1111 / btrfs subvol=@ 0 0\n\
                     UUID=home-uuid /home ext4 defaults 0 2\n\
                     UUID=archive-uuid /archive ext4 noauto 0 2\n\
                     PARTUUID=p-0005 /win ntfs-3g defaults 0 0\n";
        let crypttab = "# name source key options\ncryptdata UUID=luks-uuid none luks\n";
        let lsblk = r#"{"blockdevices":[
          {"name":"sdd","type":"disk","fstype":null,"uuid":null,"partuuid":null,"mountpoints":[null],
           "children":[{"name":"sdd1","type":"part","fstype":"linux_raid_member","uuid":"raid-uuid","partuuid":"p-0001","mountpoints":[null],
             "children":[{"name":"md0","type":"raid1","fstype":"ext4","uuid":"2222","mountpoints":["/srv/data"]}]}]},
          {"name":"sde","type":"disk","fstype":"zfs_member","uuid":"1234567890","partuuid":null,"mountpoints":[null]},
          {"name":"sdf","type":"disk","fstype":null,"mountpoints":[null],
           "children":[{"name":"sdf1","type":"part","fstype":"ntfs","uuid":"NTFS01","partuuid":"p-0005","mountpoints":["/win"]}]}
        ]}"#;
        recorded_ids(&l, Some(fstab), Some(crypttab), Some(lsblk))
    }

    #[test]
    fn empty_disk_is_free() {
        let a = assess(&disk("/dev/sdx", vec![]), &recorded(), &[]);
        assert!(!a.holds_data());
        assert!(a.lines("now").is_empty());
    }

    #[test]
    fn separate_home_disk_of_the_backed_up_computer() {
        let d = disk(
            "/dev/sdb",
            vec![item("/dev/sdb1", "part", "ext4", "home-uuid", "home")],
        );
        let a = assess(&d, &recorded(), &[]);
        assert!(a.holds_data());
        assert!(a.of_backed_up_computer());
        assert_eq!(a.findings[0].recorded, vec!["/home".to_string()]);
        assert_eq!(a.findings[0].scope, Some(Scope::System));
        let line = &a.lines("2026-11-08 22:00")[0];
        assert!(
            line.starts_with("sdb1: ext4 file system 'home' = /home"),
            "{line}"
        );
        assert!(line.contains("2026-11-08 22:00"), "{line}");
    }

    #[test]
    fn data_disk_and_unmounted_fstab_entry() {
        let d = disk(
            "/dev/sdc",
            vec![
                item("/dev/sdc1", "part", "ext4", "2222", ""),
                item("/dev/sdc2", "part", "ext4", "archive-uuid", ""),
            ],
        );
        let a = assess(&d, &recorded(), &[]);
        assert_eq!(a.findings[0].scope, Some(Scope::Data));
        assert_eq!(a.findings[0].recorded, vec!["/srv/data".to_string()]);
        // Not mounted during the backup, known only from fstab.
        assert_eq!(a.findings[1].scope, Some(Scope::Data));
        assert_eq!(a.findings[1].recorded, vec!["/archive".to_string()]);
        assert!(a.lines("t")[0].contains("NOT part of the system backup"));
    }

    #[test]
    fn raid_member_of_the_backed_up_computer() {
        let d = disk(
            "/dev/sdd",
            vec![item(
                "/dev/sdd1",
                "part",
                "linux_raid_member",
                "raid-uuid",
                "desktop:0",
            )],
        );
        let a = assess(&d, &recorded(), &[]);
        let f = &a.findings[0];
        assert_eq!(f.signature, "member of a software RAID (md) 'desktop:0'");
        // The array carried /srv/data, a data disk.
        assert_eq!(f.recorded, vec!["/srv/data".to_string()]);
        assert_eq!(f.scope, Some(Scope::Data));
        // An unknown RAID member is still data.
        let other = disk(
            "/dev/sdd",
            vec![item("/dev/sdd1", "part", "linux_raid_member", "x", "")],
        );
        let a = assess(&other, &recorded(), &[]);
        assert!(a.holds_data());
        assert!(!a.of_backed_up_computer());
    }

    #[test]
    fn zfs_member_on_the_whole_disk() {
        let mut d = disk("/dev/sde", vec![]);
        d.items[0] = item("/dev/sde", "disk", "zfs_member", "1234567890", "tank");
        let a = assess(&d, &recorded(), &[]);
        assert!(a.holds_data());
        let f = &a.findings[0];
        assert_eq!(f.signature, "member of the ZFS pool 'tank'");
        assert_eq!(f.scope, Some(Scope::Unknown));
        assert!(
            a.lines("t")[0].contains("recorded on the backed-up computer as zfs_member on sde")
        );
    }

    #[test]
    fn ntfs_disk_mounted_with_ntfs_3g() {
        // fuseblk mounts are not in layout.json; fstab and lsblk.json know it.
        let mut part = item("/dev/sdf1", "part", "ntfs", "NTFS01", "Media");
        part.partuuid = "p-0005".into();
        let a = assess(&disk("/dev/sdf", vec![part]), &recorded(), &[]);
        let f = &a.findings[0];
        assert_eq!(f.signature, "ntfs file system 'Media'");
        assert_eq!(f.recorded, vec!["/win".to_string()]);
        assert_eq!(f.scope, Some(Scope::Data));
        // Unknown ntfs disk: holds data, nothing recorded.
        let a = assess(
            &disk("/dev/sdf", vec![item("/dev/sdf1", "part", "ntfs", "X", "")]),
            &recorded(),
            &[],
        );
        assert!(a.holds_data() && !a.of_backed_up_computer());
    }

    #[test]
    fn luks_container_from_crypttab() {
        let d = disk(
            "/dev/sdg",
            vec![item("/dev/sdg1", "part", "crypto_LUKS", "luks-uuid", "")],
        );
        let a = assess(&d, &recorded(), &[]);
        let f = &a.findings[0];
        assert_eq!(f.signature, "encrypted LUKS container");
        assert_eq!(
            f.recorded,
            vec!["encrypted volume 'cryptdata' (crypttab)".to_string()]
        );
        // A locked LUKS container of the old layout matches its crypt UUID.
        let mut l = btrfs_layout();
        l.mounts[4].crypt_uuid = Some("data-luks".into());
        let r = recorded_ids(&l, None, None, None);
        let d = disk(
            "/dev/sda",
            vec![item("/dev/sda1", "part", "crypto_LUKS", "data-luks", "")],
        );
        let a = assess(&d, &r, &[]);
        assert_eq!(a.findings[0].recorded, vec!["/srv/data".to_string()]);
        assert_eq!(a.findings[0].scope, Some(Scope::Data));
    }

    #[test]
    fn old_btrfs_system_disk_lists_its_subvolumes_once() {
        let d = disk(
            "/dev/nvme0n1",
            vec![
                item("/dev/nvme0n1p1", "part", "vfat", "AAAA-BBBB", ""),
                item("/dev/nvme0n1p2", "part", "btrfs", "1111", ""),
            ],
        );
        let a = assess(&d, &recorded(), &[]);
        assert_eq!(a.findings[1].recorded, vec!["/", "/.snapshots"]);
        assert_eq!(a.findings[1].scope, Some(Scope::System));
    }

    #[test]
    fn key_stick_and_bare_partitions() {
        let d = disk(
            "/dev/sdh",
            vec![item("/dev/sdh1", "part", "vfat", "1A2B-3C4D", "KEYS")],
        );
        let a = assess(&d, &recorded(), &["/dev/sdh1".to_string()]);
        assert!(a.holds_data());
        assert!(a.findings[0].key_media);
        assert!(a.lines("t")[0].contains("key or settings file"));
        // Partitions without signature (e.g. VeraCrypt) count as data.
        let d = disk(
            "/dev/sdi",
            vec![
                item("/dev/sdi1", "part", "", "", ""),
                item("/dev/sdi2", "part", "", "", ""),
            ],
        );
        let a = assess(&d, &[], &[]);
        assert!(a.holds_data());
        assert_eq!(
            a.lines("t"),
            vec!["2 partitions without a recognisable file system"]
        );
    }

    #[test]
    fn preselection_rule() {
        let c = |holds_data, fits| DiskChoice { holds_data, fits };
        // Exactly one empty disk that fits.
        assert_eq!(preselect(&[c(true, true), c(false, true)]), Some(1));
        // Never a disk that holds data, even if it is the only one.
        assert_eq!(preselect(&[c(true, true)]), None);
        // Two empty candidates: the user must choose.
        assert_eq!(preselect(&[c(false, true), c(false, true)]), None);
        // An empty disk that is too small is not a candidate.
        assert_eq!(preselect(&[c(false, false), c(false, true)]), Some(1));
        assert_eq!(preselect(&[c(false, false)]), None);
        assert_eq!(preselect(&[]), None);
    }

    #[test]
    fn warning_names_the_disk() {
        let d = disk(
            "/dev/sdb",
            vec![item("/dev/sdb1", "part", "ext4", "home-uuid", "home")],
        );
        let a = assess(&d, &recorded(), &[]);
        let w = warning(&d, &a, "t");
        assert!(
            w.contains("WDC WD40EFRX, 4.0 TiB, serial WD-0001, /dev/sdb"),
            "{w}"
        );
        assert!(w.contains("HOLDS DATA OF THE BACKED-UP COMPUTER"), "{w}");
        assert_eq!(erase_phrase(&d), "ERASE sdb");
    }
}
