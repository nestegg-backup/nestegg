//! Privileged media operations. Shell code is embedded in the trusted binary,
//! never loaded from the medium. All mounts are private to the helper.
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::{PartitionPlan, capture, digest, valid_uuid};
use crate::rescue::{Disk, guard};

pub(super) struct VerifiedImage {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
    pub size: u64,
}
impl VerifiedImage {
    pub fn open(iso: &Path) -> Result<Self> {
        let checksum = fs::read_to_string(format!("{}.sha256", iso.display())).context(
            "release ISO needs its adjacent .sha256 file; obtain both from a trusted release",
        )?;
        let expected = checksum
            .split_whitespace()
            .next()
            .context("empty ISO checksum")?;
        if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("invalid ISO checksum");
        }
        let dir = tempfile::Builder::new().prefix("nestegg-iso-").tempdir()?;
        let path = dir.path().join("verified.iso");
        // Verify the private copy that will be mounted, closing a source-file
        // substitution race between hashing and privileged use.
        let mut input = File::open(iso)?;
        let mut output = File::create(&path)?;
        io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        let actual = digest(&path)?;
        if !actual.eq_ignore_ascii_case(expected) {
            bail!("ISO SHA-256 mismatch; nothing on the target disk was changed");
        }
        let size = output.metadata()?.len();
        Ok(Self {
            _dir: dir,
            path,
            size,
        })
    }
}

/// Reuse 0010's content classifier and live-use guard. Loop devices are accepted
/// only inside the image-file test path, after this process allocated that loop.
pub(super) fn inspect(path: &Path, owned_loop: bool) -> Result<Disk> {
    let path = fs::canonicalize(path)?;
    if !fs::metadata(&path)?.file_type().is_block_device() {
        bail!("target must be a whole block disk");
    }
    let text = capture(
        Command::new("lsblk")
            .args([
                "-J",
                "-b",
                "-o",
                "NAME,PATH,TYPE,SIZE,MODEL,SERIAL,TRAN,RO,MOUNTPOINTS,FSTYPE,UUID,PARTUUID,LABEL",
            ])
            .arg(&path),
    )?;
    let mut tree: serde_json::Value = serde_json::from_str(&text)?;
    let first = &mut tree["blockdevices"][0];
    // The shared parser deliberately excludes loops from rescue target lists.
    // For a loop we own, map only its display identity through that parser;
    // the returned content and children remain the real probed values.
    if owned_loop && first["type"] == "loop" {
        first["type"] = "disk".into();
        first["path"] = "/dev/test-owned-disk".into();
    }
    let mut disks = crate::rescue::parse_disks(&tree.to_string(), &[])?;
    let mut disk = disks.pop().context(
        "not a writable whole disk (partitions, mapped or read-only devices are refused)",
    )?;
    if owned_loop {
        disk.path = path.to_string_lossy().into_owned();
    }
    ensure_free(&disk)?;
    Ok(disk)
}

pub(super) fn ensure_free(disk: &Disk) -> Result<()> {
    if disk.in_use
        || disk
            .items
            .iter()
            .any(|i| !matches!(i.kind.as_str(), "disk" | "part"))
    {
        bail!("target disk is in use (mount, swap, encryption, LVM or RAID)");
    }
    crate::rescue::exec::check_disk_free(&disk.path)
}

pub(super) fn confirm(disk: &Disk) -> Result<()> {
    ensure_free(disk)?;
    let assessment = guard::assess(disk, &[], &[]);
    println!("All content on {} will be erased.", guard::identity(disk));
    for line in assessment.lines("") {
        println!("  {}", crate::rescue::printable(&line));
    }
    for (index, phrase) in confirmation_phrases(disk).iter().enumerate() {
        let prompt = if index == 0 {
            "Type the disk name to continue"
        } else {
            "Disk is not empty; also type ERASE followed by the disk name"
        };
        if crate::rescue::ask(prompt, None)? != *phrase {
            bail!("confirmation did not match; nothing changed");
        }
    }
    Ok(())
}

fn confirmation_phrases(disk: &Disk) -> Vec<String> {
    let mut phrases = vec![disk.path.rsplit('/').next().unwrap_or_default().to_owned()];
    if guard::assess(disk, &[], &[]).holds_data() {
        phrases.push(guard::erase_phrase(disk));
    }
    phrases
}

pub(super) struct OwnedLoop {
    path: PathBuf,
}
impl OwnedLoop {
    pub fn open(image: &Path) -> Result<Self> {
        if !fs::symlink_metadata(image)?.is_file() {
            bail!("--test-image requires an existing regular disposable image file");
        }
        let image = fs::canonicalize(image)?;
        // Never reuse somebody else's existing loop attachment.
        if !capture(Command::new("losetup").arg("-j").arg(&image))?.is_empty() {
            bail!("test image already has a loop attachment");
        }
        let path = capture(
            Command::new("losetup")
                .args(["--find", "--show", "--partscan"])
                .arg(&image),
        )?;
        Ok(Self { path: path.into() })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for OwnedLoop {
    fn drop(&mut self) {
        let _ = Command::new("losetup").arg("-d").arg(&self.path).status();
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Info {
    pub data_uuid: String,
    pub boot_uuid: String,
    pub active_slot: String,
    pub image_sha256: String,
}
impl Info {
    pub fn validate(&self) -> Result<()> {
        if !valid_uuid(&self.data_uuid)
            || !matches!(self.active_slot.as_str(), "a" | "b")
            || self.boot_uuid.len() != 9
            || !self.boot_uuid.bytes().enumerate().all(|(i, b)| {
                if i == 4 {
                    b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            })
            || self.image_sha256.len() != 64
            || !self.image_sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            bail!("invalid nestegg stick metadata");
        }
        Ok(())
    }
}

pub(super) fn operate(
    action: &str,
    disk: &Disk,
    image: Option<&VerifiedImage>,
    cache: &Path,
) -> Result<Info> {
    // Coordinate with udev block-device probes while writing the partition
    // table/filesystems; all our child tools use their normal device access.
    let _device_lock = lock_device(&disk.path)?;
    // Fail closed immediately before the helper starts, after confirmation and
    // ISO verification, including mounts/holders that appeared in the meantime.
    ensure_free(disk)?;
    let plan = image
        .map(|i| PartitionPlan::new(disk.size_bytes, i.size))
        .transpose()?;
    let work = tempfile::Builder::new()
        .prefix("nestegg-media-")
        .tempdir()?;
    let mut cmd = Command::new("unshare");
    cmd.args([
        "--mount",
        "--propagation",
        "private",
        "bash",
        "-c",
        include_str!("media.sh"),
        "--",
    ])
    .arg(action)
    .arg(&disk.path)
    .arg(
        image
            .map(|i| i.path.as_path())
            .unwrap_or(Path::new("/dev/null")),
    )
    .arg(cache)
    .arg(work.path())
    .arg(plan.map(|p| p.esp_mib).unwrap_or(0).to_string())
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .stdin(Stdio::null());
    // Stop the helper gracefully on SIGINT/SIGTERM, so it unmounts before the
    // owned test loop is detached. The CLI is single threaded at this point.
    use std::os::unix::process::CommandExt;
    // SAFETY: setpgid is async-signal-safe and isolates only our helper group.
    unsafe {
        cmd.pre_exec(|| {
            if nix::libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let _signals = SignalGuard::install()?;
    let mut child = cmd.spawn()?;
    let mut output = child.stdout.take().context("missing helper output")?;
    let status = loop {
        if INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(-(child.id() as i32)),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let mut text = String::new();
    output.read_to_string(&mut text)?;
    if !status.success() {
        if action == "update" {
            bail!(
                "stick update failed ({status}); inactive slot may be incomplete; previous committed slot and data were retained"
            );
        }
        bail!("stick {action} failed ({status})");
    }
    let info: Info = serde_json::from_str(text.trim()).context("invalid helper result")?;
    info.validate()?;
    Ok(info)
}

fn lock_device(path: &str) -> Result<nix::fcntl::Flock<File>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut file = File::open(path)?;
    loop {
        match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
            Ok(lock) => return Ok(lock),
            Err((owned, nix::errno::Errno::EAGAIN)) if std::time::Instant::now() < deadline => {
                file = owned;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err((_, error)) => bail!("target disk is locked by another operation: {error}"),
        }
    }
}

static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
extern "C" fn interrupted(_: i32) {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
}
struct SignalGuard(Vec<(nix::sys::signal::Signal, nix::sys::signal::SigAction)>);
impl SignalGuard {
    fn install() -> Result<Self> {
        use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};
        INTERRUPTED.store(false, std::sync::atomic::Ordering::Relaxed);
        let action = SigAction::new(
            SigHandler::Handler(interrupted),
            SaFlags::empty(),
            SigSet::empty(),
        );
        let mut result = Self(Vec::new());
        for sig in [Signal::SIGINT, Signal::SIGTERM] {
            result
                .0
                .push((sig, unsafe { signal::sigaction(sig, &action)? }));
        }
        Ok(result)
    }
}
impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (sig, action) in &self.0 {
            let _ = unsafe { nix::sys::signal::sigaction(*sig, action) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_busy_target_without_running_writer() {
        let d = Disk {
            path: "/dev/nonexistent".into(),
            size_bytes: 8 << 30,
            model: String::new(),
            serial: String::new(),
            transport: String::new(),
            in_use: true,
            items: vec![],
        };
        assert!(ensure_free(&d).unwrap_err().to_string().contains("in use"));
    }
    #[test]
    fn existing_rescue_guard_requires_second_confirmation_for_any_content() {
        let mut d = Disk {
            path: "/dev/sdx".into(),
            size_bytes: 8 << 30,
            model: String::new(),
            serial: String::new(),
            transport: String::new(),
            in_use: false,
            items: vec![],
        };
        assert_eq!(confirmation_phrases(&d), vec!["sdx"]);
        d.items.push(guard::BlockItem {
            path: "/dev/sdx1".into(),
            kind: "part".into(),
            ..Default::default()
        });
        assert_eq!(confirmation_phrases(&d), vec!["sdx", "ERASE sdx"]);
        d.items[0].fstype = "linux_raid_member".into();
        assert_eq!(confirmation_phrases(&d), vec!["sdx", "ERASE sdx"]);
        d.items[0].kind = "crypt".into();
        assert!(ensure_free(&d).is_err());
    }

    #[test]
    fn checksum_failure_precedes_disk_operations() {
        let d = tempfile::tempdir().unwrap();
        let iso = d.path().join("bad.iso");
        fs::write(&iso, b"not a release").unwrap();
        fs::write(
            d.path().join("bad.iso.sha256"),
            format!("{}  bad.iso\n", "0".repeat(64)),
        )
        .unwrap();
        assert!(
            VerifiedImage::open(&iso)
                .err()
                .unwrap()
                .to_string()
                .contains("mismatch")
        );
    }
}
