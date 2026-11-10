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
    # An old server lists the files without their crypt mode.
    cm=",\"crypt-mode\":\"$mode\""
    if [ -n "$FAKE_NO_CRYPT_MODE" ]; then cm=""; fi
    mkdir -p "$FAKE_STORE"
    files=""
    for a in "$@"; do
      case "$a" in
        nestegg-meta.conf:*)
          cp "${a#nestegg-meta.conf:}" "$FAKE_STORE/meta"
          files="$files{\"filename\":\"nestegg-meta.conf.blob\"$cm,\"size\":300},"
          ;;
        *.pxar:*)
          name="${a%%:*}"
          [ "$name" = "$FAKE_DROP_ARCHIVE" ] && continue
          files="$files{\"filename\":\"$name.didx\"$cm,\"size\":100},"
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
      # A test can make the Nth and later `snapshot files` calls block, to
      # exercise a server that answers the first listings but then stops
      # answering the listing the restore does on its own (encryption off).
      if [ -n "$FAKE_BLOCK_LISTING_AFTER" ]; then
        cnt_file="$FAKE_STORE/listing-count"
        mkdir -p "$FAKE_STORE"
        n=$(cat "$cnt_file" 2>/dev/null || echo 0)
        n=$((n + 1))
        echo "$n" > "$cnt_file"
        if [ "$n" -gt "$FAKE_BLOCK_LISTING_AFTER" ]; then
          touch "${FAKE_READBACK_BLOCK:-$FAKE_STORE/blocked}"
          exec sleep 30
        fi
      fi
      # The read-back lists the files first; a test can make this call block
      # (touch a sentinel, then wait to be killed) to exercise a hanging or
      # cancelled read-back.
      if [ -n "$FAKE_READBACK_BLOCK" ]; then
        touch "$FAKE_READBACK_BLOCK"
        # exec, so killing this pid kills the waiter and closes the pipes at
        # once (a plain `sleep` child would be orphaned and hold them open).
        exec sleep 30
      fi
      # A test can make the listing fail with an error text of its choice.
      if [ -n "$FAKE_FILES_ERROR" ]; then
        echo "$FAKE_FILES_ERROR" >&2
        exit 255
      fi
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
fn cancel_during_read_back_ends_cancelled_and_cleans_up() {
    let env = Env::new();
    let sentinel = env.root().join("readback-started");
    let child = env
        .command(&["run", "--trigger", "cli"])
        .env("FAKE_READBACK_BLOCK", &sentinel)
        .spawn()
        .unwrap();
    // Wait until the read-back's `snapshot files` call is running.
    let start = Instant::now();
    while !sentinel.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the read-back did not start"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Ask the run to cancel, the way the daemon does (SIGTERM).
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.code().is_some(),
        "the run was killed instead of cancelling itself"
    );

    // The run is recorded as cancelled, not as a success.
    let state = env.state();
    assert_eq!(state["last_run"]["state"], "cancelled", "{state}");

    // current.json is gone, the metadata file is removed, and the read-back
    // staging directory holds no leftover copies (only its empty base dir,
    // which always persists, may remain).
    assert!(!env.root().join("state/current.json").exists());
    let tmp = env.root().join("run/tmp");
    let meta_leftovers: Vec<_> = fs::read_dir(&tmp)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".meta.json"))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        meta_leftovers.is_empty(),
        "metadata leftovers: {meta_leftovers:?}"
    );
    let verify = tmp.join("verify");
    if verify.exists() {
        let inside: Vec<_> = fs::read_dir(&verify)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(inside.is_empty(), "read-back staging leftovers: {inside:?}");
    }

    // The run lock was released: a second run can start.
    let out = env.run(&["run", "--trigger", "cli"]);
    assert_eq!(
        code(&out),
        0,
        "second run after cancel: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn read_back_that_hangs_times_out_as_verification_failed() {
    let env = Env::new();
    let sentinel = env.root().join("readback-started");
    // The read-back's `snapshot files` call blocks; with a 1 s time limit the
    // run must end as a failed verification that says what timed out, instead
    // of hanging and holding the run lock (decision 0015).
    let out = env
        .command(&["run", "--trigger", "cli"])
        .env("FAKE_READBACK_BLOCK", &sentinel)
        .env("NESTEGG_READBACK_TIMEOUT_SECS", "1")
        .output()
        .unwrap();
    assert_ne!(code(&out), 0);
    let state = env.state();
    assert_eq!(state["last_run"]["state"], "failed", "{state}");
    assert_eq!(state["last_run"]["error"]["kind"], "verification_failed");
    let message = state["last_run"]["error"]["message"].as_str().unwrap();
    assert!(message.contains("timed out"), "message: {message}");
    assert!(!env.root().join("state/current.json").exists());
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
fn read_back_timeout_with_encryption_off_is_a_transient_failure() {
    // With encryption off the read-back's metadata restore resolves its key
    // from a listing of its own. That listing must be bounded and cancellable
    // too, whatever the configuration (decision 0015): a server that answers
    // the first listings but then stops answering must not hang the run. The
    // run ends as a verification failure, but a transient one — a server
    // availability problem that does not pause the retries (decision 0021).
    let env = Env::new();
    fs::remove_file(env.root().join("cred/encryption-key")).unwrap();
    env.write_config("encryption = false");
    let out = env
        .command(&["run", "--trigger", "cli"])
        // Answer the first two listings (read-back step 1 and the metadata
        // fetch), then block the restore's own listing.
        .env("FAKE_BLOCK_LISTING_AFTER", "2")
        .env("NESTEGG_READBACK_TIMEOUT_SECS", "1")
        .output()
        .unwrap();
    assert_ne!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let state = env.state();
    assert_eq!(state["last_run"]["state"], "failed", "{state}");
    assert_eq!(state["last_run"]["error"]["kind"], "verification_failed");
    assert_eq!(state["last_run"]["transient"], true, "{state}");
    // Like pbs_unreachable, it does not count towards the growing pause.
    assert_eq!(state["consecutive_failures"], 0, "{state}");
    assert!(!env.root().join("state/current.json").exists());

    // So the next timer check retries at once instead of pausing.
    let out = env.run(&["run", "--trigger", "timer"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("not retrying yet"),
        "a transient read-back failure must not pause the retries"
    );
}

#[test]
fn a_read_back_mismatch_counts_as_a_failure() {
    // A mismatch (content differs) is a real failure: it counts towards the
    // growing pause, unlike a transient availability problem.
    let env = Env::new();
    let out = env
        .command(&["run"])
        .env("FAKE_TAMPER_META", "1")
        .output()
        .unwrap();
    assert_eq!(code(&out), 1, "{}", String::from_utf8_lossy(&out.stderr));
    let state = env.state();
    assert_eq!(state["last_run"]["error"]["kind"], "verification_failed");
    // Not transient, so it counts.
    assert_eq!(state["last_run"]["transient"], serde_json::Value::Null);
    assert_eq!(state["consecutive_failures"], 1, "{state}");
}

#[test]
fn timer_retries_despite_a_failure_stamp_in_the_future() {
    // Two real failures in a row put the timer into its pause (the state then
    // carries the real configuration stamp, so nothing else ends the pause).
    // Then the record of the last failure is moved into the future, as if the
    // clock had been ahead when it was written and was corrected afterwards.
    // Capping the stamp at "now" would still push every attempt a full pause
    // ahead and freeze the retries; a future stamp means no pause, so the
    // timer must really start the run.
    let env = Env::new();
    let timer = ["run", "--trigger", "timer"];
    let failing = || {
        env.command(&timer)
            .env("FAKE_FAIL_AUTH", "1")
            .output()
            .unwrap()
    };
    let attempts = || env.fake_log().matches("ARGS backup").count();
    assert_eq!(code(&failing()), 1);
    assert_eq!(code(&failing()), 1);
    assert_eq!(attempts(), 2);
    // The pause is really in force with the true stamps.
    let out = env.run(&timer);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not retrying yet"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(attempts(), 2);

    let future = "2099-01-01T00:00:00Z";
    let mut state = env.state();
    state["last_run"]["started_at"] = serde_json::json!(future);
    state["last_run"]["finished_at"] = serde_json::json!(future);
    state["consecutive_failures"] = serde_json::json!(5);
    fs::write(env.root().join("state/state.json"), state.to_string()).unwrap();

    let out = env.run(&timer);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        attempts(),
        3,
        "the timer run must really start, not stay frozen behind the future stamp: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(env.state()["last_run"]["state"], "succeeded");
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
fn status_errors_when_the_config_exists_but_cannot_be_read() {
    // A config that exists but cannot be read (0640 root:root for a group
    // member) must not be shown as the built-in defaults: status errors with a
    // non-zero exit code. Only a missing file means "not configured".
    let env = Env::new();
    let cfg = env.root().join("etc/config.toml");
    fs::set_permissions(&cfg, fs::Permissions::from_mode(0o000)).unwrap();
    let out = env.run(&["status", "--json"]);
    assert_ne!(
        code(&out),
        0,
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("config"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Restore so the tempdir cleans up.
    fs::set_permissions(&cfg, fs::Permissions::from_mode(0o644)).unwrap();

    // A missing config is "unconfigured", not an error.
    fs::remove_file(&cfg).unwrap();
    let out = env.run(&["status", "--json"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["health"], "unconfigured");
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
fn a_copy_inside_a_backup_source_counts_as_on_this_computer() {
    // Whatever disk it lies on (an external disk that is itself a source
    // looks removable), a copy inside what the backup contains exists only
    // together with the data and, in the backup, encrypted with itself.
    let env = Env::new();
    env.set_key_exported(false);
    let stderr = |out: &Output| String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = |out: &Output| String::from_utf8_lossy(&out.stdout).into_owned();
    let inside = env.root().join("src/home/alice/key-copy.json");
    let inside = inside.to_str().unwrap();

    // Export says so.
    let out = env.root_cmd(&["key", "export", inside]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("part of the backup"),
        "{}",
        stdout(&out)
    );
    // A link to the copy from outside the sources does not hide it.
    let link = env.root().join("link.json");
    std::os::unix::fs::symlink(inside, &link).unwrap();
    // Confirming needs the statement, and the message names the reason.
    for file in [inside, link.to_str().unwrap()] {
        let out = env.root_cmd(&["key", "confirm-exported", "--file", file]);
        assert_eq!(code(&out), 1, "{file}");
        assert!(
            stderr(&out).contains("part of the backup"),
            "{}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("--accept-local-copy"),
            "{}",
            stderr(&out)
        );
    }
    assert!(env.state()["key_exported_digest"].is_null());
    // Import of the same file does not count as exported either.
    env.set_state("key_fingerprint", serde_json::json!("aa:01"));
    let out = env.root_cmd(&["key", "import", inside]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(env.state()["key_exported_digest"].is_null());
    // The statement still works.
    let out = env.root_cmd(&[
        "key",
        "confirm-exported",
        "--file",
        inside,
        "--accept-local-copy",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
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

    // The files are remembered (their paths are not secret); a paper key
    // piped to another program leaves nothing to remember.
    let remembered = || -> Vec<String> {
        env.state()["paperkey_files"]
            .as_array()
            .map(|files| {
                files
                    .iter()
                    .map(|f| f["path"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(
        remembered(),
        vec![canonical_name(&txt), canonical_name(&html)]
    );
    assert!(env.root_cmd(&["key", "paperkey"]).status.success());
    assert_eq!(remembered().len(), 2);

    // Saved paper keys are proof, as files ...
    for file in [&txt, &html] {
        let out = env.root_cmd(&["key", "verify", file.to_str().unwrap()]);
        assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stdout));
    }
    // ... also as the statement-backed confirmation (on this disk).
    let out = env.root_cmd(&[
        "key",
        "confirm-exported",
        "--file",
        html.to_str().unwrap(),
        "--accept-local-copy",
    ]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(code(&env.run(&["run"])), 0);
    env.set_key_exported(false);
    let stderr = |out: &Output| String::from_utf8_lossy(&out.stderr).into_owned();

    // The shortcut: the paper key was written to a file on this computer,
    // the code is read off the file, and no paper was ever printed. This
    // exact way is refused, with or without the statement, and the message
    // names the file and says what to do.
    let typed = code_of.replace('-', "").to_lowercase();
    for extra in [&[][..], &["--printed"][..]] {
        let mut args = vec!["key", "confirm-exported", "--paper-code", code_of.as_str()];
        args.extend_from_slice(extra);
        let out = env.root_cmd(&args);
        assert_eq!(code(&out), 1, "{extra:?}");
        let message = stderr(&out);
        assert!(
            message.contains(&canonical_name(&txt)) || message.contains(&canonical_name(&html)),
            "the message names the file: {message}"
        );
        assert!(
            message.contains("delete the file or move it to a USB stick"),
            "{message}"
        );
        assert!(message.contains("Nothing was recorded"), "{message}");
        assert!(env.state()["key_exported_digest"].is_null());
        assert_eq!(code(&env.run(&["run"])), 1);
    }

    // One of two files gone is not enough ...
    fs::remove_file(&txt).unwrap();
    let out = env.root_cmd(&[
        "key",
        "confirm-exported",
        "--paper-code",
        &typed,
        "--printed",
    ]);
    assert_eq!(code(&out), 1);
    assert!(
        stderr(&out).contains(&canonical_name(&html)),
        "{}",
        stderr(&out)
    );
    // ... after printing, the files are deleted. Now the statement decides.
    fs::remove_file(&html).unwrap();
    // No statement (no terminal, no --printed): nothing is recorded, and the
    // message says why.
    let out = env.root_cmd(&["key", "confirm-exported", "--paper-code", &typed]);
    assert_eq!(code(&out), 1);
    assert!(stderr(&out).contains("--printed"), "{}", stderr(&out));
    assert!(env.state()["key_exported_digest"].is_null());
    assert_eq!(code(&env.run(&["run"])), 1);
    // A wrong or malformed code fails even with the statement.
    let wrong = if code_of == "0000-0000" {
        "1111-1111"
    } else {
        "0000-0000"
    };
    for bad in [wrong, "nonsense"] {
        let out = env.root_cmd(&["key", "confirm-exported", "--paper-code", bad, "--printed"]);
        assert_eq!(code(&out), 1, "{bad}");
    }
    assert!(env.state()["key_exported_digest"].is_null());
    assert_eq!(code(&env.run(&["run"])), 1);
    // --printed belongs to --paper-code only.
    let out = env.root_cmd(&["key", "confirm-exported", "--printed"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    // The statement and the right code (lower case, no hyphen) confirm.
    let out = env.root_cmd(&[
        "key",
        "confirm-exported",
        "--paper-code",
        &typed,
        "--printed",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(code(&env.run(&["run"])), 0);
}

fn canonical_name(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_str()
        .unwrap()
        .to_string()
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
fn a_file_listing_without_crypt_modes_still_passes_the_read_back() {
    // Older servers list the files of a snapshot without `crypt-mode`. The
    // metadata blob must still be found and read back (with the key, which
    // no longer depends on what the listing says).
    let env = Env::new();
    let out = env
        .command(&["run", "--trigger", "cli"])
        .env("FAKE_NO_CRYPT_MODE", "1")
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let listing = fs::read_to_string(env.root().join("fake-store/files.json")).unwrap();
    assert!(!listing.contains("crypt-mode\":\"encrypt"), "{listing}");
    assert!(listing.contains("nestegg-meta.conf.blob"), "{listing}");
    let state = env.state();
    assert_eq!(state["last_success"]["state"], "succeeded");
    assert_eq!(state["last_success"]["verified"], true);
    let log = env.fake_log();
    assert!(log.contains("ARGS restore host/test/"), "{log}");
}

#[test]
fn cli_restore_refuses_an_unprotected_archive_when_encryption_is_on() {
    // With encryption configured, an archive that is neither encrypted nor
    // signed (an old snapshot, or one planted by someone without the key) is
    // not restored silently: the CLI refuses it and names --no-key (decision
    // 0016).
    let env = Env::new();
    env.set_files(r#"[{"filename":"home.pxar.didx","crypt-mode":"none","size":10}]"#);
    let target = env.root().join("restore-out");
    let snapshot = "host/test/2026-11-06T21:20:00Z";
    let out = env.root_cmd(&["restore", snapshot, "home.pxar", target.to_str().unwrap()]);
    assert_eq!(code(&out), 1, "{}", String::from_utf8_lossy(&out.stderr));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--no-key"), "{err}");
    assert!(err.contains("not protected by the encryption key"), "{err}");
    assert!(!target.exists(), "nothing was restored");

    // With --no-key the key refusal is gone (it may still fail later for other
    // reasons in this harness, but not for the missing key).
    let out = env.root_cmd(&[
        "restore",
        "--no-key",
        snapshot,
        "home.pxar",
        target.to_str().unwrap(),
    ]);
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("not protected by the encryption key"),
        "--no-key must bypass the refusal: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_paper_key_redirected_into_a_file_is_remembered_like_an_output_file() {
    // `nestegg key paperkey > key.txt` instead of `--output key.txt`.
    let env = Env::new();
    env.set_key_exported(false);
    let file = env.root().join("redirected-paper-key.txt");
    let out = env
        .command(&["key", "paperkey"])
        .env("NESTEGG_ALLOW_NON_ROOT", "1")
        .env_remove("SUDO_UID")
        .stdout(fs::File::create(&file).unwrap())
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let text = fs::read_to_string(&file).unwrap();
    let check_code = text
        .lines()
        .find_map(|l| l.strip_prefix("Check code:"))
        .expect("the paper key has a check code")
        .trim()
        .to_string();
    let remembered: Vec<String> = env.state()["paperkey_files"]
        .as_array()
        .expect("the redirected file is remembered")
        .iter()
        .map(|f| f["path"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        remembered,
        vec![
            fs::canonicalize(&file)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        ]
    );

    // Reading the code off that file is the shortcut: refused while the
    // file lies here, also with the statement.
    let confirm = || {
        env.root_cmd(&[
            "key",
            "confirm-exported",
            "--paper-code",
            &check_code,
            "--printed",
        ])
    };
    let out = confirm();
    assert_eq!(code(&out), 1);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("redirected-paper-key.txt"), "{err}");
    assert!(env.state()["key_exported_digest"].is_null());
    // With the file gone the statement counts.
    fs::remove_file(&file).unwrap();
    let out = confirm();
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));

    // The two statements cannot be mixed up.
    let out = env.root_cmd(&["key", "confirm-exported", "--file", "x", "--printed"]);
    assert_eq!(code(&out), 2, "{}", String::from_utf8_lossy(&out.stderr));
    let out = env.root_cmd(&[
        "key",
        "confirm-exported",
        "--paper-code",
        &check_code,
        "--accept-local-copy",
    ]);
    assert_eq!(code(&out), 2, "{}", String::from_utf8_lossy(&out.stderr));
}

impl Env {
    /// Paths remembered as paper key files in the state.
    fn remembered_paper_keys(&self) -> Vec<String> {
        self.state()["paperkey_files"]
            .as_array()
            .map(|files| {
                files
                    .iter()
                    .map(|f| f["path"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `key paperkey` with standard output redirected to `stdout`.
    fn paperkey_to(&self, stdout: Stdio) -> Output {
        self.command(&["key", "paperkey"])
            .env("NESTEGG_ALLOW_NON_ROOT", "1")
            .env_remove("SUDO_UID")
            .stdout(stdout)
            .output()
            .unwrap()
    }

    /// Remember `paths` as paper key files for the stored key.
    fn remember_paper_keys(&self, paths: &[PathBuf]) {
        let digest = nestegg::secrets::digest(&nestegg::secrets::Secret::new(
            fs::read(self.root().join("cred/encryption-key")).unwrap(),
        ));
        self.set_state(
            "paperkey_files",
            serde_json::Value::Array(
                paths
                    .iter()
                    .map(|p| serde_json::json!({"path": p, "key_digest": digest}))
                    .collect(),
            ),
        );
    }
}

#[test]
fn a_paper_key_to_dev_null_or_a_pipe_is_written_and_nothing_is_remembered() {
    let env = Env::new();
    env.set_key_exported(false);
    let out = env.paperkey_to(Stdio::null());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(env.remembered_paper_keys().is_empty());
    // A pipe: the key is on standard output, nothing to remember.
    let out = env.paperkey_to(Stdio::piped());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("BEGIN PROXMOX BACKUP KEY"),
        "the key is written to the pipe"
    );
    assert!(env.remembered_paper_keys().is_empty());
}

impl Env {
    /// The digest of the key that `key paperkey` last wrote out, as recorded.
    fn last_exported_digest(&self) -> Option<String> {
        self.state()["key_last_exported_digest"]
            .as_str()
            .map(str::to_string)
    }

    fn stored_key_digest(&self) -> String {
        nestegg::secrets::digest(&nestegg::secrets::Secret::new(
            fs::read(self.root().join("cred/encryption-key")).unwrap(),
        ))
    }
}

#[test]
fn a_paper_key_into_a_pipe_is_recorded_only_after_it_was_written_completely() {
    let env = Env::new();
    env.set_key_exported(false);
    assert_eq!(env.last_exported_digest(), None);
    // A reader that closes at once (`| head -1` closing early, a printer that
    // died): the write fails, so nothing is recorded and the exit code says so.
    let mut child = env
        .command(&["key", "paperkey"])
        .env("NESTEGG_ALLOW_NON_ROOT", "1")
        .env_remove("SUDO_UID")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    assert_ne!(code(&out), 0);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("could not be written completely"), "{err}");
    assert!(err.contains("nothing was recorded"), "{err}");
    assert_eq!(env.last_exported_digest(), None, "state unchanged");
    // A reader that reads everything: recorded, and the lock is free again.
    let out = env.paperkey_to(Stdio::piped());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("BEGIN PROXMOX BACKUP KEY"));
    assert_eq!(env.last_exported_digest(), Some(env.stored_key_digest()));
    let lock_file = fs::File::open(env.root().join("run/run.lock")).unwrap();
    assert!(
        nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusiveNonblock).is_ok()
    );
}

#[test]
fn a_paper_key_redirected_into_an_already_remembered_file_is_allowed_with_50_files() {
    let env = Env::new();
    env.set_key_exported(false);
    let files: Vec<PathBuf> = (0..50)
        .map(|i| env.write_user_file(&format!("old-{i}.txt"), "x").into())
        .collect();
    env.remember_paper_keys(&files);
    // The shell empties the file (the redirect), the target is in the list: it
    // is no new entry, so the key is written.
    let target = &files[7];
    let out = env.paperkey_to(Stdio::from(fs::File::create(target).unwrap()));
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        fs::read_to_string(target)
            .unwrap()
            .contains("BEGIN PROXMOX BACKUP KEY")
    );
    let remembered = env.remembered_paper_keys();
    assert_eq!(remembered.len(), 50);
    assert_eq!(
        remembered
            .iter()
            .filter(|p| p.as_str() == target.to_str().unwrap())
            .count(),
        1,
        "no duplicate"
    );
    assert_eq!(env.last_exported_digest(), Some(env.stored_key_digest()));
    // Another file is still refused while all 50 exist, and stays empty.
    let other = env.root().join("other.txt");
    let out = env.paperkey_to(Stdio::from(fs::File::create(&other).unwrap()));
    assert_eq!(code(&out), 1);
    assert_eq!(fs::metadata(&other).unwrap().len(), 0);
    assert_eq!(env.remembered_paper_keys().len(), 50);
}

#[test]
fn a_paper_key_file_name_that_is_not_there_yet_is_remembered_in_its_canonical_form() {
    // `--output` names a file that does not exist yet; before and after writing
    // it is the same entry (here through a link to the directory).
    let env = Env::new();
    env.set_key_exported(false);
    let real = env.root().join("real-dir");
    fs::create_dir(&real).unwrap();
    let link = env.root().join("link-dir");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let args = |f: &Path| {
        vec![
            "key".to_string(),
            "paperkey".to_string(),
            "--output".to_string(),
            f.to_str().unwrap().to_string(),
        ]
    };
    let a = args(&link.join("one.txt"));
    let out = env.root_cmd(&a.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        env.remembered_paper_keys(),
        vec![canonical_name(&real.join("one.txt"))]
    );
}

#[test]
fn a_paper_key_redirected_to_a_file_with_a_name_that_cannot_be_stored_is_not_written() {
    use std::os::unix::ffi::OsStrExt;
    let env = Env::new();
    env.set_key_exported(false);
    let odd = env
        .root()
        .join(std::ffi::OsStr::from_bytes(b"paper-\xff-key.txt"));
    let out = env.paperkey_to(Stdio::from(fs::File::create(&odd).unwrap()));
    assert_eq!(code(&out), 1);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("not valid UTF-8"), "{err}");
    assert!(err.contains("Nothing was written"), "{err}");
    // The shell's empty file is all there is: the key was not written.
    assert_eq!(fs::metadata(&odd).unwrap().len(), 0);
    assert!(env.remembered_paper_keys().is_empty());
    // With a plain name it works.
    let plain = env.root().join("plain.txt");
    let out = env.paperkey_to(Stdio::from(fs::File::create(&plain).unwrap()));
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        fs::read_to_string(&plain)
            .unwrap()
            .contains("BEGIN PROXMOX")
    );
}

#[test]
fn a_remembered_paper_key_file_that_exists_is_never_forgotten() {
    let env = Env::new();
    env.set_key_exported(false);
    let files: Vec<PathBuf> = (0..50)
        .map(|i| env.write_user_file(&format!("old-{i}.html"), "x").into())
        .collect();
    env.remember_paper_keys(&files);
    // The 51st would push out the oldest, which still exists: refused, with
    // a message that says to delete old ones; nothing is written.
    let new = env.root().join("new.html");
    let args = ["key", "paperkey", "--output", new.to_str().unwrap()];
    let out = env.root_cmd(&args);
    assert_eq!(code(&out), 1);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("delete the others"), "{err}");
    assert!(
        err.contains("old-0.html") && err.contains("old-49.html"),
        "{err}"
    );
    assert!(!new.exists());
    assert_eq!(env.remembered_paper_keys().len(), 50);
    // The same for a redirect: the shell's empty file stays empty.
    let redirected = env.root().join("redirected.txt");
    let out = env.paperkey_to(Stdio::from(fs::File::create(&redirected).unwrap()));
    assert_eq!(code(&out), 1);
    assert_eq!(fs::metadata(&redirected).unwrap().len(), 0);
    // A pipe leaves no file and is not affected.
    assert_eq!(code(&env.paperkey_to(Stdio::piped())), 0);
    // Three files deleted: entries whose file is gone make room, the others
    // stay, the new one is added.
    for f in &files[..3] {
        fs::remove_file(f).unwrap();
    }
    let out = env.root_cmd(&args);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let remembered = env.remembered_paper_keys();
    assert_eq!(remembered.len(), 48);
    assert!(remembered.contains(&canonical_name(&new)));
    assert!(remembered.contains(&files[3].to_str().unwrap().to_string()));
    assert!(!remembered.contains(&files[0].to_str().unwrap().to_string()));
}

#[test]
fn the_refusal_for_a_paper_key_file_that_stays_names_the_way_out() {
    let env = Env::new();
    env.set_key_exported(false);
    let file = env.root().join("key.html");
    let out = env.root_cmd(&[
        "key",
        "paperkey",
        "--format",
        "html",
        "--output",
        file.to_str().unwrap(),
    ]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let check_code = nestegg::keyproof::KeyMaterial::from_key_json(KEY_JSON)
        .unwrap()
        .check_code();
    let out = env.root_cmd(&[
        "key",
        "confirm-exported",
        "--paper-code",
        &check_code,
        "--printed",
    ]);
    assert_eq!(code(&out), 1);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    // The file cannot be deleted (say): the message names the other way,
    // with this very file.
    let named = canonical_name(&file);
    assert!(err.contains("cannot be deleted"), "{err}");
    assert!(
        err.contains(&format!(
            "confirm-exported --file {named} --accept-local-copy"
        )),
        "{err}"
    );
    assert!(env.state()["key_exported_digest"].is_null());
    // And that way works: the file is a copy of the key, with the statement.
    let out = env.root_cmd(&[
        "key",
        "confirm-exported",
        "--file",
        &named,
        "--accept-local-copy",
    ]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!env.state()["key_exported_digest"].is_null());
    assert_eq!(code(&env.run(&["run"])), 0);
}

#[test]
fn key_paperkey_takes_the_run_lock_and_says_to_try_again_while_a_backup_runs() {
    let env = Env::new();
    env.set_key_exported(false);
    let file = env.root().join("key.txt");
    let redirected = env.root().join("redirected.txt");
    {
        let lock_file = fs::File::create(env.root().join("run/run.lock")).unwrap();
        let _held =
            nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusive).unwrap();
        let out = env.root_cmd(&["key", "paperkey", "--output", file.to_str().unwrap()]);
        assert_eq!(code(&out), 1);
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(err.contains("a backup is running; try again"), "{err}");
        assert!(err.contains("Nothing was written"), "{err}");
        assert!(!file.exists());
        // Redirected into a file, and into a pipe: the same, nothing out.
        let out = env.paperkey_to(Stdio::from(fs::File::create(&redirected).unwrap()));
        assert_eq!(code(&out), 1);
        assert_eq!(fs::metadata(&redirected).unwrap().len(), 0);
        let out = env.paperkey_to(Stdio::piped());
        assert_eq!(code(&out), 1);
        assert!(out.stdout.is_empty());
    }
    // Backup over: it works, and the lock is not kept by the command.
    let out = env.root_cmd(&["key", "paperkey", "--output", file.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let lock_file = fs::File::open(env.root().join("run/run.lock")).unwrap();
    assert!(
        nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusiveNonblock).is_ok(),
        "the lock is free again"
    );
}

#[test]
fn a_stick_mounted_below_a_source_is_not_inside_the_backup() {
    // The client stays on the file system of a source. With another file
    // system mounted below the source, a copy there is not "part of the backup" (the
    // device then decides, here a disk of this computer or unknown); a copy
    // elsewhere in the source still is.
    let env = Env::new();
    env.set_key_exported(false);
    let stick = env.root().join("src/home/alice/STICK");
    fs::create_dir_all(&stick).unwrap();
    let on_stick = stick.join("key.json");
    let in_home = env.root().join("src/home/alice/key.json");
    let stderr = |out: &Output| String::from_utf8_lossy(&out.stderr).into_owned();
    for file in [&on_stick, &in_home] {
        let out = env.root_cmd(&["key", "export", file.to_str().unwrap()]);
        assert_eq!(code(&out), 0, "{}", stderr(&out));
    }
    let mountinfo = env.root().join("mountinfo");
    fs::write(
        &mountinfo,
        format!(
            "22 1 259:2 / / rw - ext4 /dev/sda1 rw\n40 22 8:17 / {} rw - vfat /dev/sdb1 rw\n",
            canonical_name(&stick)
        ),
    )
    .unwrap();
    let confirm = |file: &Path, mounts: bool| {
        let mut cmd = env.command(&["key", "confirm-exported", "--file", file.to_str().unwrap()]);
        cmd.env("NESTEGG_ALLOW_NON_ROOT", "1")
            .env_remove("SUDO_UID");
        if mounts {
            cmd.env("NESTEGG_MOUNTINFO", &mountinfo);
        }
        let out = cmd.output().unwrap();
        assert_eq!(code(&out), 1, "no statement: refused");
        stderr(&out)
    };
    // With the mount table: the stick is outside, the rest of the source in.
    let msg = confirm(&on_stick, true);
    assert!(!msg.contains("lies inside"), "{msg}");
    assert!(confirm(&in_home, true).contains("lies inside"));
    // Without a mount there (the table as it is): everything below the
    // source is inside.
    let plain = env.root().join("mountinfo-plain");
    fs::write(&plain, "22 1 259:2 / / rw - ext4 /dev/sda1 rw\n").unwrap();
    let mut cmd = env.command(&[
        "key",
        "confirm-exported",
        "--file",
        on_stick.to_str().unwrap(),
    ]);
    cmd.env("NESTEGG_ALLOW_NON_ROOT", "1")
        .env_remove("SUDO_UID")
        .env("NESTEGG_MOUNTINFO", &plain);
    assert!(stderr(&cmd.output().unwrap()).contains("lies inside"));
    // A second mount of the source's own file system there (a bind mount: the
    // same device number): the client backs it up, so the copy is inside.
    let bind = env.root().join("mountinfo-bind");
    fs::write(
        &bind,
        format!(
            "22 1 259:2 / / rw - ext4 /dev/sda1 rw\n40 22 259:2 /x {} rw - ext4 /dev/sda1 rw\n",
            canonical_name(&stick)
        ),
    )
    .unwrap();
    let mut cmd = env.command(&[
        "key",
        "confirm-exported",
        "--file",
        on_stick.to_str().unwrap(),
    ]);
    cmd.env("NESTEGG_ALLOW_NON_ROOT", "1")
        .env_remove("SUDO_UID")
        .env("NESTEGG_MOUNTINFO", &bind);
    assert!(stderr(&cmd.output().unwrap()).contains("lies inside"));
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

// ---------------------------------------------------------------- notify

impl Env {
    /// A configuration with a notification hook that appends
    /// `event|message` lines to `hook.log` (the desktop channel is off), and
    /// the backup timer switched on.
    fn with_notify_hook(&self) {
        let hook = self.root().join("hook.sh");
        fs::write(
            &hook,
            "#!/bin/sh\nprintf '%s|%s\\n' \"$NESTEGG_EVENT\" \"$NESTEGG_MESSAGE\" >> \"$(dirname \"$0\")/hook.log\"\n",
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        self.write_config(&format!(
            "\n[notify]\ncommand = [\"{}\"]\ndesktop = false\n",
            hook.display()
        ));
        fs::write(self.root().join("timer-state"), "enabled\n").unwrap();
        // A clean situation but for what the test sets up: the optional source
        // exists (the backup is complete) and the server found nothing damaged.
        fs::create_dir_all(self.root().join("does-not-exist")).unwrap();
        let mut state = self.state();
        state["server_verify_failed"] = serde_json::json!([]);
        state["last_success"]["incomplete"] = serde_json::json!([]);
        fs::write(self.root().join("state/state.json"), state.to_string()).unwrap();
    }

    fn hook_lines(&self) -> Vec<String> {
        fs::read_to_string(self.root().join("hook.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Stamp the last success (and the last run) at `at`.
    fn stamp_success(&self, at: &str) {
        let mut state = self.state();
        for run in ["last_run", "last_success"] {
            state[run]["started_at"] = serde_json::json!(at);
            state[run]["finished_at"] = serde_json::json!(at);
        }
        fs::write(self.root().join("state/state.json"), state.to_string()).unwrap();
    }

    fn notify(&self) -> Output {
        self.root_cmd(&["notify"])
    }
}

#[test]
fn a_success_stamp_in_the_future_is_reported_as_overdue_not_as_fresh_or_recovered() {
    let env = Env::new();
    assert_eq!(code(&env.run(&["run"])), 0);
    env.with_notify_hook();
    let real = env.state()["last_success"]["finished_at"]
        .as_str()
        .unwrap()
        .to_string();

    // A fresh backup: nothing to report.
    let out = env.notify();
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("everything is fine"));
    assert!(env.hook_lines().is_empty());

    // An old one is overdue ...
    env.stamp_success(&(chrono::Utc::now() - chrono::Duration::hours(50)).to_rfc3339());
    assert_eq!(code(&env.notify()), 0);
    let lines = env.hook_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("overdue|"), "{lines:?}");
    assert!(lines[0].contains("ago"), "{lines:?}");

    // ... and when the stamp then lies in the future (the clock was ahead) the
    // check does not conclude "fresh backup" and does not announce a recovery:
    // it says the time lies in the future and the clock was wrong.
    env.stamp_success("2099-01-01T00:00:00Z");
    let out = env.notify();
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(!stdout.contains("everything is fine"), "{stdout}");
    let lines = env.hook_lines();
    assert!(
        lines.iter().all(|l| !l.starts_with("recovered|")),
        "{lines:?}"
    );
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[1].starts_with("overdue|"), "{lines:?}");
    assert!(lines[1].contains("lies in the future"), "{lines:?}");
    assert!(
        lines[1].contains("clock of this computer was wrong"),
        "{lines:?}"
    );
    // Not repeated at the next check.
    assert_eq!(code(&env.notify()), 0);
    assert_eq!(env.hook_lines().len(), 2);

    // The next backup puts a real stamp there: now it is a recovery.
    env.stamp_success(&real);
    assert_eq!(code(&env.notify()), 0);
    let lines = env.hook_lines();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines[2].starts_with("recovered|"), "{lines:?}");
}

#[test]
fn a_success_stamp_in_the_future_with_the_timer_off_is_reported_without_a_wrong_age() {
    let env = Env::new();
    assert_eq!(code(&env.run(&["run"])), 0);
    env.with_notify_hook();
    fs::write(env.root().join("timer-state"), "disabled\n").unwrap();
    env.stamp_success("2099-01-01T00:00:00Z");
    assert_eq!(code(&env.notify()), 0);
    let lines = env.hook_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("automatic_off|"), "{lines:?}");
    assert!(lines[0].contains("lies in the future"), "{lines:?}");
    assert!(!lines[0].contains("not been backed up"), "{lines:?}");
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

#[test]
fn a_run_lock_blocked_without_a_run_is_an_error_and_recorded() {
    // Something that is not a backup run holds a shared lock on the run lock
    // file. That must not be a silent "already running" with exit 0: the timer
    // run fails, says who holds the lock, is recorded (health failing) and
    // uploads nothing.
    let env = Env::new();
    // An earlier success, long ago: a timer backup is due.
    assert_eq!(code(&env.run(&["run", "--trigger", "cli"])), 0);
    let old = "2000-01-01T00:00:00Z";
    let mut state = env.state();
    state["last_success"]["finished_at"] = serde_json::json!(old);
    state["last_run"]["finished_at"] = serde_json::json!(old);
    fs::write(env.root().join("state/state.json"), state.to_string()).unwrap();
    let lock_path = env.root().join("run/run.lock");
    let holder = fs::File::open(&lock_path).unwrap();
    let shared = nix::fcntl::Flock::lock(holder, nix::fcntl::FlockArg::LockShared).unwrap();
    let out = env.run(&["run", "--trigger", "timer"]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(code(&out), 1, "{err}");
    assert!(err.contains("no backup is running"), "{err}");
    if fs::read_to_string("/proc/locks").is_ok() {
        assert!(
            err.contains(&format!("pid {}", std::process::id())),
            "names the holder: {err}"
        );
    }
    assert_eq!(env.fake_log().matches("ARGS backup").count(), 1);
    let state = env.state();
    assert_eq!(state["last_run"]["state"], "failed", "{state}");
    assert!(
        state["last_run"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("run lock"),
        "{state}"
    );
    let out = env.run(&["status", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["health"], "failing");
    // A run the daemon promised gets its final record under its own ID; its
    // own reservation in current.json is not taken for another run.
    let id = "20261108T120000Z-manual-beef";
    fs::write(
        env.root().join("state/current.json"),
        serde_json::json!({"id": id, "trigger": "manual", "state": "starting",
                           "started_at": nestegg::state::format_time(chrono::Utc::now())})
        .to_string(),
    )
    .unwrap();
    let out = env.run(&["run", "--trigger", "manual", "--run-id", id]);
    assert_eq!(code(&out), 1, "{}", String::from_utf8_lossy(&out.stderr));
    let record: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(env.root().join(format!("state/runs/{id}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(record["state"], "failed");
    assert!(!env.root().join("state/current.json").exists());
    drop(shared);

    // A real run holds the lock exclusively: still "already running", exit 0,
    // nothing recorded.
    let holder = fs::File::open(&lock_path).unwrap();
    let exclusive = nix::fcntl::Flock::lock(holder, nix::fcntl::FlockArg::LockExclusive).unwrap();
    let before = env.state()["last_run"]["id"].clone();
    let out = env.run(&["run", "--trigger", "timer"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("in progress"));
    assert_eq!(env.state()["last_run"]["id"], before);
    drop(exclusive);

    // Two blocked attempts are two failures in a row, so the timer pauses
    // like after any other repeated failure (decision 0021); a backup started
    // by hand runs at once now that the lock is free.
    assert_eq!(env.state()["consecutive_failures"], 2);
    let out = env.run(&["run", "--trigger", "timer"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("not retrying yet"));
    let out = env.run(&["run", "--trigger", "cli"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.fake_log().matches("ARGS backup").count(), 2);
}

#[test]
fn a_success_stamp_in_the_future_makes_the_timer_back_up_at_once() {
    let env = Env::new();
    let timer = ["run", "--trigger", "timer"];
    assert_eq!(code(&env.run(&timer)), 0);
    assert_eq!(env.fake_log().matches("ARGS backup").count(), 1);
    // The clock was ahead when that backup finished, and was corrected.
    let future = "2099-01-01T00:00:00Z";
    let mut state = env.state();
    for run in ["last_success", "last_run"] {
        state[run]["started_at"] = serde_json::json!(future);
        state[run]["finished_at"] = serde_json::json!(future);
    }
    fs::write(env.root().join("state/state.json"), state.to_string()).unwrap();
    // Not "ok" on the strength of that stamp, with the reason.
    let out = env.run(&["status", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["health"], "overdue");
    let out = env.run(&["status"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("lies in the future"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    // The timer backs up at once and records a correct stamp.
    let out = env.run(&timer);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.fake_log().matches("ARGS backup").count(), 2);
    let state = env.state();
    assert_eq!(state["last_success"]["state"], "succeeded");
    assert!(
        !state["last_success"]["finished_at"]
            .as_str()
            .unwrap()
            .starts_with("2099"),
        "{state}"
    );
    let out = env.run(&["status", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    // (The harness config has an optional source that does not exist, so a
    // good backup here is "incomplete".)
    assert_eq!(status["health"], "incomplete");
}

#[test]
fn only_the_first_unavailable_read_back_in_a_row_is_free() {
    // A server that always falls asleep right after the upload must not cause
    // a full upload at every hourly check forever: the first unavailable
    // read-back is transient, the next one directly after it counts.
    let env = Env::new();
    let sentinel = env.root().join("readback-started");
    let hanging = || {
        env.command(&["run", "--trigger", "cli"])
            .env("FAKE_READBACK_BLOCK", &sentinel)
            .env("NESTEGG_READBACK_TIMEOUT_SECS", "1")
            .output()
            .unwrap()
    };
    assert_eq!(code(&hanging()), 1);
    let state = env.state();
    assert_eq!(state["last_run"]["transient"], true, "{state}");
    assert_eq!(state["consecutive_failures"], 0, "{state}");
    assert_eq!(state["readback_unavailable_in_a_row"], 1, "{state}");

    let out = hanging();
    assert_eq!(code(&out), 1);
    let state = env.state();
    assert_eq!(state["last_run"]["error"]["kind"], "verification_failed");
    assert_eq!(
        state["last_run"]["transient"],
        serde_json::Value::Null,
        "{state}"
    );
    assert_eq!(state["consecutive_failures"], 1, "{state}");
    assert!(
        state["last_run"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("again"),
        "{state}"
    );

    // A completed read-back resets it: the next unavailable one is free again.
    assert_eq!(code(&env.run(&["run", "--trigger", "cli"])), 0);
    assert_eq!(
        env.state()["readback_unavailable_in_a_row"],
        serde_json::Value::Null
    );
    assert_eq!(code(&hanging()), 1);
    assert_eq!(env.state()["last_run"]["transient"], true);
}

#[test]
fn a_server_error_that_says_timed_out_is_a_real_read_back_failure() {
    // The words "timed out" in an error from the server do not make a failed
    // read-back transient: only nestegg's own time limits and transport
    // failures of the client do (decided by type).
    let env = Env::new();
    let out = env
        .command(&["run", "--trigger", "cli"])
        // The upload's own listing is not affected (FAKE_FILES_ERROR only
        // hits `snapshot files`, which the read-back calls first).
        .env(
            "FAKE_FILES_ERROR",
            "Error: verification task timed out on the server",
        )
        .output()
        .unwrap();
    assert_eq!(code(&out), 1, "{}", String::from_utf8_lossy(&out.stderr));
    let state = env.state();
    assert_eq!(state["last_run"]["error"]["kind"], "verification_failed");
    assert_eq!(
        state["last_run"]["transient"],
        serde_json::Value::Null,
        "{state}"
    );
    assert_eq!(state["consecutive_failures"], 1, "{state}");

    // A transport failure of the client is an unavailable server.
    let out = env
        .command(&["run", "--trigger", "cli"])
        .env(
            "FAKE_FILES_ERROR",
            "Error: error trying to connect: tcp connect error: Connection refused (os error 111)",
        )
        .output()
        .unwrap();
    assert_eq!(code(&out), 1);
    assert_eq!(env.state()["last_run"]["transient"], true);
}

#[test]
fn cli_restore_of_an_archive_that_is_not_in_the_snapshot_says_so() {
    let env = Env::new();
    env.set_files(r#"[{"filename":"etc.pxar.didx","crypt-mode":"encrypt","size":10}]"#);
    let target = env.root().join("restore-out");
    let snapshot = "host/test/2026-11-06T21:20:00Z";
    let out = env.root_cmd(&["restore", snapshot, "home.pxar", target.to_str().unwrap()]);
    assert_eq!(code(&out), 1);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(&format!("home.pxar is not in {snapshot}")),
        "{err}"
    );
    assert!(!err.contains("not protected"), "{err}");
    assert!(!target.exists());
}

#[test]
fn check_says_when_the_server_was_not_woken_again() {
    // Through the daemon (the test does not run as root): a wake attempt that
    // really sent a packet starts the one-minute interval; the next `check
    // --wake` within it says the server was not woken again.
    let mut env = Env::new();
    env.close_pbs();
    let config = env.root().join("etc/config.toml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(
        "\n[wake]\nmac = \"00:00:5e:00:53:01\"\nbroadcast = \"127.0.0.1\"\nport = 9\n\
         timeout_seconds = 1\n",
    );
    fs::write(&config, text).unwrap();
    let _daemon = start_daemon(&env);
    let out = env.run(&["check", "--wake"]);
    let first = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(!first.contains("not woken again"), "{first}");
    assert!(first.contains("Wake-on-LAN"), "{first}");
    let out = env.run(&["check", "--wake"]);
    let second = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(second.contains("was not woken again"), "{second}");
}
