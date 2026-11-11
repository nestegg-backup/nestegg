//! Privileged PBS launcher. The caller supplies a desktop-trusted squashfs and
//! a mounted ext4 data partition. Only its `nestegg/` directory is persistent.
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::process::{ChildStdout, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nix::sys::memfd::{MFdFlags, memfd_create};

use super::{capture, private_dir, trusted_path};

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
    /// Root-owned squashfs installed from a verified release ISO, never the stick.
    pub image: PathBuf,
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
        self.control.take();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("PBS supervisor panicked"))??;
        }
        Ok(())
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
        trusted_path(&options.image)?;
        if !fs::metadata(&options.image)?.is_file() {
            bail!("PBS image must be a regular trusted squashfs");
        }
        private_dir(&options.runtime_dir)?;
        let data = fs::canonicalize(&options.data_mount)?;
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
        let store = data.join("nestegg");
        let meta = fs::symlink_metadata(&store).context("missing nestegg/ datastore directory")?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            bail!("nestegg/ must be a real directory");
        }
        // One local server per mounted datastore, across handles and processes.
        let lock = File::open(&store)?;
        let lock = nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .map_err(|(_, e)| anyhow::anyhow!("datastore already in use: {e}"))?;
        let runtime = tempfile::Builder::new()
            .prefix("pbs-")
            .tempdir_in(&options.runtime_dir)?;
        fs::write(runtime.path().join("server.sh"), include_str!("server.sh"))?;
        let log = File::create(runtime.path().join("launcher.log"))?;
        let token = File::from(memfd_create(c"nestegg-stick-token", MFdFlags::MFD_CLOEXEC)?);
        let mut command = Command::new("unshare");
        command
            .args([
                "--mount",
                "--pid",
                "--net",
                "--uts",
                "--fork",
                "--kill-child=KILL",
                "--propagation",
                "private",
                "bash",
                "-c",
                include_str!("namespace.sh"),
                "--",
            ])
            .arg(runtime.path())
            .arg(&options.image)
            .arg(&store)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log);
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

#[cfg(test)]
mod tests {
    use super::*;
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
