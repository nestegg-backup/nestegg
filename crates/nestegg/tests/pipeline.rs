//! End-to-end tests of the run pipeline and the daemon with a fake
//! `proxmox-backup-client`. No PBS and no root needed.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const FAKE_CLIENT: &str = r#"#!/bin/bash
# Fake proxmox-backup-client: records calls, checks secret descriptors.
echo "ARGS $*" >> "$FAKE_LOG"
echo "REPO $PBS_REPOSITORY" >> "$FAKE_LOG"
if [ -n "$PBS_PASSWORD" ]; then echo "LEAK plain password env" >> "$FAKE_LOG"; fi
if [ -n "$PBS_PASSWORD_FD" ]; then
  pw=$(cat <&"$PBS_PASSWORD_FD")
  echo "PW $pw" >> "$FAKE_LOG"
fi
case "$1" in
  backup)
    keyfd=""
    prev=""
    for a in "$@"; do
      if [ "$prev" = "--keyfd" ]; then keyfd="$a"; fi
      prev="$a"
    done
    if [ -n "$keyfd" ]; then
      key=$(cat <&"$keyfd")
      echo "KEY $key" >> "$FAKE_LOG"
    fi
    if [ -n "$FAKE_FAIL_AUTH" ]; then
      echo "Error: authentication failed - invalid credentials" >&2
      exit 255
    fi
    echo "Starting backup: host/test/..."
    echo "home.pxar: had to backup 1 MiB of 2 MiB (compressed 1 MiB) in 0.10 s"
    echo "etc.pxar: had to backup 0 B of 1 KiB (compressed 0 B) in 0.01 s"
    exit 0
    ;;
  snapshot)
    if [ "$2" = "list" ]; then
      echo '[{"backup-type":"host","backup-id":"test","backup-time":1794000000,"files":[{"filename":"home.pxar.didx","crypt-mode":"encrypt"}],"size":2048,"protected":false}]'
    fi
    exit 0
    ;;
  version)
    echo '{"client":{"version":"4.2.10","release":"1"}}'
    exit 0
    ;;
  key)
    # `key show <file> --output-format json`: test key files are JSON
    # objects like {"fp":"aa:01"}; "scrypt" marks a password-protected key.
    if [ "$2" = "show" ]; then
      content=$(cat "$3")
      fp=$(printf '%s' "$content" | sed -n 's/.*"fp":"\([^"]*\)".*/\1/p')
      if [ -z "$fp" ]; then
        echo "Error: unable to parse key file" >&2
        exit 255
      fi
      case "$content" in
        *scrypt*) kdf=scrypt ;;
        *) kdf=none ;;
      esac
      echo "{\"path\":\"$3\",\"kdf\":\"$kdf\",\"fingerprint\":\"$fp\",\"hint\":null}"
    fi
    exit 0
    ;;
esac
exit 0
"#;

/// Fake `systemd-creds`: logs its arguments, "seals" by prefixing `SEALED:`
/// and fails like a replaced TPM for content `FOREIGN`.
const FAKE_CREDS: &str = r#"#!/bin/bash
echo "$*" >> "$FAKE_CREDS_LOG"
case "$1" in
  encrypt)
    { printf 'SEALED:'; cat; } > "${@: -1}"
    ;;
  decrypt)
    content=$(cat "$3")
    if [ "$content" = "FOREIGN" ]; then
      echo "Failed to unseal secret using TPM2: State not recoverable" >&2
      exit 1
    fi
    printf '%s' "${content#SEALED:}"
    ;;
esac
"#;

struct Env {
    dir: tempfile::TempDir,
    pbs: Option<TcpListener>,
    port: u16,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for d in ["etc", "cred", "state", "run", "src/home/alice", "src/etc"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("src/home/alice/notes.txt"), "hello").unwrap();
        fs::write(root.join("src/etc/hostname"), "test").unwrap();
        let client = root.join("fake-client");
        fs::write(&client, FAKE_CLIENT).unwrap();
        fs::set_permissions(&client, fs::Permissions::from_mode(0o755)).unwrap();
        let creds = root.join("fake-systemd-creds");
        fs::write(&creds, FAKE_CREDS).unwrap();
        fs::set_permissions(&creds, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(root.join("cred"), fs::Permissions::from_mode(0o700)).unwrap();
        write_secret(&root.join("cred/password"), "token-secret\n");
        write_secret(&root.join("cred/encryption-key"), "{\"kdf\":null}");
        let pbs = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = pbs.local_addr().unwrap().port();
        let env = Env {
            dir,
            pbs: Some(pbs),
            port,
        };
        env.write_config("");
        env.set_key_exported(true);
        env
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn write_config(&self, extra_backup: &str) {
        let root = self.root();
        let text = format!(
            r#"backup_id = "test"

[pbs]
server = "127.0.0.1"
port = {port}
datastore = "backups"
namespace = "desktops"
auth_id = "backup@pbs!test"

[backup]
sources = [
  {{ archive = "home", path = "{home}" }},
  {{ archive = "etc", path = "{etc}" }},
  {{ archive = "opt", path = "{root}/does-not-exist", optional = true }},
]
excludes = ["**/.cache"]
inventory = false
{extra_backup}
"#,
            port = self.port,
            home = root.join("src/home").display(),
            etc = root.join("src/etc").display(),
            root = root.display(),
        );
        fs::write(root.join("etc/config.toml"), text).unwrap();
    }

    fn set_key_exported(&self, exported: bool) {
        let path = self.root().join("state/state.json");
        let mut state: serde_json::Value = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let digest = nestegg::secrets::digest(&nestegg::secrets::Secret::new(
            fs::read(self.root().join("cred/encryption-key")).unwrap_or_default(),
        ));
        state["key_digest"] = serde_json::json!(digest);
        state["key_exported_digest"] = if exported {
            serde_json::json!(digest)
        } else {
            serde_json::Value::Null
        };
        fs::write(path, state.to_string()).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let root = self.root();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_nestegg"));
        cmd.args(args)
            .env("NESTEGG_CONFIG", root.join("etc/config.toml"))
            .env("NESTEGG_CREDENTIALS_DIR", root.join("cred"))
            .env("NESTEGG_STATE_DIR", root.join("state"))
            .env("NESTEGG_RUNTIME_DIR", root.join("run"))
            .env("NESTEGG_PBS_CLIENT", root.join("fake-client"))
            .env("FAKE_LOG", root.join("fake.log"))
            .env("NESTEGG_SYSTEMD_CREDS", root.join("fake-systemd-creds"))
            .env("FAKE_CREDS_LOG", root.join("creds.log"))
            .env_remove("CREDENTIALS_DIRECTORY")
            .env("PBS_PASSWORD", "must-not-reach-the-client");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn fake_log(&self) -> String {
        fs::read_to_string(self.root().join("fake.log")).unwrap_or_default()
    }

    fn state(&self) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(self.root().join("state/state.json")).unwrap())
            .unwrap()
    }

    fn close_pbs(&mut self) {
        self.pbs = None;
    }
}

fn write_secret(path: &Path, value: &str) {
    fs::write(path, value).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

#[test]
fn successful_run_records_state_and_passes_secrets_via_fds() {
    let env = Env::new();
    let out = env.run(&["run", "--trigger", "cli"]);
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let log = env.fake_log();
    assert!(log.contains("PW token-secret\n"), "password via fd: {log}");
    assert!(log.contains("KEY {\"kdf\":null}"), "key via fd: {log}");
    assert!(
        !log.contains("LEAK"),
        "plain PBS_PASSWORD must be removed: {log}"
    );
    assert!(
        !log.contains("token-secret --"),
        "secret must not be an argument"
    );
    assert!(log.contains("REPO backup@pbs!test@127.0.0.1:"));
    assert!(log.contains("--ns desktops"));
    assert!(log.contains("--crypt-mode encrypt"));
    assert!(log.contains("--exclude **/.cache"));
    assert!(log.contains("snapshot notes update host/test/"));
    assert!(
        !log.contains("opt.pxar"),
        "missing optional source is skipped"
    );

    let state = env.state();
    assert_eq!(state["last_success"]["state"], "succeeded");
    assert_eq!(state["last_run"]["uploaded_bytes"], 1_048_576);
    assert_eq!(state["pbs_reachable"], true);
    assert_eq!(state["last_run"]["warnings"].as_array().unwrap().len(), 1);
    assert!(!env.root().join("state/current.json").exists());

    let history = env.run(&["history", "--json"]);
    let runs: serde_json::Value = serde_json::from_slice(&history.stdout).unwrap();
    assert_eq!(runs.as_array().unwrap().len(), 1);
}

#[test]
fn timer_run_skips_when_not_due() {
    let env = Env::new();
    assert_eq!(code(&env.run(&["run", "--trigger", "timer"])), 0);
    let calls = env.fake_log().matches("ARGS backup").count();
    assert_eq!(calls, 1);
    let out = env.run(&["run", "--trigger", "timer"]);
    assert_eq!(code(&out), 0);
    assert!(String::from_utf8_lossy(&out.stderr).contains("not due"));
    assert_eq!(env.fake_log().matches("ARGS backup").count(), 1);
    // A manual run ignores the interval.
    assert_eq!(code(&env.run(&["run", "--trigger", "manual"])), 0);
    assert_eq!(env.fake_log().matches("ARGS backup").count(), 2);
}

#[test]
fn unreachable_pbs_is_its_own_state() {
    let mut env = Env::new();
    env.close_pbs();
    let out = env.run(&["run", "--trigger", "timer"]);
    assert_eq!(code(&out), 75);
    let state = env.state();
    assert_eq!(state["last_run"]["state"], "pbs_unreachable");
    assert_eq!(state["last_run"]["error"]["kind"], "pbs_unreachable");
    assert_eq!(state["pbs_reachable"], false);
    assert!(state["last_success"].is_null());
    assert!(!env.fake_log().contains("ARGS backup"));
}

#[test]
fn wake_packet_that_cannot_be_sent_ends_as_unreachable() {
    let mut env = Env::new();
    // Port 0: Linux refuses the datagram (EINVAL), a deterministic stand-in
    // for "Network is unreachable" on a laptop that is offline.
    let config = env.root().join("etc/config.toml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(
        "\n[wake]\nmac = \"00:00:5e:00:53:01\"\nbroadcast = \"255.255.255.255\"\nport = 0\ntimeout_seconds = 0\n",
    );
    fs::write(&config, text).unwrap();
    env.close_pbs();
    let out = env.run(&["run", "--trigger", "timer"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(code(&out), 75, "stderr: {stderr}");
    assert!(
        stderr.contains("warning: could not send the Wake-on-LAN packet"),
        "stderr: {stderr}"
    );
    let state = env.state();
    assert_eq!(state["last_run"]["state"], "pbs_unreachable");
    assert_eq!(state["last_run"]["error"]["kind"], "pbs_unreachable");
    assert!(
        state["last_run"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("sending the wake-up packet failed")
    );
    let wake_warnings = state["last_run"]["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w.as_str().unwrap().contains("Wake-on-LAN"))
        .count();
    assert_eq!(wake_warnings, 1, "state: {state}");
    assert_eq!(state["pbs_reachable"], false);
    assert!(!env.fake_log().contains("ARGS backup"));
}

#[test]
fn authentication_failure_is_classified() {
    let env = Env::new();
    let out = env
        .command(&["run"])
        .env("FAKE_FAIL_AUTH", "1")
        .output()
        .unwrap();
    assert_eq!(code(&out), 1);
    let state = env.state();
    assert_eq!(state["last_run"]["state"], "failed");
    assert_eq!(state["last_run"]["error"]["kind"], "authentication");
}

#[test]
fn unexported_key_blocks_backups() {
    let env = Env::new();
    env.set_key_exported(false);
    let out = env.run(&["run"]);
    assert_eq!(code(&out), 1);
    let state = env.state();
    assert_eq!(state["last_run"]["error"]["kind"], "config");
    assert!(
        state["last_run"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("exported")
    );
    assert!(!env.fake_log().contains("ARGS backup"));
}

#[test]
fn missing_required_source_fails() {
    let env = Env::new();
    fs::remove_dir_all(env.root().join("src/etc")).unwrap();
    let out = env.run(&["run"]);
    assert_eq!(code(&out), 1);
    assert_eq!(env.state()["last_run"]["error"]["kind"], "source_missing");
}

#[test]
fn unencrypted_mode_does_not_need_a_key() {
    let env = Env::new();
    fs::remove_file(env.root().join("cred/encryption-key")).unwrap();
    env.write_config("encryption = false");
    env.set_key_exported(false);
    assert_eq!(code(&env.run(&["run"])), 0);
    let log = env.fake_log();
    assert!(log.contains("--crypt-mode none"));
    assert!(!log.contains("KEY "));
}

#[test]
fn world_readable_secret_is_refused() {
    let env = Env::new();
    fs::set_permissions(
        env.root().join("cred/password"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let out = env.run(&["run"]);
    assert_eq!(code(&out), 1);
    let msg = env.state()["last_run"]["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(msg.contains("accessible by other users"), "{msg}");
}

#[test]
fn replaced_key_invalidates_export_confirmation() {
    let env = Env::new();
    // Confirmation belongs to the old key; a new key was stored afterwards
    // (e.g. `key create --force` whose state update failed).
    write_secret(&env.root().join("cred/encryption-key"), "{\"new\":1}");
    let out = env.run(&["run"]);
    assert_eq!(code(&out), 1);
    assert_eq!(env.state()["last_run"]["error"]["kind"], "config");
    assert!(!env.fake_log().contains("ARGS backup"));
}

#[test]
fn unmounted_fstab_source_fails_instead_of_empty_backup() {
    let env = Env::new();
    let home = env.root().join("src/home");
    fs::write(
        env.root().join("fstab"),
        format!("UUID=x {} ext4 defaults 0 2\n", home.display()),
    )
    .unwrap();
    fs::write(
        env.root().join("mountinfo"),
        "22 1 259:2 / / rw - ext4 /dev/sda1 rw\n",
    )
    .unwrap();
    let out = env
        .command(&["run"])
        .env("NESTEGG_FSTAB", env.root().join("fstab"))
        .env("NESTEGG_MOUNTINFO", env.root().join("mountinfo"))
        .output()
        .unwrap();
    assert_eq!(code(&out), 1);
    let state = env.state();
    assert_eq!(state["last_run"]["error"]["kind"], "source_missing");
    assert!(state["last_success"].is_null());
    assert!(!env.fake_log().contains("ARGS backup"));
}

#[test]
fn status_works_with_read_only_lock_file_during_run() {
    let env = Env::new();
    // Simulate a running backup: hold the lock, current.json present.
    let lock_path = env.root().join("run/run.lock");
    let lock_file = fs::File::create(&lock_path).unwrap();
    let _held = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusive).unwrap();
    fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o444)).unwrap();
    let mut run = serde_json::json!({
        "id": "20261108T000000Z-timer-0001", "trigger": "timer", "state": "uploading",
        "started_at": "2020-01-01T00:00:00Z"
    });
    run["finished_at"] = serde_json::Value::Null;
    fs::write(env.root().join("state/current.json"), run.to_string()).unwrap();
    let out = env.run(&["status", "--json"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["health"], "running");
}

#[test]
fn promised_run_that_cannot_get_the_lock_is_recorded_as_cancelled() {
    let env = Env::new();
    let lock_file = fs::File::create(env.root().join("run/run.lock")).unwrap();
    let _held = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusive).unwrap();
    let id = "20261108T120000Z-manual-abcd";
    fs::write(
        env.root().join("state/current.json"),
        serde_json::json!({"id": id, "trigger": "manual", "state": "starting",
                           "started_at": "2026-11-08T12:00:00Z"})
        .to_string(),
    )
    .unwrap();
    let out = env.run(&["run", "--trigger", "manual", "--run-id", id]);
    assert_eq!(code(&out), 0);
    let record: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(env.root().join(format!("state/runs/{id}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(record["state"], "cancelled");
    assert_eq!(record["error"]["kind"], "already_running");
    assert!(!env.root().join("state/current.json").exists());
}

#[test]
fn confirmation_requires_exporting_the_current_key() {
    let env = Env::new();
    env.set_key_exported(false);
    let key_cmd = |args: &[&str]| {
        env.command(args)
            .env("NESTEGG_ALLOW_NON_ROOT", "1")
            .env_remove("SUDO_UID")
            .output()
            .unwrap()
    };
    let out = key_cmd(&["key", "confirm-exported"]);
    assert_eq!(code(&out), 1, "confirm without export must fail");
    assert!(String::from_utf8_lossy(&out.stderr).contains("not been exported"));
    let export = env.root().join("exported-key.json");
    assert_eq!(
        code(&key_cmd(&["key", "export", export.to_str().unwrap()])),
        0
    );
    assert_eq!(
        fs::metadata(&export).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Key replaced after the export: confirming must fail.
    write_secret(&env.root().join("cred/encryption-key"), "{\"other\":2}");
    assert_eq!(code(&key_cmd(&["key", "confirm-exported"])), 1);
    // Export the new key, then confirm works and backups run.
    let export2 = env.root().join("exported-key-2.json");
    assert_eq!(
        code(&key_cmd(&["key", "export", export2.to_str().unwrap()])),
        0
    );
    assert_eq!(code(&key_cmd(&["key", "confirm-exported"])), 0);
    assert_eq!(code(&env.run(&["run"])), 0);
    // Export refuses to overwrite.
    assert_eq!(
        code(&key_cmd(&["key", "export", export2.to_str().unwrap()])),
        1
    );
}

impl Env {
    /// A root command (`key …`) run by the test user.
    fn root_cmd(&self, args: &[&str]) -> Output {
        self.command(args)
            .env("NESTEGG_ALLOW_NON_ROOT", "1")
            .env_remove("SUDO_UID")
            .output()
            .unwrap()
    }

    fn write_user_file(&self, name: &str, content: &str) -> String {
        let path = self.root().join(name);
        fs::write(&path, content).unwrap();
        path.to_str().unwrap().to_string()
    }

    fn set_state(&self, key: &str, value: serde_json::Value) {
        let path = self.root().join("state/state.json");
        let mut state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        state[key] = value;
        fs::write(path, state.to_string()).unwrap();
    }
}

#[test]
fn key_import_stores_the_key_without_pcr_binding_and_counts_as_exported() {
    let env = Env::new();
    fs::remove_file(env.root().join("cred/encryption-key")).unwrap();
    env.set_key_exported(false);
    let file = env.write_user_file("exported.json", r#"{"fp":"aa:01"}"#);

    let out = env.root_cmd(&["key", "import", &file]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("aa:01"));
    assert!(env.root().join("cred/encryption-key.cred").exists());
    assert!(!env.root().join("cred/encryption-key").exists());
    let creds_log = fs::read_to_string(env.root().join("creds.log")).unwrap();
    assert!(
        creds_log.contains("encrypt --name=encryption-key --tpm2-pcrs= -"),
        "{creds_log}"
    );

    let digest = nestegg::secrets::digest(&nestegg::secrets::Secret::new(
        br#"{"fp":"aa:01"}"#.to_vec(),
    ));
    let state = env.state();
    assert_eq!(state["key_fingerprint"], "aa:01");
    assert_eq!(state["key_digest"], digest.as_str());
    assert_eq!(state["key_exported_digest"], digest.as_str());
    assert_eq!(state["key_last_exported_digest"], digest.as_str());

    // Backups run with exactly that key, without a separate confirmation.
    assert_eq!(code(&env.run(&["run"])), 0);
    assert!(env.fake_log().contains(r#"KEY {"fp":"aa:01"}"#));
}

#[test]
fn key_import_refuses_unusable_files_and_other_keys() {
    let env = Env::new();
    env.set_state("key_fingerprint", serde_json::json!("aa:01"));
    let before = fs::read(env.root().join("cred/encryption-key")).unwrap();

    let garbage = env.write_user_file("garbage.json", "not a key");
    let out = env.root_cmd(&["key", "import", &garbage]);
    assert_eq!(code(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a usable key file"));

    let protected = env.write_user_file("protected.json", r#"{"fp":"bb:02","kdf":"scrypt"}"#);
    let out = env.root_cmd(&["key", "import", &protected]);
    assert_eq!(code(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("protected with a password"));

    let other = env.write_user_file("other.json", r#"{"fp":"bb:02"}"#);
    let out = env.root_cmd(&["key", "import", &other]);
    assert_eq!(code(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("different encryption key"));
    assert_eq!(
        fs::read(env.root().join("cred/encryption-key")).unwrap(),
        before
    );
    assert_eq!(env.state()["key_fingerprint"], "aa:01");
    // The validation copies are gone.
    assert_eq!(fs::read_dir(env.root().join("run/tmp")).unwrap().count(), 0);

    let out = env.root_cmd(&["key", "import", "--force", &other]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.state()["key_fingerprint"], "bb:02");
}

#[test]
fn unreadable_credentials_are_reported_with_the_fix_and_import_repairs_them() {
    let env = Env::new();
    // The stored key was sealed to a TPM this computer no longer has.
    fs::remove_file(env.root().join("cred/encryption-key")).unwrap();
    write_secret(&env.root().join("cred/encryption-key.cred"), "FOREIGN");
    env.set_state("key_fingerprint", serde_json::json!("aa:01"));

    let out = env.run(&["run"]);
    assert_eq!(code(&out), 1);
    let error = &env.state()["last_run"]["error"];
    assert_eq!(error["kind"], "credentials_unreadable", "{error}");
    let msg = error["message"].as_str().unwrap();
    assert!(
        msg.contains("sudo nestegg key import <exported key file>"),
        "{msg}"
    );
    assert!(
        msg.contains("sudo nestegg credentials set-password"),
        "{msg}"
    );
    assert!(!env.fake_log().contains("ARGS backup"));

    // Importing the exported copy of the recorded key needs no --force.
    let file = env.write_user_file("exported.json", r#"{"fp":"aa:01"}"#);
    let out = env.root_cmd(&["key", "import", &file]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(code(&env.run(&["run"])), 0);

    // An unreadable token secret is classified the same way.
    fs::remove_file(env.root().join("cred/password")).unwrap();
    write_secret(&env.root().join("cred/password.cred"), "FOREIGN");
    assert_eq!(code(&env.run(&["run"])), 1);
    assert_eq!(
        env.state()["last_run"]["error"]["kind"],
        "credentials_unreadable"
    );
}

#[test]
fn restore_target_with_dotdot_is_rejected() {
    let env = Env::new();
    let out = env
        .command(&[
            "restore",
            "host/test/2026-11-06T21:20:00Z",
            "etc.pxar",
            "/root/missing/../../tmp/x",
        ])
        .env("NESTEGG_ALLOW_NON_ROOT", "1")
        .output()
        .unwrap();
    assert_eq!(code(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("'..'"));
}

// ---------------------------------------------------------------- daemon

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_daemon(env: &Env) -> Daemon {
    let child = env
        .command(&["daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = env.root().join("run/api.sock");
    let start = Instant::now();
    while !socket.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Daemon(child)
}

fn request(socket: &PathBuf, method: &str, path: &str) -> (u16, serde_json::Value) {
    request_body(socket, method, path, "")
}

fn request_body(
    socket: &PathBuf,
    method: &str,
    path: &str,
    body: &str,
) -> (u16, serde_json::Value) {
    let mut s = UnixStream::connect(socket).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.0\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap();
    let code = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (
        code,
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
    )
}

#[test]
fn daemon_serves_status_runs_and_snapshots() {
    let env = Env::new();
    let _daemon = start_daemon(&env);
    let socket = env.root().join("run/api.sock");

    let (code, health) = request(&socket, "GET", "/api/v1/health");
    assert_eq!(code, 200);
    assert_eq!(health["api_version"], "1");

    let (code, status) = request(&socket, "GET", "/api/v1/status");
    assert_eq!(code, 200);
    assert_eq!(status["health"], "never");
    assert_eq!(status["machine"]["backup_id"], "test");
    assert_eq!(status["pbs"]["namespace"], "desktops");

    let (code, run) = request(&socket, "POST", "/api/v1/runs");
    assert_eq!(code, 202, "{run}");
    let id = run["id"].as_str().unwrap().to_string();
    assert!(id.contains("-manual-"));

    // Wait for the detached run to finish.
    let start = Instant::now();
    loop {
        let (_, status) = request(&socket, "GET", "/api/v1/status");
        if status["health"] == "ok" {
            assert_eq!(status["last_success"]["id"], id.as_str());
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "run did not finish: {status}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    let (code, detail) = request(&socket, "GET", &format!("/api/v1/runs/{id}"));
    assert_eq!(code, 200);
    assert!(
        detail["log_tail"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l.as_str().unwrap().contains("run finished"))
    );
    let log_text = detail["log_tail"].to_string();
    assert!(
        !log_text.contains("token-secret"),
        "secrets must never appear in logs"
    );

    let (code, runs) = request(&socket, "GET", "/api/v1/runs?limit=5");
    assert_eq!(code, 200);
    assert_eq!(runs["runs"].as_array().unwrap().len(), 1);

    let (code, snaps) = request(&socket, "GET", "/api/v1/snapshots");
    assert_eq!(code, 200, "{snaps}");
    assert_eq!(
        snaps["snapshots"][0]["snapshot"],
        "host/test/2026-11-06T21:20:00Z"
    );

    let (code, cfg) = request(&socket, "GET", "/api/v1/config");
    assert_eq!(code, 200);
    assert_eq!(cfg["sources"][0]["archive"], "home.pxar");

    // Concurrent start requests: exactly one run starts, the rest get 409,
    // no internal errors.
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let socket = socket.clone();
            std::thread::spawn(move || request(&socket, "POST", "/api/v1/runs").0)
        })
        .collect();
    let codes: Vec<u16> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(codes.iter().filter(|c| **c == 202).count(), 1, "{codes:?}");
    assert!(codes.iter().all(|c| *c == 202 || *c == 409), "{codes:?}");
    let start = Instant::now();
    loop {
        let (_, runs) = request(&socket, "GET", "/api/v1/runs");
        let done = runs["runs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["state"] == "succeeded")
            .count();
        if done == 2 {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "{runs}");
        std::thread::sleep(Duration::from_millis(200));
    }

    let (code, err) = request(&socket, "GET", "/api/v1/runs/..%2Fetc");
    assert_eq!(code, 404, "{err}");
    // Restores and browsing: the test user's home is not part of the
    // configured sources, so both are refused for this (non-root) caller.
    let (code, err) = request_body(
        &socket,
        "POST",
        "/api/v1/restores",
        r#"{"snapshot_time":"2026-11-06T21:20:00Z","archive":"home.pxar","paths":[],"target_dir":"/tmp/nestegg-test-target-does-not-exist"}"#,
    );
    assert_eq!(code, 403, "{err}");
    assert_eq!(err["code"], "forbidden");
    let (code, err) = request(
        &socket,
        "GET",
        "/api/v1/snapshots/2026-11-06T21:20:00Z/browse?archive=home.pxar&path=/",
    );
    assert_eq!(code, 403, "{err}");
    let (code, err) = request(
        &socket,
        "GET",
        "/api/v1/snapshots/2026-11-06T21:20:00Z/browse?archive=..%2Fx&path=/",
    );
    assert_eq!(code, 400, "{err}");

    let (code, session) = request(&socket, "GET", "/api/v1/session");
    assert_eq!(code, 200);
    assert_eq!(session["uid"], nix::unistd::getuid().as_raw());

    let (code, cfg) = request(&socket, "GET", "/api/v1/config");
    assert_eq!(code, 200);
    assert_eq!(cfg["interval_hours"], 24);

    let (code, err) = request(
        &socket,
        "POST",
        "/api/v1/runs/20261108T000000Z-manual-0000/cancel",
    );
    assert_eq!(code, 404, "{err}");

    let (code, checks) = request(&socket, "POST", "/api/v1/pbs/check");
    assert_eq!(code, 200, "{checks}");
    assert!(
        checks["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "PBS reachable" && c["ok"] == true)
    );
}
