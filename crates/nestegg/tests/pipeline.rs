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
    # Keep what a server would store: the file list and the metadata blob.
    mode=encrypt
    case " $* " in *" --crypt-mode none "*) mode=none ;; esac
    mkdir -p "$FAKE_STORE"
    files=""
    for a in "$@"; do
      case "$a" in
        nestegg-meta.conf:*)
          cp "${a#nestegg-meta.conf:}" "$FAKE_STORE/meta"
          files="$files{\"filename\":\"nestegg-meta.conf.blob\",\"crypt-mode\":\"$mode\",\"size\":300},"
          ;;
        *.pxar:*)
          name="${a%%:*}"
          [ "$name" = "$FAKE_DROP_ARCHIVE" ] && continue
          files="$files{\"filename\":\"$name.didx\",\"crypt-mode\":\"$mode\",\"size\":100},"
          ;;
      esac
    done
    echo "[$files{\"filename\":\"index.json.blob\",\"crypt-mode\":\"none\",\"size\":1}]" > "$FAKE_STORE/files.json"
    echo "Starting backup: host/test/..."
    echo "home.pxar: had to backup 1 MiB of 2 MiB (compressed 1 MiB) in 0.10 s"
    echo "etc.pxar: had to backup 0 B of 1 KiB (compressed 0 B) in 0.01 s"
    exit 0
    ;;
  snapshot)
    if [ "$2" = "list" ]; then
      echo '[{"backup-type":"host","backup-id":"test","backup-time":1794000000,"files":[{"filename":"home.pxar.didx","crypt-mode":"encrypt"}],"size":2048,"protected":false,"verification":{"state":"failed","upid":"x"}}]'
    fi
    if [ "$2" = "files" ]; then
      # A test may dictate the file list; otherwise what "backup" stored.
      if [ -f "$FAKE_FILES_FILE" ]; then
        cat "$FAKE_FILES_FILE"
      else
        cat "$FAKE_STORE/files.json"
      fi
    fi
    exit 0
    ;;
  restore)
    # restore <snapshot> <archive> <target> ...
    if [ "$3" = "nestegg-meta.conf" ]; then
      cp "$FAKE_STORE/meta" "$4"
      if [ -n "$FAKE_TAMPER_META" ]; then echo "tampered" >> "$4"; fi
      exit 0
    fi
    # `restore <snapshot> index.json - --keyfd N`: only the key the
    # snapshot was made with ("c2Vj..." in its data) opens the manifest.
    keyfd=""
    prev=""
    for a in "$@"; do
      if [ "$prev" = "--keyfd" ]; then keyfd="$a"; fi
      prev="$a"
    done
    key=""
    if [ -n "$keyfd" ]; then key=$(cat <&"$keyfd"); fi
    echo "RESTORE-KEY $key" >> "$FAKE_LOG"
    case "$key" in
      *c2VjcmV0LWtleS1kYXRh*) echo '{"backup-type":"host","backup-id":"test"}' ;;
      *) echo "Error: unable to verify signature of the manifest" >&2; exit 255 ;;
    esac
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
    # `key paperkey <file> --output-format text|html`: the key pretty
    # printed (one member per line) between the markers, then a "QR code".
    if [ "$2" = "paperkey" ]; then
      block=$(printf '%s' "$(cat "$3")" | sed 's/,"/,\n"/g')
      case "$*" in
        *html*) printf '<html>\n<body>\n<p>\n-----BEGIN PROXMOX BACKUP KEY-----\n%s\n-----END PROXMOX BACKUP KEY-----\n</p>\n<img src="qr">\n</body>\n</html>\n' "$block" ;;
        *) printf -- '-----BEGIN PROXMOX BACKUP KEY-----\n%s\n-----END PROXMOX BACKUP KEY-----\nQR-CODE\n' "$block" ;;
      esac
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

/// Fake `systemctl` for the backup timer: the state lives in a file,
/// `<state>.fail` makes enable/disable fail like a missing system bus.
const FAKE_SYSTEMCTL: &str = r#"#!/bin/bash
echo "$*" >> "$FAKE_SYSTEMCTL_STATE.log"
case "$1" in
  is-enabled)
    [ "$(cat "$FAKE_SYSTEMCTL_STATE" 2>/dev/null)" = enabled ]
    ;;
  enable|disable)
    if [ -e "$FAKE_SYSTEMCTL_STATE.fail" ]; then
      echo "Failed to connect to bus: No such file or directory" >&2
      exit 1
    fi
    echo "$1d" > "$FAKE_SYSTEMCTL_STATE"
    ;;
  *)
    exit 1
    ;;
esac
"#;

/// The key of the test environment. `data` is what makes it a key; `fp`
/// is what the fake client reports as fingerprint.
const KEY_JSON: &str = r#"{"kdf":null,"data":"c2VjcmV0LWtleS1kYXRh","fp":"aa:01"}"#;

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
        let systemctl = root.join("fake-systemctl");
        fs::write(&systemctl, FAKE_SYSTEMCTL).unwrap();
        fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
        let creds = root.join("fake-systemd-creds");
        fs::write(&creds, FAKE_CREDS).unwrap();
        fs::set_permissions(&creds, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(root.join("cred"), fs::Permissions::from_mode(0o700)).unwrap();
        write_secret(&root.join("cred/password"), "token-secret\n");
        write_secret(&root.join("cred/encryption-key"), KEY_JSON);
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
            .env("FAKE_FILES_FILE", root.join("snapshot-files.json"))
            .env("FAKE_STORE", root.join("fake-store"))
            .env("NESTEGG_SYSTEMD_CREDS", root.join("fake-systemd-creds"))
            .env("FAKE_CREDS_LOG", root.join("creds.log"))
            .env("NESTEGG_SYSTEMCTL", root.join("fake-systemctl"))
            .env("FAKE_SYSTEMCTL_STATE", root.join("timer-state"))
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
    assert!(
        log.contains(&format!("KEY {KEY_JSON}")),
        "key via fd: {log}"
    );
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
    assert!(log.contains(" nestegg-meta.conf:"), "metadata blob: {log}");
    assert!(log.contains("ARGS snapshot files host/test/"), "{log}");
    assert!(log.contains("ARGS restore host/test/"), "{log}");
    // The notes carry the new fields.
    let notes_line = log
        .lines()
        .find(|l| l.starts_with("ARGS snapshot notes update"))
        .unwrap();
    assert!(notes_line.contains("incomplete, "), "{notes_line}");
    assert!(log.contains("\"verified\":true"), "{log}");
    assert!(log.contains("\"incomplete\":[\"optional source"), "{log}");

    let state = env.state();
    assert_eq!(state["last_success"]["state"], "succeeded");
    assert_eq!(state["last_success"]["verified"], true);
    assert_eq!(state["last_run"]["uploaded_bytes"], 1_048_576);
    assert_eq!(state["pbs_reachable"], true);
    assert_eq!(state["last_run"]["warnings"].as_array().unwrap().len(), 1);
    // The missing optional source makes the backup incomplete.
    let incomplete = state["last_success"]["incomplete"].as_array().unwrap();
    assert_eq!(incomplete.len(), 1, "{state}");
    assert!(
        incomplete[0].as_str().unwrap().contains("does-not-exist"),
        "{state}"
    );
    assert_eq!(
        state["server_verify_failed"],
        serde_json::json!(["host/test/2026-11-06T21:20:00Z"])
    );
    // No metadata file is left behind.
    let leftovers: Vec<_> = fs::read_dir(env.root().join("run/tmp"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().ends_with(".meta.json"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let out = env.run(&["status", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["health"], "incomplete");
    let text = env.run(&["status"]);
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains("Incomplete:"), "{text}");
    let text = env.run(&["history"]);
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains("part "), "{text}");
    assert!(text.contains("does-not-exist"), "{text}");
    assert!(!env.root().join("state/current.json").exists());

    let history = env.run(&["history", "--json"]);
    let runs: serde_json::Value = serde_json::from_slice(&history.stdout).unwrap();
    assert_eq!(runs.as_array().unwrap().len(), 1);
}

#[test]
fn read_back_mismatch_fails_the_run() {
    let env = Env::new();
    let out = env
        .command(&["run"])
        .env("FAKE_TAMPER_META", "1")
        .output()
        .unwrap();
    assert_eq!(code(&out), 1, "{}", String::from_utf8_lossy(&out.stderr));
    let state = env.state();
    assert_eq!(state["last_run"]["state"], "failed");
    assert_eq!(state["last_run"]["error"]["kind"], "verification_failed");
    let msg = state["last_run"]["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("nestegg-meta.conf has different content"),
        "{msg}"
    );
    assert_eq!(state["last_run"]["verified"], false);
    assert!(state["last_success"].is_null());
    // The snapshot exists: it is recorded, and the server state was read.
    assert!(state["last_run"]["snapshot"].is_string());
    assert_eq!(state["server_verify_failed"].as_array().unwrap().len(), 1);
    assert!(!env.fake_log().contains("snapshot notes update"));

    let out = env
        .command(&["run"])
        .env("FAKE_DROP_ARCHIVE", "etc.pxar")
        .output()
        .unwrap();
    assert_eq!(code(&out), 1);
    let state = env.state();
    assert_eq!(state["last_run"]["error"]["kind"], "verification_failed");
    let msg = state["last_run"]["error"]["message"].as_str().unwrap();
    assert!(msg.contains("etc.pxar is missing"), "{msg}");

    let out = env.run(&["status", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    // Never succeeded so far: "never" takes precedence over "failing".
    assert_eq!(status["health"], "never");
    assert_eq!(code(&env.run(&["run"])), 0);
    assert_eq!(
        code(
            &env.command(&["run"])
                .env("FAKE_TAMPER_META", "1")
                .output()
                .unwrap()
        ),
        1
    );
    let out = env.run(&["status", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["health"], "failing");
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
fn switched_off_wake_on_lan_is_not_used() {
    let mut env = Env::new();
    // Port 0 would make a send attempt fail with a warning (see above).
    let config = env.root().join("etc/config.toml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(
        "\n[wake]\nmac = \"00:00:5e:00:53:01\"\nport = 0\ntimeout_seconds = 0\nenabled = false\n",
    );
    fs::write(&config, text).unwrap();
    env.close_pbs();
    let out = env.run(&["run", "--trigger", "timer"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(code(&out), 75, "stderr: {stderr}");
    assert!(!stderr.contains("Wake-on-LAN"), "stderr: {stderr}");
    let state = env.state();
    assert_eq!(state["last_run"]["state"], "pbs_unreachable");
    assert_eq!(state["last_run"]["error"]["message"], "PBS did not answer");
    assert!(
        !state["last_run"]["warnings"]
            .to_string()
            .contains("Wake-on-LAN"),
        "{state}"
    );
    let out = env.run(&["status", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["pbs"]["wake_on_lan"], false);
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
fn confirmation_needs_proof_of_the_current_key() {
    let env = Env::new();
    env.set_key_exported(false);
    let key_cmd = |args: &[&str]| env.root_cmd(args);
    let stderr = |out: &Output| String::from_utf8_lossy(&out.stderr).into_owned();
    let confirmed = || env.state()["key_exported_digest"].is_string();

    // No proof: a bare confirmation is refused, with the syntax to use.
    let out = key_cmd(&["key", "confirm-exported"]);
    assert_eq!(code(&out), 1, "confirm without proof must fail");
    assert!(stderr(&out).contains("proof needed"), "{}", stderr(&out));
    assert!(!confirmed());

    // Export writes the file, reads it back and warns about its location.
    let export = env.root().join("exported-key.json");
    let out = key_cmd(&["key", "export", export.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("read back"));
    assert_eq!(
        fs::metadata(&export).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Exporting alone confirms nothing.
    assert_eq!(code(&env.run(&["run"])), 1);
    assert!(!confirmed());

    // A file with another key is refused, nothing recorded.
    let other = env.write_user_file(
        "other.json",
        r#"{"kdf":null,"data":"b3RoZXI=","fp":"bb:02"}"#,
    );
    let out = key_cmd(&["key", "confirm-exported", "--file", &other]);
    assert_eq!(code(&out), 1);
    assert!(stderr(&out).contains("does not contain the key"));
    // Not a key at all.
    let junk = env.write_user_file("junk.txt", "hello");
    let out = key_cmd(&["key", "confirm-exported", "--file", &junk]);
    assert_eq!(code(&out), 1);
    assert!(stderr(&out).contains("no key found"), "{}", stderr(&out));
    // A wrong or malformed check code is refused.
    for bad in ["0000-0000", "nonsense"] {
        let out = key_cmd(&["key", "confirm-exported", "--paper-code", bad]);
        assert_eq!(code(&out), 1, "{bad}");
    }
    assert!(!confirmed());

    // The right file lies on this computer's disk (a temp directory, never
    // a removable medium), and there is no terminal: it needs the statement.
    let export = export.to_str().unwrap();
    let out = key_cmd(&["key", "confirm-exported", "--file", export]);
    assert_eq!(code(&out), 1);
    assert!(
        stderr(&out).contains("--accept-local-copy"),
        "{}",
        stderr(&out)
    );
    assert!(!confirmed());
    let out = key_cmd(&[
        "key",
        "confirm-exported",
        "--file",
        export,
        "--accept-local-copy",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(confirmed());
    assert_eq!(code(&env.run(&["run"])), 0);
    // Export refuses to overwrite.
    assert_eq!(code(&key_cmd(&["key", "export", export])), 1);

    // Key replaced afterwards: the old confirmation and the old copy are void.
    write_secret(
        &env.root().join("cred/encryption-key"),
        r#"{"kdf":null,"data":"bmV3LWtleQ==","fp":"cc:03"}"#,
    );
    assert_eq!(code(&env.run(&["run"])), 1);
    let out = key_cmd(&[
        "key",
        "confirm-exported",
        "--file",
        export,
        "--accept-local-copy",
    ]);
    assert_eq!(code(&out), 1, "the old copy must not confirm the new key");
    assert!(stderr(&out).contains("does not contain the key"));
}

#[test]
fn paper_key_has_our_header_outside_the_key_block_and_its_code_proves_the_sheet() {
    let env = Env::new();
    env.set_key_exported(false);
    let code_of = nestegg::keyproof::KeyMaterial::from_key_json(KEY_JSON)
        .unwrap()
        .check_code();

    // Text, piped (stdout is not a terminal): allowed.
    let out = env.root_cmd(&["key", "paperkey"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let begin = text.find("-----BEGIN PROXMOX BACKUP KEY-----").unwrap();
    let header = &text[..begin];
    assert!(header.contains("nestegg paper key"));
    assert!(
        header
            .lines()
            .any(|l| l.starts_with("Check code:") && l.ends_with(&code_of)),
        "{header}"
    );
    assert!(
        header
            .lines()
            .any(|l| l.starts_with("Key fingerprint:") && l.ends_with("aa:01")),
        "{header}"
    );
    assert!(header.contains("127.0.0.1:"), "{header}");
    assert!(header.contains("backups"), "datastore: {header}");
    assert!(header.contains("test"), "backup id: {header}");
    assert!(
        text.ends_with("QR-CODE\n"),
        "client output stays unmodified"
    );
    assert_eq!(
        nestegg::keyproof::KeyMaterial::from_copy(text.as_bytes()).unwrap(),
        nestegg::keyproof::KeyMaterial::from_key_json(KEY_JSON).unwrap()
    );

    // To a file, in both formats; the check code is not echoed.
    let txt = env.root().join("key.txt");
    let html = env.root().join("key.html");
    let out = env.root_cmd(&["key", "paperkey", "--output", txt.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&code_of));
    let out = env.root_cmd(&[
        "key",
        "paperkey",
        "--format",
        "html",
        "--output",
        html.to_str().unwrap(),
    ]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let html_text = fs::read_to_string(&html).unwrap();
    assert!(html_text.find("nestegg paper key").unwrap() > html_text.find("<body>").unwrap());
    assert!(
        html_text.find("nestegg paper key").unwrap() < html_text.find("BEGIN PROXMOX").unwrap()
    );
    assert!(html_text.contains(&code_of));

    // Saved paper keys are proof, as files and by their code.
    for file in [&txt, &html] {
        let out = env.root_cmd(&["key", "verify", file.to_str().unwrap()]);
        assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stdout));
    }
    let wrong = if code_of == "0000-0000" {
        "1111-1111"
    } else {
        "0000-0000"
    };
    let out = env.root_cmd(&["key", "confirm-exported", "--paper-code", wrong]);
    assert_eq!(code(&out), 1);
    assert!(env.state()["key_exported_digest"].is_null());
    assert_eq!(code(&env.run(&["run"])), 1);
    // Lower case and a missing hyphen are fine.
    let typed = code_of.replace('-', "").to_lowercase();
    let out = env.root_cmd(&["key", "confirm-exported", "--paper-code", &typed]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(code(&env.run(&["run"])), 0);

    // A saved paper key also confirms (on this disk: needs the statement).
    env.set_key_exported(false);
    let out = env.root_cmd(&[
        "key",
        "confirm-exported",
        "--file",
        html.to_str().unwrap(),
        "--accept-local-copy",
    ]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(code(&env.run(&["run"])), 0);
}

#[test]
fn key_verify_compares_the_copy_and_opens_the_newest_snapshot() {
    let env = Env::new();
    let good = env.write_user_file("copy.json", KEY_JSON);
    let other = env.write_user_file("other.json", r#"{"kdf":null,"data":"b3RoZXI="}"#);
    let verify = |file: &str| env.root_cmd(&["key", "verify", file]);
    let text = |out: &Output| String::from_utf8_lossy(&out.stdout).into_owned();

    // The PBS answers, the newest snapshot is encrypted and the fake client
    // opens its manifest with the right key only.
    env.set_files(r#"[{"filename":"index.json.blob","crypt-mode":"sign-only"}]"#);
    let out = verify(&good);
    assert_eq!(code(&out), 0, "{}", text(&out));
    assert!(
        text(&out).contains("matches the stored key:  yes"),
        "{}",
        text(&out)
    );
    assert!(
        text(&out).contains("opens the newest backup: yes (host/test/"),
        "{}",
        text(&out)
    );
    let log = env.fake_log();
    assert!(log.contains("restore host/test/"), "{log}");
    assert!(log.contains("index.json -"), "{log}");
    assert!(log.contains(&format!("RESTORE-KEY {KEY_JSON}")), "{log}");

    // A different key: no match, and the backup is not even tried.
    let before = env.fake_log().matches("ARGS restore").count();
    let out = verify(&other);
    assert_eq!(code(&out), 1);
    assert!(text(&out).contains("NO"), "{}", text(&out));
    assert_eq!(env.fake_log().matches("ARGS restore").count(), before);
    // Not a key at all.
    let junk = env.write_user_file("junk.txt", "hello");
    assert_eq!(code(&verify(&junk)), 1);

    // The stored key matches the copy, but the client cannot open the
    // snapshot with it (key replaced after the backup).
    write_secret(
        &env.root().join("cred/encryption-key"),
        r#"{"kdf":null,"data":"bmV3LWtleQ=="}"#,
    );
    let replaced = env.write_user_file("replaced.json", r#"{"kdf":null,"data":"bmV3LWtleQ=="}"#);
    let out = verify(&replaced);
    assert_eq!(code(&out), 1, "{}", text(&out));
    assert!(text(&out).contains("matches the stored key:  yes"));
    assert!(
        text(&out).contains("opens the newest backup: NO"),
        "{}",
        text(&out)
    );
    write_secret(&env.root().join("cred/encryption-key"), KEY_JSON);

    // Nothing to test against: unencrypted snapshot.
    env.set_files(r#"[{"filename":"index.json.blob","crypt-mode":"none"}]"#);
    let out = verify(&good);
    assert_eq!(code(&out), 0, "{}", text(&out));
    assert!(text(&out).contains("not checked"), "{}", text(&out));
}

#[test]
fn key_verify_without_server_checks_the_copy_only() {
    let mut env = Env::new();
    let good = env.write_user_file("copy.json", KEY_JSON);
    env.close_pbs();
    let out = env.root_cmd(&["key", "verify", &good]);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(code(&out), 0, "{stdout}");
    assert!(stdout.contains("matches the stored key:  yes"), "{stdout}");
    assert!(
        stdout.contains("not checked - the PBS does not answer"),
        "{stdout}"
    );
    assert!(!env.fake_log().contains("ARGS restore"));
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

    /// What the fake client prints for `snapshot files`.
    fn set_files(&self, json: &str) {
        fs::write(self.root().join("snapshot-files.json"), json).unwrap();
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
fn key_import_stores_the_key_without_pcr_binding_and_needs_proof_for_a_local_file() {
    const IMPORTED: &str = r#"{"kdf":null,"data":"aW1wb3J0ZWQ=","fp":"aa:01"}"#;
    let env = Env::new();
    fs::remove_file(env.root().join("cred/encryption-key")).unwrap();
    env.set_key_exported(false);
    let file = env.write_user_file("exported.json", IMPORTED);

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

    let digest =
        nestegg::secrets::digest(&nestegg::secrets::Secret::new(IMPORTED.as_bytes().to_vec()));
    let state = env.state();
    assert_eq!(state["key_fingerprint"], "aa:01");
    assert_eq!(state["key_digest"], digest.as_str());
    // The file lies on this computer's own disk (the test directory): that
    // is no copy outside the computer, so the import alone confirms nothing.
    assert!(state["key_exported_digest"].is_null(), "{state}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("does NOT count as exported"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(code(&env.run(&["run"])), 1);
    assert_eq!(env.state()["last_run"]["error"]["kind"], "config");

    // With the statement that another copy exists, backups run with exactly
    // that key.
    let out = env.root_cmd(&[
        "key",
        "confirm-exported",
        "--file",
        &file,
        "--accept-local-copy",
    ]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.state()["key_exported_digest"], digest.as_str());
    assert_eq!(code(&env.run(&["run"])), 0);
    assert!(env.fake_log().contains(&format!("KEY {IMPORTED}")));
}

#[test]
fn repeated_failures_pause_the_timer_but_not_manual_runs() {
    let env = Env::new();
    let failing = |args: &[&str]| {
        env.command(args)
            .env("FAKE_FAIL_AUTH", "1")
            .output()
            .unwrap()
    };
    let attempts = || env.fake_log().matches("ARGS backup").count();
    let timer = ["run", "--trigger", "timer"];

    // Two failures in a row: both are real attempts.
    assert_eq!(code(&failing(&timer)), 1);
    assert_eq!(code(&failing(&timer)), 1);
    assert_eq!(attempts(), 2);
    assert_eq!(env.state()["consecutive_failures"], 2);

    // The next timer check does not try again yet and leaves no record.
    let out = failing(&timer);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not retrying yet: 2 failed runs in a row"),
        "{stderr}"
    );
    assert_eq!(attempts(), 2);
    assert_eq!(env.state()["last_run"]["state"], "failed");

    // Storing the credentials again (somebody fixed it) ends the pause.
    let mut set = env.command(&["credentials", "set-password", "--allow-plain"]);
    set.env("NESTEGG_ALLOW_NON_ROOT", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = set.spawn().unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"new-secret\n")
            .unwrap();
    }
    assert!(child.wait_with_output().unwrap().status.success());
    assert_eq!(env.state()["consecutive_failures"], 0);
    assert_eq!(code(&failing(&timer)), 1);
    assert_eq!(code(&failing(&timer)), 1);
    assert_eq!(attempts(), 4);

    // Paused again; a manual run is never held back, and a success clears
    // the count.
    assert_eq!(code(&failing(&timer)), 0);
    assert_eq!(attempts(), 4);
    assert_eq!(code(&env.run(&["run", "--trigger", "manual"])), 0);
    assert_eq!(attempts(), 5);
    assert_eq!(env.state()["consecutive_failures"], 0);
}

#[test]
fn key_export_then_import_of_the_same_file_does_not_confirm_the_key() {
    let env = Env::new();
    env.set_state("key_fingerprint", serde_json::json!("aa:01"));
    env.set_key_exported(false);
    let export = env.root().join("exported.json");
    let export = export.to_str().unwrap();
    let out = env.root_cmd(&["key", "export", export]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let out = env.root_cmd(&["key", "import", export]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(env.state()["key_exported_digest"].is_null());
    assert_eq!(code(&env.run(&["run"])), 1);
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

    // Importing the exported copy of the recorded key needs no --force, and
    // the confirmation given for exactly this key before stays valid.
    let file = env.write_user_file("exported.json", KEY_JSON);
    let out = env.root_cmd(&["key", "import", &file]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("counts as exported"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
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
        // The optional source of the test configuration is missing.
        if status["health"] == "incomplete" {
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

#[test]
fn daemon_changes_settings() {
    let env = Env::new();
    let config = env.root().join("etc/config.toml");
    // A comment and a [wake] section the change must keep.
    let original = format!(
        "# Hand-written, keep me.\n{}\n[wake]\nmac = \"00:00:5e:00:53:01\" # PBS\n",
        fs::read_to_string(&config).unwrap()
    );
    fs::write(&config, &original).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
    let _daemon = start_daemon(&env);
    let socket = env.root().join("run/api.sock");

    let (code, view) = request(&socket, "GET", "/api/v1/config");
    assert_eq!(code, 200, "{view}");
    assert_eq!(view["automatic_backups"], false);
    assert_eq!(view["wake_on_lan"], true);
    assert_eq!(view["wake_on_lan_configured"], true);
    assert_eq!(view["limits"]["interval_hours"]["max"], 168);
    assert!(
        view["editable"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("wake_on_lan"))
    );

    let (code, view) = request_body(
        &socket,
        "PATCH",
        "/api/v1/config",
        r#"{"interval_hours":12,"overdue_after_hours":20,"excludes":["**/.cache","*.iso"],"wake_on_lan":false,"automatic_backups":true}"#,
    );
    assert_eq!(code, 200, "{view}");
    assert_eq!(view["interval_hours"], 12);
    assert_eq!(view["schedule"], "every 12 h");
    assert_eq!(view["overdue_after_hours"], 20);
    assert_eq!(view["excludes"], serde_json::json!(["**/.cache", "*.iso"]));
    assert_eq!(view["wake_on_lan"], false);
    assert_eq!(view["wake_on_lan_configured"], true);
    assert_eq!(view["automatic_backups"], true);
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.starts_with("# Hand-written, keep me.\n"), "{text}");
    assert!(
        text.contains("mac = \"00:00:5e:00:53:01\" # PBS\nenabled = false\n"),
        "{text}"
    );
    assert!(text.contains("server = \"127.0.0.1\""));
    assert_eq!(
        fs::metadata(&config).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert_eq!(
        fs::read_to_string(env.root().join("timer-state")).unwrap(),
        "enabled\n"
    );
    // GET /status reflects the new values.
    let (_, status) = request(&socket, "GET", "/api/v1/status");
    assert_eq!(status["schedule"]["enabled"], true);
    assert_eq!(status["schedule"]["interval_hours"], 12);
    assert!(status["schedule"]["next_run"].is_string(), "{status}");
    assert_eq!(status["overdue_after_hours"], 20);
    assert_eq!(status["pbs"]["wake_on_lan"], false);

    // Validation errors are 400 bad_request and change nothing.
    let before = fs::read_to_string(&config).unwrap();
    for (body, needle) in [
        (r#"{"interval_hours":0}"#, "interval_hours"),
        (r#"{"interval_hours":200}"#, "interval_hours"),
        (r#"{"interval_hours":24}"#, "greater than interval_hours"),
        (r#"{"excludes":["**"]}"#, "exclude everything"),
        (r#"{"excludes":["a\nb"]}"#, "control character"),
        (r#"{"server":"pbs.example.com"}"#, "cannot be changed here"),
        (r#"{"sources":[]}"#, "cannot be changed here"),
        (r#"{"bogus":1}"#, "unknown setting"),
        (r#"{"interval_hours":"12"}"#, "invalid settings"),
        ("not json", ""),
    ] {
        let (code, err) = request_body(&socket, "PATCH", "/api/v1/config", body);
        assert_eq!(code, 400, "{body}: {err}");
        assert_eq!(err["code"], "bad_request", "{body}: {err}");
        assert!(
            err["message"].as_str().unwrap().contains(needle),
            "{body}: {err}"
        );
    }
    assert_eq!(fs::read_to_string(&config).unwrap(), before);

    // A failing systemctl leaves everything as it was.
    fs::write(env.root().join("timer-state.fail"), "").unwrap();
    let (code, err) = request_body(
        &socket,
        "PATCH",
        "/api/v1/config",
        r#"{"interval_hours":6,"automatic_backups":false}"#,
    );
    assert_eq!(code, 500, "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("Failed to connect to bus"),
        "{err}"
    );
    assert_eq!(fs::read_to_string(&config).unwrap(), before);
    fs::remove_file(env.root().join("timer-state.fail")).unwrap();

    // Concurrent changes of different settings are all kept.
    let bodies = [
        r#"{"interval_hours":8}"#,
        r#"{"overdue_after_hours":30}"#,
        r#"{"excludes":["**/.cache"]}"#,
        r#"{"wake_on_lan":true}"#,
        r#"{"automatic_backups":false}"#,
    ];
    let handles: Vec<_> = bodies
        .iter()
        .map(|body| {
            let socket = socket.clone();
            let body = body.to_string();
            std::thread::spawn(move || request_body(&socket, "PATCH", "/api/v1/config", &body))
        })
        .collect();
    for h in handles {
        let (code, v) = h.join().unwrap();
        assert_eq!(code, 200, "{v}");
    }
    let (_, view) = request(&socket, "GET", "/api/v1/config");
    assert_eq!(view["interval_hours"], 8);
    assert_eq!(view["overdue_after_hours"], 30);
    assert_eq!(view["excludes"], serde_json::json!(["**/.cache"]));
    assert_eq!(view["wake_on_lan"], true);
    assert_eq!(view["automatic_backups"], false);
    let leftovers: Vec<_> = fs::read_dir(env.root().join("etc"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(leftovers, ["config.toml"]);
}

#[test]
fn wake_on_lan_cannot_be_switched_on_without_a_mac() {
    let env = Env::new();
    let _daemon = start_daemon(&env);
    let socket = env.root().join("run/api.sock");
    let (_, view) = request(&socket, "GET", "/api/v1/config");
    assert_eq!(view["wake_on_lan_configured"], false);
    assert_eq!(view["wake_on_lan"], false);
    assert!(
        !view["editable"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("wake_on_lan"))
    );
    let (code, err) = request_body(
        &socket,
        "PATCH",
        "/api/v1/config",
        r#"{"wake_on_lan":true}"#,
    );
    assert_eq!(code, 400, "{err}");
    assert!(err["message"].as_str().unwrap().contains("[wake]"), "{err}");
    let (code, _) = request_body(
        &socket,
        "PATCH",
        "/api/v1/config",
        r#"{"wake_on_lan":false}"#,
    );
    assert_eq!(code, 200);
}
