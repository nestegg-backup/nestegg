//! Bootable removable media and an isolated, local PBS (decision 0023).
//! Media preparation, registered desktop targets and protected recovery keys.
pub mod cli;
pub mod key;
pub mod launcher;
mod media;
pub mod target;

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// Exact ext4 UUID syntax, also safe to use as a /dev/disk/by-uuid component.
pub fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    A,
    B,
}
impl Slot {
    pub fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::A => "a",
            Self::B => "b",
        }
    }
}

/// Sizes in MiB, aligned to MiB boundaries. Leave GPT headers at either end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionPlan {
    pub esp_mib: u64,
    pub slot_mib: u64,
    pub data_start_mib: u64,
}
impl PartitionPlan {
    pub fn new(disk_bytes: u64, live_bytes: u64) -> Result<Self> {
        // Each independent FAT slot has 50% image headroom plus 128 MiB,
        // with a 2 GiB floor. The ESP contains only GRUB and never updates.
        let image_mib = live_bytes
            .checked_add((1 << 20) - 1)
            .context("image too large")?
            >> 20;
        let slot_mib = image_mib
            .checked_add(image_mib / 2)
            .and_then(|n| n.checked_add(128))
            .context("image too large")?
            .max(2048);
        let esp_mib = 64;
        let data_start_mib = slot_mib
            .checked_mul(2)
            .and_then(|n| n.checked_add(esp_mib + 2))
            .context("image too large")?;
        // PBS preallocates 65,536 chunk directories. A 1 GiB ext4 filesystem
        // with the default inode ratio cannot even hold that directory tree.
        if disk_bytes >> 20 < data_start_mib + 2049 {
            bail!("disk too small: need two live slots and at least 2 GiB of data space");
        }
        Ok(Self {
            esp_mib,
            slot_mib,
            data_start_mib,
        })
    }
}

pub(crate) fn capture(cmd: &mut Command) -> Result<String> {
    let out = cmd
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .context("cannot start system tool")?;
    if !out.status.success() {
        bail!(
            "{} failed: {}",
            cmd.get_program().to_string_lossy(),
            crate::rescue::printable_block(&String::from_utf8_lossy(&out.stderr))
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

pub(crate) fn digest(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    digest_reader(&mut file)
}

pub(crate) fn digest_reader(file: &mut impl Read) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buf = vec![0; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// A trusted input is a regular root-owned file, with no writable or symbolic
/// components. Storage provenance is checked separately by trusted_file().
pub(crate) fn trusted_path(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if !path.is_absolute() {
        bail!("trusted path must be absolute");
    }
    for part in path.ancestors() {
        let meta = fs::symlink_metadata(part)?;
        if meta.file_type().is_symlink()
            || meta.uid() != 0
            || (meta.mode() & 0o022 != 0
                && !(meta.is_dir() && meta.mode() & 0o1000 != 0 && part != path))
        {
            bail!(
                "trusted image/runtime path must be root-owned, without symlinks or group/other write permission"
            );
        }
    }
    Ok(())
}

/// Reject FAT/optical media and any removable, USB or loop-backed block ancestor.
/// An immutable-looking root-owned mount is not evidence of desktop provenance.
pub(crate) fn local_storage(path: &Path) -> Result<()> {
    let mount: serde_json::Value = serde_json::from_str(&capture(
        Command::new("findmnt")
            .args(["--json", "--target"])
            .arg(path)
            .args(["-o", "FSTYPE,MAJ:MIN,UUID"]),
    )?)?;
    let fs = &mount["filesystems"][0];
    let fstype = fs["fstype"].as_str().context("missing filesystem type")?;
    if matches!(fstype, "vfat" | "exfat" | "msdos" | "iso9660" | "udf") {
        bail!("trusted input must not reside on FAT or removable/optical media");
    }
    let device = fs["maj:min"].as_str().context("missing backing device")?;
    for device in backing_devices(
        fstype,
        device,
        fs["uuid"].as_str(),
        Path::new("/sys/fs/btrfs"),
    )? {
        let tree: serde_json::Value = serde_json::from_str(&capture(
            Command::new("lsblk")
                .args(["--json", "--inverse", "-o", "NAME,TYPE,RM,TRAN,SUBSYSTEMS"])
                .arg(format!("/dev/block/{device}")),
        )?)?;
        if !fixed_devices(&tree["blockdevices"]) {
            bail!("trusted input must not reside on removable, USB or loop-backed storage");
        }
    }
    Ok(())
}

/// Btrfs has anonymous st_dev/mountinfo numbers even on real disks. Resolve
/// ALL members (including seed devices) through the kernel's sysfs links.
fn backing_devices(
    fstype: &str,
    device: &str,
    uuid: Option<&str>,
    btrfs_sys: &Path,
) -> Result<Vec<String>> {
    if fstype == "btrfs" {
        let uuid = uuid
            .filter(|v| valid_uuid(v))
            .context("cannot resolve Btrfs filesystem UUID")?;
        let base = btrfs_sys.join(uuid.to_ascii_lowercase());
        // A degraded filesystem has an unknown backing member: fail closed.
        let mut members = 0;
        for member in fs::read_dir(base.join("devinfo"))? {
            if fs::read_to_string(member?.path().join("missing"))?.trim() != "0" {
                bail!("cannot establish trusted storage for degraded Btrfs");
            }
            members += 1;
        }
        let mut devices = Vec::new();
        for entry in fs::read_dir(base.join("devices"))? {
            let dev = fs::read_to_string(entry?.path().join("dev"))?;
            let dev = dev.trim();
            if !valid_device_number(dev) || dev.starts_with("0:") {
                bail!("invalid Btrfs backing device");
            }
            devices.push(dev.to_owned());
        }
        if devices.is_empty() || devices.len() != members {
            bail!("cannot establish every Btrfs backing device");
        }
        return Ok(devices);
    }
    if !valid_device_number(device) {
        bail!("invalid backing device number");
    }
    if device.starts_with("0:") {
        // Only explicitly identified RAM filesystems lack backing devices.
        // Overlay, FUSE and unknown anonymous sources are not trusted evidence.
        if matches!(fstype, "tmpfs" | "ramfs") {
            return Ok(Vec::new());
        }
        bail!("cannot establish fixed local backing for anonymous filesystem {fstype}");
    }
    Ok(vec![device.to_owned()])
}

fn valid_device_number(value: &str) -> bool {
    value.split_once(':').is_some_and(|(major, minor)| {
        !major.is_empty()
            && !minor.is_empty()
            && major.bytes().all(|b| b.is_ascii_digit())
            && minor.bytes().all(|b| b.is_ascii_digit())
            && major.parse::<u32>().is_ok()
            && minor.parse::<u32>().is_ok()
    })
}

fn local_transport(value: &str) -> bool {
    matches!(
        value,
        "ata" | "sata" | "sas" | "spi" | "nvme" | "pcie" | "virtio" | "mmc"
    )
}

fn local_virtual_disk(node: &serde_json::Value) -> bool {
    let name = node["name"].as_str().unwrap_or_default();
    let named = ["vd", "xvd"].iter().any(|prefix| {
        name.strip_prefix(prefix).is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_lowercase())
        })
    }) || ["ram", "zram", "pmem"].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
    });
    // SCSI disks behind known VM buses may not expose a TRAN value. NBD/RBD
    // and unknown physical/network drivers must not inherit this exception.
    named
        || node["subsystems"].as_str().is_some_and(|subsystems| {
            subsystems
                .split(':')
                .any(|bus| matches!(bus, "virtio" | "vmbus"))
        })
}

fn fixed_devices(devices: &serde_json::Value) -> bool {
    devices.as_array().is_some_and(|items| {
        !items.is_empty()
            && items.iter().all(|d| {
                if d["rm"] != false {
                    return false;
                }
                let transport = d["tran"].as_str().filter(|s| !s.is_empty());
                if transport.is_some_and(|t| !local_transport(t)) {
                    return false;
                }
                let kind = d["type"].as_str().unwrap_or_default();
                if kind == "disk" {
                    return (transport.is_some() || local_virtual_disk(d))
                        && (d.get("children").is_none() || fixed_devices(&d["children"]));
                }
                let stacked = matches!(kind, "part" | "crypt" | "lvm" | "dm" | "md" | "mpath")
                    || kind.strip_prefix("raid").is_some_and(|suffix| {
                        !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
                    });
                stacked && fixed_devices(&d["children"])
            })
    })
}

pub(crate) fn trusted_file(path: &Path) -> Result<()> {
    trusted_path(path)?;
    if !fs::symlink_metadata(path)?.is_file() {
        bail!("trusted input must be a regular file");
    }
    local_storage(path)
}

pub(crate) fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Create only missing directories with private permissions. Existing paths
/// are validated before use and never chmod-ed through a possible symlink.
pub(crate) fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let existing = path
        .ancestors()
        .find(|p| fs::symlink_metadata(p).is_ok())
        .context("no existing parent")?;
    trusted_path(existing)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    trusted_path(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.mode() & 0o077 != 0 {
        bail!("private runtime/cache directory must have mode 0700");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_removable_ancestors_even_if_partition_is_not_removable() {
        let fixed = serde_json::json!([{"type":"part", "rm":false, "children":[{"type":"disk", "rm":false, "tran":"nvme"}]}]);
        assert!(fixed_devices(&fixed));
        for (field, value) in [
            ("rm", serde_json::json!(true)),
            ("tran", serde_json::json!("usb")),
            ("type", serde_json::json!("loop")),
        ] {
            let mut bad = fixed.clone();
            bad[0]["children"][0][field] = value;
            assert!(!fixed_devices(&bad));
        }
        assert!(!fixed_devices(&serde_json::json!([])));
    }
    #[test]
    fn network_and_unknown_disks_are_not_local_virtual_disks() {
        for (name, transport) in [
            ("nbd0", None),
            ("rbd0", None),
            ("sda", Some("iscsi")),
            ("nvme1n1", Some("tcp")),
            ("sda", None),
        ] {
            let tree = serde_json::json!([{"name":name,"type":"disk","rm":false,"tran":transport}]);
            assert!(!fixed_devices(&tree), "{name} {transport:?}");
        }
        for name in ["vda", "xvdb", "zram0", "pmem0"] {
            let tree = serde_json::json!([{"name":name,"type":"disk","rm":false,"tran":null}]);
            assert!(fixed_devices(&tree), "{name}");
        }
        let vm_scsi = serde_json::json!([{"name":"sda","type":"disk","rm":false,"tran":null,"subsystems":"block:scsi:virtio:pci"}]);
        assert!(fixed_devices(&vm_scsi));
        assert!(!fixed_devices(
            &serde_json::json!([{"type":"part","rm":false,"tran":null}])
        ));
    }
    #[test]
    fn anonymous_btrfs_resolves_every_member_and_unknown_filesystems_fail_closed() {
        let d = tempfile::tempdir().unwrap();
        let uuid = "01234567-89ab-cdef-0123-456789abcdef";
        let base = d.path().join(uuid);
        for (index, name, dev) in [(1, "sdx", "8:16"), (2, "sdy", "8:32")] {
            fs::create_dir_all(base.join("devices").join(name)).unwrap();
            fs::write(base.join("devices").join(name).join("dev"), dev).unwrap();
            fs::create_dir_all(base.join("devinfo").join(index.to_string())).unwrap();
            fs::write(
                base.join("devinfo").join(index.to_string()).join("missing"),
                "0",
            )
            .unwrap();
        }
        let mut devices = backing_devices("btrfs", "0:19", Some(uuid), d.path()).unwrap();
        devices.sort();
        assert_eq!(devices, ["8:16", "8:32"]);
        for fstype in ["overlay", "fuse", "nfs", "unknown"] {
            assert!(backing_devices(fstype, "0:19", None, d.path()).is_err());
        }
        assert!(
            backing_devices("tmpfs", "0:19", None, d.path())
                .unwrap()
                .is_empty()
        );
        assert!(backing_devices("btrfs", "0:19", None, d.path()).is_err());
        fs::write(base.join("devinfo/2/missing"), "1").unwrap();
        assert!(backing_devices("btrfs", "0:19", Some(uuid), d.path()).is_err());
    }
    #[test]
    fn partition_capacity_and_alignment() {
        let p = PartitionPlan::new(8 << 30, 900 << 20).unwrap();
        assert_eq!(p.esp_mib, 64);
        assert_eq!(p.slot_mib, 2048);
        assert_eq!(p.data_start_mib, 4162);
        assert!(PartitionPlan::new(2 << 30, 900 << 20).is_err());
        assert!(PartitionPlan::new(u64::MAX, u64::MAX).is_err());
        assert!(PartitionPlan::new(3 << 30, 100 << 20).is_err());
        assert!(PartitionPlan::new(7 << 30, 100 << 20).is_ok());
    }
    #[test]
    fn switches_only_to_inactive_slot() {
        assert_eq!(Slot::A.other(), Slot::B);
        assert_eq!(Slot::B.other(), Slot::A);
    }
    #[test]
    fn uuid_path_injection_rejected() {
        assert!(valid_uuid("01234567-89ab-cdef-0123-456789abcdef"));
        for s in ["../disk", "x", "01234567-89ab-cdef-0123-456789abcdeg"] {
            assert!(!valid_uuid(s));
        }
    }
}
