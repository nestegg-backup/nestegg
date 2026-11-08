//! Restore plan: how the empty target disk is partitioned and which file
//! systems are created, derived from the recorded layout. Pure functions,
//! nothing is executed here.
//!
//! The new layout is deliberately simple and does not try to clone the old
//! one: one EFI system partition (UEFI) or a BIOS boot partition, an
//! optional `/boot` (only with encryption), an optional swap partition and
//! one root file system of the original type. btrfs subvolumes are
//! recreated with their original names; other separate system file systems
//! (e.g. an ext4 `/home` partition) are merged into the root file system.

use std::fmt::Write as _;

use anyhow::{Result, bail};

use crate::layout::{Layout, MountRole};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const ESP_MIB: u64 = 1024;
const BOOT_MIB: u64 = 1536;
const MAX_SWAP: u64 = 16 * GIB;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    BiosBoot,
    Esp,
    Boot,
    Swap,
    Root,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Partition {
    pub number: u32,
    pub purpose: Purpose,
    /// Size in MiB; `None` = rest of the disk.
    pub size_mib: Option<u64>,
    /// sgdisk type code.
    pub typecode: &'static str,
    pub label: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Subvolume {
    /// Path below the btrfs top level, e.g. `@home`.
    pub name: String,
    pub point: String,
    /// Content comes from the backup (otherwise created empty).
    pub restored: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwapFile {
    pub path: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub disk: String,
    pub disk_bytes: u64,
    pub uefi: bool,
    pub encrypt: bool,
    pub partitions: Vec<Partition>,
    pub root_fstype: String,
    /// btrfs subvolume that is mounted as `/` (e.g. `@`); `None` = top level.
    pub root_subvolume: Option<String>,
    /// btrfs compression of the old system (`compress=zstd:3`), used while
    /// restoring so the data fits like before.
    pub root_compression: Option<String>,
    pub subvolumes: Vec<Subvolume>,
    /// Mount point of the EFI system partition in the restored system.
    pub esp_point: Option<String>,
    pub swap_files: Vec<SwapFile>,
    /// Separate file systems of the old system merged into `/`.
    pub merged: Vec<String>,
    /// Mount points of the old system that keep their fstab entry but get
    /// `nofail` (data disks, removable media).
    pub foreign: Vec<String>,
    pub needed_bytes: u64,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Target {
    pub disk: String,
    pub size_bytes: u64,
    /// The rescue system was booted in UEFI mode.
    pub uefi: bool,
    /// Size of `system.pxar` as reported by the PBS, if known.
    pub archive_bytes: Option<u64>,
}

impl Plan {
    /// Device path of a partition (`/dev/sda2`, `/dev/nvme0n1p2`).
    pub fn part_path(&self, number: u32) -> String {
        part_path(&self.disk, number)
    }

    pub fn partition(&self, purpose: Purpose) -> Option<&Partition> {
        self.partitions.iter().find(|p| p.purpose == purpose)
    }

    /// Human-readable summary shown before anything is changed.
    pub fn describe(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "Target disk {} ({}), boot mode {}{}",
            self.disk,
            human(self.disk_bytes),
            if self.uefi { "UEFI" } else { "BIOS" },
            if self.encrypt {
                ", encrypted (LUKS2)"
            } else {
                ""
            }
        );
        for p in &self.partitions {
            let size = p.size_mib.map_or("rest".to_string(), |m| human(m * MIB));
            let what = match p.purpose {
                Purpose::BiosBoot => "BIOS boot".to_string(),
                Purpose::Esp => format!(
                    "EFI system partition (vfat) -> {}",
                    self.esp_point.as_deref().unwrap_or("/boot/efi")
                ),
                Purpose::Boot => "/boot (ext4)".to_string(),
                Purpose::Swap => "swap".to_string(),
                Purpose::Root => format!("/ ({})", self.root_fstype),
            };
            let _ = writeln!(s, "  {}  {:>9}  {}", self.part_path(p.number), size, what);
        }
        for sv in &self.subvolumes {
            let _ = writeln!(
                s,
                "  btrfs subvolume {} -> {}{}",
                sv.name,
                sv.point,
                if sv.restored { "" } else { " (empty)" }
            );
        }
        for f in &self.swap_files {
            let _ = writeln!(s, "  swap file {} ({})", f.path, human(f.size_bytes));
        }
        let _ = writeln!(s, "Data to restore: about {}", human(self.needed_bytes));
        for m in &self.merged {
            let _ = writeln!(
                s,
                "Note: {m} was a separate file system; it becomes part of /."
            );
        }
        for f in &self.foreign {
            let _ = writeln!(
                s,
                "Note: {f} is not part of the system backup; its fstab entry gets 'nofail'."
            );
        }
        for n in &self.notes {
            let _ = writeln!(s, "Note: {n}");
        }
        s
    }
}

pub fn part_path(disk: &str, number: u32) -> String {
    if disk.ends_with(|c: char| c.is_ascii_digit()) {
        format!("{disk}p{number}")
    } else {
        format!("{disk}{number}")
    }
}

pub fn human(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < units.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

fn subvolume_name(fsroot: &str) -> Option<String> {
    let name = fsroot.trim_start_matches('/');
    (!name.is_empty()).then(|| name.to_string())
}

pub fn make_plan(layout: &Layout, target: &Target, encrypt: bool) -> Result<Plan> {
    if layout.version > crate::layout::LAYOUT_VERSION {
        bail!(
            "the backup was made by a newer nestegg (layout version {}); use a newer rescue stick",
            layout.version
        );
    }
    let Some(root) = layout.root() else {
        bail!("the backup contains no usable root file system record");
    };
    if layout.bootloader != "grub" {
        bail!(
            "the backed-up system uses the boot loader '{}'; the rescue stick can only reinstall GRUB",
            layout.bootloader
        );
    }
    let mut notes = Vec::new();
    let root_fstype = match root.fstype.as_str() {
        "ext4" | "btrfs" | "xfs" => root.fstype.clone(),
        "ext3" | "ext2" => {
            notes.push(format!(
                "the root file system was {}; it is recreated as ext4",
                root.fstype
            ));
            "ext4".to_string()
        }
        other => bail!("the root file system type {other} cannot be recreated"),
    };
    if layout.boot_mode != (if target.uefi { "uefi" } else { "bios" }) {
        notes.push(format!(
            "the backed-up system booted in {} mode, this computer was started in {} mode; \
             the boot loader is installed for {} mode",
            layout.boot_mode.to_uppercase(),
            if target.uefi { "UEFI" } else { "BIOS" },
            if target.uefi { "UEFI" } else { "BIOS" },
        ));
    }
    if root.crypt_uuid.is_some() && !encrypt {
        notes.push(
            "the old system was encrypted; without encryption the restored disk is readable \
             by anyone who has it"
                .into(),
        );
    }
    if root.on_lvm {
        notes.push("LVM is not recreated; the root file system becomes a plain partition".into());
    }

    let mut partitions = Vec::new();
    let mut n = 0;
    let mut add = |purpose, size_mib, typecode, label| {
        n += 1;
        partitions.push(Partition {
            number: n,
            purpose,
            size_mib,
            typecode,
            label,
        });
    };
    if target.uefi {
        add(Purpose::Esp, Some(ESP_MIB), "ef00", "EFI");
    } else {
        add(Purpose::BiosBoot, Some(1), "ef02", "bios_grub");
    }
    if encrypt {
        add(Purpose::Boot, Some(BOOT_MIB), "8300", "boot");
    }

    let mut swap_files: Vec<SwapFile> = Vec::new();
    let mut swap_part_bytes = 0;
    for s in &layout.swap {
        if s.kind == "partition" && !encrypt {
            swap_part_bytes = swap_part_bytes.max(s.size_bytes.min(MAX_SWAP));
        } else if s.kind == "partition" {
            // On btrfs the file gets its own subvolume (added below): an
            // active swap file blocks snapshots of the subvolume holding it.
            let path = if root_fstype == "btrfs" {
                "/swap/swapfile"
            } else {
                "/swapfile"
            };
            // Several swap partitions become one file.
            match swap_files.iter_mut().find(|f| f.path == path) {
                Some(f) => f.size_bytes = (f.size_bytes + s.size_bytes).min(MAX_SWAP),
                None => {
                    notes.push(format!(
                        "the swap partition is replaced by {path} inside the encrypted root"
                    ));
                    swap_files.push(SwapFile {
                        path: path.into(),
                        size_bytes: s.size_bytes.min(MAX_SWAP),
                    });
                }
            }
        } else {
            swap_files.push(SwapFile {
                path: s.path.clone(),
                size_bytes: s.size_bytes.min(MAX_SWAP),
            });
        }
    }
    if swap_part_bytes > 0 {
        add(
            Purpose::Swap,
            Some(swap_part_bytes.div_ceil(MIB)),
            "8200",
            "swap",
        );
    }
    add(Purpose::Root, None, "8304", "root");

    let mut subvolumes = Vec::new();
    let mut merged = Vec::new();
    let mut foreign = Vec::new();
    let mut esp_point = None;
    let root_subvolume = if root_fstype == "btrfs" {
        subvolume_name(&root.fsroot)
    } else {
        None
    };
    for m in &layout.mounts {
        match layout.role(m) {
            MountRole::Root => {}
            MountRole::System | MountRole::EmptySubvolume
                if m.fstype == "btrfs" && root_fstype == "btrfs" && m.source == root.source =>
            {
                match subvolume_name(&m.fsroot) {
                    Some(name) if Some(&name) != root_subvolume.as_ref() => {
                        subvolumes.push(Subvolume {
                            name,
                            point: m.point.clone(),
                            restored: layout.role(m) == MountRole::System,
                        })
                    }
                    // Same subvolume mounted twice, or the top level: the
                    // content is reachable through the root mount already.
                    _ => merged.push(m.point.clone()),
                }
            }
            MountRole::System
                if m.fstype == "vfat" && Some(m.point.as_str()) == layout.efi_point() =>
            {
                if target.uefi {
                    esp_point = Some(m.point.clone());
                } else {
                    merged.push(m.point.clone());
                }
            }
            MountRole::System => merged.push(m.point.clone()),
            MountRole::EmptySubvolume => foreign.push(m.point.clone()),
            MountRole::Other => foreign.push(m.point.clone()),
        }
    }
    if target.uefi && esp_point.is_none() {
        esp_point = Some("/boot/efi".into());
    }
    if swap_files.iter().any(|f| f.path == "/swap/swapfile")
        && root_fstype == "btrfs"
        && !subvolumes.iter().any(|s| s.point == "/swap")
    {
        let name = match &root_subvolume {
            Some(_) => "@swap",
            None => "swap",
        };
        subvolumes.push(Subvolume {
            name: name.into(),
            point: "/swap".into(),
            restored: false,
        });
    }
    // On btrfs the used space includes snapshots, which are not restored;
    // the archive size is the better estimate there.
    let root_compression = (root_fstype == "btrfs")
        .then(|| {
            root.super_options
                .split(',')
                .find(|o| o.starts_with("compress=") || o.starts_with("compress-force="))
                .map(String::from)
        })
        .flatten();
    let used = match target.archive_bytes {
        // Compressed: the archive counts uncompressed bytes, the old file
        // system (with snapshots) compressed ones; the smaller is closer.
        Some(bytes) if root_compression.is_some() => bytes.min(layout.used_bytes()),
        Some(bytes) if root_fstype == "btrfs" => bytes,
        _ => layout.used_bytes(),
    };
    let fixed: u64 = partitions.iter().filter_map(|p| p.size_mib).sum::<u64>() * MIB;
    let swap_file_bytes: u64 = swap_files.iter().map(|f| f.size_bytes).sum();
    // 15 % headroom for metadata and file system differences, at least 2 GiB.
    let needed = used + (used / 100 * 15).max(2 * GIB) + swap_file_bytes;
    if fixed + needed > target.size_bytes {
        bail!(
            "the target disk is too small: {} needed (data {} plus partitions and headroom), \
             disk has {}",
            human(fixed + needed),
            human(used),
            human(target.size_bytes)
        );
    }

    Ok(Plan {
        disk: target.disk.clone(),
        disk_bytes: target.size_bytes,
        uefi: target.uefi,
        encrypt,
        partitions,
        root_fstype,
        root_subvolume,
        root_compression,
        subvolumes,
        esp_point,
        swap_files,
        merged,
        foreign,
        needed_bytes: used,
        notes,
    })
}

/// sgdisk arguments that create the partition table of the plan.
pub fn sgdisk_args(plan: &Plan) -> Vec<String> {
    let mut args = Vec::new();
    for p in &plan.partitions {
        let end = p.size_mib.map_or("0".to_string(), |m| format!("+{m}M"));
        args.push(format!("--new={}:0:{end}", p.number));
        args.push(format!("--typecode={}:{}", p.number, p.typecode));
        args.push(format!("--change-name={}:{}", p.number, p.label));
    }
    args.push(plan.disk.clone());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::tests::{btrfs_layout, rec};

    fn target(uefi: bool) -> Target {
        Target {
            disk: "/dev/nvme1n1".into(),
            size_bytes: 256 * GIB,
            uefi,
            archive_bytes: None,
        }
    }

    #[test]
    fn btrfs_space_uses_the_archive_size() {
        let mut l = btrfs_layout();
        // Snapshots fill the file system: statvfs says 300 GiB used.
        for m in &mut l.mounts {
            m.used_bytes = Some(300 * GIB);
        }
        assert!(make_plan(&l, &target(true), false).is_err());
        let mut t = target(true);
        t.archive_bytes = Some(40 * GIB);
        let plan = make_plan(&l, &t, false).unwrap();
        assert_eq!(plan.needed_bytes, 40 * GIB);
        assert_eq!(plan.root_compression, None);
        // Compressed btrfs: the archive (uncompressed) is bigger than the
        // space the data took; the smaller number counts and the new file
        // system is mounted with the same compression while restoring.
        for m in &mut l.mounts {
            m.used_bytes = Some(20 * GIB);
        }
        l.mounts[0].super_options = "rw,compress=zstd:3,space_cache=v2,subvol=/@".into();
        t.archive_bytes = Some(60 * GIB);
        let plan = make_plan(&l, &t, false).unwrap();
        assert!(l.used_bytes() < 60 * GIB);
        assert_eq!(plan.needed_bytes, l.used_bytes());
        assert_eq!(plan.root_compression.as_deref(), Some("compress=zstd:3"));
    }

    #[test]
    fn btrfs_uefi() {
        let plan = make_plan(&btrfs_layout(), &target(true), false).unwrap();
        let purposes: Vec<Purpose> = plan.partitions.iter().map(|p| p.purpose).collect();
        assert_eq!(purposes, vec![Purpose::Esp, Purpose::Swap, Purpose::Root]);
        assert_eq!(plan.partitions[1].size_mib, Some(8192));
        assert_eq!(plan.root_fstype, "btrfs");
        assert_eq!(plan.root_subvolume.as_deref(), Some("@"));
        assert_eq!(
            plan.subvolumes,
            vec![
                Subvolume {
                    name: "@home".into(),
                    point: "/home".into(),
                    restored: true
                },
                Subvolume {
                    name: "@snapshots".into(),
                    point: "/.snapshots".into(),
                    restored: false
                },
            ]
        );
        assert_eq!(plan.esp_point.as_deref(), Some("/boot/efi"));
        assert_eq!(plan.foreign, vec!["/srv/data", "/media/alice/USB"]);
        assert_eq!(plan.part_path(3), "/dev/nvme1n1p3");
        let args = sgdisk_args(&plan);
        assert_eq!(args[0], "--new=1:0:+1024M");
        assert!(args.contains(&"--new=3:0:0".to_string()));
        assert!(plan.describe().contains("btrfs subvolume @home -> /home"));
    }

    #[test]
    fn encrypted_btrfs_gets_a_swap_subvolume() {
        let plan = make_plan(&btrfs_layout(), &target(true), true).unwrap();
        assert_eq!(plan.swap_files.len(), 1);
        assert_eq!(plan.swap_files[0].path, "/swap/swapfile");
        assert!(plan.subvolumes.contains(&Subvolume {
            name: "@swap".into(),
            point: "/swap".into(),
            restored: false
        }));
        let mut ids = crate::rescue::fixup::NewIds::default();
        ids.fs_uuid.insert("/".into(), "r".into());
        ids.fs_uuid.insert("/swap".into(), "r".into());
        let (fstab, _) = crate::rescue::fixup::rewrite_fstab("", &plan, &ids, &[]);
        let swap_mount = fstab.find("UUID=r\t/swap\tbtrfs\tsubvol=/@swap").unwrap();
        let swap_file = fstab.find("/swap/swapfile\tnone\tswap").unwrap();
        assert!(swap_mount < swap_file);
    }

    #[test]
    fn ext4_bios_with_separate_home_and_encryption() {
        let mut l = btrfs_layout();
        l.boot_mode = "bios".into();
        l.mounts = vec![
            rec("/", "ext4", "/dev/sda2", "/", "1"),
            rec("/home", "ext4", "/dev/sda3", "/", "2"),
            rec("/boot", "ext4", "/dev/sda1", "/", "3"),
        ];
        l.swap = vec![crate::layout::SwapRecord {
            kind: "file".into(),
            path: "/swapfile".into(),
            size_bytes: 2 * GIB,
            uuid: None,
            crypt_uuid: None,
        }];
        let plan = make_plan(&l, &target(false), true).unwrap();
        let purposes: Vec<Purpose> = plan.partitions.iter().map(|p| p.purpose).collect();
        assert_eq!(
            purposes,
            vec![Purpose::BiosBoot, Purpose::Boot, Purpose::Root]
        );
        assert_eq!(plan.merged, vec!["/home", "/boot"]);
        assert_eq!(plan.esp_point, None);
        assert_eq!(plan.swap_files.len(), 1);
        assert_eq!(plan.part_path(2), "/dev/nvme1n1p2");
        let sda = Target {
            disk: "/dev/sda".into(),
            ..target(false)
        };
        assert_eq!(
            make_plan(&l, &sda, false).unwrap().part_path(2),
            "/dev/sda2"
        );
    }

    #[test]
    fn refuses_small_disk_and_other_bootloaders() {
        let small = Target {
            size_bytes: 10 * GIB,
            ..target(true)
        };
        let err = make_plan(&btrfs_layout(), &small, false).unwrap_err();
        assert!(err.to_string().contains("too small"), "{err}");
        let mut l = btrfs_layout();
        l.bootloader = "systemd-boot".into();
        assert!(make_plan(&l, &target(true), false).is_err());
    }

    #[test]
    fn boot_mode_change_is_noted() {
        let plan = make_plan(&btrfs_layout(), &target(false), false).unwrap();
        assert!(plan.notes.iter().any(|n| n.contains("UEFI mode")));
        assert_eq!(plan.merged, vec!["/boot/efi"]);
    }
}
