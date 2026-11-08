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
esac
exit 0
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
        state["key_exported"] = serde_json::json!(exported);
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
    let mut s = UnixStream::connect(socket).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.0\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n"
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
    assert!(id.ends_with("-manual"));

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

    let (code, err) = request(&socket, "GET", "/api/v1/runs/..%2Fetc");
    assert_eq!(code, 404, "{err}");
    let (code, err) = request(&socket, "POST", "/api/v1/restores");
    assert_eq!(code, 501, "{err}");
    assert_eq!(err["code"], "not_implemented");
}
