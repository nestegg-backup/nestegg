//! Privileged PBS launcher. The fixed desktop cache supplies the squashfs and
//! a mounted ext4 data partition. Only its `nestegg/` directory is persistent.
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{ChildStdout, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nix::sys::memfd::{MFdFlags, memfd_create};

use super::{capture, private_dir, trusted_file, trusted_path, valid_sha256};

pub const IMAGE_PATH: &str = "/var/lib/nestegg/stick/pbs.squashfs";

/// AF_INET6 is needed even when clients connect to IPv4 loopback.
pub fn preflight() -> Result<()> {
    // SAFETY: socket has no pointer arguments and the returned descriptor is closed.
    let fd = unsafe {
        nix::libc::socket(
            nix::libc::AF_INET6,
            nix::libc::SOCK_STREAM | nix::libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(socket_error(std::io::Error::last_os_error()));
    }
    unsafe {
        nix::libc::close(fd);
    }
    if !nix::unistd::Uid::effective().is_root() {
        bail!("local PBS requires the privileged root helper; rootless startup is unsupported");
    }
    Ok(())
}

fn socket_error(error: std::io::Error) -> anyhow::Error {
    if matches!(
        error.raw_os_error(),
        Some(nix::libc::EAFNOSUPPORT | nix::libc::EPROTONOSUPPORT)
    ) {
        anyhow::anyhow!(
            "stick target unsupported: kernel IPv6 socket support is unavailable ({error}); PBS requires AF_INET6, including for IPv4 loopback. Host settings were not changed"
        )
    } else {
        anyhow::anyhow!("stick IPv6 socket preflight failed: {error}")
    }
}

pub struct LauncherOptions {
    /// Existing ext4 mount, with nodev,nosuid,noexec. Only nestegg/ is exposed.
    pub data_mount: PathBuf,
    /// Root-owned private runtime parent, normally /run/nestegg/stick.
    pub runtime_dir: PathBuf,
}

/// Holds the helper and network namespace alive. Dropping it kills PID 1 in the
/// private PID namespace and all descendants; mounts/loop leases then disappear.
pub struct LocalPbs {
    supervisor: Supervisor,
    runtime: Option<tempfile::TempDir>,
    network: Option<File>,
    token: File,
    fingerprint: String,
    _datastore_lock: nix::fcntl::Flock<File>,
}

/// Linux parent-death signals follow the spawning THREAD. Keep that thread
/// alive with the handle, even when start() runs on a temporary async worker.
struct Supervisor {
    id: u32,
    control: Option<std::sync::mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<Result<()>>>,
}

impl Supervisor {
    fn start(mut command: Command) -> Result<(Self, ChildStdout)> {
        use std::os::unix::process::CommandExt;
        let parent = std::process::id();
        // SAFETY: only async-signal-safe syscalls before exec. The stable
        // supervisor thread's death kills unshare and its private PID 1.
        unsafe {
            command.pre_exec(move || {
                if nix::libc::prctl(nix::libc::PR_SET_PDEATHSIG, nix::libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if nix::libc::getppid() as u32 != parent {
                    return Err(std::io::Error::other("launcher parent exited"));
                }
                Ok(())
            });
        }
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (control, stop) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("stick-pbs-supervisor".into())
            .spawn(move || -> Result<()> {
                let mut child = match command.spawn() {
                    Ok(child) => child,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return Ok(());
                    }
                };
                let stdout = child
                    .stdout
                    .take()
                    .expect("supervised command has piped stdout");
                if ready_tx.send(Ok((child.id(), stdout))).is_ok() {
                    let _ = stop.recv();
                }
                if child.try_wait()?.is_none() {
                    child.kill()?;
                }
                child.wait()?;
                Ok(())
            })?;
        match ready_rx.recv() {
            Ok(Ok((id, stdout))) => Ok((
                Self {
                    id,
                    control: Some(control),
                    worker: Some(worker),
                },
                stdout,
            )),
            result => {
                drop(control);
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("PBS supervisor panicked"))??;
                match result {
                    Ok(Err(error)) => Err(error).context("cannot start privileged PBS helper"),
                    _ => bail!("PBS supervisor did not start"),
                }
            }
        }
    }
    fn stop(&mut self) -> Result<()> {
        if self.worker.is_none() {
            return Ok(());
        }
        // unshare's death kills its namespace PID 1 asynchronously. A pidfd
        // lets us wait for that process to release mounts and its children,
        // without ever signalling a possibly recycled PID.
        let namespace_exit = self.namespace_exit_fd();
        self.control.take();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("PBS supervisor panicked"))??;
        }
        if let Some(exit) = namespace_exit? {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let mut poll = nix::libc::pollfd {
                fd: exit.as_raw_fd(),
                events: nix::libc::POLLIN,
                revents: 0,
            };
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    bail!("PBS namespace did not stop; stick is not safe to unplug");
                }
                // SAFETY: poll points to one live stack entry; the owned pidfd
                // remains open throughout this bounded wait.
                let ready = unsafe {
                    nix::libc::poll(&mut poll, 1, remaining.as_millis().min(30_000) as i32)
                };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error).context("cannot wait for PBS namespace shutdown");
                }
                if ready > 0 && poll.revents & nix::libc::POLLIN != 0 {
                    break;
                }
                if ready > 0 {
                    bail!("PBS namespace shutdown descriptor failed");
                }
            }
        }
        Ok(())
    }

    fn namespace_exit_fd(&self) -> Result<Option<File>> {
        let children = match fs::read_to_string(format!("/proc/{0}/task/{0}/children", self.id)) {
            Ok(children) => children,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("cannot inspect PBS namespace process"),
        };
        let Some(pid) = children.split_whitespace().next() else {
            return Ok(None);
        };
        let pid: i32 = pid.parse().context("invalid PBS namespace process ID")?;
        // SAFETY: pidfd_open has no pointer arguments. A successful syscall
        // returns a fresh CLOEXEC descriptor whose ownership is transferred.
        let fd = unsafe { nix::libc::syscall(nix::libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(nix::libc::ESRCH) {
                return Ok(None);
            }
            return Err(error).context("cannot track PBS namespace shutdown");
        }
        Ok(Some(unsafe { File::from_raw_fd(fd as i32) }))
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl LocalPbs {
    pub fn start(options: &LauncherOptions) -> Result<Self> {
        preflight()?;
        use std::os::unix::fs::OpenOptionsExt;
        let image_path = Path::new(IMAGE_PATH);
        trusted_file(image_path)?;
        let checksum_path = Path::new("/var/lib/nestegg/stick/pbs.squashfs.sha256");
        trusted_file(checksum_path)?;
        let expected = fs::read_to_string(checksum_path)?;
        let expected = expected.trim();
        let mut image = fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(image_path)?;
        if !valid_sha256(expected)
            || !super::digest_reader(&mut image)?.eq_ignore_ascii_case(expected)
        {
            bail!("desktop PBS cache SHA-256 mismatch; reinstall from a trusted release ISO");
        }
        Self::start_image(options, image)
    }

    /// Only the image already running under live-boot is trusted here. The
    /// desktop entry point above deliberately retains its fixed-cache policy.
    pub(crate) fn start_live(options: &LauncherOptions) -> Result<Self> {
        preflight()?;
        Self::start_image(options, crate::rescue::source::boot_image()?)
    }

    fn start_image(options: &LauncherOptions, image: File) -> Result<Self> {
        private_dir(&options.runtime_dir)?;
        trusted_path(&options.data_mount)?;
        let data_root = open_directory(&options.data_mount)?;
        let data = options.data_mount.clone();
        let mount: serde_json::Value = serde_json::from_str(&capture(
            Command::new("findmnt")
                .args(["--json", "--mountpoint"])
                .arg(&data)
                .args(["-o", "FSTYPE,OPTIONS"]),
        )?)?;
        let m = &mount["filesystems"][0];
        let opts: Vec<_> = m["options"]
            .as_str()
            .unwrap_or_default()
            .split(',')
            .collect();
        if m["fstype"] != "ext4"
            || !["nodev", "nosuid", "noexec"]
                .iter()
                .all(|o| opts.contains(o))
        {
            bail!("datastore must be an ext4 mount with nodev,nosuid,noexec");
        }
        let store = File::from(
            nix::fcntl::openat(
                &data_root,
                "nestegg",
                nix::fcntl::OFlag::O_RDONLY
                    | nix::fcntl::OFlag::O_DIRECTORY
                    | nix::fcntl::OFlag::O_NOFOLLOW
                    | nix::fcntl::OFlag::O_CLOEXEC,
                nix::sys::stat::Mode::empty(),
            )
            .context("missing or symbolic nestegg/ datastore directory")?,
        );
        validate_directory(&store)?;
        // Lock and bind the same opened inode, never a subsequently resolved path.
        let lock = nix::fcntl::Flock::lock(store, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .map_err(|(_, e)| anyhow::anyhow!("datastore already in use: {e}"))?;
        let pinned_mount = clone_datastore(&lock)?;
        let runtime = tempfile::Builder::new()
            .prefix("pbs-")
            .tempdir_in(&options.runtime_dir)?;
        let pinned_target = runtime.path().join("data");
        fs::create_dir(&pinned_target)?;
        use std::os::unix::ffi::OsStrExt;
        let mount_target = std::ffi::CString::new(pinned_target.as_os_str().as_bytes())?;
        fs::write(runtime.path().join("server.sh"), include_str!("server.sh"))?;
        let log = File::create(runtime.path().join("launcher.log"))?;
        let token = File::from(memfd_create(c"nestegg-stick-token", MFdFlags::MFD_CLOEXEC)?);
        let mut command = Command::new("unshare");
        command
            .args([
                "--pid",
                "--net",
                "--uts",
                "--fork",
                "--kill-child=KILL",
                "bash",
                "-c",
                include_str!("namespace.sh"),
                "--",
            ])
            .arg(runtime.path())
            .arg(format!("/proc/self/fd/{}", image.as_raw_fd()))
            .arg(&pinned_target)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log);
        // Prepare the mount namespace in the child before exec. Clone/attach
        // the validated inode there, then the shell uses only this protected
        // runtime path. No assumption about the caller's executable is needed.
        use std::os::unix::process::CommandExt;
        // SAFETY: only async-signal-safe syscalls with prebuilt arguments.
        unsafe {
            command.pre_exec(move || {
                attach_datastore(&pinned_mount, &mount_target)?;
                if nix::libc::fcntl(image.as_raw_fd(), nix::libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let (supervisor, stdout) = Supervisor::start(command)?;
        let mut handle = Self {
            supervisor,
            runtime: Some(runtime),
            network: None,
            token,
            fingerprint: String::new(),
            _datastore_lock: lock,
        };
        // Readiness has a deadline even if a daemon hangs during startup.
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let r = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = tx.send(r);
        });
        let ready = rx.recv_timeout(Duration::from_secs(180));
        if ready.is_err() {
            let _ = handle.supervisor.stop();
        }
        let _ = reader.join();
        let ready = ready.context("local PBS startup timed out")??;
        #[derive(serde::Deserialize)]
        struct Ready {
            fingerprint: String,
            secret: String,
        }
        let ready_text = crate::secrets::Secret::new(ready.into_bytes());
        let ready: Ready = serde_json::from_slice(ready_text.as_bytes()).map_err(|_| {
            let log = fs::read_to_string(
                handle
                    .runtime
                    .as_ref()
                    .expect("runtime")
                    .path()
                    .join("launcher.log"),
            )
            .unwrap_or_default();
            let tail = log
                .lines()
                .rev()
                .take(32)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::anyhow!(
                "local PBS did not become ready: {}",
                crate::rescue::printable_block(&tail)
            )
        })?;
        if ready.secret.is_empty() || ready.fingerprint.len() != 95 {
            bail!("invalid PBS readiness response");
        }
        let secret = crate::secrets::Secret::new(ready.secret.into_bytes());
        handle.token.write_all(secret.as_bytes())?;
        handle.token.seek(SeekFrom::Start(0))?;
        handle.fingerprint = ready.fingerprint.to_ascii_lowercase();
        handle.network = Some(File::open(format!(
            "/proc/{}/ns/net",
            handle.supervisor.id
        ))?);
        Ok(handle)
    }
    pub fn repository(&self) -> &'static str {
        "nestegg@pbs!stick@127.0.0.1:8007:stick"
    }
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub fn token_secret(&self) -> BorrowedFd<'_> {
        self.token.as_fd()
    }
    pub fn netns(&self) -> BorrowedFd<'_> {
        self.network.as_ref().expect("running PBS").as_fd()
    }
    /// nsenter receives ONLY --net: host mounts, PID namespace and files remain.
    /// The returned command borrows the live handle operationally: keep it alive
    /// until the client exits. Token and fingerprint are supplied without secrets
    /// in argv/environment values. Build a fresh command for each invocation;
    /// each gets an independent token offset.
    pub fn client_command(&self, program: &Path) -> Result<Command> {
        let mut cmd = Command::new("nsenter");
        cmd.arg(format!(
            "--net=/proc/{}/fd/{}",
            std::process::id(),
            self.netns().as_raw_fd()
        ))
        .arg("--")
        .arg(program);
        let token = File::open(format!("/proc/self/fd/{}", self.token.as_raw_fd()))?;
        // A dedicated read descriptor is inherited as fd 9; clear CLOEXEC only
        // in the child, leaving the parent secret descriptor private.
        use std::os::unix::process::CommandExt;
        let fd = token.as_raw_fd();
        unsafe {
            cmd.pre_exec(move || {
                if nix::libc::dup2(fd, 9) < 0 || nix::libc::fcntl(9, nix::libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // Keep source descriptor alive through spawn via a captured File.
        unsafe {
            cmd.pre_exec(move || {
                let _keep_alive = &token;
                Ok(())
            });
        }
        cmd.env("PBS_PASSWORD_FD", "9")
            .env("PBS_FINGERPRINT", self.fingerprint())
            .env("PBS_REPOSITORY", self.repository());
        Ok(cmd)
    }
    /// Run the official manager inside the trusted PBS root. Verification and
    /// GC are synchronous CLI tasks; a successful spawn alone is no evidence.
    pub fn maintenance(&self, args: &[&str]) -> Result<()> {
        self.maintenance_program("/usr/sbin/proxmox-backup-manager", args)
    }

    /// The official debug CLI exposes the snapshot-scoped verify API and
    /// waits for its worker. The manager's verify command only accepts a store.
    pub fn verify_snapshot(&self, backup_id: &str, backup_time: i64) -> Result<()> {
        self.maintenance_program(
            "/usr/sbin/proxmox-backup-debug",
            &[
                "api",
                "create",
                "/admin/datastore/stick/verify",
                "--backup-type",
                "host",
                "--backup-id",
                backup_id,
                "--backup-time",
                &backup_time.to_string(),
                "--ignore-verified",
                "false",
            ],
        )
    }

    fn maintenance_program(&self, program: &str, args: &[&str]) -> Result<()> {
        let runtime = self.runtime.as_ref().context("PBS is stopped")?;
        let mut cmd = Command::new("nsenter");
        cmd.arg(format!("--mount=/proc/{}/ns/mnt", self.supervisor.id))
            .arg(format!("--net=/proc/{}/ns/net", self.supervisor.id))
            .arg("--")
            .arg("chroot")
            .arg(runtime.path().join("ram/root"))
            .arg(program)
            .args(args)
            // The official CLI authenticates against the ephemeral local
            // server and waits for its task; never let the caller switch it
            // into direct API-code mode (which lacks worker initialization).
            .env_remove("PROXMOX_DEBUG_API_CODE");
        let mut log = tempfile::tempfile()?;
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?)
            .spawn()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(12 * 3600);
        loop {
            if let Some(status) = child.try_wait()? {
                if status.success() {
                    return Ok(());
                }
                let len = log.metadata()?.len();
                log.seek(SeekFrom::Start(len.saturating_sub(4096)))?;
                let mut tail = String::new();
                std::io::Read::read_to_string(&mut log, &mut tail)?;
                bail!(
                    "PBS maintenance failed: {}",
                    crate::rescue::printable_block(&tail)
                );
            }
            if crate::cancel::requested() || std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("PBS maintenance cancelled or timed out");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn stop(mut self) -> Result<()> {
        self.teardown()
    }
    fn teardown(&mut self) -> Result<()> {
        if self.runtime.is_none() {
            return Ok(());
        }
        self.supervisor.stop()?;
        self.network.take();
        // Namespace destruction closes mount/loop references asynchronously.
        self.runtime.take().expect("runtime").close()?;
        Ok(())
    }
}
impl Drop for LocalPbs {
    fn drop(&mut self) {
        let _ = self.teardown();
    }
}

/// Clone before unshare: directory fds retain their original mount namespace.
/// A detached clone can then be attached in the helper's private namespace.
fn clone_datastore(file: &File) -> Result<File> {
    use std::os::fd::FromRawFd;
    let tree = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_open_tree,
            file.as_raw_fd(),
            c"".as_ptr(),
            nix::libc::AT_EMPTY_PATH as u32
                | nix::libc::OPEN_TREE_CLONE
                | nix::libc::O_CLOEXEC as u32,
        )
    };
    if tree < 0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot clone pinned datastore mount (open_tree required)");
    }
    // SAFETY: open_tree returned a newly owned mount descriptor.
    Ok(unsafe { File::from_raw_fd(tree as i32) })
}

/// Called only after fork, before exec; keep this free of allocation/locks.
fn attach_datastore(tree: &File, target: &std::ffi::CStr) -> std::io::Result<()> {
    // SAFETY: valid descriptors/strings, in the spawning child only.
    unsafe {
        if nix::libc::unshare(nix::libc::CLONE_NEWNS) < 0
            || nix::libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                nix::libc::MS_REC | nix::libc::MS_PRIVATE,
                std::ptr::null(),
            ) < 0
            || nix::libc::syscall(
                nix::libc::SYS_move_mount,
                tree.as_raw_fd(),
                c"".as_ptr(),
                nix::libc::AT_FDCWD,
                target.as_ptr(),
                nix::libc::MOVE_MOUNT_F_EMPTY_PATH,
            ) < 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn open_directory(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
        .open(path)?;
    validate_directory(&file)?;
    Ok(file)
}
fn validate_directory(file: &File) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
        bail!(
            "data root and nestegg/ must be root-owned directories without group/other write permission"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_symlink_and_writable_datastore_directories() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let d = tempfile::tempdir().unwrap();
        let link = d.path().join("link");
        symlink(d.path(), &link).unwrap();
        assert!(open_directory(&link).is_err());
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(open_directory(d.path()).is_err());
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o700)).unwrap();
        if nix::unistd::Uid::effective().is_root() {
            let pinned = open_directory(d.path()).unwrap();
            let moved = d.path().join("moved");
            fs::create_dir(&moved).unwrap();
            let original = pinned.metadata().unwrap();
            use std::os::unix::fs::MetadataExt;
            assert_eq!(original.ino(), fs::metadata(d.path()).unwrap().ino());
        }
    }
    #[test]
    fn unsupported_ipv6_is_distinct_from_resource_failure() {
        assert!(
            socket_error(std::io::Error::from_raw_os_error(nix::libc::EAFNOSUPPORT))
                .to_string()
                .contains("unsupported")
        );
        assert!(
            !socket_error(std::io::Error::from_raw_os_error(nix::libc::EMFILE))
                .to_string()
                .contains("unsupported")
        );
    }
}
