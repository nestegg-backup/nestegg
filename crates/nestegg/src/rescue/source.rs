//! The booted medium is identified by live-boot, never by a volume label alone.
use std::fs::{self, File};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::config::Paths;
use crate::stick::{capture, valid_uuid};

const MEDIUM: &str = "/run/live/medium";

#[derive(Debug, PartialEq, Eq)]
pub struct BootMedium {
    data_uuid: String,
    image: PathBuf,
}

fn boot_arguments(cmdline: &str) -> Result<(&str, &str)> {
    let value = |name: &str| -> Result<&str> {
        let mut values = cmdline
            .split_whitespace()
            .filter_map(|w| w.strip_prefix(name));
        let v = values
            .next()
            .context("not booted from a nestegg backup stick")?;
        ensure!(values.next().is_none(), "ambiguous live boot arguments");
        Ok(v)
    };
    let uuid = value("live-media=")?
        .strip_prefix("/dev/disk/by-uuid/")
        .context("live medium is not UUID-pinned")?;
    ensure!(
        uuid.len() == 9
            && uuid.as_bytes()[4] == b'-'
            && uuid
                .bytes()
                .enumerate()
                .all(|(i, c)| i == 4 || c.is_ascii_hexdigit()),
        "invalid live slot UUID"
    );
    let slot = value("live-media-path=")?;
    ensure!(matches!(slot, "/live/a" | "/live/b"), "invalid live slot");
    Ok((uuid, slot))
}

fn marker_data(marker: &str, slot_uuid: &str) -> Result<String> {
    let lines: Vec<_> = marker.lines().collect();
    ensure!(
        lines.len() == 3 && lines[0] == "nestegg-slot-v2",
        "invalid stick marker"
    );
    ensure!(
        lines[1]
            .strip_prefix("slot_uuid=")
            .is_some_and(|u| u.eq_ignore_ascii_case(slot_uuid)),
        "marker does not identify the booted live partition"
    );
    let data = lines[2]
        .strip_prefix("data_uuid=")
        .context("missing data UUID")?;
    ensure!(valid_uuid(data), "invalid data UUID");
    Ok(data.to_ascii_lowercase())
}

fn check_data_partition(
    devs: &[crate::layout::BlockDev],
    boot: &Path,
    data_uuid: &str,
) -> Result<()> {
    let live = devs
        .iter()
        .find(|d| Path::new(&d.path) == boot)
        .context("boot device missing")?;
    let matches: Vec<_> = devs
        .iter()
        .filter(|d| {
            d.uuid
                .as_deref()
                .is_some_and(|u| u.eq_ignore_ascii_case(data_uuid))
        })
        .collect();
    ensure!(matches.len() == 1, "missing or cloned stick data UUID");
    let data = matches[0];
    ensure!(
        data.kind == "part"
            && data.fstype.as_deref() == Some("ext4")
            && data.pkname.is_some()
            && data.pkname == live.pkname,
        "data partition is not on the booted medium"
    );
    Ok(())
}

impl BootMedium {
    pub fn detect() -> Result<Self> {
        let cmdline = fs::read_to_string("/proc/cmdline")?;
        let (uuid, slot) = boot_arguments(&cmdline)?;
        let mounts = capture(Command::new("findmnt").args([
            "--json",
            "--mountpoint",
            MEDIUM,
            "-o",
            "SOURCE,FSTYPE,OPTIONS,UUID",
        ]))?;
        let mounts: serde_json::Value = serde_json::from_str(&mounts)?;
        let m = &mounts["filesystems"][0];
        ensure!(
            m["fstype"] == "vfat"
                && m["uuid"]
                    .as_str()
                    .is_some_and(|u| u.eq_ignore_ascii_case(uuid))
                && m["options"]
                    .as_str()
                    .unwrap_or_default()
                    .split(',')
                    .any(|o| o == "ro"),
            "live medium must be the read-only slot partition named at boot"
        );
        let boot = fs::canonicalize(m["source"].as_str().context("missing boot device")?)?;
        let marker = fs::read_to_string(Path::new(MEDIUM).join("nestegg-stick"))?;
        let data_uuid = marker_data(&marker, uuid)?;
        let devs = crate::layout::lsblk()?;
        check_data_partition(&devs, &boot, &data_uuid)?;
        Ok(Self {
            data_uuid,
            image: Path::new(MEDIUM)
                .join(slot.trim_start_matches('/'))
                .join("filesystem.squashfs"),
        })
    }

    pub fn open(&self) -> Result<crate::stick::target::Session> {
        let paths = live_paths();
        crate::stick::target::isolate_mounts()?;
        let mut session = crate::stick::target::Session::open(&paths, &self.data_uuid)
            .map_err(|e| anyhow::anyhow!("{}", e.message))?;
        session.start_live(&paths)?;
        Ok(session)
    }
}

/// Offline keys, logs and inventory must never land on a computer's disk,
/// even when a root caller overrides the usual /run work directory.
pub fn require_ram_workspace() -> Result<()> {
    let work = super::work_dir();
    let ancestor = work
        .ancestors()
        .find(|p| p.exists())
        .context("no rescue workspace ancestor")?;
    let kind = capture(
        Command::new("findmnt")
            .args(["-n", "-o", "FSTYPE", "--target"])
            .arg(ancestor),
    )?;
    ensure!(
        kind == "tmpfs",
        "offline rescue workspace must be on RAM tmpfs"
    );
    ensure!(
        crate::layout::parse_proc_swaps(&fs::read_to_string("/proc/swaps")?).is_empty(),
        "offline backup requires no active swap before opening keys"
    );
    Ok(())
}

pub fn live_paths() -> Paths {
    let mut paths = Paths::from_env();
    paths.runtime_dir = super::work_dir().join("local");
    paths.state_dir = super::work_dir().join("state");
    paths.credentials_dir = super::work_dir().join("credentials");
    paths
}

/// IMAGE_PATH in a booted image is the squashfs that live-boot has already
/// mounted. Check its loop backing file too: a mounted foreign live partition is not
/// enough to make an image executable. This entry point is unavailable on a
/// desktop, and never changes the desktop launcher's fixed-cache policy.
pub(crate) fn boot_image() -> Result<File> {
    let medium = BootMedium::detect()?;
    let mounted: serde_json::Value =
        serde_json::from_str(&capture(Command::new("findmnt").args([
            "--json",
            "--mountpoint",
            "/run/live/rootfs/filesystem.squashfs",
            "-o",
            "SOURCE,FSTYPE",
        ]))?)?;
    let m = &mounted["filesystems"][0];
    ensure!(m["fstype"] == "squashfs", "live-boot root is not squashfs");
    let loop_path = fs::canonicalize(m["source"].as_str().context("missing live loop")?)?;
    let name = loop_path.file_name().context("invalid live loop")?;
    ensure!(
        name.to_string_lossy().starts_with("loop"),
        "live root must use a loop device"
    );
    let backing = fs::read_to_string(
        Path::new("/sys/class/block")
            .join(name)
            .join("loop/backing_file"),
    )?;
    let backing = fs::canonicalize(backing.trim())?;
    ensure!(
        backing == fs::canonicalize(&medium.image)?,
        "image is not the booted live system"
    );
    let image = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&medium.image)?;
    if !image.metadata()?.is_file() {
        bail!("live image must be a regular file");
    }
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requires_pinned_boot_medium_and_one_exact_slot() {
        assert_eq!(
            boot_arguments(
                "boot=live live-media=/dev/disk/by-uuid/1234-ABCD live-media-path=/live/b"
            )
            .unwrap(),
            ("1234-ABCD", "/live/b")
        );
        for s in [
            "live-media=/dev/disk/by-uuid/1234-ABCD live-media=/dev/foreign live-media-path=/live/a",
            "",
            "live-media=/dev/sda2 live-media-path=/live/a",
            "live-media=/dev/disk/by-uuid/1234-ABCD live-media-path=/live/../a",
            "live-media=/dev/disk/by-uuid/1234-ABCD live-media-path=/live/a live-media-path=/live/b",
        ] {
            assert!(boot_arguments(s).is_err(), "{s}");
        }
    }
    #[test]
    fn marker_must_match_slot_and_have_exact_fields() {
        let m = "nestegg-slot-v2\nslot_uuid=1234-ABCD\ndata_uuid=01234567-89ab-cdef-0123-456789abcdef\n";
        assert!(marker_data(m, "1234-abcd").is_ok());
        assert!(marker_data(m, "0000-0000").is_err());
        assert!(marker_data(&format!("{m}extra=x\n"), "1234-ABCD").is_err());
        assert!(
            marker_data(
                &m.replace("01234567-89ab-cdef-0123-456789abcdef", "../../etc"),
                "1234-ABCD"
            )
            .is_err()
        );
    }
    #[test]
    fn data_must_be_the_unique_ext4_sibling_of_the_booted_slot() {
        use crate::layout::BlockDev;
        let uuid = "01234567-89ab-cdef-0123-456789abcdef";
        let boot = BlockDev {
            path: "/dev/vda3".into(),
            pkname: Some("vda".into()),
            kind: "part".into(),
            ..Default::default()
        };
        let data = BlockDev {
            path: "/dev/vda5".into(),
            pkname: Some("vda".into()),
            kind: "part".into(),
            fstype: Some("ext4".into()),
            uuid: Some(uuid.into()),
            ..Default::default()
        };
        assert!(
            check_data_partition(&[boot.clone(), data.clone()], Path::new("/dev/vda3"), uuid)
                .is_ok()
        );
        let mut wrong = data.clone();
        wrong.pkname = Some("vdb".into());
        assert!(
            check_data_partition(&[boot.clone(), wrong], Path::new("/dev/vda3"), uuid).is_err()
        );
        assert!(
            check_data_partition(
                &[boot.clone(), data.clone(), data.clone()],
                Path::new("/dev/vda3"),
                uuid
            )
            .is_err()
        );
        let mut wrong = data;
        wrong.fstype = Some("btrfs".into());
        assert!(check_data_partition(&[boot, wrong], Path::new("/dev/vda3"), uuid).is_err());
    }
}
