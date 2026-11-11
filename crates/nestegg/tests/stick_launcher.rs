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
#[ignore = "requires root, trusted PBS squashfs, mounted disposable ext4 and official client"]
fn isolated_pbs_backup_restore_and_restart() {
    let env = |key| PathBuf::from(std::env::var_os(key).expect(key));
    let options = LauncherOptions {
        image: env("NESTEGG_STICK_IMAGE"),
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
        image: options.image.clone(),
        data_mount: options.data_mount.clone(),
        runtime_dir: options.runtime_dir.clone(),
    };
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
