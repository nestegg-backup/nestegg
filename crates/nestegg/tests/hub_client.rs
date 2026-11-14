//! Client integration tests. TLS stays in-process; every local command and
//! filesystem path is isolated. No systemd, accounts or real PBS are touched.
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use nestegg::config::{Config, Paths};
use nestegg::state::Store;
use serde_json::{Value, json};

type Requests = Arc<Mutex<Vec<(String, String, Value)>>>;
struct Hub {
    url: String,
    pin: String,
    requests: Requests,
    responses: Arc<Mutex<VecDeque<(u16, Value)>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Hub {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Hub {
    async fn start() -> Self {
        let cert =
            rcgen::generate_simple_self_signed(vec!["unrelated.example.com".into()]).unwrap();
        let pin = nestegg::pinned::certificate_pin(cert.cert.der()).unwrap();
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        let requests: Requests = Default::default();
        let responses: Arc<Mutex<VecDeque<(u16, Value)>>> = Default::default();
        let seen = requests.clone();
        let replies = responses.clone();
        let task = tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
            while let Ok((socket, _)) = listener.accept().await {
                let Ok(tls) = acceptor.accept(socket).await else {
                    continue;
                };
                let requests = seen.clone();
                let responses = replies.clone();
                let service = hyper::service::service_fn(
                    move |request: hyper::Request<hyper::body::Incoming>| {
                        let requests = requests.clone();
                        let responses = responses.clone();
                        async move {
                            let path = request.uri().path().to_string();
                            let auth = request
                                .headers()
                                .get("authorization")
                                .map(|v| v.to_str().unwrap().to_string())
                                .unwrap_or_default();
                            let body = request.into_body().collect().await.unwrap().to_bytes();
                            requests.lock().unwrap().push((
                                path,
                                auth,
                                serde_json::from_slice(&body).unwrap(),
                            ));
                            let (status, body) = responses
                                .lock()
                                .unwrap()
                                .pop_front()
                                .expect("unexpected hub request");
                            if status == 0 {
                                return Err(std::io::Error::other("deliberately dropped response"));
                            }
                            Ok::<_, std::io::Error>(
                                hyper::Response::builder()
                                    .status(status)
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from(if status == 204 {
                                        vec![]
                                    } else {
                                        serde_json::to_vec(&body).unwrap()
                                    })))
                                    .unwrap(),
                            )
                        }
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                    .await;
            }
        });
        Self {
            url,
            pin,
            requests,
            responses,
            task,
        }
    }
    fn reply(&self, status: u16, value: Value) {
        self.responses.lock().unwrap().push_back((status, value));
    }
    fn success(&self) -> Value {
        let pem = include_str!("fixtures/master-public.fixture");
        json!({"machine_id":"fixture-machine", "existing_snapshots":0, "backup_id":"desktop", "machine_credential":"fixture-machine-credential", "pbs":{
            "server":"pbs.example.com", "port":8007, "datastore":"backups", "namespace":"desktops", "fingerprint":null,
            "auth_id":"backup@pbs!desktop", "secret":"fixture-pbs-credential"},
            "master_key":{"public_key_pem":pem, "fingerprint":nestegg::masterkey::fingerprint(pem.as_bytes()).unwrap(), "proven_at":"2026-01-01T12:00:00Z"},
            "hub":{"name":"Home", "url":self.url, "pin":self.pin},
            "wake":{"mac":"00:00:5e:00:53:01","broadcast":"192.0.2.255","timeout_seconds":90}, "future_field":"ignored"})
    }
}

struct Computer {
    dir: tempfile::TempDir,
    paths: Paths,
    client: PathBuf,
    systemctl: PathBuf,
}
fn script(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}
impl Computer {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = Paths {
            config_file: root.join("config.toml"),
            credentials_dir: root.join("credentials"),
            state_dir: root.join("state"),
            runtime_dir: root.join("run"),
            systemctl: root.join("systemctl"),
        };
        let client = root.join("client");
        let systemctl = paths.systemctl.clone();
        script(
            &client,
            "#!/bin/sh\ncase \"$1 $2\" in\n'version ') echo client-fixture;;\n'key create') for last do :; done; printf '%s' '{\"data\":\"fixture-key-material\",\"kdf\":\"none\"}' > \"$last\";;\n'key show') printf '%s' '{\"fingerprint\":\"fixture-fingerprint\",\"kdf\":\"none\"}';;\n*) exit 99;;\nesac\n",
        );
        script(
            &systemctl,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$NESTEGG_TEST_LOG\"\ncase \"$1\" in\nis-enabled) echo disabled; exit 1;;\nis-active) echo inactive; exit 3;;\nesac\nif [ \"$NESTEGG_TEST_FAIL_STEP\" = \"interrupt-$1\" ]; then kill -KILL \"$PPID\"; exit 1; fi\n[ \"$1\" != \"$NESTEGG_TEST_FAIL_STEP\" ]\n",
        );
        let original = std::fs::read_to_string(&systemctl).unwrap();
        let check = r#"if [ "$1" = start ]; then
  test ! -d "$NESTEGG_STATE_DIR/join-transaction" || exit 97
  flock -n "$NESTEGG_RUNTIME_DIR/run.lock" true || exit 98
fi
"#;
        script(
            &systemctl,
            &original.replacen("#!/bin/sh\n", &format!("#!/bin/sh\n{check}"), 1),
        );
        Self {
            dir,
            paths,
            client,
            systemctl,
        }
    }
    async fn command(&self, args: Vec<String>, failure: &str) -> Output {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_nestegg"))
            .args(args)
            .env("NESTEGG_CONFIG", &self.paths.config_file)
            .env("NESTEGG_CREDENTIALS_DIR", &self.paths.credentials_dir)
            .env("NESTEGG_STATE_DIR", &self.paths.state_dir)
            .env("NESTEGG_RUNTIME_DIR", &self.paths.runtime_dir)
            .env("NESTEGG_PBS_CLIENT", &self.client)
            .env("NESTEGG_SYSTEMCTL", &self.systemctl)
            .env(
                "NESTEGG_SYSTEMD_CREDS",
                self.dir.path().join("no-systemd-creds"),
            )
            .env("NESTEGG_ALLOW_NON_ROOT", "1")
            .env("NESTEGG_TEST_FAIL_STEP", failure)
            .env("NESTEGG_TEST_LOG", self.dir.path().join("commands.log"))
            .env_remove("SUDO_USER")
            .env_remove("SUDO_UID")
            .env_remove("CREDENTIALS_DIRECTORY")
            .output()
            .await
            .unwrap()
    }
    async fn join(&self, hub: &Hub, extra: &[&str], failure: &str) -> Output {
        let mut args = vec![
            "join".into(),
            hub.url.clone(),
            "--pin".into(),
            hub.pin.clone(),
            "--code".into(),
            "fixture-join-code".into(),
            "--name".into(),
            "desktop".into(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        self.command(args, failure).await
    }
}
fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
fn success(output: &Output) {
    assert!(output.status.success(), "{}", text(output));
}

#[tokio::test]
async fn join_leave_tls_and_status_contract() {
    let hub = Hub::start().await;
    let computer = Computer::new();
    hub.reply(200, hub.success());
    let out = computer.join(&hub, &["--adopt"], "").await;
    success(&out);
    assert!(!text(&out).contains("fixture-machine-credential"));
    assert!(!text(&out).contains("fixture-pbs-credential"));
    let cfg = Config::load(&computer.paths.config_file).unwrap();
    assert!(cfg.backup.system && cfg.backup.encryption);
    assert_eq!(cfg.wake.as_ref().unwrap().timeout_seconds, 90);
    assert_eq!(cfg.hub.as_ref().unwrap().machine_id, "fixture-machine");
    assert_eq!(Config::parse(&cfg.to_toml().unwrap()).unwrap(), cfg);
    let state = Store::new(&computer.paths).load_state().unwrap();
    assert!(!state.key_exported());
    assert!(nestegg::masterkey::covered(&cfg, &state).unwrap().is_some());
    let status = nestegg::status::build(&computer.paths, Some(&cfg), true).unwrap();
    let value = serde_json::to_value(status).unwrap();
    assert_eq!(value["hub"], json!({"name":"Home", "url":hub.url}));
    assert_eq!(value["encryption"]["key_exported"], false);
    assert_eq!(
        value["encryption"]["master_key"]["proven_at"],
        "2026-01-01T12:00:00Z"
    );
    let log = std::fs::read_to_string(computer.dir.path().join("commands.log")).unwrap();
    assert!(log.contains("enable --now nestegg.service nestegg-backup.timer nestegg-notify.timer"));
    assert!(log.contains("start --no-block nestegg-backup.service"));
    for path in [
        computer.paths.credentials_dir.join("password"),
        computer.paths.credentials_dir.join("machine-credential"),
        computer.paths.credentials_dir.join("encryption-key"),
    ] {
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }
    {
        let requests = hub.requests.lock().unwrap();
        assert_eq!(requests[0].0, "/api/hub/v1/join");
        assert_eq!(requests[0].2["adopt"], true);
        assert_eq!(requests[0].2["backup_id"], "desktop");
        assert!(requests[0].2["hostname"].is_string() && requests[0].2["os"].is_string());
        assert_eq!(requests[0].2["nestegg_version"], env!("CARGO_PKG_VERSION"));
    }
    hub.reply(204, Value::Null);
    success(&computer.command(vec!["leave".into()], "").await);
    let cfg = Config::load(&computer.paths.config_file).unwrap();
    assert!(cfg.hub.is_none());
    assert!(
        computer
            .paths
            .credentials_dir
            .join("encryption-key")
            .exists()
    );
    assert!(
        !computer
            .paths
            .credentials_dir
            .join("machine-credential")
            .exists()
    );
    assert!(!computer.paths.credentials_dir.join("password").exists());
    let request = &hub.requests.lock().unwrap()[1];
    assert_eq!(request.0, "/api/hub/v1/machines/self/leave");
    assert_eq!(request.1, "Bearer fixture-machine-credential");
}

#[tokio::test]
async fn every_contract_error_is_actionable_and_no_local_setup_is_left() {
    let hub = Hub::start().await;
    for (code, status, hint) in [
        ("invalid_code", 403, "new code"),
        ("name_taken", 409, "--name"),
        ("name_exists_on_pbs", 409, "SAME computer"),
        ("pbs_unreachable", 503, "switched on"),
        ("pbs_refused", 503, "permissions"),
        ("master_key_unproven", 409, "confirmation"),
        ("no_package", 409, "package"),
        ("unauthorized", 401, "join again"),
        ("rate_limited", 429, "Wait"),
        ("not_found", 404, "--force"),
        ("bad_request", 400, "fresh command"),
        ("unknown_future_code", 500, "administrator"),
    ] {
        let computer = Computer::new();
        hub.reply(
            status,
            json!({"code":code,"message":"fixture-secret-must-not-be-echoed"}),
        );
        let out = computer.join(&hub, &[], "").await;
        assert!(!out.status.success(), "{code}");
        let message = text(&out);
        assert!(message.contains(hint), "{code}: {message}");
        assert!(!message.contains("fixture-secret-must-not-be-echoed"));
        assert!(!computer.paths.config_file.exists());
        assert!(!computer.paths.credentials_dir.join("password").exists());
        assert!(
            !computer
                .paths
                .credentials_dir
                .join("encryption-key")
                .exists()
        );
        assert_eq!(
            computer.paths.state_dir.join("join-transaction").exists(),
            status >= 500
        );
    }
}

#[tokio::test]
async fn wrong_pin_names_both_and_sends_no_code() {
    let hub = Hub::start().await;
    let computer = Computer::new();
    let wrong = nestegg::pinned::pin_der(b"different public key");
    let out = computer
        .command(
            vec![
                "join".into(),
                hub.url.clone(),
                "--pin".into(),
                wrong.clone(),
                "--code".into(),
                "fixture-code".into(),
                "--name".into(),
                "desktop".into(),
            ],
            "",
        )
        .await;
    assert!(!out.status.success());
    let message = text(&out);
    assert!(
        message.contains(&wrong) && message.contains(&hub.pin),
        "{message}"
    );
    assert!(hub.requests.lock().unwrap().is_empty());
    assert!(!computer.paths.config_file.exists());
    assert!(!computer.paths.state_dir.join("join-transaction").exists());
}

#[tokio::test]
async fn activation_failures_keep_committed_setup_and_existing_key() {
    let hub = Hub::start().await;
    for step in ["enable", "start"] {
        let computer = Computer::new();
        std::fs::create_dir_all(&computer.paths.credentials_dir).unwrap();
        let key = computer.paths.credentials_dir.join("encryption-key");
        nestegg::secrets::write_private_file(&key, b"existing-key-must-not-change").unwrap();
        let cfg = nestegg::config::example("pbs.example.com", "old-backups", "backup@pbs!desktop");
        std::fs::write(&computer.paths.config_file, cfg.to_toml().unwrap()).unwrap();
        hub.reply(200, hub.success());
        let out = computer.join(&hub, &["--replace-config"], step).await;
        assert!(!out.status.success());
        assert!(
            Config::load(&computer.paths.config_file)
                .unwrap()
                .hub
                .is_some()
        );
        assert_eq!(
            std::fs::read(&key).unwrap(),
            b"existing-key-must-not-change"
        );
        assert_eq!(
            computer.paths.state_dir.join("join-transaction").exists(),
            step == "enable"
        );
        if step == "enable" {
            success(&computer.join(&hub, &["--replace-config"], "").await);
        }
    }
    assert_eq!(hub.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn already_configured_refused_before_contact_and_replace_keeps_key() {
    let hub = Hub::start().await;
    let computer = Computer::new();
    let cfg = nestegg::config::example("pbs.example.com", "backups", "backup@pbs!desktop");
    std::fs::write(&computer.paths.config_file, cfg.to_toml().unwrap()).unwrap();
    let out = computer.join(&hub, &[], "").await;
    assert!(text(&out).contains("--replace-config"), "{}", text(&out));
    assert!(hub.requests.lock().unwrap().is_empty());
    std::fs::create_dir_all(&computer.paths.credentials_dir).unwrap();
    let key = computer.paths.credentials_dir.join("encryption-key");
    nestegg::secrets::write_private_file(&key, b"existing-key-must-not-change").unwrap();
    hub.reply(200, hub.success());
    let mut previous = nestegg::state::Run::new(nestegg::state::Trigger::Timer, chrono::Utc::now());
    previous.finish(nestegg::state::RunState::Succeeded, chrono::Utc::now());
    Store::new(&computer.paths)
        .record_finished(&previous, 10)
        .unwrap();
    let out = computer.join(&hub, &["--replace-config"], "").await;
    success(&out);
    let state = Store::new(&computer.paths).load_state().unwrap();
    assert!(state.join_backup_pending);
    assert_eq!(state.last_success.unwrap().id, previous.id);
    assert!(text(&out).contains("kept unchanged"));
    assert_eq!(
        std::fs::read(&key).unwrap(),
        b"existing-key-must-not-change"
    );
    let out = computer.join(&hub, &[], "").await;
    assert!(text(&out).contains("leave first"));
    assert_eq!(hub.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn failed_leave_needs_force_and_force_revokes_local_credential_only() {
    let hub = Hub::start().await;
    let computer = Computer::new();
    hub.reply(200, hub.success());
    success(&computer.join(&hub, &[], "").await);
    hub.reply(503, json!({"code":"pbs_unreachable"}));
    let out = computer.command(vec!["leave".into()], "").await;
    assert!(!out.status.success());
    assert!(text(&out).contains("--force"));
    assert!(
        Config::load(&computer.paths.config_file)
            .unwrap()
            .hub
            .is_some()
    );
    hub.reply(503, json!({"code":"pbs_unreachable"}));
    let out = computer
        .command(vec!["leave".into(), "--force".into()], "")
        .await;
    success(&out);
    assert!(text(&out).contains("may remain on the hub"));
    assert!(
        Config::load(&computer.paths.config_file)
            .unwrap()
            .hub
            .is_none()
    );
    assert!(
        !computer
            .paths
            .credentials_dir
            .join("machine-credential")
            .exists()
    );
    assert!(
        computer
            .paths
            .credentials_dir
            .join("encryption-key")
            .exists()
    );
}

#[tokio::test]
async fn interrupted_activation_recovers_from_durable_journal_on_next_invocation() {
    let hub = Hub::start().await;
    let computer = Computer::new();
    hub.reply(200, hub.success());
    let out = computer.join(&hub, &[], "interrupt-enable").await;
    assert!(!out.status.success());
    assert!(
        computer
            .paths
            .state_dir
            .join("join-transaction/journal.json")
            .exists()
    );
    let key = computer.paths.credentials_dir.join("encryption-key");
    let before = std::fs::read(&key).unwrap();
    let mut run = nestegg::state::Run::new(nestegg::state::Trigger::Manual, chrono::Utc::now());
    run.finish(nestegg::state::RunState::Succeeded, chrono::Utc::now());
    Store::new(&computer.paths)
        .record_finished(&run, 10)
        .unwrap();
    success(&computer.join(&hub, &[], "").await);
    assert!(
        Config::load(&computer.paths.config_file)
            .unwrap()
            .hub
            .is_some()
    );
    assert_eq!(std::fs::read(key).unwrap(), before);
    assert!(!computer.paths.state_dir.join("join-transaction").exists());
    assert_eq!(hub.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn failed_remote_rollback_keeps_cleanup_credential_and_retries_before_new_join() {
    let hub = Hub::start().await;
    let computer = Computer::new();
    let mut response = hub.success();
    response["master_key"]["fingerprint"] = json!("wrong");
    hub.reply(200, response);
    hub.reply(503, json!({"code":"pbs_unreachable"}));
    let out = computer.join(&hub, &[], "").await;
    assert!(!out.status.success());
    assert!(text(&out).contains("Cleanup is unfinished"));
    assert!(!computer.paths.config_file.exists());
    let credential = computer
        .paths
        .state_dir
        .join("join-transaction/remote-credential");
    assert!(credential.exists());
    assert_eq!(
        credential.metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );
    hub.reply(200, hub.success());
    success(&computer.join(&hub, &[], "").await);
    assert!(!computer.paths.state_dir.join("join-transaction").exists());
    assert_eq!(hub.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn invalid_master_response_rolls_back_remote_enrollment_and_symlinks_are_refused() {
    let hub = Hub::start().await;
    let computer = Computer::new();
    let mut response = hub.success();
    response["master_key"]["fingerprint"] = json!("wrong");
    hub.reply(200, response);
    hub.reply(204, Value::Null);
    let out = computer.join(&hub, &[], "").await;
    assert!(
        text(&out).contains("does not match the fingerprint"),
        "{}",
        text(&out)
    );
    assert!(!computer.paths.config_file.exists());
    let unchanged = computer.dir.path().join("untouched-config");
    std::fs::write(&unchanged, "not a config").unwrap();
    std::os::unix::fs::symlink(&unchanged, &computer.paths.config_file).unwrap();
    let out = computer.join(&hub, &["--replace-config"], "").await;
    assert!(text(&out).contains("not a regular file"));
    assert_eq!(std::fs::read_to_string(unchanged).unwrap(), "not a config");
    assert_eq!(hub.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn unreadable_existing_sealed_key_is_never_replaced_or_sent_to_the_hub() {
    let hub = Hub::start().await;
    let computer = Computer::new();
    std::fs::create_dir_all(&computer.paths.credentials_dir).unwrap();
    let sealed = computer.paths.credentials_dir.join("encryption-key.cred");
    nestegg::secrets::write_private_file(&sealed, b"fixture-unreadable-sealed-key").unwrap();
    let out = computer.join(&hub, &["--replace-config"], "").await;
    assert!(!out.status.success());
    assert!(text(&out).contains("systemd-creds"));
    assert_eq!(
        std::fs::read(&sealed).unwrap(),
        b"fixture-unreadable-sealed-key"
    );
    assert!(
        !computer
            .paths
            .credentials_dir
            .join("encryption-key")
            .exists()
    );
    assert!(!computer.paths.config_file.exists());
    assert!(hub.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn adopted_snapshots_defer_key_and_backup() {
    let hub = Hub::start().await;
    let computer = Computer::new();
    let mut response = hub.success();
    response["existing_snapshots"] = json!(4);
    hub.reply(200, response);
    let out = computer.join(&hub, &["--adopt"], "").await;
    success(&out);
    assert!(
        !nestegg::secrets::SecretStore::new(&computer.paths.credentials_dir)
            .exists(nestegg::secrets::ENCRYPTION_KEY)
    );
    assert!(text(&out).contains("--install") && text(&out).contains("key create"));
    assert!(
        !Store::new(&computer.paths)
            .load_state()
            .unwrap()
            .join_backup_pending
    );
    let log = std::fs::read_to_string(computer.dir.path().join("commands.log")).unwrap();
    assert!(!log.contains("start --no-block"));
}

#[tokio::test]
async fn replace_config_preserves_household_settings_and_keeps_backup_copy() {
    let hub = Hub::start().await;
    let computer = Computer::new();
    let mut cfg = nestegg::config::example("old.example.com", "old-backups", "backup@pbs!old");
    cfg.backup.excludes.push("keep-my-exclude".into());
    cfg.backup.system = false;
    cfg.schedule.interval_hours = 13;
    cfg.notify.desktop = false;
    cfg.notify.command = Some(vec!["/usr/local/bin/notify-backup".into()]);
    cfg.stick =
        Some(toml::from_str(r#"data_uuids = ["01234567-89ab-cdef-0123-456789abcdef"]"#).unwrap());
    let old = cfg.to_toml().unwrap();
    std::fs::write(&computer.paths.config_file, &old).unwrap();
    hub.reply(200, hub.success());
    success(&computer.join(&hub, &["--replace-config"], "").await);
    let actual = Config::load(&computer.paths.config_file).unwrap();
    assert_eq!(actual.backup.excludes, cfg.backup.excludes);
    assert_eq!(actual.backup.system, cfg.backup.system);
    assert_eq!(actual.backup.sources, cfg.backup.sources);
    assert_eq!(actual.stick, cfg.stick);
    assert_eq!(actual.notify, cfg.notify);
    assert_eq!(
        toml::to_string(&actual.schedule).unwrap(),
        toml::to_string(&cfg.schedule).unwrap()
    );
    let backup = std::fs::read_dir(computer.dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("config.toml.before-join-")
        })
        .unwrap();
    assert_eq!(std::fs::read_to_string(backup.path()).unwrap(), old);
}

#[tokio::test]
async fn dropped_response_retries_with_same_durable_attempt_after_restart() {
    let hub = Hub::start().await;
    let computer = Computer::new();
    for _ in 0..3 {
        hub.reply(0, Value::Null);
    }
    let out = computer.join(&hub, &[], "").await;
    assert!(!out.status.success());
    assert!(
        computer
            .paths
            .state_dir
            .join("join-transaction/journal.json")
            .exists()
    );
    hub.reply(200, hub.success());
    success(&computer.join(&hub, &[], "").await);
    let requests = hub.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let attempt = requests[0].2["attempt"].as_str().unwrap();
    assert_eq!(attempt.len(), 43);
    assert!(requests.iter().all(|r| r.2["attempt"] == attempt));
}

#[tokio::test]
async fn upstream_failure_keeps_the_bound_attempt_for_a_later_retry() {
    let hub = Hub::start().await;
    let computer = Computer::new();
    hub.reply(503, json!({"code":"pbs_refused"}));
    assert!(!computer.join(&hub, &[], "").await.status.success());
    hub.reply(200, hub.success());
    success(&computer.join(&hub, &[], "").await);
    let requests = hub.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].2["attempt"], requests[1].2["attempt"]);
}
