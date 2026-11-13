//! Privileged media operations. Shell code is embedded in the trusted binary,
//! never loaded from the medium. All mounts are private to the helper.
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::{PartitionPlan, capture, digest, private_dir, trusted_file, valid_sha256, valid_uuid};
use crate::rescue::{Disk, guard};

pub(super) struct VerifiedImage {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
    pub size: u64,
}
impl VerifiedImage {
    pub fn open(iso: &Path, pinned: Option<&str>, cache: &Path) -> Result<Self> {
        trusted_file(iso)?;
        let expected = if let Some(pin) = pinned {
            pin.to_owned()
        } else {
            let mut checksum_path = iso.as_os_str().to_os_string();
            checksum_path.push(".sha256");
            let checksum_path = PathBuf::from(checksum_path);
            trusted_file(&checksum_path).context("adjacent .sha256 must be a trusted root-owned local file; alternatively supply --sha256")?;
            let checksum = fs::read_to_string(checksum_path)?;
            checksum
                .split_whitespace()
                .next()
                .context("empty ISO checksum")?
                .to_owned()
        };
        if !valid_sha256(&expected) {
            bail!("invalid ISO checksum");
        }
        private_dir(cache)?;
        super::local_storage(cache)?;
        let dir = tempfile::Builder::new().prefix("iso-").tempdir_in(cache)?;
        let path = dir.path().join("verified.iso");
        // Verify the private copy that will be mounted, closing a source-file
        // substitution race between hashing and privileged use.
        let mut input = File::open(iso)?;
        let mut output = File::create(&path)?;
        use std::io::Write;
        let mut buffer = vec![0; 1024 * 1024];
        loop {
            check_interrupted()?;
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            output.write_all(&buffer[..n])?;
        }
        output.sync_all()?;
        let actual = digest(&path)?;
        if !actual.eq_ignore_ascii_case(&expected) {
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
pub(super) struct InspectedDisk {
    disk: Disk,
    identity: serde_json::Value,
    owned_loop: bool,
}
impl std::ops::Deref for InspectedDisk {
    type Target = Disk;
    fn deref(&self) -> &Disk {
        &self.disk
    }
}

/// lsblk reads file systems, UUIDs and labels from the udev database. Callers
/// let udev finish probing a just attached or plugged-in disk first
/// ([`settle_udev`]), before any lock on the device, so two inspections of an
/// unchanged disk see the same identity.
pub(super) fn inspect(path: &Path, owned_loop: bool) -> Result<InspectedDisk> {
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
                "NAME,PATH,TYPE,SIZE,MODEL,SERIAL,TRAN,RO,MOUNTPOINTS,FSTYPE,UUID,PARTUUID,LABEL,WWN,PTUUID,START",
            ])
            .arg(&path),
    )?;
    let mut tree: serde_json::Value = serde_json::from_str(&text)?;
    let first = &mut tree["blockdevices"][0];
    probe_content(first)?;
    let identity = first.clone();
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
    Ok(InspectedDisk {
        disk,
        identity,
        owned_loop,
    })
}

/// Probe signatures directly: lsblk's udev properties can change from stale to
/// current just after attaching a partitioned loop or hotplugging a disk.
fn probe_content(node: &mut serde_json::Value) -> Result<()> {
    let path = node["path"].as_str().context("missing block path")?;
    let out = Command::new("blkid")
        .args(["-p", "-o", "export", "--"])
        .arg(path)
        .stdin(Stdio::null())
        .env("LC_ALL", "C")
        .output()?;
    if !out.status.success() && out.status.code() != Some(2) {
        bail!("cannot safely probe block content");
    }
    let text = String::from_utf8(out.stdout)?;
    let properties: std::collections::BTreeMap<_, _> = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    for (key, property) in [
        ("fstype", "TYPE"),
        ("uuid", "UUID"),
        ("partuuid", "PART_ENTRY_UUID"),
        ("label", "LABEL"),
        ("ptuuid", "PTUUID"),
    ] {
        node[key] = properties.get(property).copied().unwrap_or_default().into();
    }
    // Include partition count/geometry and any additional signature fields.
    node["probe"] = serde_json::to_value(&properties)?;
    if let Some(children) = node["children"].as_array_mut() {
        for child in children.iter_mut() {
            probe_content(child)?;
        }
        children.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    }
    Ok(())
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
        if ask_confirmation(prompt)? != *phrase {
            bail!("confirmation did not match; nothing changed");
        }
    }
    Ok(())
}

/// Do not let stdio's read_line retry EINTR indefinitely after the early signal
/// guard is installed. Poll stdin so Ctrl-C also unwinds the verified ISO while
/// the user is thinking at either destructive-confirmation prompt.
fn ask_confirmation(prompt: &str) -> Result<String> {
    use std::io::Write;
    let input = std::io::stdin();
    loop {
        check_interrupted()?;
        eprint!("{prompt}: ");
        std::io::stderr().flush()?;
        let mut line = Vec::new();
        loop {
            check_interrupted()?;
            let mut poll = nix::libc::pollfd {
                fd: 0,
                events: nix::libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid stack-allocated pollfd, with a bounded timeout.
            let ready = unsafe { nix::libc::poll(&mut poll, 1, 100) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if ready == 0 {
                continue;
            }
            let mut byte = [0];
            match nix::unistd::read(&input, &mut byte) {
                Ok(0) => bail!("input closed"),
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => line.push(byte[0]),
                Err(nix::errno::Errno::EINTR) => continue,
                Err(error) => return Err(error.into()),
            }
            if line.len() > 4096 {
                bail!("confirmation input too long");
            }
        }
        let line = String::from_utf8(line)?;
        if !line.trim().is_empty() {
            return Ok(line.trim().to_owned());
        }
    }
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
    image: PathBuf,
}
impl OwnedLoop {
    pub fn open(image: &Path) -> Result<Self> {
        if !fs::symlink_metadata(image)?.is_file() {
            bail!("--test-image requires an existing regular disposable image file");
        }
        let image = fs::canonicalize(image)?;
        // Never reuse somebody else's existing loop attachment. A device that
        // was just released can still be listed while udev finishes with its
        // partitions, so give such a release a few seconds before refusing.
        if !wait_until_detached(&image)? {
            bail!("test image already has a loop attachment");
        }
        let path = capture(
            Command::new("losetup")
                .args(["--find", "--show", "--partscan"])
                .arg(&image),
        )?;
        settle_udev();
        Ok(Self {
            path: path.into(),
            image,
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for OwnedLoop {
    /// `losetup -d` only marks a busy device for release (udev may still be
    /// probing its partitions); wait until the image is really detached, so
    /// the next `--test-image` run does not see it as somebody else's.
    fn drop(&mut self) {
        let _ = Command::new("losetup").arg("-d").arg(&self.path).status();
        let _ = wait_until_detached(&self.image);
    }
}

pub(super) fn settle_udev() {
    let _ = Command::new("udevadm")
        .args(["settle", "--timeout=30"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// True once `losetup -j` lists no device for `image`; false after about ten
/// seconds of waiting for udev to let go of a released device.
fn wait_until_detached(image: &Path) -> Result<bool> {
    for _ in 0..100 {
        if capture(Command::new("losetup").arg("-j").arg(image))?.is_empty() {
            return Ok(true);
        }
        settle_udev();
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(false)
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
    disk: &InspectedDisk,
    image: Option<&VerifiedImage>,
    cache: &Path,
) -> Result<Info> {
    // Coordinate with udev block-device probes while writing the partition
    // table/filesystems; all our child tools use their normal device access.
    settle_udev();
    let _device_lock = lock_device(&disk.path)?;
    // Fail closed immediately before the helper starts, after confirmation and
    // ISO verification, including mounts/holders that appeared in the meantime.
    let current = inspect(Path::new(&disk.path), disk.owned_loop)?;
    same_identity(&disk.identity, &current.identity)?;
    let plan = image
        .filter(|_| action == "create")
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
    .arg(plan.map(|p| p.slot_mib).unwrap_or(0).to_string())
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
    check_interrupted()?;
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

fn same_identity(confirmed: &serde_json::Value, current: &serde_json::Value) -> Result<()> {
    if confirmed != current {
        bail!(
            "target disk identity or content changed after inspection/confirmation; nothing changed"
        );
    }
    Ok(())
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

pub(super) fn check_interrupted() -> Result<()> {
    if INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
        bail!("stick operation interrupted");
    }
    Ok(())
}

static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
extern "C" fn interrupted(_: i32) {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
}
pub(super) struct SignalGuard(Vec<(nix::sys::signal::Signal, nix::sys::signal::SigAction)>);
impl SignalGuard {
    pub(super) fn install() -> Result<Self> {
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
    fn refuses_every_changed_disk_identity_component() {
        let confirmed = serde_json::json!({
            "size": 8589934592_u64, "model": "external disk", "serial": "sample",
            "wwn": "sample-wwn", "ptuuid": "sample-table",
            "children": [{"path":"/dev/sdx1", "size":1024, "uuid":"sample-fs"}]
        });
        same_identity(&confirmed, &confirmed).unwrap();
        for field in ["size", "model", "serial", "wwn", "ptuuid", "children"] {
            let mut changed = confirmed.clone();
            changed[field] = serde_json::Value::Null;
            assert!(same_identity(&confirmed, &changed).is_err(), "{field}");
        }
    }
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
    fn pinned_checksum_precedes_sidecar_and_private_copy_is_removed() {
        if !nix::unistd::Uid::effective().is_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let iso = d.path().join("release.iso");
        fs::write(&iso, b"test release").unwrap();
        let cache = d.path().join("cache");
        let pin = digest(&iso).unwrap();
        fs::write(d.path().join("release.iso.sha256"), "invalid").unwrap();
        let verified = VerifiedImage::open(&iso, Some(&pin), &cache).unwrap();
        let copy = verified.path.clone();
        assert!(copy.starts_with(&cache));
        assert_eq!(digest(&copy).unwrap(), pin);
        drop(verified);
        assert!(!copy.exists());
        assert!(VerifiedImage::open(&iso, None, &cache).is_err());
        fs::set_permissions(&iso, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(VerifiedImage::open(&iso, Some(&pin), &cache).is_err());
        fs::set_permissions(&iso, fs::Permissions::from_mode(0o644)).unwrap();
        nix::unistd::chown(&iso, Some(nix::unistd::Uid::from_raw(65534)), None).unwrap();
        assert!(VerifiedImage::open(&iso, Some(&pin), &cache).is_err());
    }
    #[test]
    fn checksum_failure_precedes_disk_operations() {
        if !nix::unistd::Uid::effective().is_root() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let iso = d.path().join("bad.iso");
        fs::write(&iso, b"not a release").unwrap();
        fs::write(
            d.path().join("bad.iso.sha256"),
            format!("{}  bad.iso\n", "0".repeat(64)),
        )
        .unwrap();
        assert!(
            VerifiedImage::open(&iso, None, &d.path().join("cache"))
                .err()
                .unwrap()
                .to_string()
                .contains("mismatch")
        );
    }
}
