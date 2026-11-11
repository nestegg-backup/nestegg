//! Bootable removable media and an isolated, local PBS (decision 0023).
//! Media preparation, registered desktop targets and protected recovery keys.
pub mod cli;
pub mod key;
pub mod launcher;
mod media;
pub mod target;

use std::fs::{self, File};
use std::io::{Read, Write};
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
    pub data_start_mib: u64,
}
impl PartitionPlan {
    pub fn new(disk_bytes: u64, live_bytes: u64) -> Result<Self> {
        // Two complete slots plus 256 MiB for GRUB and filesystem overhead.
        let slot_mib = live_bytes
            .checked_add((1 << 20) - 1)
            .context("image too large")?
            >> 20;
        let esp_mib = slot_mib
            .checked_mul(2)
            .and_then(|n| n.checked_add(256))
            .context("image too large")?
            .max(1024);
        // FAT32 cannot hold an individual squashfs >= 4 GiB (checked separately).
        let data_start_mib = esp_mib + 2;
        // PBS preallocates 65,536 chunk directories. A 1 GiB ext4 filesystem
        // with the default inode ratio cannot even hold that directory tree.
        if disk_bytes >> 20 < data_start_mib + 2049 {
            bail!("disk too small: need two live slots and at least 2 GiB of data space");
        }
        Ok(Self {
            esp_mib,
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

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing parent")?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// A trusted input is a regular root-owned file, with no writable or symbolic
/// components. A mounted stick can never be selected as the desktop image.
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
    fn partition_capacity_and_alignment() {
        let p = PartitionPlan::new(8 << 30, 900 << 20).unwrap();
        assert_eq!(p.esp_mib, 2056);
        assert_eq!(p.data_start_mib, 2058);
        assert!(PartitionPlan::new(2 << 30, 900 << 20).is_err());
        assert!(PartitionPlan::new(u64::MAX, u64::MAX).is_err());
        assert!(PartitionPlan::new(3 << 30, 100 << 20).is_err());
        assert!(PartitionPlan::new(4 << 30, 100 << 20).is_ok());
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
