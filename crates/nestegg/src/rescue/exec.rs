//! Carry out a restore plan on the target disk. Runs as root on the rescue
//! system. Every command is written to the log; secrets only ever travel
//! through stdin or file descriptors.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::fixup::{self, LUKS_NAME, NewIds};
use super::plan::{Plan, Purpose, human};
use crate::client::{Client, RestoreKey, RestoreRequest};
use crate::layout::{Layout, SYSTEM_ARCHIVE};
use crate::secrets::Secret;

pub const TARGET_ROOT: &str = "/mnt/nestegg-target";
const TOP_LEVEL: &str = "/mnt/nestegg-btrfs-top";

pub struct Runner {
    pub log: Box<dyn Write + Send>,
    /// Prefix for `chroot` and the paths below the target (tests).
    pub target: PathBuf,
}

impl Runner {
    pub fn new(log: Box<dyn Write + Send>) -> Runner {
        Runner {
            log,
            target: PathBuf::from(TARGET_ROOT),
        }
    }

    fn note(&mut self, line: &str) {
        let _ = writeln!(self.log, "{line}");
    }

    fn command(&mut self, program: &str, args: &[&str], input: Option<&[u8]>) -> Result<String> {
        self.note(&format!("$ {program} {}", args.join(" ")));
        let mut child = Command::new(program)
            .args(args)
            .env("LC_ALL", "C")
            .env("DEBIAN_FRONTEND", "noninteractive")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("cannot run {program}"))?;
        if let Some(bytes) = input {
            child.stdin.take().context("no stdin")?.write_all(bytes)?;
        }
        let out = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        for l in stdout.lines().chain(stderr.lines()) {
            self.note(&format!("  {l}"));
        }
        if !out.status.success() {
            let last = stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .or_else(|| stdout.lines().last())
                .unwrap_or("")
                .trim()
                .to_string();
            bail!("{program} failed ({}): {last}", out.status);
        }
        Ok(stdout)
    }

    pub fn run(&mut self, program: &str, args: &[&str]) -> Result<()> {
        self.command(program, args, None).map(|_| ())
    }

    pub fn capture(&mut self, program: &str, args: &[&str]) -> Result<String> {
        self.command(program, args, None)
    }

    pub fn run_with_input(&mut self, program: &str, args: &[&str], input: &[u8]) -> Result<()> {
        self.command(program, args, Some(input)).map(|_| ())
    }

    fn chroot(&mut self, args: &[&str]) -> Result<()> {
        let root = self.target.to_string_lossy().into_owned();
        let mut all = vec![root.as_str()];
        all.extend_from_slice(args);
        self.run("chroot", &all)
    }

    fn in_target(&self, path: &str) -> PathBuf {
        self.target.join(path.trim_start_matches('/'))
    }
}

/// Everything needed to restore, gathered by the wizard or the CLI.
pub struct Job<'a> {
    pub plan: &'a Plan,
    pub layout: &'a Layout,
    pub client: &'a Client<'a>,
    pub snapshot: &'a str,
    pub source_target: crate::state::Target,
    /// How to read `system.pxar`, as decided by the key check
    /// ([`super::Protection::restore_key`]).
    pub key: &'a RestoreKey,
    pub luks_passphrase: Option<&'a Secret>,
    /// Package selections of the backed-up system (from the inventory).
    pub dpkg_selections: Option<&'a str>,
}

pub struct Report {
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
}

fn installed(selections: Option<&str>, package: &str) -> Option<bool> {
    let text = selections?;
    Some(text.lines().any(|l| {
        let mut f = l.split_whitespace();
        let name = f.next().unwrap_or("");
        let name = name.split(':').next().unwrap_or(name);
        name == package && f.next() == Some("install")
    }))
}

/// Checks that need the backup but must happen before the disk is wiped.
pub fn preflight(plan: &Plan, selections: Option<&str>) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    // The restore runs update-initramfs/update-grub of a Debian-based system.
    // Without a dpkg package list it is something else (Fedora, Arch, ...):
    // refuse now, before the disk is erased.
    if selections.is_none_or(|s| s.trim().is_empty()) {
        bail!(
            "the backup has no Debian package list (dpkg); the rescue stick only restores \
             Debian-based systems such as Debian, Ubuntu and Linux Mint"
        );
    }
    if plan.encrypt {
        let initramfs = [
            installed(selections, "cryptsetup-initramfs"),
            installed(selections, "dracut"),
        ];
        if initramfs.contains(&Some(true)) {
            // The restored system can unlock the disk at boot.
        } else if initramfs.contains(&Some(false)) {
            bail!(
                "encryption was requested, but the backed-up system has no cryptsetup-initramfs; \
                 it could not unlock the disk at boot. Restore without encryption instead."
            );
        }
    }
    let grub_pkgs: &[&str] = if plan.uefi {
        &[
            "grub-efi-amd64",
            "grub-efi-amd64-bin",
            "grub-efi-amd64-signed",
        ]
    } else {
        &["grub-pc", "grub-pc-bin"]
    };
    if let Some(sel) = selections
        && !grub_pkgs
            .iter()
            .any(|p| installed(Some(sel), p) == Some(true))
    {
        warnings.push(format!(
            "the backed-up system has no GRUB package for {} mode; the boot loader of the rescue \
             stick is used. Install {} after the first start.",
            if plan.uefi { "UEFI" } else { "BIOS" },
            grub_pkgs[0]
        ));
    }
    Ok(warnings)
}

/// Check the live fallback installer before erasing a disk. A firmware-mode
/// change can need it even when the source system has its own GRUB.
pub fn check_live_grub() -> Result<()> {
    let available = std::env::var_os("PATH").is_some_and(|paths| live_grub_available(&paths));
    anyhow::ensure!(
        available,
        "the live system lacks grub-install (grub2-common); use a complete rescue image. Nothing was erased"
    );
    Ok(())
}

fn live_grub_available(paths: &std::ffi::OsStr) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(paths).any(|dir| {
        fs::metadata(dir.join("grub-install"))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// The rescue stick's mkfs tools are newer than many restored systems.
/// These ext4 features are off so that GRUB 2.06 and e2fsprogs 1.46
/// (Ubuntu 22.04, Linux Mint 21, Debian 12) can read and check the disk.
const EXT4_COMPAT: &str = "^metadata_csum_seed,^orphan_file";
/// xfs features of Linux 5.10 LTS (no nrext64/bigtime/exchange/parent).
const XFS_COMPAT_PROFILE: &str = "/usr/share/xfsprogs/mkfs/lts_5.10.conf";
const XFS_COMPAT_OPTIONS: &str = "options=/usr/share/xfsprogs/mkfs/lts_5.10.conf";

/// `/dev/disk/by-id/...` name of a disk (stays valid when the order of the
/// disks changes), or the plain path when there is none.
fn stable_disk_name(disk: &str) -> String {
    let Ok(target) = fs::canonicalize(disk) else {
        return disk.to_string();
    };
    let mut names: Vec<PathBuf> = fs::read_dir("/dev/disk/by-id")
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| fs::canonicalize(p).is_ok_and(|t| t == target))
                .collect()
        })
        .unwrap_or_default();
    // Prefer the names grub-pc's own installer shows (ata-, nvme-, scsi-).
    names.sort_by_key(|p| {
        let n = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        (
            n.starts_with("wwn-") || n.contains("-eui.") || n.starts_with("nvme-nvme."),
            n,
        )
    });
    names
        .first()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| disk.to_string())
}

fn mounted_sources() -> Vec<(String, String)> {
    fs::read_to_string("/proc/self/mountinfo")
        .map(|t| {
            crate::layout::parse_mountinfo(&t)
                .into_iter()
                .map(|m| (m.source, m.point))
                .collect()
        })
        .unwrap_or_default()
}

fn belongs_to_disk(dev: &str, disk: &str) -> bool {
    dev == disk
        || dev.strip_prefix(disk).is_some_and(|rest| {
            rest.trim_start_matches('p')
                .chars()
                .all(|c| c.is_ascii_digit())
        })
}

/// Refuse a disk that is in use (mounted, swap, rescue medium).
pub fn check_disk_free(disk: &str) -> Result<()> {
    for (source, point) in mounted_sources() {
        if belongs_to_disk(&source, disk) {
            bail!("{source} is mounted at {point}; this disk is in use");
        }
    }
    if let Ok(swaps) = fs::read_to_string("/proc/swaps") {
        for (_, path, _) in crate::layout::parse_proc_swaps(&swaps) {
            if belongs_to_disk(&path, disk) {
                bail!("{path} is used as swap; run swapoff first");
            }
        }
    }
    // LVM, LUKS or md RAID on the disk (e.g. auto-activated volume groups).
    let kname = disk.rsplit('/').next().unwrap_or(disk);
    let sys = Path::new("/sys/block").join(kname);
    let mut nodes = vec![sys.clone()];
    if let Ok(entries) = fs::read_dir(&sys) {
        nodes.extend(
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(kname))
                .map(|e| e.path()),
        );
    }
    for node in nodes {
        if let Ok(holders) = fs::read_dir(node.join("holders"))
            && let Some(h) = holders.flatten().next()
        {
            bail!(
                "{} is in use by {} (LVM, encryption or RAID is active on it); \
                 deactivate it first or choose another disk",
                node.file_name().unwrap_or_default().to_string_lossy(),
                h.file_name().to_string_lossy()
            );
        }
    }
    if Path::new(TARGET_ROOT).exists()
        && mounted_sources()
            .iter()
            .any(|(_, p)| p.starts_with(TARGET_ROOT))
    {
        bail!("{TARGET_ROOT} is still mounted from an earlier attempt; reboot the rescue system");
    }
    Ok(())
}

/// What is on the disk now (shown before the confirmation). Whether it
/// holds data is decided by `guard::assess`.
pub fn disk_contents(disk: &str) -> String {
    std::process::Command::new("lsblk")
        .args(["-n", "-o", "NAME,SIZE,FSTYPE,LABEL", disk])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn wait_for(path: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !Path::new(path).exists() {
        if Instant::now() > deadline {
            bail!("{path} did not appear");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn uuid_of(runner: &mut Runner, dev: &str) -> Result<String> {
    let out = runner.capture("blkid", &["-s", "UUID", "-o", "value", dev])?;
    let uuid = out.trim().to_string();
    if uuid.is_empty() {
        bail!("no UUID found on {dev}");
    }
    Ok(uuid)
}

/// Unmounts everything below the target and closes the LUKS mapping.
fn cleanup(runner: &mut Runner, plan: &Plan) {
    let target = runner.target.to_string_lossy().into_owned();
    let _ = runner.run("sync", &[]);
    if runner.run("umount", &["-R", &target]).is_err() {
        let _ = runner.run("umount", &["-R", "-l", &target]);
    }
    let _ = runner.run("umount", &[TOP_LEVEL]);
    if plan.encrypt && Path::new(&format!("/dev/mapper/{LUKS_NAME}")).exists() {
        let _ = runner.run("cryptsetup", &["close", LUKS_NAME]);
    }
}

pub fn execute(job: &Job<'_>, runner: &mut Runner, say: &mut dyn FnMut(&str)) -> Result<Report> {
    let result = execute_inner(job, runner, say);
    say("Unmounting the restored system ...");
    cleanup(runner, job.plan);
    result
}

fn execute_inner(job: &Job<'_>, runner: &mut Runner, say: &mut dyn FnMut(&str)) -> Result<Report> {
    let plan = job.plan;
    let mut notes = Vec::new();
    let mut warnings = Vec::new();
    check_disk_free(&plan.disk)?;
    let target = runner.target.clone();
    let t = target.to_string_lossy().into_owned();

    // 1. Partition table.
    say(&format!("Creating partitions on {} ...", plan.disk));
    runner.run("wipefs", &["--all", "--force", &plan.disk])?;
    runner.run("sgdisk", &["--zap-all", &plan.disk])?;
    let args = super::plan::sgdisk_args(plan);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    runner.run("sgdisk", &args)?;
    // The kernel must see the NEW partitions, not stale device nodes.
    if runner.run("partprobe", &[&plan.disk]).is_err() {
        runner
            .run("blockdev", &["--rereadpt", &plan.disk])
            .context("the kernel did not accept the new partition table; reboot and try again")?;
    }
    let _ = runner.run("udevadm", &["settle"]);
    for p in &plan.partitions {
        let path = plan.part_path(p.number);
        wait_for(&path)?;
        // Old signatures at the same offsets must not confuse blkid.
        runner.run("wipefs", &["--all", "--force", &path])?;
    }

    // 2. Encryption and file systems.
    let root_part = plan.part_path(
        plan.partition(Purpose::Root)
            .context("no root partition")?
            .number,
    );
    let mut ids = NewIds::default();
    let root_dev = if plan.encrypt {
        let pass = job
            .luks_passphrase
            .context("no passphrase for the encryption")?;
        say("Encrypting the root partition (LUKS2) ...");
        runner.run_with_input(
            "cryptsetup",
            &[
                "luksFormat",
                "--type",
                "luks2",
                "--batch-mode",
                "--key-file",
                "-",
                &root_part,
            ],
            pass.as_bytes(),
        )?;
        runner.run_with_input(
            "cryptsetup",
            &["open", "--key-file", "-", &root_part, LUKS_NAME],
            pass.as_bytes(),
        )?;
        ids.luks_uuid = Some(
            runner
                .capture("cryptsetup", &["luksUUID", &root_part])?
                .trim()
                .to_string(),
        );
        format!("/dev/mapper/{LUKS_NAME}")
    } else {
        root_part.clone()
    };
    say("Creating file systems ...");
    match plan.root_fstype.as_str() {
        "btrfs" => runner.run("mkfs.btrfs", &["-f", "-L", "root", &root_dev])?,
        "xfs" => {
            let mut args = vec!["-f", "-L", "root"];
            if Path::new(XFS_COMPAT_PROFILE).exists() {
                args.extend(["-c", XFS_COMPAT_OPTIONS]);
            }
            args.push(&root_dev);
            runner.run("mkfs.xfs", &args)?
        }
        _ => runner.run(
            "mkfs.ext4",
            &["-F", "-q", "-O", EXT4_COMPAT, "-L", "root", &root_dev],
        )?,
    }
    let root_uuid = uuid_of(runner, &root_dev)?;
    ids.fs_uuid.insert("/".into(), root_uuid.clone());
    let mut extra_mounts: Vec<(String, String, Vec<String>)> = Vec::new(); // (point, device, mount args)
    for p in &plan.partitions {
        let path = plan.part_path(p.number);
        match p.purpose {
            Purpose::Esp => {
                runner.run("mkfs.vfat", &["-F", "32", "-n", "EFI", &path])?;
                let point = plan.esp_point.clone().unwrap_or_else(|| "/boot/efi".into());
                ids.fs_uuid.insert(point.clone(), uuid_of(runner, &path)?);
                extra_mounts.push((point, path, vec!["-o".into(), "umask=0077".into()]));
            }
            Purpose::Boot => {
                runner.run(
                    "mkfs.ext4",
                    &["-F", "-q", "-O", EXT4_COMPAT, "-L", "boot", &path],
                )?;
                ids.fs_uuid.insert("/boot".into(), uuid_of(runner, &path)?);
                extra_mounts.push(("/boot".into(), path, vec![]));
            }
            Purpose::Swap => {
                runner.run("mkswap", &["-L", "swap", &path])?;
                ids.swap_uuid = Some(uuid_of(runner, &path)?);
            }
            Purpose::BiosBoot | Purpose::Root => {}
        }
    }

    // 3. btrfs subvolumes.
    let mut root_opts: Vec<String> = Vec::new();
    if plan.root_fstype == "btrfs" {
        fs::create_dir_all(TOP_LEVEL)?;
        runner.run("mount", &["-o", "subvolid=5", &root_dev, TOP_LEVEL])?;
        let mut names: Vec<&str> = plan.root_subvolume.iter().map(String::as_str).collect();
        names.extend(plan.subvolumes.iter().map(|s| s.name.as_str()));
        names.sort_by_key(|n| (n.matches('/').count(), *n));
        names.dedup();
        for name in names {
            let path = Path::new(TOP_LEVEL).join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            runner.run("btrfs", &["subvolume", "create", &path.to_string_lossy()])?;
        }
        runner.run("umount", &[TOP_LEVEL])?;
        let mut opts: Vec<String> = plan.root_compression.iter().cloned().collect();
        if let Some(name) = &plan.root_subvolume {
            opts.push(format!("subvol={name}"));
        }
        if !opts.is_empty() {
            root_opts = vec!["-o".into(), opts.join(",")];
        }
        for sv in &plan.subvolumes {
            ids.fs_uuid.insert(sv.point.clone(), root_uuid.clone());
            extra_mounts.push((
                sv.point.clone(),
                root_dev.clone(),
                vec!["-o".into(), format!("subvol={}", sv.name)],
            ));
        }
    }

    // 4. Mount the new system.
    fs::create_dir_all(&target)?;
    let mut args: Vec<&str> = root_opts.iter().map(String::as_str).collect();
    args.extend([root_dev.as_str(), t.as_str()]);
    runner.run("mount", &args)?;
    extra_mounts.sort_by_key(|(point, _, _)| point.matches('/').count());
    for (point, dev, opts) in &extra_mounts {
        let dir = runner.in_target(point);
        fs::create_dir_all(&dir)?;
        let dir = dir.to_string_lossy().into_owned();
        let mut args: Vec<&str> = opts.iter().map(String::as_str).collect();
        args.extend([dev.as_str(), dir.as_str()]);
        runner.run("mount", &args)?;
    }

    // 5. Restore.
    say(&format!(
        "Restoring {SYSTEM_ARCHIVE}.pxar from {} (about {}). This takes a while ...",
        job.snapshot,
        human(plan.needed_bytes)
    ));
    let archive = format!("{SYSTEM_ARCHIVE}.pxar");
    let stop = Arc::new(AtomicBool::new(false));
    let ticker = {
        let stop = stop.clone();
        let target = target.clone();
        let total = plan.needed_bytes;
        let start = Instant::now();
        let baseline = used_bytes(&target);
        std::thread::spawn(move || {
            let mut last = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                if last.elapsed() < Duration::from_secs(5) {
                    continue;
                }
                last = Instant::now();
                let done = used_bytes(&target).saturating_sub(baseline);
                let pct = (done * 100).checked_div(total).unwrap_or(0).min(99);
                eprint!(
                    "\r  {} written (about {pct} %), {} min    ",
                    human(done),
                    start.elapsed().as_secs() / 60
                );
            }
            eprintln!();
        })
    };
    let request = RestoreRequest {
        snapshot: job.snapshot,
        archive: &archive,
        target: &target,
        patterns: &[],
        key: job.key.duplicate(),
    };
    let restored = job.client.restore_into_existing(&request, &mut *runner.log);
    stop.store(true, Ordering::Relaxed);
    let _ = ticker.join();
    restored.context("restore failed")?;
    say("Files restored.");

    // 6. Swap files.
    for f in &plan.swap_files {
        let path = runner.in_target(&f.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let _ = fs::remove_file(&path);
        let p = path.to_string_lossy().into_owned();
        let size = f.size_bytes.to_string();
        if plan.root_fstype == "btrfs" {
            runner.run("btrfs", &["filesystem", "mkswapfile", "--size", &size, &p])?;
        } else {
            runner.run("fallocate", &["-l", &size, &p])?;
            runner.run("chmod", &["600", &p])?;
            runner.run("mkswap", &[&p])?;
        }
        notes.push(format!("created swap file {}", f.path));
    }

    // 7. Configuration of the new disk.
    say("Adjusting fstab, crypttab and boot configuration ...");
    let fstab_path = runner.in_target("/etc/fstab");
    let fstab = fs::read_to_string(&fstab_path).unwrap_or_default();
    let map = ids.uuid_map(job.layout);
    let (text, n) = fixup::rewrite_fstab(&fstab, plan, &ids, &map);
    write_with_backup(&target, &fstab_path, &fstab, &text)?;
    notes.extend(n);
    let crypttab_path = runner.in_target("/etc/crypttab");
    let crypttab = fs::read_to_string(&crypttab_path).unwrap_or_default();
    if !crypttab.is_empty() || plan.encrypt {
        let (text, n) = fixup::rewrite_crypttab(&crypttab, job.layout, &ids);
        write_with_backup(&target, &crypttab_path, &crypttab, &text)?;
        notes.extend(n);
    }
    let mut files = vec![
        runner.in_target("/etc/default/grub"),
        runner.in_target("/etc/kernel/cmdline"),
        runner.in_target("/etc/initramfs-tools/conf.d/resume"),
    ];
    for dir in ["/etc/default/grub.d", "/etc/initramfs-tools/conf.d"] {
        if let Ok(entries) = fs::read_dir(runner.in_target(dir)) {
            files.extend(entries.flatten().map(|e| e.path()));
        }
    }
    // GRUB stubs on the ESP (`search.fs_uuid <uuid>`): grub-install rewrites
    // only the one of the directory it installs into.
    if let Some(esp) = &plan.esp_point
        && let Ok(entries) = fs::read_dir(runner.in_target(esp).join("EFI"))
    {
        files.extend(
            entries
                .flatten()
                .map(|e| e.path().join("grub.cfg"))
                .filter(|p| p.is_file()),
        );
    }
    files.sort();
    files.dedup();
    for f in files {
        let Ok(old) = fs::read_to_string(&f) else {
            continue;
        };
        let new = fixup::replace_uuids(&old, &map, &ids);
        if new != old {
            write_with_backup(&target, &f, &old, &new)?;
            notes.push(format!(
                "updated device references in {}",
                Path::new("/")
                    .join(f.strip_prefix(&target).unwrap_or(&f))
                    .display()
            ));
        }
    }

    let network = super::network::fix_network(&target, Path::new("/sys/class/net"))?;
    notes.extend(network);

    // 8. Boot loader and initramfs inside the restored system.
    say("Installing the boot loader ...");
    bind_mounts(runner, plan.uefi)?;
    if runner.in_target("/usr/sbin/update-initramfs").exists() {
        runner.chroot(&["update-initramfs", "-u", "-k", "all"])?;
    } else if runner.in_target("/usr/bin/dracut").exists() {
        runner.chroot(&["dracut", "--regenerate-all", "--force"])?;
    } else {
        warnings.push("no initramfs tool found in the restored system".into());
    }
    install_grub(runner, plan, job.layout, &mut warnings)?;
    if runner.in_target("/usr/sbin/update-grub").exists() {
        runner.chroot(&["update-grub"])?;
    } else {
        runner.chroot(&["grub-mkconfig", "-o", "/boot/grub/grub.cfg"])?;
    }

    let paths = crate::config::Paths {
        config_file: runner.in_target("/etc/nestegg/config.toml"),
        credentials_dir: runner.in_target("/etc/nestegg/credentials"),
        state_dir: runner.in_target("/var/lib/nestegg"),
        runtime_dir: runner.in_target("/run/nestegg"),
        systemctl: "systemctl".into(),
    };
    crate::state::Store::for_target(&paths, job.source_target)
        .record_restore(job.snapshot, chrono::Utc::now())?;
    // flock locks are kernel state and cannot survive a restore. Remove the
    // captured runtime files too; no process uses this offline target tree.
    for file in [paths.lock_file(), paths.state_dir.join(".current.lock")] {
        match fs::remove_file(file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    notes.push(format!("Status: restored from {}; its snapshot time is the last successful backup time; cleared stale current run", job.snapshot));

    // 9. Report inside the restored system.
    let report = format!(
        "Restored by the nestegg rescue system on {}\nSnapshot: {}\n\n{}\n{}\n",
        crate::state::format_time(chrono::Utc::now()),
        job.snapshot,
        plan.describe(),
        notes
            .iter()
            .chain(warnings.iter())
            .map(|n| format!("- {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let report_path = runner.in_target("/var/lib/nestegg/rescue-report.txt");
    if let Some(parent) = report_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&report_path, report).context("cannot write rescue report")?;
    Ok(Report { notes, warnings })
}

fn used_bytes(path: &Path) -> u64 {
    nix::sys::statvfs::statvfs(path)
        .map(|st| {
            let f = st.fragment_size();
            st.blocks().saturating_sub(st.blocks_free()) * f
        })
        .unwrap_or(0)
}

/// Where the previous versions of rewritten files are kept inside the
/// restored system. Not next to the file: initramfs-tools reads every file
/// in `conf.d`, so a copy there would bring the old setting back.
const BACKUP_DIR: &str = "var/lib/nestegg/rescue-backup";

fn write_with_backup(target: &Path, path: &Path, old: &str, new: &str) -> Result<()> {
    if new == old {
        return Ok(());
    }
    if !old.is_empty() {
        let rel = path.strip_prefix(target).unwrap_or(path);
        let rel = rel.strip_prefix("/").unwrap_or(rel);
        let backup = target.join(BACKUP_DIR).join(rel);
        if let Some(parent) = backup.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&backup, old)?;
    }
    fs::write(path, new).with_context(|| format!("cannot write {}", path.display()))
}

fn bind_mounts(runner: &mut Runner, uefi: bool) -> Result<()> {
    let base = runner.target.clone();
    let t = |p: &str| base.join(p).to_string_lossy().into_owned();
    let (dev, proc_, sys, run) = (t("dev"), t("proc"), t("sys"), t("run"));
    for d in [&dev, &proc_, &sys, &run] {
        fs::create_dir_all(d)?;
    }
    runner.run("mount", &["--rbind", "/dev", &dev])?;
    runner.run("mount", &["--make-rslave", &dev])?;
    runner.run("mount", &["-t", "proc", "proc", &proc_])?;
    runner.run("mount", &["--rbind", "/sys", &sys])?;
    runner.run("mount", &["--make-rslave", &sys])?;
    runner.run("mount", &["-t", "tmpfs", "tmpfs", &run])?;
    let udev = t("run/udev");
    fs::create_dir_all(&udev)?;
    fs::create_dir_all(t("run/cryptsetup"))?;
    if Path::new("/run/udev").exists() {
        runner.run("mount", &["--bind", "/run/udev", &udev])?;
    }
    if uefi && !Path::new("/sys/firmware/efi/efivars").join(".").exists() {
        runner.note("note: efivars not available, boot entry cannot be written");
    }
    Ok(())
}

/// Directory below `EFI/` on the restored ESP that holds the distribution's
/// boot files (signed GRUB images look for their configuration there).
fn efi_bootloader_id(esp: &Path, layout: &Layout) -> String {
    let expected = match layout.os_id.as_str() {
        "linuxmint" | "pop" | "elementary" | "zorin" => "ubuntu".to_string(),
        "" => "nestegg".to_string(),
        id => id.to_string(),
    };
    if let Ok(entries) = fs::read_dir(esp.join("EFI")) {
        let mut found: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter(|e| {
                ["grubx64.efi", "shimx64.efi"]
                    .iter()
                    .any(|f| e.path().join(f).exists())
            })
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.eq_ignore_ascii_case("boot"))
            .collect();
        found.sort();
        // Several distributions on the ESP (dual boot, leftovers): the one
        // of the restored system wins.
        if found.contains(&expected) {
            return expected;
        }
        if let Some(first) = found.into_iter().next() {
            return first;
        }
    }
    expected
}

fn install_grub(
    runner: &mut Runner,
    plan: &Plan,
    layout: &Layout,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if plan.uefi {
        let esp = plan.esp_point.clone().unwrap_or_else(|| "/boot/efi".into());
        let id = efi_bootloader_id(&runner.in_target(&esp), layout);
        let id_arg = format!("--bootloader-id={id}");
        let esp_arg = format!("--efi-directory={esp}");
        if runner.in_target("/usr/lib/grub/x86_64-efi").exists() {
            let base = [
                "grub-install",
                "--target=x86_64-efi",
                &esp_arg,
                &id_arg,
                "--recheck",
            ];
            let attempts: [&[&str]; 3] = [
                &["--force-extra-removable"],
                &[],
                &["--removable", "--no-nvram"],
            ];
            let mut last = None;
            for extra in attempts {
                let mut args: Vec<&str> = base.to_vec();
                args.extend_from_slice(extra);
                match runner.chroot(&args) {
                    Ok(()) => {
                        if extra.contains(&"--no-nvram") {
                            warnings.push(
                                "no firmware boot entry could be written; the disk boots through \
                                 the fallback path EFI/BOOT/BOOTX64.EFI"
                                    .into(),
                            );
                        }
                        return Ok(());
                    }
                    Err(e) => last = Some(e),
                }
            }
            return Err(last.unwrap().context("grub-install failed"));
        }
        // The restored system has no UEFI GRUB: use the rescue stick's.
        let esp_dir = runner.in_target(&esp).to_string_lossy().into_owned();
        let boot_dir = runner.in_target("/boot").to_string_lossy().into_owned();
        runner.run(
            "grub-install",
            &[
                "--target=x86_64-efi",
                &format!("--efi-directory={esp_dir}"),
                &format!("--boot-directory={boot_dir}"),
                "--removable",
                "--no-nvram",
            ],
        )?;
        warnings.push(
            "installed GRUB from the rescue stick; install grub-efi-amd64 after the first start"
                .into(),
        );
    } else if runner.in_target("/usr/lib/grub/i386-pc").exists() {
        runner.chroot(&["grub-install", "--target=i386-pc", "--recheck", &plan.disk])?;
        // grub-pc's package scripts reinstall GRUB on every update onto the
        // devices recorded in debconf; those still name the OLD disk.
        if runner.in_target("/usr/bin/debconf-set-selections").exists() {
            let dev = stable_disk_name(&plan.disk);
            let line = format!("grub-pc grub-pc/install_devices multiselect {dev}\n");
            let root = runner.target.to_string_lossy().into_owned();
            if runner
                .run_with_input(
                    "chroot",
                    &[&root, "/usr/bin/debconf-set-selections"],
                    line.as_bytes(),
                )
                .is_err()
            {
                warnings.push(
                    "could not record the boot disk for GRUB updates; run \
                     'dpkg-reconfigure grub-pc' after the first start"
                        .into(),
                );
            }
        }
    } else {
        let boot_dir = runner.in_target("/boot").to_string_lossy().into_owned();
        runner.run(
            "grub-install",
            &[
                "--target=i386-pc",
                &format!("--boot-directory={boot_dir}"),
                &plan.disk,
            ],
        )?;
        warnings.push(
            "installed GRUB from the rescue stick; install grub-pc after the first start".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_grub_must_be_an_executable_file_before_restore() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        assert!(!live_grub_available(dir.path().as_os_str()));
        let binary = dir.path().join("grub-install");
        fs::write(&binary, "fixture").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!live_grub_available(dir.path().as_os_str()));
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(live_grub_available(dir.path().as_os_str()));
    }

    #[test]
    fn disk_membership() {
        assert!(belongs_to_disk("/dev/sda1", "/dev/sda"));
        assert!(belongs_to_disk("/dev/nvme0n1p2", "/dev/nvme0n1"));
        assert!(!belongs_to_disk("/dev/sdab1", "/dev/sda"));
        assert!(!belongs_to_disk("/dev/nvme0n10p1", "/dev/nvme0n1"));
    }

    #[test]
    fn package_checks() {
        let sel = "grub-efi-amd64\t\t\t\tinstall\ncryptsetup-initramfs\t\t\tinstall\nlibc6:amd64\tinstall\n";
        assert_eq!(installed(Some(sel), "libc6"), Some(true));
        assert_eq!(installed(Some(sel), "grub-pc"), Some(false));
        let plan = crate::rescue::plan::make_plan(
            &crate::layout::tests::btrfs_layout(),
            &crate::rescue::plan::Target {
                disk: "/dev/vda".into(),
                size_bytes: 200 << 30,
                uefi: true,
                archive_bytes: None,
            },
            true,
        )
        .unwrap();
        assert!(preflight(&plan, Some(sel)).unwrap().is_empty());
        assert!(preflight(&plan, Some("grub-pc install\n")).is_err());
        assert!(preflight(&plan, None).is_err());
    }

    #[test]
    fn backups_go_outside_config_dirs() {
        let root = tempfile::tempdir().unwrap();
        let conf = root.path().join("etc/initramfs-tools/conf.d");
        fs::create_dir_all(&conf).unwrap();
        let file = conf.join("resume");
        fs::write(&file, "RESUME=UUID=old\n").unwrap();
        write_with_backup(root.path(), &file, "RESUME=UUID=old\n", "RESUME=none\n").unwrap();
        assert_eq!(fs::read_dir(&conf).unwrap().count(), 1);
        let kept = root
            .path()
            .join(BACKUP_DIR)
            .join("etc/initramfs-tools/conf.d/resume");
        assert_eq!(fs::read_to_string(kept).unwrap(), "RESUME=UUID=old\n");
    }

    #[test]
    fn bootloader_id_from_esp() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("EFI/BOOT")).unwrap();
        fs::write(dir.path().join("EFI/BOOT/BOOTX64.EFI"), "").unwrap();
        fs::create_dir_all(dir.path().join("EFI/ubuntu")).unwrap();
        fs::write(dir.path().join("EFI/ubuntu/shimx64.efi"), "").unwrap();
        let layout = crate::layout::tests::btrfs_layout();
        assert_eq!(efi_bootloader_id(dir.path(), &layout), "ubuntu");
        fs::create_dir_all(dir.path().join("EFI/debian")).unwrap();
        fs::write(dir.path().join("EFI/debian/grubx64.efi"), "").unwrap();
        assert_eq!(efi_bootloader_id(dir.path(), &layout), "ubuntu");
        assert_eq!(
            efi_bootloader_id(Path::new("/nonexistent"), &layout),
            "ubuntu"
        );
    }
}
