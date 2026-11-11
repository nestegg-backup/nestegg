//! `nestegg daemon`: local JSON API on a Unix socket (see `api/openapi.yaml`).

use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::Extension;
use axum::extract::connect_info::{ConnectInfo, Connected};
use axum::extract::{Path as UrlPath, Query, State as AxState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use crate::config::{Config, Paths};
use crate::secrets::SecretStore;
use crate::state::{Run, Store, Trigger, is_valid_run_id};

pub const GROUP: &str = "nestegg";
pub const API_VERSION: &str = "1";

#[derive(Clone)]
struct App {
    paths: Paths,
    /// Serialises "check for an active run, reserve, spawn".
    start: Arc<Mutex<()>>,
    restores: Arc<Mutex<HashMap<String, RestoreJob>>>,
    /// Serialises catalog dumps (one dump per snapshot, no duplicates).
    catalog: Arc<Mutex<()>>,
    /// Backup metadata of snapshots, keyed by everything the answer depends
    /// on (datastore, namespace, encryption, snapshot); `None` for snapshots
    /// without trustworthy metadata (decision 0016). Snapshots do not change
    /// once finished, but the configuration can change in the file while the
    /// daemon runs, so it is part of the key. Bounded in size.
    metadata: Arc<Mutex<MetadataCache>>,
    /// Serialises settings changes (read, modify, write, switch the timer).
    settings: Arc<Mutex<()>>,
    /// Lets only one PBS check run at a time: a second request while one runs
    /// is told "already checking" instead of starting another wake-up burst.
    check_gate: Arc<tokio::sync::Mutex<()>>,
    /// When the last wake attempt was sent, to keep a minimum interval between
    /// wake-ups so group members cannot make the daemon spam Wake-on-LAN.
    last_wake: Arc<Mutex<Option<std::time::Instant>>>,
    /// What the last `POST /pbs/check` found out about reachability, and
    /// when. `GET /status` reports it while it is newer than what the last
    /// backup run recorded, so every page shows the newest thing known
    /// instead of "reachable" from a run hours ago right after a check that
    /// got no answer. Kept in memory only: after a restart of the daemon the
    /// last run's value is the newest again.
    last_check: Arc<Mutex<Option<LastCheck>>>,
}

/// Whether the server answered at the last connection check, and when.
type LastCheck = (bool, chrono::DateTime<Utc>);

/// Overlay the last connection check on the status when it is the newer
/// information.
fn newest_reachability(pbs: &mut crate::status::PbsStatus, last_check: Option<LastCheck>) {
    let Some((reachable, at)) = last_check else {
        return;
    };
    if pbs.checked_at.is_none_or(|run| at > run) {
        pbs.reachable = Some(reachable);
        pbs.checked_at = Some(at);
    }
}

/// Shortest interval between two Wake-on-LAN attempts from `POST /pbs/check`.
const MIN_WAKE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Key for the metadata cache. The blob a snapshot has, and whether it may
/// be trusted, depends not only on the snapshot path but on the server
/// (address and port), datastore and namespace it is read from and on whether
/// encryption is configured.
/// All of these can change in the configuration file while the daemon runs,
/// so a stale answer must not survive a configuration change (decision 0016).
#[derive(Clone, PartialEq, Eq, Hash)]
struct MetaKey {
    server: String,
    port: u16,
    datastore: String,
    namespace: String,
    encryption: bool,
    snapshot: String,
}

impl MetaKey {
    fn new(config: &Config, snapshot: &str) -> MetaKey {
        MetaKey {
            server: config
                .pbs
                .as_ref()
                .map(|p| p.server.clone())
                .unwrap_or_default(),
            port: config.pbs.as_ref().map(|p| p.port).unwrap_or(8007),
            datastore: config
                .pbs
                .as_ref()
                .map(|p| p.datastore.clone())
                .unwrap_or_default(),
            namespace: config
                .pbs
                .as_ref()
                .and_then(|p| p.namespace.clone())
                .unwrap_or_default(),
            encryption: config.backup.encryption,
            snapshot: snapshot.to_string(),
        }
    }
}

/// Bounded cache of backup metadata. Finished snapshots do not change, so a
/// hit never goes stale for its key; the bound only caps memory for a daemon
/// that browses very many snapshots over its lifetime. Oldest inserted
/// entries are evicted first.
#[derive(Default)]
struct MetadataCache {
    entries: HashMap<MetaKey, Option<crate::metadata::BackupMetadata>>,
    order: VecDeque<MetaKey>,
}

impl MetadataCache {
    const MAX: usize = 512;

    fn get(&self, key: &MetaKey) -> Option<Option<crate::metadata::BackupMetadata>> {
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: MetaKey, value: Option<crate::metadata::BackupMetadata>) {
        if !self.entries.contains_key(&key) {
            self.order.push_back(key.clone());
            while self.order.len() > Self::MAX {
                if let Some(old) = self.order.pop_front() {
                    self.entries.remove(&old);
                }
            }
        }
        self.entries.insert(key, value);
    }
}

/// Credentials of the process on the other end of the socket.
#[derive(Clone, Debug)]
pub struct Peer {
    pub uid: Option<u32>,
    /// Primary gid, from the same peer credentials; the settings exclude guard
    /// reads folders as this uid/gid (decision 0022).
    pub gid: Option<u32>,
}

impl Connected<axum::serve::IncomingStream<'_, tokio::net::UnixListener>> for Peer {
    fn connect_info(stream: axum::serve::IncomingStream<'_, tokio::net::UnixListener>) -> Self {
        let cred = stream.io().peer_cred().ok();
        Peer {
            uid: cred.as_ref().map(|c| c.uid()),
            gid: cred.as_ref().map(|c| c.gid()),
        }
    }
}

type PeerInfo = Option<Extension<ConnectInfo<Peer>>>;

fn requester(peer: &PeerInfo) -> ApiResult<crate::restore::Requester> {
    let uid = peer
        .as_ref()
        .and_then(|Extension(ConnectInfo(p))| p.uid)
        .ok_or_else(|| ApiError::new(StatusCode::FORBIDDEN, "forbidden", "unknown caller"))?;
    if uid == 0 {
        return Ok(crate::restore::Requester::Root { sudo: None });
    }
    let user = crate::identity::UserIdentity::from_uid(uid)
        .map_err(|e| ApiError::new(StatusCode::FORBIDDEN, "forbidden", format!("{e:#}")))?;
    Ok(crate::restore::Requester::User(user))
}

#[derive(Clone, serde::Serialize)]
struct RestoreJob {
    #[serde(skip)]
    owner: u32,
    id: String,
    state: &'static str,
    target_dir: String,
    #[serde(with = "crate::state::rfc3339_opt")]
    started_at: Option<chrono::DateTime<Utc>>,
    #[serde(with = "crate::state::rfc3339_opt")]
    finished_at: Option<chrono::DateTime<Utc>>,
    error: Option<String>,
    #[serde(skip)]
    restored: Arc<std::sync::atomic::AtomicU64>,
    restored_bytes: u64,
    total_bytes: Option<u64>,
    warnings: Vec<String>,
}

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }

    fn internal(e: impl std::fmt::Display) -> ApiError {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "code": self.code, "message": self.message })),
        )
            .into_response()
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(ApiError::internal)?
        .map_err(|e| ApiError::internal(format!("{e:#}")))
}

pub fn router(paths: Paths) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/status", get(status))
        .route("/api/v1/runs", get(list_runs).post(start_run))
        .route(
            "/api/v1/targets/{target}/runs",
            get(target_runs).post(start_target_run),
        )
        .route("/api/v1/runs/{id}", get(get_run))
        .route("/api/v1/snapshots", get(list_snapshots))
        .route("/api/v1/runs/{id}/cancel", axum::routing::post(cancel_run))
        .route("/api/v1/snapshots/{time}/browse", get(browse))
        .route("/api/v1/restores", axum::routing::post(start_restore))
        .route("/api/v1/restores/{id}", get(get_restore))
        .route("/api/v1/config", get(config).patch(update_config))
        .route("/api/v1/session", get(session))
        .route("/api/v1/pbs", get(pbs_info))
        .route("/api/v1/pbs/check", axum::routing::post(pbs_check))
        .route("/api/v1/machines", get(machines))
        .fallback(|| async {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "unknown endpoint")
        })
        .with_state(App {
            paths,
            start: Default::default(),
            restores: Default::default(),
            catalog: Default::default(),
            metadata: Default::default(),
            settings: Default::default(),
            check_gate: Default::default(),
            last_wake: Default::default(),
            last_check: Default::default(),
        })
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "version": env!("CARGO_PKG_VERSION"), "api_version": API_VERSION }))
}

async fn status(AxState(app): AxState<App>) -> ApiResult<Json<serde_json::Value>> {
    let status = blocking(move || {
        // A config that exists but cannot be read or parsed is an error, not a
        // reason to report the built-in defaults as the configuration.
        let config = crate::status::config_for_status(&app.paths.config_file)?;
        let mut status = crate::status::build(
            &app.paths,
            config.as_ref(),
            crate::status::timer_enabled(&app.paths),
        )?;
        let last_check = *app.last_check.lock().unwrap_or_else(|e| e.into_inner());
        newest_reachability(&mut status.pbs, last_check);
        Ok(serde_json::to_value(status)?)
    })
    .await?;
    Ok(Json(status))
}

#[derive(Deserialize)]
struct Limit {
    limit: Option<usize>,
}

async fn list_runs(
    AxState(app): AxState<App>,
    Query(q): Query<Limit>,
) -> ApiResult<Json<serde_json::Value>> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let runs = blocking(move || {
        let store = Store::new(&app.paths);
        let mut runs = Vec::new();
        if let Some(current) = crate::run::active_run(&app.paths)? {
            runs.push(current);
        }
        runs.extend(store.list_runs(usize::MAX)?);
        let target = Config::load(&app.paths.config_file)
            .map(|c| legacy_target(&c))
            .unwrap_or(crate::state::TargetSelection::Pbs);
        runs.retain(|r| target.includes(r.target));
        runs.truncate(limit);
        Ok(runs)
    })
    .await?;
    Ok(Json(json!({ "runs": runs })))
}

async fn get_run(
    AxState(app): AxState<App>,
    UrlPath(id): UrlPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    if !is_valid_run_id(&id) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "unknown run",
        ));
    }
    let found = blocking(move || {
        let store = Store::new(&app.paths);
        let Some(run) = store.get_run(&id)? else {
            return Ok(None);
        };
        let tail = log_tail(&store.log_path(&id), 200);
        let mut value = serde_json::to_value(run)?;
        value["log_tail"] = json!(tail);
        Ok(Some(value))
    })
    .await?;
    found
        .map(Json)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "unknown run"))
}

fn log_tail(path: &Path, lines: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

async fn start_run(AxState(app): AxState<App>) -> ApiResult<(StatusCode, Json<Run>)> {
    let paths = app.paths.clone();
    let target = blocking(move || Ok(legacy_target(&Config::load(&paths.config_file)?))).await?;
    start_selected_run(app, target).await
}

fn legacy_target(config: &Config) -> crate::state::TargetSelection {
    if config.pbs.is_some() {
        crate::state::TargetSelection::Pbs
    } else {
        crate::state::TargetSelection::Stick
    }
}

fn selection(value: &str) -> ApiResult<crate::state::TargetSelection> {
    match value {
        "pbs" => Ok(crate::state::TargetSelection::Pbs),
        "stick" => Ok(crate::state::TargetSelection::Stick),
        "all" => Ok(crate::state::TargetSelection::All),
        _ => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "target must be pbs, stick or all",
        )),
    }
}

async fn target_runs(
    AxState(app): AxState<App>,
    UrlPath(target): UrlPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = selection(&target)?;
    let runs = blocking(move || {
        let store = Store::new(&app.paths);
        let mut runs = store.list_runs(usize::MAX)?;
        if let Some(current) = crate::run::active_run(&app.paths)? {
            runs.insert(0, current);
        }
        runs.retain(|r| target.includes(r.target));
        runs.truncate(200);
        Ok(runs)
    })
    .await?;
    Ok(Json(json!({ "runs": runs })))
}

async fn start_target_run(
    AxState(app): AxState<App>,
    UrlPath(target): UrlPath<String>,
) -> ApiResult<(StatusCode, Json<Run>)> {
    start_selected_run(app, selection(&target)?).await
}

async fn start_selected_run(
    app: App,
    target: crate::state::TargetSelection,
) -> ApiResult<(StatusCode, Json<Run>)> {
    let run = blocking(move || -> Result<std::result::Result<Run, &'static str>> {
        let _guard = app.start.lock().unwrap_or_else(|e| e.into_inner());
        if crate::run::active_run(&app.paths)?.is_some() {
            return Ok(Err("a backup run is already in progress"));
        }
        let config = Config::load(&app.paths.config_file)?;
        let mut run = Run::new(Trigger::Manual, Utc::now());
        run.target = if target.includes(crate::state::Target::Stick) && config.stick.is_some() {
            crate::state::Target::Stick
        } else {
            crate::state::Target::Pbs
        };
        if !match run.target {
            crate::state::Target::Stick => config.stick.is_some(),
            crate::state::Target::Pbs => {
                config.pbs.is_some() && target.includes(crate::state::Target::Pbs)
            }
        } {
            return Ok(Err("requested target is not configured"));
        }
        let store = Store::new(&app.paths);
        store.save_current(&run)?;
        if let Err(e) = spawn_run(&run.id, target) {
            let _ = store.clear_current_if(&run.id);
            return Err(e);
        }
        Ok(Ok(run))
    })
    .await?;
    match run {
        Ok(run) => Ok((StatusCode::ACCEPTED, Json(run))),
        Err("requested target is not configured") => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "requested target is not configured",
        )),
        Err(msg) => Err(ApiError::new(StatusCode::CONFLICT, "run_in_progress", msg)),
    }
}

/// Start `nestegg run` outside the daemon process: as a transient systemd
/// unit when running under systemd (own cgroup, journal), otherwise as a
/// detached child process.
fn spawn_run(run_id: &str, target: crate::state::TargetSelection) -> Result<()> {
    let exe = std::env::current_exe().context("cannot locate nestegg binary")?;
    let under_systemd =
        Path::new("/run/systemd/system").exists() && nix::unistd::geteuid().is_root();
    let mut cmd = if under_systemd {
        let mut c = Command::new("systemd-run");
        c.args([
            &format!("--unit=nestegg-run-{run_id}"),
            "--description=nestegg manual backup",
            "--collect",
            "--no-block",
            "--property=Nice=10",
            "--property=IOSchedulingClass=idle",
            "--property=SuccessExitStatus=75",
        ]);
        c.arg(&exe);
        c
    } else {
        Command::new(&exe)
    };
    cmd.args([
        "run",
        "--trigger",
        "manual",
        "--run-id",
        run_id,
        "--target",
        target.as_str(),
    ]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if under_systemd {
        let status = cmd.status().context("cannot run systemd-run")?;
        anyhow::ensure!(status.success(), "systemd-run failed with {status}");
    } else {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::from)?;
                Ok(())
            });
        }
        cmd.spawn().context("cannot start backup run")?;
    }
    Ok(())
}

/// Start the health check outside the daemon so a protection-reducing
/// settings change is reported right away, not only at the next hourly timer
/// (decision 0022). Best effort: under systemd it starts the oneshot unit
/// (`--no-block`, so the daemon does not wait for the hook); otherwise it
/// spawns a detached `nestegg notify`. A failure only delays the report to
/// the next hourly check, so it is logged, not raised.
fn spawn_notify(paths: &Paths) {
    let under_systemd =
        Path::new("/run/systemd/system").exists() && nix::unistd::geteuid().is_root();
    let result = if under_systemd {
        Command::new(&paths.systemctl)
            .args(["start", "--no-block", crate::status::NOTIFY_UNIT])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|_| ())
    } else {
        match std::env::current_exe() {
            Ok(exe) => Command::new(exe)
                .arg("notify")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map(|_| ()),
            Err(e) => Err(e),
        }
    };
    if let Err(e) = result {
        eprintln!("warning: could not start the health check after a settings change: {e}");
    }
}

async fn list_snapshots(AxState(app): AxState<App>) -> ApiResult<Json<serde_json::Value>> {
    let result = tokio::task::spawn_blocking(
        move || -> Result<std::result::Result<serde_json::Value, String>> {
            let config = Config::load(&app.paths.config_file)?;
            if !crate::wake::is_reachable(&config, std::time::Duration::from_secs(3)) {
                return Ok(Err("PBS is not reachable".into()));
            }
            let client =
                crate::client::Client::new(&config, SecretStore::new(&app.paths.credentials_dir));
            let snapshots = client.list_snapshots(&config.effective_backup_id())?;
            Ok(Ok(json!({ "snapshots": snapshots })))
        },
    )
    .await
    .map_err(ApiError::internal)?
    .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    match result {
        Ok(v) => Ok(Json(v)),
        Err(msg) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "pbs_unreachable",
            msg,
        )),
    }
}

async fn config(AxState(app): AxState<App>) -> ApiResult<Json<serde_json::Value>> {
    let (config, timer) = blocking(move || {
        let config = Config::load(&app.paths.config_file)?;
        Ok((config, crate::status::timer_enabled(&app.paths)))
    })
    .await?;
    Ok(Json(crate::status::config_view(&config, timer)))
}

/// `PATCH /api/v1/config`: change the settings group members may change
/// (decision 0013). Every change and every refusal is logged with the
/// caller to the daemon's journal.
async fn update_config(
    AxState(app): AxState<App>,
    peer: PeerInfo,
    body: std::result::Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<serde_json::Value>> {
    let uid = peer
        .as_ref()
        .and_then(|Extension(ConnectInfo(p))| p.uid)
        .ok_or_else(|| forbidden("unknown caller"))?;
    // The exclude guard reads a folder's real entries with the requester's own
    // identity (uid + primary gid of the socket), never as root, so its answer
    // never reveals a name the requester could not list themselves. If the gid
    // is not known no folder is read (decision 0022).
    let identity = peer
        .as_ref()
        .and_then(|Extension(ConnectInfo(p))| p.gid)
        .map(|gid| crate::settings::PeerIdentity { uid, gid });
    let user_name = match nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) {
        Ok(Some(user)) => user.name,
        _ => format!("uid {uid}"),
    };
    let caller = if user_name.starts_with("uid ") {
        user_name.clone()
    } else {
        format!("{user_name} (uid {uid})")
    };
    let refuse = |msg: String| {
        eprintln!("settings change by {caller} refused: {msg}");
        bad_request(msg)
    };
    let Json(body) = body.map_err(|e| refuse(e.body_text()))?;
    let patch =
        crate::settings::SettingsPatch::from_json(body).map_err(|e| refuse(e.to_string()))?;
    let paths = app.paths.clone();
    let record_user = user_name.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _one_change_at_a_time = app.settings.lock().unwrap_or_else(|e| e.into_inner());
        // A group member's change is reported to the users and the hook; root
        // is trusted and not reported (decision 0022). The report is written
        // ahead as an *intent*, before the configuration is changed, so a crash
        // in between still leads to a message; if even the intent cannot be
        // recorded the change is not applied (item 8).
        let intent = if uid != 0 {
            match crate::notify::record_settings_intent(&app.paths, &record_user) {
                Ok(id) => Some(id),
                Err(e) => {
                    return Err(crate::settings::SettingsError::Failed(e.context(
                        "the settings change was not applied because its report could not be \
                         recorded; try again",
                    )));
                }
            }
        } else {
            None
        };
        let timer = crate::settings::SystemdTimer {
            systemctl: &app.paths.systemctl,
        };
        match crate::settings::update(&app.paths.config_file, &patch, &timer, identity) {
            Ok(outcome) => {
                // The change took effect: complete the intent with the real
                // reductions (or drop it when nothing was lowered). Best effort
                // — the intent already guarantees a message if this fails.
                if let Some(id) = intent {
                    if let Err(e) = crate::notify::finalize_settings_change(
                        &app.paths,
                        id,
                        &record_user,
                        outcome.reductions.clone(),
                    ) {
                        eprintln!("warning: could not complete the settings report: {e:#}");
                    }
                }
                Ok(outcome)
            }
            Err(e) => {
                // Nothing took effect: drop the intent so no message is sent.
                if let Some(id) = intent {
                    let _ = crate::notify::cancel_settings_intent(&app.paths, id);
                }
                Err(e)
            }
        }
    })
    .await
    .map_err(ApiError::internal)?;
    match result {
        Ok(outcome) => {
            if outcome.changes.is_empty() {
                eprintln!("settings saved by {caller}: no changes");
            } else {
                eprintln!(
                    "settings changed by {caller}: {}",
                    outcome.changes.join("; ")
                );
            }
            // When the change lowered the protection, trigger the check so the
            // recorded report is delivered within seconds (a change that took
            // effect but ended in an error, `outcome.error`, is reported too).
            if uid != 0 && !outcome.reductions.is_empty() {
                // Triggering the check runs `systemctl start`; keep it off the
                // async reactor.
                let notify_paths = paths.clone();
                tokio::task::spawn_blocking(move || spawn_notify(&notify_paths));
            }
            if let Some(err) = outcome.error {
                eprintln!("settings change by {caller} partly failed: {err:#}");
                return Err(ApiError::internal(err));
            }
            Ok(Json(crate::status::config_view(
                &outcome.config,
                outcome.timer_enabled,
            )))
        }
        Err(crate::settings::SettingsError::Invalid(msg)) => Err(refuse(msg)),
        Err(e @ crate::settings::SettingsError::Failed(_)) => {
            eprintln!("settings change by {caller} failed: {e}");
            Err(ApiError::internal(e))
        }
    }
}

fn pbs_unreachable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "pbs_unreachable",
        "PBS is not reachable",
    )
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "bad_request", msg)
}

fn forbidden(msg: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "forbidden", msg)
}

/// Map typed restore errors to API errors.
fn restore_error(e: anyhow::Error) -> ApiError {
    match e.downcast_ref::<crate::restore::RestoreError>() {
        Some(crate::restore::RestoreError::TargetExists(_)) => {
            ApiError::new(StatusCode::CONFLICT, "target_exists", e.to_string())
        }
        Some(crate::restore::RestoreError::Forbidden(m)) => forbidden(m.clone()),
        Some(crate::restore::RestoreError::BadRequest(m)) => bad_request(m.clone()),
        None => ApiError::internal(format!("{e:#}")),
    }
}

/// `host/<backup_id>/<time>` for a snapshot time of this machine.
fn snapshot_of(config: &Config, time: &str) -> ApiResult<String> {
    let t = chrono::DateTime::parse_from_rfc3339(time)
        .map_err(|_| bad_request("invalid snapshot time"))?
        .with_timezone(&Utc);
    Ok(format!(
        "host/{}/{}",
        config.effective_backup_id(),
        crate::state::format_time(t)
    ))
}

/// Source path of `archive` in `snapshot`, for deciding what an
/// unprivileged user may see (decision 0016). It comes from the snapshot's
/// metadata blob or, for snapshots of older versions, from the local run
/// record of exactly that snapshot. Never from the snapshot notes: anyone
/// with write access to the backup group can change those.
fn snapshot_source(
    app: &App,
    config: &Config,
    snapshot: &str,
    archive: &str,
) -> ApiResult<PathBuf> {
    let not_in_archive = || forbidden("this archive does not contain your home directory");
    let key = MetaKey::new(config, snapshot);
    let cached = app
        .metadata
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key);
    let meta = match cached {
        Some(m) => m,
        None => {
            if !crate::wake::is_reachable(config, std::time::Duration::from_secs(3)) {
                return Err(pbs_unreachable());
            }
            let m = load_metadata(&app.paths, config, snapshot)?;
            app.metadata
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, m.clone());
            m
        }
    };
    if let Some(meta) = meta {
        return meta
            .source_of(archive)
            .map(PathBuf::from)
            .ok_or_else(not_in_archive);
    }
    let record = Store::new(&app.paths)
        .run_for_snapshot(snapshot)
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    match record {
        Some(run) => run
            .archives
            .iter()
            .find(|a| a.name == archive)
            .map(|a| PathBuf::from(&a.source))
            .ok_or_else(not_in_archive),
        None => Err(forbidden(format!(
            "this snapshot has no backup metadata (made by an older nestegg version) and \
             no local run record, so its contents cannot be assigned to your home \
             directory; ask an administrator to restore it with \
             `sudo nestegg restore {snapshot} {archive} <target>`"
        ))),
    }
}

/// Read and check the metadata blob of `snapshot`. `None`: no blob, or a
/// blob that cannot be trusted for permissions (not signed with the
/// configured key although encryption is on, so it could have been made
/// without the key).
///
/// With encryption on, [`crate::metadata::fetch`] hands the key to the
/// client, which verifies the manifest signature: a blob that is not signed
/// with our key (an unencrypted, possibly forged snapshot) makes the client
/// refuse it, and that refusal is treated as "not trustworthy" here rather
/// than a hard error, so the caller falls back to the local run record
/// (decision 0016). The decision never depends on the server's file listing.
fn load_metadata(
    paths: &Paths,
    config: &Config,
    snapshot: &str,
) -> ApiResult<Option<crate::metadata::BackupMetadata>> {
    let client = crate::client::Client::new(config, SecretStore::new(&paths.credentials_dir));
    let work = crate::secrets::create_private_dir(&paths.tmp_dir())
        .and_then(|()| crate::staging::StagingDir::create(&paths.tmp_dir().join("metadata")))
        .map_err(|e| {
            ApiError::internal(format!(
                "cannot prepare to read the backup metadata of {snapshot}: {e:#}"
            ))
        })?;
    let fetched = crate::metadata::fetch(&client, snapshot, work.path());
    interpret_metadata(fetched, config.backup.encryption, snapshot)
}

/// Turn the outcome of reading the blob into the permission decision. Pure
/// (no I/O) so the documented fallbacks can be unit-tested without a server.
fn interpret_metadata(
    fetched: Result<Option<crate::metadata::Fetched>>,
    encryption: bool,
    snapshot: &str,
) -> ApiResult<Option<crate::metadata::BackupMetadata>> {
    let untrusted = |why: String| {
        eprintln!("backup metadata of {snapshot} refused: {why}");
        forbidden(format!(
            "the backup metadata of this snapshot cannot be used ({why}); ask an \
             administrator to restore it with `sudo nestegg restore`"
        ))
    };
    match fetched {
        // No blob at all (older nestegg version): fall back to the local run
        // record.
        Ok(None) => Ok(None),
        // The blob was restored (with the key when encryption is on, so the
        // client verified the manifest signature). It must still parse and
        // describe exactly this snapshot.
        Ok(Some(fetched)) => {
            let meta = crate::metadata::BackupMetadata::parse(&fetched.bytes)
                .map_err(|e| untrusted(format!("{e:#}")))?;
            if !meta.belongs_to(snapshot) {
                return Err(untrusted(format!(
                    "it describes {}, not this snapshot",
                    meta.snapshot
                )));
            }
            Ok(Some(meta))
        }
        // Encryption is on but the snapshot's manifest is not signed with our
        // key: it could have been made without the key, so it is not trusted.
        // Fall back to the local run record (decision 0016).
        Err(e) if encryption && crate::client::manifest_unsigned_error(&format!("{e:#}")) => {
            eprintln!(
                "backup metadata of {snapshot} is not signed with the configured key although \
                 encryption is on; not used for permissions"
            );
            Ok(None)
        }
        // Anything else (wrong key, transport or file-system error): a blob
        // that cannot be read is an error (decision 0016).
        Err(e) => Err(ApiError::internal(format!(
            "cannot read the backup metadata of {snapshot}: {e:#}"
        ))),
    }
}

fn user_scope_for(
    app: &App,
    config: &Config,
    snapshot: &str,
    archive: &str,
    user: &crate::identity::UserIdentity,
) -> ApiResult<crate::restore::UserScope> {
    let source = snapshot_source(app, config, snapshot, archive)?;
    crate::restore::user_scope(&source, &user.home, user.uid)
        .ok_or_else(|| forbidden("this archive does not contain your home directory"))
}

fn check_archive_name(archive: &str) -> ApiResult<()> {
    let ok = archive.ends_with(".pxar")
        && archive.len() > 5
        && archive[..archive.len() - 5]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(bad_request("invalid archive name"))
    }
}

async fn session(peer: PeerInfo) -> ApiResult<Json<serde_json::Value>> {
    let uid = peer
        .as_ref()
        .and_then(|Extension(ConnectInfo(p))| p.uid)
        .ok_or_else(|| forbidden("unknown caller"))?;
    let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .map_err(ApiError::internal)?
        .ok_or_else(|| forbidden("unknown user"))?;
    Ok(Json(json!({
        "user": user.name,
        "home": user.dir,
        "uid": uid,
        "root": uid == 0,
    })))
}

#[derive(Deserialize)]
struct BrowseQuery {
    archive: String,
    path: Option<String>,
}

async fn browse(
    AxState(app): AxState<App>,
    peer: PeerInfo,
    UrlPath(time): UrlPath<String>,
    q: std::result::Result<Query<BrowseQuery>, axum::extract::rejection::QueryRejection>,
) -> ApiResult<Json<serde_json::Value>> {
    let Query(q) = q.map_err(|e| bad_request(e.body_text()))?;
    let requester = requester(&peer)?;
    check_archive_name(&q.archive)?;
    let path = crate::restore::normalize_archive_path(q.path.as_deref().unwrap_or("/"))
        .map_err(restore_error)?;
    let config = blocking({
        let p = app.paths.clone();
        move || Config::load(&p.config_file)
    })
    .await?;
    let snapshot = snapshot_of(&config, &time)?;
    let archive = q.archive.clone();
    let entries =
        tokio::task::spawn_blocking(move || -> ApiResult<Vec<crate::catalog::DirEntry>> {
            let scope = match &requester {
                crate::restore::Requester::User(user) => {
                    let scope = user_scope_for(&app, &config, &snapshot, &archive, user)?;
                    if !scope.contains(&path) && !scope.is_ancestor(&path) {
                        return Err(forbidden("you can only browse your own home directory"));
                    }
                    Some(scope)
                }
                crate::restore::Requester::Root { .. } => None,
            };
            // Catalogs list every file name of the machine: root-only directory
            // on disk (not /run, which is RAM).
            let cache_root = app.paths.state_dir.join("catalog-cache");
            let cache = crate::catalog::cache_path(&cache_root, &snapshot);
            {
                let _one_dump_at_a_time = app.catalog.lock().unwrap_or_else(|e| e.into_inner());
                if !cache.exists() {
                    if !crate::wake::is_reachable(&config, std::time::Duration::from_secs(3)) {
                        return Err(pbs_unreachable());
                    }
                    crate::secrets::create_private_dir(&cache_root).map_err(ApiError::internal)?;
                    let client = crate::client::Client::new(
                        &config,
                        SecretStore::new(&app.paths.credentials_dir),
                    );
                    client
                        .catalog_dump(&snapshot, &cache)
                        .map_err(|e| ApiError::internal(format!("{e:#}")))?;
                    crate::catalog::prune_dir(&cache_root, 5, &cache);
                }
            }
            crate::catalog::list_children(&cache, &archive, &path, scope.as_ref())
                .map_err(|e| ApiError::internal(format!("{e:#}")))
        })
        .await
        .map_err(ApiError::internal)??;
    Ok(Json(json!({ "entries": entries })))
}

#[derive(Deserialize)]
struct ArchivePaths {
    archive: String,
    #[serde(default)]
    paths: Vec<String>,
}

#[derive(Deserialize)]
struct RestoreBody {
    snapshot_time: String,
    archive: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    archives: Option<Vec<ArchivePaths>>,
    target_dir: String,
}

async fn start_restore(
    AxState(app): AxState<App>,
    peer: PeerInfo,
    body: std::result::Result<Json<RestoreBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<(StatusCode, Json<RestoreJob>)> {
    let Json(body) = body.map_err(|e| bad_request(e.body_text()))?;
    let requester = requester(&peer)?;
    let owner = match &requester {
        crate::restore::Requester::User(u) => u.uid,
        crate::restore::Requester::Root { .. } => 0,
    };
    let mut selections: Vec<ArchivePaths> = match (body.archives, body.archive) {
        (Some(list), _) if !list.is_empty() => list,
        (_, Some(archive)) => vec![ArchivePaths {
            archive,
            paths: body.paths,
        }],
        _ => return Err(bad_request("archive or archives is required")),
    };
    let target = PathBuf::from(&body.target_dir);
    if !target.is_absolute() {
        return Err(bad_request("target_dir must be an absolute path"));
    }
    crate::restore::absolute_target(&target).map_err(restore_error)?;
    {
        let mut seen = std::collections::HashSet::new();
        if let Some(dup) = selections.iter().find(|s| !seen.insert(s.archive.clone())) {
            return Err(bad_request(format!(
                "archive {} is listed twice",
                dup.archive
            )));
        }
    }
    let config = blocking({
        let p = app.paths.clone();
        move || Config::load(&p.config_file)
    })
    .await?;
    let snapshot = snapshot_of(&config, &body.snapshot_time)?;
    // Validate archives and paths; unprivileged users are limited to their
    // own home directory, as recorded in the snapshot.
    let (selections, config, snapshot, requester) = {
        let app = app.clone();
        let target = target.clone();
        tokio::task::spawn_blocking(move || -> ApiResult<_> {
            let exists = match &requester {
                crate::restore::Requester::User(u) => crate::identity::exists_as(u, &target)
                    .map_err(|e| ApiError::internal(format!("{e:#}")))?,
                crate::restore::Requester::Root { .. } => {
                    std::fs::symlink_metadata(&target).is_ok()
                }
            };
            if exists {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "target_exists",
                    format!("{} already exists", target.display()),
                ));
            }
            if selections.len() > 1
                && matches!(requester, crate::restore::Requester::Root { .. })
                && !crate::restore::root_controlled_parent(&target).map_err(ApiError::internal)?
            {
                return Err(forbidden(
                    "as root, restore several archives only into a root-owned location",
                ));
            }
            for sel in &mut selections {
                check_archive_name(&sel.archive)?;
                let mut normalized = Vec::new();
                for p in &sel.paths {
                    normalized
                        .push(crate::restore::normalize_archive_path(p).map_err(restore_error)?);
                }
                if let crate::restore::Requester::User(user) = &requester {
                    let scope = user_scope_for(&app, &config, &snapshot, &sel.archive, user)?;
                    if normalized.is_empty() || normalized.iter().any(|p| p == "/") {
                        normalized = vec![scope.prefix.clone()];
                    }
                    if let Some(p) = normalized.iter().find(|p| !scope.contains(p)) {
                        return Err(forbidden(format!("{p} is outside your home directory")));
                    }
                }
                sel.paths = normalized;
            }
            Ok((selections, config, snapshot, requester))
        })
        .await
        .map_err(ApiError::internal)??
    };
    let mut jobs = app.restores.lock().unwrap_or_else(|e| e.into_inner());
    // Keep the 20 most recent finished jobs.
    let mut finished: Vec<(String, Option<chrono::DateTime<Utc>>)> = jobs
        .values()
        .filter(|j| j.state != "running")
        .map(|j| (j.id.clone(), j.finished_at))
        .collect();
    finished.sort_by_key(|(_, t)| *t);
    let excess = finished.len().saturating_sub(20);
    for (id, _) in finished.into_iter().take(excess) {
        jobs.remove(&id);
    }
    if jobs.values().any(|j| j.state == "running") {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "restore_in_progress",
            "another restore is running",
        ));
    }
    let id = format!(
        "{}-{:016x}",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        rand::random::<u64>()
    );
    let job = RestoreJob {
        owner,
        id: id.clone(),
        state: "running",
        target_dir: target.display().to_string(),
        started_at: Some(Utc::now()),
        finished_at: None,
        error: None,
        restored: Default::default(),
        restored_bytes: 0,
        total_bytes: None,
        warnings: Vec::new(),
    };
    jobs.insert(id.clone(), job.clone());
    drop(jobs);

    let restores = app.restores.clone();
    let paths = app.paths.clone();
    let restored = job.restored.clone();
    std::thread::spawn(move || {
        let result = run_restore_job(
            &paths,
            &config,
            &snapshot,
            &selections,
            &target,
            &requester,
            &restored,
            &restores,
            &id,
        );
        let mut jobs = restores.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(j) = jobs.get_mut(&id) {
            j.finished_at = Some(Utc::now());
            j.restored_bytes = restored.load(Ordering::Relaxed);
            match result {
                Ok(warnings) => {
                    j.state = "succeeded";
                    j.warnings = warnings;
                }
                Err(e) => {
                    j.state = "failed";
                    j.error = Some(format!("{e:#}"));
                }
            }
        }
    });
    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// Decide the restore key for `archive` in `snapshot` and, with encryption
/// configured, warn when the archive is not protected by the key.
///
/// Permissions no longer depend on the key, but the content does: with
/// encryption configured, a protected archive (encrypted or signed) is restored
/// with the configured key handed over explicitly — never decided by the
/// restore's own second listing — so the client verifies the manifest
/// signature. An archive that is neither encrypted nor signed (an old snapshot
/// made before encryption was switched on, or one planted by someone without
/// the key) is restored without a key, but with a warning that it was not
/// verified against the key. Without encryption configured the key is chosen as
/// before (`Auto`). (decision 0015, 0016)
fn restore_key_and_warning(
    client: &crate::client::Client,
    config: &Config,
    snapshot: &str,
    archive: &str,
) -> Result<(crate::client::RestoreKey, Option<String>)> {
    if !config.backup.encryption {
        return Ok((crate::client::RestoreKey::Auto, None));
    }
    let files = client.snapshot_files(snapshot)?;
    let protection = crate::client::archive_protection(&files, archive);
    if protection == crate::client::Protection::NotListed {
        anyhow::bail!("{archive} is not in {snapshot}");
    }
    if protection == crate::client::Protection::Protected {
        Ok((client.configured_key()?, None))
    } else {
        Ok((
            crate::client::RestoreKey::Without,
            Some(format!(
                "{archive} in this snapshot is not protected by the encryption key (it was \
                 made before encryption was switched on, or by someone without the key); it \
                 was restored without verifying it against the key"
            )),
        ))
    }
}

/// Copies the progress of one archive restore into the job once a second.
/// Dropping it stops the thread and waits for it, so the thread can never
/// outlive the restore of its archive, whichever way that ends.
struct ProgressMonitor {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ProgressMonitor {
    fn start(
        done_before: u64,
        src: Arc<std::sync::atomic::AtomicU64>,
        dst: Arc<std::sync::atomic::AtomicU64>,
        jobs: Arc<Mutex<HashMap<String, RestoreJob>>>,
        id: &str,
    ) -> ProgressMonitor {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let id = id.to_string();
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let now = done_before + src.load(Ordering::Relaxed);
                    dst.store(now, Ordering::Relaxed);
                    if let Some(j) = jobs.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&id) {
                        j.restored_bytes = now;
                    }
                    // Short steps, so a stop is noticed quickly.
                    for _ in 0..10 {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            })
        };
        ProgressMonitor {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for ProgressMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One line in the daemon's log (the journal) when an archive that is not
/// protected by the configured encryption key is restored, besides the
/// warning in the job: who restored what, without verification by the key.
fn log_unprotected_restore(snapshot: &str, archive: &str, requester: &crate::restore::Requester) {
    eprintln!("{}", unprotected_restore_line(snapshot, archive, requester));
}

fn unprotected_restore_line(
    snapshot: &str,
    archive: &str,
    requester: &crate::restore::Requester,
) -> String {
    let name = |uid: u32| match nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) {
        Ok(Some(user)) => format!("{} (uid {uid})", user.name),
        _ => format!("uid {uid}"),
    };
    let who = match requester {
        crate::restore::Requester::User(u) => name(u.uid),
        crate::restore::Requester::Root { sudo: Some(u) } => {
            format!("root (sudo by {})", name(u.uid))
        }
        crate::restore::Requester::Root { sudo: None } => "root".to_string(),
    };
    format!(
        "warning: restore of {archive} from {snapshot} for {who}: the archive is not protected \
         by the encryption key; it is restored without verifying it against the key"
    )
}

#[allow(clippy::too_many_arguments)]
fn run_restore_job(
    paths: &Paths,
    config: &Config,
    snapshot: &str,
    selections: &[ArchivePaths],
    target: &Path,
    requester: &crate::restore::Requester,
    restored: &Arc<std::sync::atomic::AtomicU64>,
    jobs: &Arc<Mutex<HashMap<String, RestoreJob>>>,
    id: &str,
) -> Result<Vec<String>> {
    let client = crate::client::Client::new(config, SecretStore::new(&paths.credentials_dir));
    let total: u64 = selections
        .iter()
        .filter(|s| s.paths.is_empty())
        .filter_map(|s| client.archive_size(snapshot, &s.archive).ok().flatten())
        .sum();
    if let Some(j) = jobs.lock().unwrap_or_else(|e| e.into_inner()).get_mut(id) {
        j.total_bytes =
            (total > 0 && selections.iter().all(|s| s.paths.is_empty())).then_some(total);
    }
    // Several archives go into `<target>/<archive name>/`.
    let multiple = selections.len() > 1;
    if multiple {
        match requester {
            crate::restore::Requester::User(user) => {
                crate::identity::make_dir_as(user, target)?;
            }
            crate::restore::Requester::Root { .. } => {
                std::fs::create_dir(target)?;
                std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o700))?;
            }
        }
    }
    let mut warnings = Vec::new();
    let mut done_before = 0;
    for sel in selections {
        let patterns = crate::restore::patterns_for(&sel.paths);
        let sub = if multiple {
            target.join(sel.archive.trim_end_matches(".pxar"))
        } else {
            target.to_path_buf()
        };
        // The fallible lookups come before the progress monitor starts, and the
        // monitor is a guard that stops and joins its thread when dropped, so
        // no early return can leave a thread behind writing into the job.
        let (key, key_warning) = restore_key_and_warning(&client, config, snapshot, &sel.archive)?;
        if let Some(w) = key_warning {
            log_unprotected_restore(snapshot, &sel.archive, requester);
            warnings.push(w);
        }
        let progress = crate::restore::Progress::default();
        let monitor = ProgressMonitor::start(
            done_before,
            progress.restored_bytes.clone(),
            restored.clone(),
            jobs.clone(),
            id,
        );
        let mut log = Vec::new();
        let result = crate::restore::execute(
            paths,
            config,
            crate::restore::Job {
                snapshot,
                archive: &sel.archive,
                target: &sub,
                patterns: &patterns,
                key,
                staging_dir: None,
            },
            requester,
            &progress,
            &mut log,
        );
        drop(monitor);
        done_before += progress.restored_bytes.load(Ordering::Relaxed);
        restored.store(done_before, Ordering::Relaxed);
        warnings.extend(
            String::from_utf8_lossy(&log)
                .lines()
                .filter(|l| l.starts_with("warning:"))
                .map(|l| l.trim_start_matches("warning:").trim().to_string()),
        );
        warnings.extend(result?);
    }
    Ok(warnings)
}

async fn get_restore(
    AxState(app): AxState<App>,
    peer: PeerInfo,
    UrlPath(id): UrlPath<String>,
) -> ApiResult<Json<RestoreJob>> {
    let caller = match requester(&peer)? {
        crate::restore::Requester::User(u) => u.uid,
        crate::restore::Requester::Root { .. } => 0,
    };
    let jobs = app.restores.lock().unwrap_or_else(|e| e.into_inner());
    let mut job = jobs
        .get(&id)
        .filter(|j| caller == 0 || j.owner == caller)
        .cloned()
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "unknown restore"))?;
    if job.state == "running" {
        job.restored_bytes = job.restored.load(Ordering::Relaxed);
    }
    Ok(Json(job))
}

/// `alice (uid 1000)` for the daemon log and run records.
fn caller_name(peer: &PeerInfo) -> String {
    match peer.as_ref().and_then(|Extension(ConnectInfo(p))| p.uid) {
        Some(uid) => match nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) {
            Ok(Some(user)) => format!("{} (uid {uid})", user.name),
            _ => format!("uid {uid}"),
        },
        None => "unknown caller".into(),
    }
}

/// What a cancel request found.
enum CancelOutcome {
    Signalled,
    /// No run with this id has ever been recorded.
    Unknown,
    /// The run exists but has not published its process yet.
    Starting,
    /// The run exists and is over.
    Finished,
}

/// `POST /api/v1/runs/{id}/cancel`: any group member may cancel (they
/// operate the backups of this computer); who did it is logged and ends up
/// in the run record. 404 for an id that does not exist, 409
/// `cannot_cancel` for a run that exists but cannot be cancelled (still
/// starting, or already over).
async fn cancel_run(
    AxState(app): AxState<App>,
    peer: PeerInfo,
    UrlPath(id): UrlPath<String>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    if !is_valid_run_id(&id) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "unknown run",
        ));
    }
    let caller = caller_name(&peer);
    let paths = app.paths.clone();
    let who = caller.clone();
    let run_id = id.clone();
    let outcome = blocking(move || {
        let id = run_id;
        let Some(record) = Store::new(&paths).get_run(&id)? else {
            return Ok(CancelOutcome::Unknown);
        };
        // The finished record is durable before current.json/run.pid are
        // cleared. That cleanup window must not accept another cancellation.
        if record.state.is_finished() {
            return Ok(CancelOutcome::Finished);
        }
        if crate::run::active_run(&paths)?.is_none_or(|r| r.id != id) {
            return Ok(CancelOutcome::Finished);
        }
        let text = std::fs::read_to_string(paths.runtime_dir.join("run.pid")).unwrap_or_default();
        let mut parts = text.split_whitespace();
        let (Some(pid), Some(pid_run_id)) = (parts.next(), parts.next()) else {
            return Ok(CancelOutcome::Starting);
        };
        if pid_run_id != id {
            return Ok(CancelOutcome::Starting);
        }
        let pid: i32 = pid.parse()?;
        // Read by the run when it records the cancellation.
        let note = crate::run::cancel_note_path(&paths, &id);
        if let Err(e) = crate::secrets::create_private_dir(&paths.tmp_dir())
            .and_then(|()| crate::secrets::write_private_file(&note, format!("{who}\n").as_bytes()))
        {
            eprintln!("cannot record who cancelled run {id}: {e:#}");
        }
        match nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        ) {
            Ok(()) => Ok(CancelOutcome::Signalled),
            // The run ended between the active-run check and the signal: there
            // is no such process any more. That is "already over", not a server
            // error (409 cannot_cancel, not 500).
            Err(nix::errno::Errno::ESRCH) => Ok(CancelOutcome::Finished),
            Err(e) => Err(e.into()),
        }
    })
    .await?;
    let (log, message) = match outcome {
        CancelOutcome::Signalled => {
            eprintln!("backup run {id} cancelled by {caller}");
            return Ok((StatusCode::ACCEPTED, Json(json!({ "cancelling": true }))));
        }
        CancelOutcome::Unknown => {
            eprintln!("cancel of backup run {id} by {caller}: unknown run");
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "unknown run",
            ));
        }
        CancelOutcome::Starting => (
            "the run is still starting",
            "the backup is still starting and cannot be cancelled yet; try again in a moment",
        ),
        CancelOutcome::Finished => (
            "the run is already over",
            "this backup has already finished and cannot be cancelled",
        ),
    };
    eprintln!("cancel of backup run {id} by {caller}: {log}");
    Err(ApiError::new(
        StatusCode::CONFLICT,
        "cannot_cancel",
        message,
    ))
}

#[derive(Deserialize)]
struct WakeQuery {
    wake: Option<bool>,
}

async fn pbs_check(
    AxState(app): AxState<App>,
    Query(q): Query<WakeQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    pbs_check_with(app, q.wake.unwrap_or(false), |paths, options| {
        crate::check::run_reporting(paths, options)
    })
    .await
}

/// Whether a wake attempt may be made now: none was sent within
/// [`MIN_WAKE_INTERVAL`] (counted from the end of the check that sent it), so
/// group members cannot make the daemon spam Wake-on-LAN.
fn wake_allowed(last_sent: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    last_sent.is_none_or(|t| now.saturating_duration_since(t) >= MIN_WAKE_INTERVAL)
}

/// `POST /pbs/check` with the check itself passed in (tests use a double).
async fn pbs_check_with(
    app: App,
    wake: bool,
    check: impl FnOnce(&Paths, &crate::check::CheckOptions) -> crate::check::Checked + Send + 'static,
) -> ApiResult<Json<serde_json::Value>> {
    // Only one check at a time: a login, a reachability probe and possibly a
    // Wake-on-LAN burst can take minutes, and any group member can ask for one.
    // A second request while one runs is refused with a clear message rather
    // than piling on more work and more wake-ups. The gate is an owned guard
    // that moves into the blocking task below: if the caller goes away and this
    // future is dropped, the check goes on, and so does the gate until the
    // check has finished.
    let Ok(gate) = app.check_gate.clone().try_lock_owned() else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "already_checking",
            "a PBS check is already running; try again in a moment",
        ));
    };
    // Keep a minimum interval between wake attempts: if one was sent very
    // recently, check without sending another wake-up burst. Only a check that
    // really sent a packet starts the interval (not one where the server
    // answered at once, or Wake-on-LAN is off or not configured).
    let effective_wake = wake
        && wake_allowed(
            *app.last_wake.lock().unwrap_or_else(|e| e.into_inner()),
            std::time::Instant::now(),
        );
    let paths = app.paths.clone();
    let last_wake = app.last_wake.clone();
    let last_check = app.last_check.clone();
    let (checks, checked_at) = blocking(move || {
        let _gate = gate;
        let checked = check(
            &paths,
            &crate::check::CheckOptions {
                wake: effective_wake,
                login: true,
            },
        );
        if checked.wake_sent {
            *last_wake.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
        }
        let checked_at = Utc::now();
        // The server answered, or it did not (failed, or asleep and not woken):
        // either way that is now the newest thing known about it. No such item
        // (the configuration could not be read): nothing was learned.
        if let Some(item) = checked
            .items
            .iter()
            .find(|i| i.name == crate::check::REACHABLE_ITEM)
        {
            let reachable = item.status == crate::check::CheckStatus::Passed;
            *last_check.lock().unwrap_or_else(|e| e.into_inner()) = Some((reachable, checked_at));
        }
        Ok((checked.items, checked_at))
    })
    .await?;
    Ok(Json(json!({
        "checks": checks,
        "checked_at": crate::state::format_time(checked_at),
        // True when a wake was asked for but skipped because one was sent very
        // recently (the minimum interval between wake attempts).
        "wake_skipped": wake && !effective_wake,
    })))
}

async fn pbs_info(AxState(app): AxState<App>) -> ApiResult<Json<serde_json::Value>> {
    let paths = app.paths.clone();
    let info = blocking(move || {
        let config = Config::load(&paths.config_file)?;
        let reachable = crate::wake::is_reachable(&config, std::time::Duration::from_secs(3));
        let usage = if reachable {
            crate::client::Client::new(&config, SecretStore::new(&paths.credentials_dir))
                .datastore_usage()
                .unwrap_or(None)
        } else {
            None
        };
        Ok(json!({
            "server": format!("{}:{}", config.remote_pbs()?.server, config.pbs.as_ref().map(|p| p.port).unwrap_or(8007)),
            "datastore": config.remote_pbs()?.datastore,
            "namespace": config.remote_pbs()?.namespace,
            "reachable": reachable,
            "used_bytes": usage.map(|u| u.0),
            "total_bytes": usage.map(|u| u.1),
            "overview_configured": config.overview.is_some(),
        }))
    })
    .await?;
    Ok(Json(info))
}

async fn machines(AxState(app): AxState<App>) -> ApiResult<Json<serde_json::Value>> {
    let paths = app.paths.clone();
    let result = tokio::task::spawn_blocking(move || -> ApiResult<serde_json::Value> {
        let config =
            Config::load(&paths.config_file).map_err(|e| ApiError::internal(format!("{e:#}")))?;
        if !crate::wake::is_reachable(&config, std::time::Duration::from_secs(3)) {
            return Err(pbs_unreachable());
        }
        let secrets = SecretStore::new(&paths.credentials_dir);
        let overview = config
            .overview
            .as_ref()
            .filter(|_| secrets.exists(crate::secrets::OVERVIEW_PASSWORD));
        let client = match overview {
            Some(o) => crate::client::Client::with_credentials(
                &config,
                secrets,
                &o.auth_id,
                crate::secrets::OVERVIEW_PASSWORD,
            ),
            None => crate::client::Client::new(&config, secrets),
        };
        let groups = client
            .list_groups()
            .map_err(|e| ApiError::internal(format!("{e:#}")))?;
        let cutoff = Utc::now() - chrono::Duration::days(14);
        let mut machines = Vec::new();
        for g in groups.into_iter().filter(|g| g.backup_type == "host") {
            let group = format!("host/{}", g.backup_id);
            let snaps = client.list_group_snapshots(&group).unwrap_or_default();
            let recent: Vec<&str> = snaps
                .iter()
                .filter(|s| crate::client::parse_time(&s.time).is_some_and(|t| t >= cutoff))
                .map(|s| s.time.as_str())
                .collect();
            let last = snaps.first();
            let report = last
                .and_then(|s| client.notes(&s.snapshot).ok())
                .and_then(|n| crate::run::parse_report(&n));
            machines.push(json!({
                "backup_id": g.backup_id,
                "hostname": report.as_ref().and_then(|r| r["hostname"].as_str()),
                "os": report.as_ref().and_then(|r| r["os"].as_str()),
                "last_snapshot": last.map(|s| s.time.clone()),
                "recent_snapshots": recent,
                "size_bytes": last.and_then(|s| s.size_bytes),
                "report": report,
            }));
        }
        Ok(json!({
            "read_at": crate::state::format_time(Utc::now()),
            "complete": overview.is_some(),
            "machines": machines,
        }))
    })
    .await
    .map_err(ApiError::internal)??;
    Ok(Json(result))
}

/// Bind the socket with mode 0660 and group `nestegg` (if it exists).
pub async fn serve(paths: Paths) -> Result<()> {
    std::fs::create_dir_all(&paths.runtime_dir)?;
    let socket = paths.socket();
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }
    let listener = tokio::net::UnixListener::bind(&socket)
        .with_context(|| format!("cannot bind {}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o660))?;
    if let Ok(Some(group)) = nix::unistd::Group::from_name(GROUP) {
        let _ = nix::unistd::chown(&socket, None, Some(group.gid));
    }
    eprintln!("nestegg daemon listening on {}", socket.display());
    let app = router(paths).into_make_service_with_connect_info::<Peer>();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// Resolves on SIGINT or SIGTERM (systemd stop).
pub async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("cannot install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn paths(dir: &Path) -> Paths {
        Paths {
            config_file: dir.join("config.toml"),
            credentials_dir: dir.join("cred"),
            state_dir: dir.join("state"),
            runtime_dir: dir.join("run"),
            systemctl: dir.join("no-systemctl"),
        }
    }

    const CONFIG: &str = "[pbs]\nserver = \"pbs.example.com\"\ndatastore = \"backups\"\nauth_id = \"backup@pbs!desktop\"\n";

    fn patch(body: &str, uid: Option<u32>) -> Request<Body> {
        let mut req = Request::builder()
            .method("PATCH")
            .uri("/api/v1/config")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        if let Some(uid) = uid {
            req.extensions_mut().insert(ConnectInfo(Peer {
                uid: Some(uid),
                gid: Some(nix::unistd::getgid().as_raw()),
            }));
        }
        req
    }

    async fn json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn app(dir: &Path) -> App {
        App {
            paths: paths(dir),
            start: Default::default(),
            restores: Default::default(),
            catalog: Default::default(),
            metadata: Default::default(),
            settings: Default::default(),
            check_gate: Default::default(),
            last_wake: Default::default(),
            last_check: Default::default(),
        }
    }

    const SNAPSHOT: &str = "host/desktop/2026-11-06T21:20:00Z";

    fn meta_for(backup_time: i64) -> crate::metadata::BackupMetadata {
        crate::metadata::BackupMetadata {
            format: crate::metadata::FORMAT,
            nestegg_version: "0.1.0".into(),
            backup_id: "desktop".into(),
            hostname: "desktop".into(),
            backup_time,
            snapshot: crate::client::snapshot_path("desktop", backup_time),
            archive_sources: [("home.pxar".to_string(), "/home".to_string())].into(),
            sources: vec!["/home".into()],
            system_mode: false,
            incomplete: vec![],
        }
    }

    #[test]
    fn legacy_start_selects_one_compatible_target() {
        let mut cfg = crate::config::example("pbs.example.com", "store", "backup@pbs!desktop");
        cfg.stick = Config::parse("[stick]\ndata_uuids=['01234567-89ab-cdef-0123-456789abcdef']")
            .unwrap()
            .stick;
        assert_eq!(legacy_target(&cfg), crate::state::TargetSelection::Pbs);
        cfg.pbs = None;
        assert_eq!(legacy_target(&cfg), crate::state::TargetSelection::Stick);
    }

    #[test]
    fn interpret_metadata_trusts_authentic_falls_back_and_refuses() {
        // A blob that was restored with the key, parses and describes exactly
        // this snapshot: trusted.
        let good = meta_for(1_794_000_000);
        assert_eq!(good.snapshot, SNAPSHOT);
        let fetched = Ok(Some(crate::metadata::Fetched {
            bytes: good.to_bytes(),
        }));
        assert_eq!(
            interpret_metadata(fetched, true, SNAPSHOT).ok(),
            Some(Some(good))
        );

        // No blob (older version): fall back to the local run record.
        assert_eq!(
            interpret_metadata(Ok(None), true, SNAPSHOT).ok(),
            Some(None)
        );

        // Encryption on, but the snapshot's manifest is not signed with our
        // key (an unencrypted, possibly forged snapshot): the client refuses
        // it; that is not a hard error, it falls back to the local record.
        let unsigned = Err(anyhow::anyhow!(
            "Error: manifest has no signature, but an encryption key is set - refusing to trust \
             a potentially tampered, unsigned backup"
        ));
        assert_eq!(
            interpret_metadata(unsigned, true, SNAPSHOT).ok(),
            Some(None)
        );

        // A blob that belongs to another snapshot: refused (403), never used.
        let other = crate::metadata::Fetched {
            bytes: meta_for(1_794_000_001).to_bytes(),
        };
        let err = interpret_metadata(Ok(Some(other)), true, SNAPSHOT).unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);

        // A blob that does not parse: refused (403).
        let bad = crate::metadata::Fetched {
            bytes: b"not json".to_vec(),
        };
        let err = interpret_metadata(Ok(Some(bad)), true, SNAPSHOT).unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);

        // Any other read failure (wrong key, transport, file system): a blob
        // that cannot be read is an error, not a silent fall-back.
        let io = Err(anyhow::anyhow!(
            "Error: wrong key - unable to verify signature since manifest's key aa does not \
             match provided key bb"
        ));
        let err = interpret_metadata(io, true, SNAPSHOT).unwrap_err();
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn metadata_cache_is_keyed_by_config_and_bounded() {
        let config = Config::parse(CONFIG).unwrap();
        let mut other = config.clone();
        // Same snapshot, different datastore -> different key -> no stale hit.
        other.pbs.as_mut().unwrap().datastore = "other".into();
        let mut cache = MetadataCache::default();
        cache.insert(
            MetaKey::new(&config, SNAPSHOT),
            Some(meta_for(1_794_000_000)),
        );
        assert!(cache.get(&MetaKey::new(&config, SNAPSHOT)).is_some());
        assert!(cache.get(&MetaKey::new(&other, SNAPSHOT)).is_none());
        // Another server with the same datastore and namespace names.
        let mut moved = config.clone();
        moved.pbs.as_mut().unwrap().server = "other.example.com".into();
        assert!(cache.get(&MetaKey::new(&moved, SNAPSHOT)).is_none());
        let mut port = config.clone();
        port.pbs.as_mut().unwrap().port += 1;
        assert!(cache.get(&MetaKey::new(&port, SNAPSHOT)).is_none());

        // The cache is bounded: inserting far more than MAX evicts the oldest.
        for i in 0..(MetadataCache::MAX as i64 + 100) {
            let snap = crate::client::snapshot_path("desktop", 1_000_000 + i);
            cache.insert(MetaKey::new(&config, &snap), None);
        }
        assert!(cache.entries.len() <= MetadataCache::MAX);
    }

    #[test]
    fn scope_comes_from_metadata_then_local_record_never_notes() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path());
        let config = Config::parse(CONFIG).unwrap();
        let meta = crate::metadata::BackupMetadata {
            format: crate::metadata::FORMAT,
            nestegg_version: "0.1.0".into(),
            backup_id: "desktop".into(),
            hostname: "desktop".into(),
            backup_time: 1_794_000_000,
            snapshot: SNAPSHOT.into(),
            archive_sources: [("home.pxar".to_string(), "/home".to_string())].into(),
            sources: vec!["/home".into()],
            system_mode: false,
            incomplete: vec![],
        };
        app.metadata
            .lock()
            .unwrap()
            .insert(MetaKey::new(&config, SNAPSHOT), Some(meta));
        // A local record that claims something else does not matter while
        // the snapshot has metadata.
        let store = Store::new(&app.paths);
        let mut run = Run::new(Trigger::Timer, Utc::now());
        run.snapshot = Some(SNAPSHOT.into());
        run.archives = vec![crate::state::ArchiveInfo {
            name: "etc.pxar".into(),
            source: "/etc".into(),
            size_bytes: None,
        }];
        run.finish(crate::state::RunState::Succeeded, Utc::now());
        store.record_run_only(&run).unwrap();
        assert_eq!(
            snapshot_source(&app, &config, SNAPSHOT, "home.pxar").ok(),
            Some(PathBuf::from("/home"))
        );
        let err = snapshot_source(&app, &config, SNAPSHOT, "etc.pxar").unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);

        // Without metadata: the local record of exactly this snapshot.
        app.metadata
            .lock()
            .unwrap()
            .insert(MetaKey::new(&config, SNAPSHOT), None);
        assert_eq!(
            snapshot_source(&app, &config, SNAPSHOT, "etc.pxar").ok(),
            Some(PathBuf::from("/etc"))
        );
        assert_eq!(
            snapshot_source(&app, &config, SNAPSHOT, "home.pxar")
                .unwrap_err()
                .status,
            StatusCode::FORBIDDEN
        );
        // Neither: refused with a pointer to the administrator.
        let other = "host/desktop/2026-11-07T00:00:00Z";
        app.metadata
            .lock()
            .unwrap()
            .insert(MetaKey::new(&config, other), None);
        let err = snapshot_source(&app, &config, other, "home.pxar").unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert!(
            err.message.contains("ask an administrator"),
            "{}",
            err.message
        );
        assert!(
            err.message.contains("sudo nestegg restore"),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn settings_change_needs_a_known_caller() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), CONFIG).unwrap();
        let resp = router(paths(dir.path()))
            .oneshot(patch(r#"{"interval_hours":12}"#, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            CONFIG
        );
    }

    #[tokio::test]
    async fn settings_change_through_the_router() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), CONFIG).unwrap();
        let me = nix::unistd::getuid().as_raw();
        let resp = router(paths(dir.path()))
            .oneshot(patch(
                r#"{"interval_hours":12,"overdue_after_hours":30}"#,
                Some(me),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let view = json(resp).await;
        assert_eq!(view["interval_hours"], 12);
        assert_eq!(view["automatic_backups"], false);
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("interval_hours = 12"), "{text}");

        // Without systemctl, automatic backups cannot be switched on.
        let resp = router(paths(dir.path()))
            .oneshot(patch(r#"{"automatic_backups":true}"#, Some(me)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let err = json(resp).await;
        assert_eq!(err["code"], "internal");
        assert!(
            err["message"].as_str().unwrap().contains("need systemd"),
            "{err}"
        );
        // Switching off what is already off needs no systemctl.
        let resp = router(paths(dir.path()))
            .oneshot(patch(r#"{"automatic_backups":false}"#, Some(me)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = router(paths(dir.path()))
            .oneshot(patch(r#"{"datastore":"other"}"#, Some(me)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json(resp).await["code"], "bad_request");
    }

    #[tokio::test]
    async fn only_one_pbs_check_runs_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("state")).unwrap();
        let app = app(dir.path());
        // Simulate a check already in progress by holding the gate.
        let held = app.check_gate.clone().lock_owned().await;
        let err = pbs_check(AxState(app.clone()), Query(WakeQuery { wake: Some(false) }))
            .await
            .expect_err("a second check must be refused while one runs");
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert_eq!(err.code, "already_checking");
        drop(held);
    }

    #[test]
    fn a_restore_whose_key_lookup_fails_leaves_no_monitor_thread_behind() {
        // Encryption on and no credentials stored: the crypt-mode lookup of
        // the archive fails before any client runs (no password). The job must
        // end with that error, and no progress thread may stay behind writing
        // into the job (it would hold a clone of `jobs` for ever).
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        std::fs::create_dir_all(&p.credentials_dir).unwrap();
        let config = crate::config::example("pbs.example.com", "backups", "backup@pbs!desktop");
        assert!(config.backup.encryption);
        let jobs: Arc<Mutex<HashMap<String, RestoreJob>>> = Default::default();
        let restored = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let selections = vec![ArchivePaths {
            archive: "home.pxar".into(),
            paths: Vec::new(),
        }];
        let result = run_restore_job(
            &p,
            &config,
            SNAPSHOT,
            &selections,
            &dir.path().join("target"),
            &crate::restore::Requester::Root { sudo: None },
            &restored,
            &jobs,
            "job",
        );
        assert!(result.is_err(), "the key lookup must fail");
        assert_eq!(
            Arc::strong_count(&jobs),
            1,
            "a monitor thread still holds the job table"
        );
        assert_eq!(Arc::strong_count(&restored), 1);
    }

    #[test]
    fn the_progress_monitor_stops_when_dropped() {
        let jobs: Arc<Mutex<HashMap<String, RestoreJob>>> = Default::default();
        let src = Arc::new(std::sync::atomic::AtomicU64::new(7));
        let dst = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let monitor = ProgressMonitor::start(5, src.clone(), dst.clone(), jobs.clone(), "x");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while dst.load(Ordering::Relaxed) != 12 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(dst.load(Ordering::Relaxed), 12);
        let started = std::time::Instant::now();
        drop(monitor);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(Arc::strong_count(&jobs), 1);
        assert_eq!(Arc::strong_count(&src), 1);
    }

    #[test]
    fn an_unprotected_restore_is_logged_with_snapshot_archive_and_requester() {
        let line = unprotected_restore_line(
            SNAPSHOT,
            "home.pxar",
            &crate::restore::Requester::Root { sudo: None },
        );
        assert!(line.contains(SNAPSHOT), "{line}");
        assert!(line.contains("home.pxar"), "{line}");
        assert!(line.contains("for root"), "{line}");
        assert!(line.contains("not protected"), "{line}");
    }

    #[test]
    fn a_wake_is_allowed_once_a_minute() {
        let now = std::time::Instant::now();
        assert!(wake_allowed(None, now));
        let later = |secs| now + std::time::Duration::from_secs(secs);
        assert!(!wake_allowed(Some(now), now));
        assert!(!wake_allowed(Some(now), later(59)));
        assert!(wake_allowed(Some(now), later(60)));
        // A stamp "after" now (cannot happen with a monotonic clock) is not
        // an underflow.
        assert!(!wake_allowed(Some(later(5)), now));
    }

    fn checked(wake_sent: bool) -> crate::check::Checked {
        crate::check::Checked {
            items: Vec::new(),
            wake_sent,
        }
    }

    #[tokio::test]
    async fn only_a_wake_that_was_really_sent_starts_the_interval() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path());
        // Asked to wake, but nothing was sent (the server answered at once):
        // the next request may still wake.
        let asked = Arc::new(Mutex::new(Vec::new()));
        let record = |sent: bool| {
            let asked = asked.clone();
            move |_: &Paths, o: &crate::check::CheckOptions| {
                asked.lock().unwrap().push(o.wake);
                checked(sent)
            }
        };
        let r = pbs_check_with(app.clone(), true, record(false))
            .await
            .ok()
            .expect("the check answers");
        assert_eq!(r.0["wake_skipped"], false);
        assert!(app.last_wake.lock().unwrap().is_none());
        // This one sends a packet.
        let r = pbs_check_with(app.clone(), true, record(true))
            .await
            .ok()
            .expect("the check answers");
        assert_eq!(r.0["wake_skipped"], false);
        assert!(app.last_wake.lock().unwrap().is_some());
        // Within the minute: checked without waking, and the caller is told.
        let r = pbs_check_with(app.clone(), true, record(false))
            .await
            .ok()
            .expect("the check answers");
        assert_eq!(r.0["wake_skipped"], true);
        // Not asked to wake: never "skipped".
        let r = pbs_check_with(app.clone(), false, record(false))
            .await
            .ok()
            .expect("the check answers");
        assert_eq!(r.0["wake_skipped"], false);
        assert_eq!(*asked.lock().unwrap(), vec![true, true, false, false]);
    }

    #[tokio::test]
    async fn the_check_gate_is_held_until_the_check_ends_even_if_the_caller_left() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path());
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let request = tokio::spawn(pbs_check_with(app.clone(), false, move |_, _| {
            started_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            checked(false)
        }));
        tokio::task::spawn_blocking(move || {
            started_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
        })
        .await
        .unwrap();
        // The caller goes away: the request future is dropped.
        request.abort();
        assert!(request.await.err().expect("aborted").is_cancelled());
        // The check is still running, so the gate is still closed.
        let err = pbs_check_with(app.clone(), false, |_, _| checked(false))
            .await
            .expect_err("the gate must stay closed while the check runs");
        assert_eq!(err.code, "already_checking");
        // The check ends: the gate opens.
        go_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.check_gate.try_lock().is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "the gate never opened"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn cancel_of_a_just_ended_run_is_409_not_500() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        std::fs::create_dir_all(&p.state_dir).unwrap();
        std::fs::create_dir_all(&p.runtime_dir).unwrap();
        let id = "20261108T120000Z-manual-abcd";
        let store = Store::new(&p);
        let mut run = Run::new(Trigger::Manual, Utc::now());
        run.id = id.into();
        store.record_run_only(&run).unwrap();
        store.save_current(&run).unwrap();
        // The run looks active: a held run lock and a fresh current.json.
        let _lock = crate::run::RunLock::try_acquire(&p).unwrap().unwrap();
        // The pid file points at a process that no longer exists, so the signal
        // gets ESRCH. That is "already finished" (409 cannot_cancel), not a
        // server error (500).
        std::fs::write(p.runtime_dir.join("run.pid"), format!("2147483646 {id}\n")).unwrap();

        let mut req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/runs/{id}/cancel"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(Peer {
            uid: Some(0),
            gid: Some(0),
        }));
        let resp = router(p).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(json(resp).await["code"], "cannot_cancel");
    }
    #[tokio::test]
    async fn a_finished_record_cannot_signal_the_still_live_cleanup_process() {
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = ChildGuard(Command::new("sleep").arg("60").spawn().unwrap());
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let store = Store::new(&p);
        let mut run = Run::new(Trigger::Manual, Utc::now());
        store.save_current(&run).unwrap();
        let _lock = crate::run::RunLock::try_acquire(&p).unwrap().unwrap();
        run.finish(crate::state::RunState::Cancelled, Utc::now());
        store.record_run_only(&run).unwrap();
        std::fs::write(
            p.runtime_dir.join("run.pid"),
            format!("{} {}\n", child.0.id(), run.id),
        )
        .unwrap();
        let mut req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/runs/{}/cancel", run.id))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(Peer {
            uid: Some(0),
            gid: Some(0),
        }));
        let response = router(p).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(child.0.try_wait().unwrap().is_none());
    }

    #[test]
    fn the_status_reports_the_newer_of_run_and_connection_check() {
        let t = |h: u32| chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 1, 5, h, 0, 0).unwrap();
        let pbs = |reachable, checked_at| crate::status::PbsStatus {
            server: "pbs.example.com".into(),
            datastore: "datastore".into(),
            namespace: None,
            reachable,
            checked_at,
            wake_on_lan: false,
            credentials_stored: None,
        };
        // A check after the last run that got no answer wins.
        let mut s = pbs(Some(true), Some(t(8)));
        newest_reachability(&mut s, Some((false, t(9))));
        assert_eq!((s.reachable, s.checked_at), (Some(false), Some(t(9))));
        // A run after the check wins again.
        let mut s = pbs(Some(true), Some(t(10)));
        newest_reachability(&mut s, Some((false, t(9))));
        assert_eq!((s.reachable, s.checked_at), (Some(true), Some(t(10))));
        // Nothing known from a run yet: the check is all there is.
        let mut s = pbs(None, None);
        newest_reachability(&mut s, Some((true, t(9))));
        assert_eq!((s.reachable, s.checked_at), (Some(true), Some(t(9))));
        // No check made: unchanged.
        let mut s = pbs(Some(true), Some(t(8)));
        newest_reachability(&mut s, None);
        assert_eq!((s.reachable, s.checked_at), (Some(true), Some(t(8))));
    }
}
