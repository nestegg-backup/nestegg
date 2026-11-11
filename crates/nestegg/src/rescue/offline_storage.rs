//! Read-only source devices, with journal recovery confined to RAM snapshots.
//! No source executable is run. Every acquired resource has an unwind path.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::layout::{BlockDev, Layout, MountRecord, SwapRecord};
use crate::secrets::{self, Secret};
use crate::stick::capture;

pub fn readonly_options(fstype: &str) -> Result<&'static str> {
    match fstype {
        "ext2" => Ok("ro,nodev,nosuid,noexec"),
        "ext3" | "ext4" => Ok("ro,noload,nodev,nosuid,noexec"),
        "xfs" => Ok("ro,norecovery,nodev,nosuid,noexec"),
        "btrfs" => Ok("ro,rescue=nologreplay,nodev,nosuid,noexec"),
        "vfat" => Ok("ro,nodev,nosuid,noexec"),
        _ => bail!("unsupported offline filesystem: {fstype}"),
    }
}

pub fn needs_recovery(fstype: &str, header: &str) -> bool {
    match fstype {
        "ext2" | "ext3" | "ext4" => {
            let mut clean = false;
            for line in header.lines() {
                if let Some(state) = line.strip_prefix("Filesystem state:") {
                    clean = state.trim() == "clean";
                }
                if line.starts_with("Filesystem features:")
                    && line.split_whitespace().any(|s| s == "needs_recovery")
                {
                    return true;
                }
            }
            !clean
        }
        // Conservatively recover their logs through RAM even when they might
        // be clean; no filesystem-specific on-disk format is reimplemented.
        "xfs" | "btrfs" => true,
        _ => false,
    }
}

fn internal_journal(header: &str) -> bool {
    if header
        .lines()
        .any(|l| l.starts_with("Journal device:") || l.starts_with("Journal UUID:"))
    {
        return false;
    }
    let journal = header
        .lines()
        .filter_map(|l| l.strip_prefix("Filesystem features:"))
        .any(|l| l.split_whitespace().any(|f| f == "has_journal"));
    !journal
        || header
            .lines()
            .filter_map(|l| l.strip_prefix("Journal inode:"))
            .any(|l| l.trim().parse::<u64>().is_ok_and(|n| n > 0))
}

fn single_btrfs_device(header: &str) -> bool {
    let counts: Vec<_> = header
        .lines()
        .filter_map(|l| {
            let mut fields = l.split_whitespace();
            (fields.next() == Some("num_devices"))
                .then(|| fields.next().and_then(|v| v.parse::<u64>().ok()))
                .flatten()
        })
        .collect();
    counts == [1]
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Read a file as if `root` were /. Absolute symlinks stay inside that root;
/// magic links and escapes are rejected by the kernel, including races.
pub fn read_root(root: &Path, path: &str) -> Result<Vec<u8>> {
    let dir = File::open(root)?;
    let path = std::ffi::CString::new(path)?;
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let how = OpenHow {
        flags: (nix::libc::O_RDONLY | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK) as u64,
        mode: 0,
        resolve: 0x10 | 0x02, // RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS
    };
    let fd = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_openat2,
            dir.as_raw_fd(),
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("cannot read file in offline root");
    }
    let file = unsafe { File::from_raw_fd(fd as i32) };
    ensure!(
        file.metadata()?.is_file(),
        "offline inventory input is not a regular file"
    );
    let mut out = Vec::new();
    file.take(32 * 1024 * 1024 + 1).read_to_end(&mut out)?;
    ensure!(
        out.len() <= 32 * 1024 * 1024,
        "offline inventory input is too large"
    );
    Ok(out)
}

pub fn text_root(root: &Path, path: &str) -> Result<String> {
    Ok(String::from_utf8(read_root(root, path)?)?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FstabEntry {
    pub spec: String,
    pub point: String,
    pub fstype: String,
    pub subvol: Option<String>,
    pub bind: bool,
}

pub fn parse_fstab(text: &str) -> Result<Vec<FstabEntry>> {
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
        let f: Vec<_> = line.split_whitespace().collect();
        if f.is_empty() {
            continue;
        }
        ensure!(f.len() >= 4, "invalid offline fstab");
        let point = f[1]
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\134", "\\");
        if f[2] == "swap" {
            entries.push(FstabEntry {
                spec: f[0].into(),
                point,
                fstype: "swap".into(),
                subvol: None,
                bind: false,
            });
            continue;
        }
        ensure!(
            seen.insert(point.clone()),
            "duplicate system mount in offline fstab"
        );
        let bind = f[3].split(',').any(|o| matches!(o, "bind" | "rbind"));
        ensure!(
            !bind || !system_point(&point),
            "offline system bind mounts need manual inspection"
        );
        ensure!(
            !f[3].split(',').any(|o| o.starts_with("subvolid=")),
            "this image cannot safely back up subvolid= mounts; nothing was written to the computer"
        );
        ensure!(
            Path::new(&point).is_absolute()
                && Path::new(&point)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_) | Component::RootDir)),
            "invalid offline mountpoint"
        );
        let subvol = f[3]
            .split(',')
            .find_map(|o| o.strip_prefix("subvol="))
            .map(str::to_owned);
        if let Some(sv) = &subvol {
            ensure!(
                !sv.is_empty()
                    && Path::new(sv)
                        .components()
                        .all(|c| matches!(c, Component::Normal(_) | Component::RootDir))
                    && !sv.contains(','),
                "invalid btrfs subvolume"
            );
        }
        entries.push(FstabEntry {
            spec: f[0].into(),
            point,
            fstype: f[2].into(),
            subvol,
            bind,
        });
    }
    entries.sort_by_key(|e| e.point.len());
    Ok(entries)
}

fn system_point(p: &str) -> bool {
    matches!(
        p,
        "/" | "/boot"
            | "/boot/efi"
            | "/efi"
            | "/home"
            | "/root"
            | "/opt"
            | "/srv"
            | "/usr"
            | "/usr/local"
            | "/var"
            | "/var/cache"
            | "/var/lib"
            | "/var/log"
    )
}

fn role_record(entry: &FstabEntry, source: &str) -> MountRecord {
    MountRecord {
        point: entry.point.clone(),
        fstype: entry.fstype.clone(),
        source: source.into(),
        fsroot: entry
            .subvol
            .as_ref()
            .map(|s| format!("/{}", s.trim_start_matches('/')))
            .unwrap_or_else(|| "/".into()),
        options: "ro".into(),
        super_options: String::new(),
        uuid: None,
        size_bytes: None,
        used_bytes: None,
        crypt_uuid: None,
        on_lvm: false,
        bind: entry.bind,
    }
}

fn entry_role(
    entry: &FstabEntry,
    dev: Option<&BlockDev>,
    root: &BlockDev,
) -> crate::layout::MountRole {
    let root_entry = FstabEntry {
        spec: root.path.clone(),
        point: "/".into(),
        fstype: root.fstype.clone().unwrap_or_default(),
        subvol: None,
        bind: false,
    };
    crate::layout::mount_role(
        &role_record(entry, dev.map(|d| d.path.as_str()).unwrap_or(&entry.spec)),
        &role_record(&root_entry, &root.path),
    )
}

fn resolve_device(devices: &[BlockDev], spec: &str) -> Result<BlockDev> {
    let found: Vec<_> = devices
        .iter()
        .filter(|d| {
            if let Some(uuid) = spec.strip_prefix("UUID=") {
                d.uuid
                    .as_deref()
                    .is_some_and(|u| u.eq_ignore_ascii_case(uuid))
            } else {
                Path::new(spec).is_absolute() && device_identity(spec) == device_identity(&d.path)
            }
        })
        .collect();
    ensure!(
        found.len() == 1,
        "filesystem is missing or ambiguous on selected disk"
    );
    Ok(found[0].clone())
}

struct Snapshot {
    name: String,
    cow_loop: String,
    created: bool,
}

pub struct Sources {
    work: tempfile::TempDir,
    pub root: PathBuf,
    pub disk_bytes: u64,
    disk: String,
    mounts: Vec<PathBuf>,
    snapshots: BTreeMap<String, Snapshot>,
    crypt: Vec<String>,
    vgs: Vec<(String, String)>,
    readonly: Vec<String>,
    devices: Vec<BlockDev>,
    mounted: Vec<(FstabEntry, BlockDev)>,
    skipped: Vec<(FstabEntry, BlockDev)>,
    ram: PathBuf,
    ram_mounted: bool,
}

impl Sources {
    pub fn open(
        disk: &super::Disk,
        mut passphrase: impl FnMut(&str) -> Result<Secret>,
    ) -> Result<Self> {
        ensure!(!disk.in_use, "source disk is in use");
        super::exec::check_disk_free(&disk.path)?;
        ensure!(
            crate::layout::parse_proc_swaps(&fs::read_to_string("/proc/swaps")?).is_empty(),
            "offline backup requires no active swap (RAM journals must stay in RAM)"
        );
        secrets::create_private_dir(&super::work_dir())?;
        let work = tempfile::Builder::new()
            .prefix("offline-")
            .tempdir_in(super::work_dir())?;
        let root = work.path().join("root");
        let ram = work.path().join("ram");
        fs::create_dir(&root)?;
        fs::create_dir(&ram)?;
        let mut s = Self {
            work,
            root,
            disk_bytes: disk.size_bytes,
            disk: disk.path.clone(),
            mounts: Vec::new(),
            snapshots: BTreeMap::new(),
            crypt: Vec::new(),
            vgs: Vec::new(),
            readonly: Vec::new(),
            devices: Vec::new(),
            mounted: Vec::new(),
            skipped: Vec::new(),
            ram,
            ram_mounted: false,
        };
        // No source mount happens until the disk and every partition are
        // kernel-read-only. The flag is restored only after all holders close.
        for item in &disk.items {
            if capture(Command::new("blockdev").args(["--getro", &item.path]))? == "0" {
                capture(Command::new("blockdev").args(["--setro", &item.path]))?;
                s.readonly.push(item.path.clone());
            }
        }
        capture(
            Command::new("mount")
                .args([
                    "-t",
                    "tmpfs",
                    "-o",
                    "noswap,nodev,nosuid,noexec,mode=0700,size=1G",
                    "tmpfs",
                ])
                .arg(&s.ram),
        )?;
        s.ram_mounted = true;
        for item in &disk.items {
            if item.fstype != "crypto_LUKS" {
                continue;
            }
            let name = format!("nestegg-offline-{}-{}", std::process::id(), s.crypt.len());
            let key = passphrase(&item.path)?;
            let mut fds = secrets::FdPassing::new();
            fds.add(&key, 10)?;
            let mut cmd = Command::new("cryptsetup");
            cmd.args([
                "open",
                "--readonly",
                "--key-file",
                "/proc/self/fd/10",
                &item.path,
                &name,
            ]);
            fds.apply(&mut cmd);
            capture(&mut cmd)?;
            s.crypt.push(name);
        }
        let all = crate::layout::lsblk()?;
        let mut allowed: BTreeSet<String> = disk
            .items
            .iter()
            .map(|i| device_identity(&i.path))
            .collect();
        for name in &s.crypt {
            allowed.insert(
                fs::canonicalize(format!("/dev/mapper/{name}"))?
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        let pvs: Vec<_> = all
            .iter()
            .filter(|d| {
                allowed.contains(&device_identity(&d.path))
                    && d.fstype.as_deref() == Some("LVM2_member")
            })
            .map(|d| d.path.clone())
            .collect();
        if !pvs.is_empty() {
            let devices = pvs.join(",");
            let json = capture(Command::new("lvs").args([
                "--reportformat",
                "json",
                "--devices",
                &devices,
                "-o",
                "vg_name,lv_name,segtype,lv_attr",
            ]))?;
            let json: serde_json::Value = serde_json::from_str(&json)?;
            let lvs = json["report"][0]["lv"]
                .as_array()
                .context("missing LVM report")?;
            let mut groups = BTreeSet::new();
            for lv in lvs {
                ensure!(
                    lv["segtype"] == "linear",
                    "only ordinary linear LVM volumes can be backed up offline"
                );
                let name = lv["vg_name"].as_str().context("missing VG name")?;
                ensure!(valid_id(name), "invalid LVM group name");
                ensure!(
                    lv["lv_attr"]
                        .as_str()
                        .is_some_and(|a| a.as_bytes().get(4) == Some(&b'-')),
                    "LVM source is already active"
                );
                groups.insert(name.to_owned());
            }
            for vg in groups {
                let config = lvm_config(&vg);
                // read_only_volume_list changes activation permissions without
                // writing LV permissions into the VG metadata (unlike -pr).
                s.vgs.push((vg.clone(), devices.clone()));
                capture(Command::new("vgchange").args([
                    "-ay",
                    "--monitor",
                    "n",
                    "--devices",
                    &devices,
                    "--config",
                    &config,
                    &vg,
                ]))?;
            }
            for d in crate::layout::lsblk()? {
                if d.kind == "lvm"
                    && d.pkname.as_ref().is_some_and(|p| {
                        all.iter()
                            .any(|a| allowed.contains(&device_identity(&a.path)) && &a.kname == p)
                    })
                {
                    allowed.insert(device_identity(&d.path));
                }
            }
        }
        s.devices = crate::layout::lsblk()?
            .into_iter()
            .filter(|d| allowed.contains(&device_identity(&d.path)))
            .collect();
        for dev in &s.devices {
            ensure!(
                capture(Command::new("blockdev").args(["--getro", &dev.path]))? == "1",
                "source mapping is writable"
            );
        }
        Ok(s)
    }

    fn filesystem(&mut self, dev: &BlockDev) -> Result<String> {
        if let Some(snapshot) = self.snapshots.get(&dev.path) {
            return Ok(format!("/dev/mapper/{}", snapshot.name));
        }
        let kind = dev.fstype.as_deref().context("source filesystem unknown")?;
        readonly_options(kind)?;
        let header = if matches!(kind, "ext2" | "ext3" | "ext4") {
            capture(Command::new("dumpe2fs").args(["-h", &dev.path]))?
        } else {
            String::new()
        };
        if matches!(kind, "ext2" | "ext3" | "ext4") {
            ensure!(
                internal_journal(&header),
                "external ext journals are unsupported: recovery must never open another source device"
            );
            ensure!(
                kind != "ext2" || !needs_recovery(kind, &header),
                "an unclean ext2 filesystem has no journal to recover; refusing an inconsistent backup"
            );
        }
        if kind == "btrfs" {
            let superblock = capture(Command::new("btrfs").args([
                "inspect-internal",
                "dump-super",
                "-f",
                &dev.path,
            ]))?;
            ensure!(
                single_btrfs_device(&superblock),
                "multi-device btrfs is unsupported: every recovery write must stay on the RAM snapshot"
            );
        }
        if !needs_recovery(kind, &header) {
            return Ok(dev.path.clone());
        }
        eprintln!("Journal recovery in RAM: {}", super::printable(&dev.path));
        let name = format!(
            "nestegg-ram-{}-{}",
            std::process::id(),
            self.snapshots.len()
        );
        let cow = self.ram.join(format!("{}.cow", self.snapshots.len()));
        let file = File::create(&cow)?;
        file.set_len(512 * 1024 * 1024)?;
        let cow_loop = capture(Command::new("losetup").args(["--find", "--show"]).arg(&cow))?;
        self.snapshots.insert(
            dev.path.clone(),
            Snapshot {
                name: name.clone(),
                cow_loop: cow_loop.clone(),
                created: false,
            },
        );
        let sectors = capture(Command::new("blockdev").args(["--getsz", &dev.path]))?;
        let sectors: u64 = sectors.parse()?;
        ensure!(sectors > 0, "empty filesystem");
        let table = format!("0 {sectors} snapshot {} {cow_loop} N 8", dev.path);
        capture(Command::new("dmsetup").args(["create", &name, "--table", &table]))?;
        self.snapshots.get_mut(&dev.path).unwrap().created = true;
        // Replay on a writable *snapshot*, exposed as a read-only mount. Then
        // mount it again with the filesystem's explicit no-replay option.
        let probe = self
            .work
            .path()
            .join(format!("replay-{}", self.snapshots.len()));
        fs::create_dir(&probe)?;
        capture(
            Command::new("mount")
                .args(["-t", kind, "-o", "ro,nodev,nosuid,noexec"])
                .arg(format!("/dev/mapper/{name}"))
                .arg(&probe),
        )?;
        self.mounts.push(probe.clone());
        capture(Command::new("umount").arg(&probe))?;
        self.mounts.pop();
        self.check_snapshots()?;
        eprintln!(
            "RAM journal snapshot: {}",
            capture(Command::new("dmsetup").args(["status", &name]))?
        );
        Ok(format!("/dev/mapper/{name}"))
    }

    pub fn candidates(&mut self) -> Result<Vec<(BlockDev, Option<String>)>> {
        let devs: Vec<_> = self
            .devices
            .iter()
            .filter(|d| {
                matches!(
                    d.fstype.as_deref(),
                    Some("ext2" | "ext3" | "ext4" | "btrfs" | "xfs")
                )
            })
            .cloned()
            .collect();
        let mut roots = Vec::new();
        for dev in devs {
            let source = self.filesystem(&dev)?;
            let probe = self.work.path().join("probe");
            fs::create_dir_all(&probe)?;
            capture(
                Command::new("mount")
                    .args([
                        "-t",
                        dev.fstype.as_deref().unwrap(),
                        "-o",
                        readonly_options(dev.fstype.as_deref().unwrap())?,
                    ])
                    .arg(source)
                    .arg(&probe),
            )?;
            self.mounts.push(probe.clone());
            if read_root(&probe, "/etc/fstab").is_ok() && read_root(&probe, "/etc/hostname").is_ok()
            {
                roots.push((dev.clone(), None));
            } else if dev.fstype.as_deref() == Some("btrfs")
                && read_root(&probe, "/@/etc/fstab").is_ok()
                && read_root(&probe, "/@/etc/hostname").is_ok()
            {
                roots.push((dev.clone(), Some("@".into())));
            }
            capture(Command::new("umount").arg(&probe))?;
            self.mounts.pop();
        }
        Ok(roots)
    }

    fn mount_entry(&mut self, entry: FstabEntry, dev: BlockDev) -> Result<()> {
        ensure!(
            entry.fstype == dev.fstype.as_deref().unwrap_or_default()
                || (entry.fstype == "ext4"
                    && matches!(dev.fstype.as_deref(), Some("ext2" | "ext3"))),
            "fstab filesystem differs from source"
        );
        let target = if entry.point == "/" {
            self.root.clone()
        } else {
            self.root.join(entry.point.trim_start_matches('/'))
        };
        // Never create a mountpoint on a source filesystem or follow a symlink
        // outside it. Missing mountpoints are an incomplete system, not success.
        let real = fs::canonicalize(&target).context("offline mountpoint missing")?;
        ensure!(
            real.starts_with(&self.root),
            "offline mountpoint escapes root"
        );
        let source = self.filesystem(&dev)?;
        let mut options = readonly_options(dev.fstype.as_deref().unwrap())?.to_owned();
        if let Some(subvol) = &entry.subvol {
            options.push_str(&format!(",subvol={subvol}"));
        }
        capture(
            Command::new("mount")
                .args(["-t", dev.fstype.as_deref().unwrap(), "-o", &options])
                .arg(source)
                .arg(&real),
        )?;
        self.mounts.push(real);
        self.mounted.push((entry, dev));
        Ok(())
    }

    pub fn mount_system(&mut self, root: BlockDev, subvol: Option<String>) -> Result<()> {
        self.mount_entry(
            FstabEntry {
                spec: root.path.clone(),
                point: "/".into(),
                fstype: root.fstype.clone().unwrap(),
                subvol,
                bind: false,
            },
            root.clone(),
        )?;
        let entries = parse_fstab(&text_root(&self.root, "/etc/fstab")?)?;
        for entry in entries {
            if entry.point == "/" || entry.fstype == "swap" {
                continue;
            }
            let dev = self.resolve(&entry.spec).ok();
            ensure!(
                dev.is_some()
                    || root.fstype.as_deref() != Some("btrfs")
                    || entry.fstype != "btrfs"
                    || entry.spec.starts_with("UUID="),
                "cannot identify this btrfs mount on the selected disk; refusing to omit files"
            );
            match entry_role(&entry, dev.as_ref(), &root) {
                crate::layout::MountRole::System => {
                    let dev = dev.with_context(|| format!("system filesystem {} is missing; refusing an incomplete offline backup", entry.point))?;
                    ensure!(
                        entry.fstype != "btrfs" || dev.path != root.path || entry.subvol.is_some(),
                        "non-root btrfs mounts of the root device need an explicit subvol= name"
                    );
                    self.mount_entry(entry, dev)?;
                }
                crate::layout::MountRole::EmptySubvolume => {
                    let dev = dev.context("cannot identify skipped root subvolume")?;
                    ensure!(
                        entry.subvol.is_some(),
                        "skipped btrfs mounts need an explicit subvol= name"
                    );
                    self.skipped.push((entry, dev));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn resolve(&self, spec: &str) -> Result<BlockDev> {
        resolve_device(&self.devices, spec)
    }

    pub fn check_snapshots(&self) -> Result<()> {
        for snapshot in self.snapshots.values() {
            let status = capture(Command::new("dmsetup").args(["status", &snapshot.name]))?;
            ensure!(
                status.contains(" snapshot ")
                    && !status.contains("Invalid")
                    && !status.contains("Overflow"),
                "RAM journal snapshot overflowed or became invalid; backup is not complete"
            );
        }
        Ok(())
    }

    pub fn inventory_tree(&self) -> Result<String> {
        capture(Command::new("lsblk").args([
            "--json",
            "-o",
            "NAME,TYPE,SIZE,FSTYPE,UUID,PARTUUID,MOUNTPOINTS,MODEL",
            "--",
            &self.disk,
        ]))
    }

    pub fn layout(&self) -> Result<Layout> {
        let os = crate::layout::parse_os_release(&text_root(&self.root, "/etc/os-release")?);
        let devs = crate::layout::lsblk()?;
        let mountinfo =
            crate::layout::parse_mountinfo(&fs::read_to_string("/proc/self/mountinfo")?);
        let mut mounts = Vec::new();
        for (entry, dev) in &self.mounted {
            let path = self.root.join(entry.point.trim_start_matches('/'));
            let stats = nix::sys::statvfs::statvfs(&path)?;
            let (crypt_uuid, on_lvm, _) = crate::layout::ancestry(&devs, dev);
            mounts.push(MountRecord {
                point: entry.point.clone(),
                fstype: entry.fstype.clone(),
                source: dev.path.clone(),
                fsroot: mountinfo
                    .iter()
                    .rev()
                    .find(|m| Path::new(&m.point) == path)
                    .context("source mount disappeared")?
                    .fsroot
                    .clone(),
                options: "ro".into(),
                super_options: mountinfo
                    .iter()
                    .rev()
                    .find(|m| Path::new(&m.point) == path)
                    .context("source mount disappeared")?
                    .super_options
                    .clone(),
                uuid: dev.uuid.clone(),
                size_bytes: Some(stats.blocks() * stats.fragment_size()),
                used_bytes: Some((stats.blocks() - stats.blocks_free()) * stats.fragment_size()),
                crypt_uuid,
                on_lvm,
                bind: false,
            });
        }
        for (entry, dev) in &self.skipped {
            let mut record = role_record(entry, &dev.path);
            record.uuid = dev.uuid.clone();
            (record.crypt_uuid, record.on_lvm, _) = crate::layout::ancestry(&devs, dev);
            mounts.push(record);
        }
        let mut swap = Vec::new();
        for e in parse_fstab(&text_root(&self.root, "/etc/fstab")?)?
            .into_iter()
            .filter(|e| e.fstype == "swap")
        {
            if let Ok(dev) = self.resolve(&e.spec) {
                swap.push(SwapRecord {
                    kind: "partition".into(),
                    path: dev.path.clone(),
                    size_bytes: dev.size.unwrap_or(0),
                    uuid: dev.uuid.clone(),
                    crypt_uuid: crate::layout::ancestry(&devs, &dev).0,
                });
            } else if e.spec.starts_with('/') {
                // The restored swap file is newly allocated; do not read its
                // contents or ever activate the offline computer's swap.
                let path = self.root.join(e.spec.trim_start_matches('/'));
                if let Ok(md) = fs::symlink_metadata(path)
                    && md.is_file()
                {
                    swap.push(SwapRecord {
                        kind: "file".into(),
                        path: e.spec,
                        size_bytes: md.len(),
                        uuid: None,
                        crypt_uuid: None,
                    });
                }
            }
        }
        let get = |k: &str| os.get(k).cloned().unwrap_or_default();
        let bootloader = if read_root(&self.root, "/boot/grub/grub.cfg").is_ok() {
            "grub"
        } else {
            "unknown"
        };
        let all_points: Vec<String> = mounts.iter().map(|m| m.point.clone()).collect();
        let nested_subvolumes = if mounts.iter().any(|m| m.point == "/" && m.fstype == "btrfs") {
            let list = capture(
                Command::new("btrfs")
                    .args(["subvolume", "list", "-a"])
                    .arg(&self.root),
            )?;
            crate::layout::nested_subvolume_points(&list, &mounts, &all_points)
        } else {
            Vec::new()
        };
        Ok(Layout {
            version: crate::layout::LAYOUT_VERSION,
            boot_mode: if mounts.iter().any(|m| m.fstype == "vfat") {
                "uefi".into()
            } else {
                "bios".into()
            },
            bootloader: bootloader.into(),
            os_id: get("ID"),
            os_version: get("VERSION_ID"),
            os_name: get("PRETTY_NAME"),
            hostname: text_root(&self.root, "/etc/hostname")?.trim().into(),
            root_disk_bytes: Some(self.disk_bytes),
            all_points,
            mounts,
            swap,
            nested_subvolumes,
        })
    }

    pub fn close(mut self) -> Result<()> {
        self.cleanup()
    }

    fn cleanup(&mut self) -> Result<()> {
        while let Some(mount) = self.mounts.last() {
            capture(Command::new("umount").arg(mount))?;
            self.mounts.pop();
        }
        // RAM must stay mounted until COW loops are gone. It is unmounted last.
        while let Some((origin, snap)) = self.snapshots.last_key_value() {
            if snap.created {
                capture(Command::new("dmsetup").args(["remove", &snap.name]))?;
            }
            capture(Command::new("losetup").args(["-d", &snap.cow_loop]))?;
            let origin = origin.clone();
            self.snapshots.remove(&origin);
        }
        if self.ram_mounted {
            capture(Command::new("umount").arg(&self.ram))?;
            self.ram_mounted = false;
        }
        while let Some((vg, devices)) = self.vgs.last() {
            capture(Command::new("vgchange").args([
                "-an",
                "--devices",
                devices,
                "--config",
                &lvm_config(vg),
                vg,
            ]))?;
            self.vgs.pop();
        }
        while let Some(crypt) = self.crypt.last() {
            capture(Command::new("cryptsetup").args(["close", crypt]))?;
            self.crypt.pop();
        }
        while let Some(dev) = self.readonly.last() {
            capture(Command::new("blockdev").args(["--setrw", dev]))?;
            self.readonly.pop();
        }
        Ok(())
    }
}
impl Drop for Sources {
    fn drop(&mut self) {
        if let Err(e) = self.cleanup() {
            self.work.disable_cleanup(true);
            eprintln!("offline source cleanup failed; source remains read-only: {e:#}");
        }
    }
}

fn device_identity(path: &str) -> String {
    fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_owned())
}

fn lvm_config(vg: &str) -> String {
    format!(
        "global {{ metadata_read_only=1 }} activation {{ read_only_volume_list=[\"{vg}\"] monitoring=0 }} backup {{ backup=0 archive=0 }}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn btrfs_scope_includes_database_siblings_and_keeps_snapshots_empty() {
        let root = BlockDev {
            path: "/dev/vda3".into(),
            fstype: Some("btrfs".into()),
            uuid: Some("root".into()),
            ..Default::default()
        };
        let entries = parse_fstab("UUID=root / btrfs subvol=@ 0 0\nUUID=root /var/lib/mysql btrfs subvol=@mysql 0 0\nUUID=root /.snapshots btrfs subvol=@snapshots 0 0\nUUID=root /var/lib/docker btrfs subvol=@docker 0 0\n").unwrap();
        let role = |point| {
            entry_role(
                entries.iter().find(|e| e.point == point).unwrap(),
                Some(&root),
                &root,
            )
        };
        assert_eq!(role("/var/lib/mysql"), crate::layout::MountRole::System);
        assert_eq!(
            role("/.snapshots"),
            crate::layout::MountRole::EmptySubvolume
        );
        assert_eq!(
            role("/var/lib/docker"),
            crate::layout::MountRole::EmptySubvolume
        );
        assert!(parse_fstab("UUID=root /home btrfs subvolid=257 0 0").is_err());
    }

    #[test]
    fn mapper_aliases_and_canonical_devices_have_the_same_identity() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("dm-fixture");
        let alias = dir.path().join("mapper-home");
        fs::write(&device, "fixture").unwrap();
        std::os::unix::fs::symlink(&device, &alias).unwrap();
        assert_eq!(
            device_identity(alias.to_str().unwrap()),
            device_identity(device.to_str().unwrap())
        );
        let mapped = BlockDev {
            path: alias.to_string_lossy().into_owned(),
            ..Default::default()
        };
        assert_eq!(
            resolve_device(std::slice::from_ref(&mapped), device.to_str().unwrap()).unwrap(),
            mapped
        );

        // UUID resolution also remains unique and case-insensitive.
        let dev = BlockDev {
            path: "/dev/mapper/fixture-home".into(),
            uuid: Some("abcd".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_device(std::slice::from_ref(&dev), "UUID=ABCD").unwrap(),
            dev
        );
        assert!(resolve_device(&[dev.clone(), dev], "UUID=abcd").is_err());
    }

    #[test]
    fn recovery_cannot_open_other_computer_disks() {
        assert!(internal_journal(
            "Filesystem features: has_journal extent\nJournal inode: 8\n"
        ));
        assert!(internal_journal("Filesystem features: ext_attr\n"));
        assert!(!internal_journal(
            "Filesystem features: has_journal\nJournal device: 0x0802\n"
        ));
        assert!(!internal_journal(
            "Filesystem features: has_journal\nJournal UUID: fixture\n"
        ));
        assert!(!internal_journal(
            "Filesystem features: has_journal\nJournal inode: 0\n"
        ));
        assert!(single_btrfs_device("num_devices\t\t1\n"));
        assert!(!single_btrfs_device("num_devices\t\t2\n"));
        assert!(!single_btrfs_device(""));
    }

    #[test]
    fn readonly_mounts_explicitly_disable_replay() {
        assert!(readonly_options("ext4").unwrap().contains("noload"));
        assert!(readonly_options("xfs").unwrap().contains("norecovery"));
        assert!(
            readonly_options("btrfs")
                .unwrap()
                .contains("rescue=nologreplay")
        );
        assert!(readonly_options("zfs").is_err());
        assert!(!needs_recovery(
            "ext4",
            "Filesystem state: clean\nFilesystem features: has_journal"
        ));
        assert!(needs_recovery(
            "ext4",
            "Filesystem state: clean\nFilesystem features: has_journal needs_recovery"
        ));
        assert!(needs_recovery("ext4", "Filesystem state: not clean"));
        assert!(needs_recovery("ext4", ""));
        assert!(needs_recovery("btrfs", ""));
    }
    #[test]
    fn offline_fstab_keeps_entries_for_the_shared_scope_decision() {
        let entries = parse_fstab("UUID=a / ext4 defaults 0 1\nUUID=b /home ext4 defaults 0 2\nserver:/x /mnt/data nfs defaults 0 0\n/swapfile none swap sw 0 0\nUUID=c /.snapshots btrfs subvol=@snapshots 0 0\n").unwrap();
        assert_eq!(entries.len(), 5);
        assert!(parse_fstab("UUID=a / ext4 defaults 0 1\nUUID=b / ext4 defaults 0 1").is_err());
        assert!(parse_fstab("/data /home none bind 0 0").is_err());
        assert!(parse_fstab("UUID=a / btrfs subvol=../escape 0 0").is_err());
    }
    #[test]
    fn root_file_reads_confine_absolute_symlinks() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("etc")).unwrap();
        fs::create_dir(d.path().join("usr")).unwrap();
        fs::write(d.path().join("usr/os-release"), b"source OS").unwrap();
        std::os::unix::fs::symlink("/usr/os-release", d.path().join("etc/os-release")).unwrap();
        assert_eq!(text_root(d.path(), "/etc/os-release").unwrap(), "source OS");
        std::os::unix::fs::symlink("/etc/shadow", d.path().join("escape")).unwrap();
        assert!(read_root(d.path(), "/escape").is_err());
        assert!(read_root(d.path(), "/etc").is_err());
    }
    #[test]
    fn backup_ids_cannot_escape_key_directory() {
        for s in ["", ".", "..", "../other", "line\nbreak", "a/b"] {
            assert!(!valid_id(s));
        }
        assert!(valid_id("desktop.example-1"));
    }
}
