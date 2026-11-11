//! Privileged integration coverage, run by tests/stick/create.sh on owned loops.
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use nestegg::stick::launcher::{LauncherOptions, LocalPbs};

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().expect("start client");
    assert!(
        out.status.success(),
        "client failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
fn token(handle: &LocalPbs) -> Vec<u8> {
    let mut v = Vec::new();
    File::open(format!(
        "/proc/self/fd/{}",
        handle.token_secret().as_raw_fd()
    ))
    .unwrap()
    .read_to_end(&mut v)
    .unwrap();
    assert!(!v.is_empty());
    v
}

#[test]
#[ignore = "requires root and Linux open_tree/move_mount"]
fn pinned_directory_survives_path_swap_across_mount_namespaces() {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, symlink};
    use std::os::unix::process::CommandExt;
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let target = fixture.path().join("target");
    let replacement = fixture.path().join("replacement");
    for p in [&source, &target, &replacement] {
        fs::create_dir(p).unwrap();
    }
    let pinned = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
        .open(&source)
        .unwrap();
    let inode = pinned.metadata().unwrap().ino();
    fs::rename(&source, fixture.path().join("original")).unwrap();
    symlink(&replacement, &source).unwrap();
    use std::os::fd::FromRawFd;
    let tree = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_open_tree,
            pinned.as_raw_fd(),
            c"".as_ptr(),
            nix::libc::AT_EMPTY_PATH as u32
                | nix::libc::OPEN_TREE_CLONE
                | nix::libc::O_CLOEXEC as u32,
        )
    };
    assert!(tree >= 0, "open_tree: {}", std::io::Error::last_os_error());
    let tree = unsafe { File::from_raw_fd(tree as i32) };
    let fd = tree.as_raw_fd();
    use std::os::unix::ffi::OsStrExt;
    let target_c = std::ffi::CString::new(target.as_os_str().as_bytes()).unwrap();
    let mut command = Command::new("stat");
    command.args(["-c", "%i"]).arg(&target);
    unsafe {
        command.pre_exec(move || {
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
                    fd,
                    c"".as_ptr(),
                    nix::libc::AT_FDCWD,
                    target_c.as_ptr(),
                    nix::libc::MOVE_MOUNT_F_EMPTY_PATH,
                ) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    assert_eq!(run(&mut command).trim(), inode.to_string());
}

#[test]
#[ignore = "requires root, trusted PBS squashfs, mounted disposable ext4 and official client"]
fn isolated_pbs_backup_restore_and_restart() {
    let env = |key| PathBuf::from(std::env::var_os(key).expect(key));
    let options = LauncherOptions {
        data_mount: env("NESTEGG_STICK_DATA"),
        runtime_dir: env("NESTEGG_STICK_RUNTIME"),
    };
    let client = env("NESTEGG_STICK_CLIENT");
    let fixture = tempfile::tempdir_in(&options.runtime_dir).unwrap();
    let source = fixture.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("report.txt"),
        b"standard PBS snapshot survives updates\n",
    )
    .unwrap();
    let key = fixture.path().join("key.json");
    run(Command::new(&client)
        .args(["key", "create", "--kdf", "none"])
        .arg(&key));
    let listeners = run(Command::new("ss").args(["-H", "-lnt", "sport = :8007"]));
    // A common async integration starts in a short-lived blocking thread and
    // moves the handle to the controller. PBS must outlive the start thread.
    let worker_options = LauncherOptions {
        data_mount: options.data_mount.clone(),
        runtime_dir: options.runtime_dir.clone(),
    };
    // Root-owned FAT-looking cache files are still rejected by filesystem type.
    // Verify the cache bytes before any privileged helper or data operation.
    let cache = Path::new(nestegg::stick::launcher::IMAGE_PATH);
    let checksum = cache.with_extension("squashfs.sha256");
    let saved_hash = fs::read(&checksum).unwrap();
    fs::write(&checksum, "0".repeat(64)).unwrap();
    assert!(
        LocalPbs::start(&options)
            .err()
            .unwrap()
            .to_string()
            .contains("SHA-256 mismatch")
    );
    fs::write(&checksum, saved_hash).unwrap();
    let first = std::thread::spawn(move || LocalPbs::start(&worker_options))
        .join()
        .unwrap()
        .unwrap();
    assert_eq!(
        listeners,
        run(Command::new("ss").args(["-H", "-lnt", "sport = :8007"]))
    );
    let secret = token(&first);
    let fp = first.fingerprint().to_owned();
    assert!(
        LocalPbs::start(&options).is_err(),
        "must refuse concurrent datastore access"
    );
    let mut shell = first.client_command(Path::new("/bin/sh")).unwrap();
    let mnt = run(shell.args(["-c", "readlink /proc/self/ns/mnt"]));
    assert_eq!(
        mnt.trim(),
        fs::read_link("/proc/self/ns/mnt")
            .unwrap()
            .to_str()
            .unwrap()
    );
    let mut links = first.client_command(Path::new("ip")).unwrap();
    assert_eq!(run(links.args(["-o", "link", "show"])).lines().count(), 1);
    let mut namespaces = first.client_command(Path::new("/bin/sh")).unwrap();
    let net = run(namespaces.args(["-c", "readlink /proc/self/ns/net"]));
    assert_ne!(
        net.trim(),
        fs::read_link("/proc/self/ns/net")
            .unwrap()
            .to_str()
            .unwrap()
    );
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 2;
    let mut backup = first.client_command(&client).unwrap();
    run(backup
        .arg("backup")
        .arg(format!("files.pxar:{}", source.display()))
        .args([
            "--backup-id",
            "stick-test",
            "--backup-time",
            &epoch.to_string(),
            "--keyfile",
        ])
        .arg(&key));
    let mut list = first.client_command(&client).unwrap();
    let value: serde_json::Value = serde_json::from_str(&run(list.args([
        "snapshot",
        "list",
        "--output-format",
        "json",
    ])))
    .unwrap();
    assert_eq!(value.as_array().unwrap().len(), 1);
    first.stop().unwrap();
    use std::os::unix::fs::MetadataExt;
    let anchor = fs::metadata(options.data_mount.join("nestegg")).unwrap();
    assert_eq!(anchor.uid(), 0);
    assert_eq!(anchor.mode() & 0o022, 0);
    let store = fs::metadata(options.data_mount.join("nestegg/store")).unwrap();
    assert_eq!(
        store.mode() & 0o777,
        0o755,
        "PBS requires this mode for reuse"
    );
    let second = LocalPbs::start(&options).unwrap();
    assert_ne!(fp, second.fingerprint(), "TLS certificate must be fresh");
    assert!(secret != token(&second), "token secret must be fresh");
    let timestamp = chrono::DateTime::from_timestamp(epoch as i64, 0)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%SZ");
    let restored = fixture.path().join("restored");
    let mut restore = second.client_command(&client).unwrap();
    run(restore
        .args([
            "restore",
            &format!("host/stick-test/{timestamp}"),
            "files.pxar",
        ])
        .arg(&restored)
        .arg("--keyfile")
        .arg(&key));
    assert_eq!(
        fs::read(source.join("report.txt")).unwrap(),
        fs::read(restored.join("report.txt")).unwrap()
    );
    // Reuse the exact owner identity after discarding ALL server config.
    let mut backup = second.client_command(&client).unwrap();
    run(backup
        .arg("backup")
        .arg(format!("files.pxar:{}", source.display()))
        .args([
            "--backup-id",
            "stick-test",
            "--backup-time",
            &(epoch + 1).to_string(),
            "--keyfile",
        ])
        .arg(&key));
    second.stop().unwrap();
    let mut entries = fs::read_dir(&options.runtime_dir).unwrap();
    assert!(entries.all(|e| !e.unwrap().file_name().to_string_lossy().starts_with("pbs-")));
}

#[test]
#[ignore = "requires root and mount namespaces; uses only a temporary tmpfs"]
fn desktop_mounts_receive_host_changes_without_propagating_back() {
    const MODE: &str = "NESTEGG_PROPAGATION_TEST";
    let executable = std::env::current_exe().unwrap();
    let test = "desktop_mounts_receive_host_changes_without_propagating_back";
    let wait_for = |path: &Path| {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < until,
                "missing {}",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    match std::env::var(MODE).as_deref() {
        Ok("child") => {
            let base = PathBuf::from(std::env::var_os("NESTEGG_PROPAGATION_DIR").unwrap());
            nestegg::stick::target::isolate_mounts().unwrap();
            run(Command::new("mount")
                .args(["-t", "tmpfs", "tmpfs"])
                .arg(base.join("outgoing")));
            fs::write(base.join("ready"), "ready").unwrap();
            wait_for(&base.join("incoming/source"));
            assert_eq!(
                fs::read(base.join("incoming/source")).unwrap(),
                b"host mount"
            );
            fs::write(base.join("received"), "received").unwrap();
            wait_for(&base.join("done"));
        }
        Ok("parent") => {
            let work = tempfile::tempdir().unwrap();
            let base = work.path();
            run(Command::new("mount")
                .args(["-t", "tmpfs", "tmpfs"])
                .arg(base));
            run(Command::new("mount").arg("--make-rshared").arg(base));
            fs::create_dir(base.join("incoming")).unwrap();
            fs::create_dir(base.join("outgoing")).unwrap();
            let mut child = Command::new(&executable)
                .args(["--ignored", "--exact", test])
                .env(MODE, "child")
                .env("NESTEGG_PROPAGATION_DIR", base)
                .spawn()
                .unwrap();
            wait_for(&base.join("ready"));
            assert!(
                !Command::new("mountpoint")
                    .arg("-q")
                    .arg(base.join("outgoing"))
                    .status()
                    .unwrap()
                    .success()
            );
            run(Command::new("mount")
                .args(["-t", "tmpfs", "tmpfs"])
                .arg(base.join("incoming")));
            fs::write(base.join("incoming/source"), "host mount").unwrap();
            wait_for(&base.join("received"));
            fs::write(base.join("done"), "done").unwrap();
            assert!(child.wait().unwrap().success());
            run(Command::new("umount").arg(base.join("incoming")));
            run(Command::new("umount").arg(base));
        }
        _ => {
            run(Command::new("unshare")
                .args(["--mount", "--propagation", "private"])
                .arg(executable)
                .args(["--ignored", "--exact", test])
                .env(MODE, "parent"));
        }
    }
}
