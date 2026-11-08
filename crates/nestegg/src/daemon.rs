//! `nestegg daemon`: local JSON API on a Unix socket (see `api/openapi.yaml`).

use std::collections::HashMap;
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
}

/// Credentials of the process on the other end of the socket.
#[derive(Clone, Debug)]
pub struct Peer {
    pub uid: Option<u32>,
}

impl Connected<axum::serve::IncomingStream<'_, tokio::net::UnixListener>> for Peer {
    fn connect_info(stream: axum::serve::IncomingStream<'_, tokio::net::UnixListener>) -> Self {
        Peer {
            uid: stream.io().peer_cred().ok().map(|c| c.uid()),
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
        .route("/api/v1/runs/{id}", get(get_run))
        .route("/api/v1/snapshots", get(list_snapshots))
        .route("/api/v1/runs/{id}/cancel", axum::routing::post(cancel_run))
        .route("/api/v1/snapshots/{time}/browse", get(browse))
        .route("/api/v1/restores", axum::routing::post(start_restore))
        .route("/api/v1/restores/{id}", get(get_restore))
        .route("/api/v1/config", get(config))
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
        })
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "version": env!("CARGO_PKG_VERSION"), "api_version": API_VERSION }))
}

fn load_config(paths: &Paths) -> Option<Config> {
    Config::load(&paths.config_file).ok()
}

async fn status(AxState(app): AxState<App>) -> ApiResult<Json<serde_json::Value>> {
    let status = blocking(move || {
        let config = load_config(&app.paths);
        let status =
            crate::status::build(&app.paths, config.as_ref(), crate::status::timer_enabled())?;
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
        runs.extend(store.list_runs(limit)?);
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
    let run = blocking(move || -> Result<std::result::Result<Run, &'static str>> {
        let _guard = app.start.lock().unwrap_or_else(|e| e.into_inner());
        if crate::run::active_run(&app.paths)?.is_some() {
            return Ok(Err("a backup run is already in progress"));
        }
        let run = Run::new(Trigger::Manual, Utc::now());
        let store = Store::new(&app.paths);
        store.save_current(&run)?;
        if let Err(e) = spawn_run(&run.id) {
            let _ = store.clear_current_if(&run.id);
            return Err(e);
        }
        Ok(Ok(run))
    })
    .await?;
    match run {
        Ok(run) => Ok((StatusCode::ACCEPTED, Json(run))),
        Err(msg) => Err(ApiError::new(StatusCode::CONFLICT, "run_in_progress", msg)),
    }
}

/// Start `nestegg run` outside the daemon process: as a transient systemd
/// unit when running under systemd (own cgroup, journal), otherwise as a
/// detached child process.
fn spawn_run(run_id: &str) -> Result<()> {
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
    cmd.args(["run", "--trigger", "manual", "--run-id", run_id]);
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
    let config = blocking(move || Config::load(&app.paths.config_file)).await?;
    Ok(Json(crate::status::config_view(&config)))
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
    Query(q): Query<BrowseQuery>,
) -> ApiResult<Json<serde_json::Value>> {
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
    let scope = match &requester {
        crate::restore::Requester::User(user) => {
            let scope = crate::restore::user_scope(&config, &q.archive, &user.home)
                .ok_or_else(|| forbidden("this archive does not contain your home directory"))?;
            if !scope.contains(&path) && !scope.is_ancestor(&path) {
                return Err(forbidden("you can only browse your own home directory"));
            }
            Some(scope)
        }
        crate::restore::Requester::Root { .. } => None,
    };
    let paths = app.paths.clone();
    let archive = q.archive.clone();
    let entries =
        tokio::task::spawn_blocking(move || -> ApiResult<Vec<crate::catalog::DirEntry>> {
            let cache = crate::catalog::cache_path(&paths.runtime_dir, &snapshot);
            if !cache.exists() {
                if !crate::wake::is_reachable(&config, std::time::Duration::from_secs(3)) {
                    return Err(pbs_unreachable());
                }
                crate::secrets::create_private_dir(cache.parent().unwrap_or(Path::new("/")))
                    .map_err(ApiError::internal)?;
                let client =
                    crate::client::Client::new(&config, SecretStore::new(&paths.credentials_dir));
                client
                    .catalog_dump(&snapshot, &cache)
                    .map_err(|e| ApiError::internal(format!("{e:#}")))?;
                crate::catalog::prune_cache(&paths.runtime_dir, 5);
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
    Json(body): Json<RestoreBody>,
) -> ApiResult<(StatusCode, Json<RestoreJob>)> {
    let requester = requester(&peer)?;
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
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "target_exists",
            format!("{} already exists", target.display()),
        ));
    }
    let config = blocking({
        let p = app.paths.clone();
        move || Config::load(&p.config_file)
    })
    .await?;
    let snapshot = snapshot_of(&config, &body.snapshot_time)?;
    // Validate archives and paths; unprivileged users are limited to their
    // own home directory.
    for sel in &mut selections {
        check_archive_name(&sel.archive)?;
        let mut normalized = Vec::new();
        for p in &sel.paths {
            normalized.push(crate::restore::normalize_archive_path(p).map_err(restore_error)?);
        }
        if let crate::restore::Requester::User(user) = &requester {
            let scope = crate::restore::user_scope(&config, &sel.archive, &user.home)
                .ok_or_else(|| forbidden("this archive does not contain your home directory"))?;
            if normalized.is_empty() || normalized.iter().any(|p| p == "/") {
                normalized = vec![scope.prefix.clone()];
            }
            if let Some(p) = normalized.iter().find(|p| !scope.contains(p)) {
                return Err(forbidden(format!("{p} is outside your home directory")));
            }
        }
        sel.paths = normalized;
    }
    let mut jobs = app.restores.lock().unwrap_or_else(|e| e.into_inner());
    if jobs.values().any(|j| j.state == "running") {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "restore_in_progress",
            "another restore is running",
        ));
    }
    let id = format!(
        "{}-{:04x}",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        rand::random::<u16>()
    );
    let job = RestoreJob {
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
        let progress = crate::restore::Progress::default();
        let monitor_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let monitor = {
            let stop = monitor_stop.clone();
            let src = progress.restored_bytes.clone();
            let dst = restored.clone();
            let jobs = jobs.clone();
            let id = id.to_string();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let now = done_before + src.load(Ordering::Relaxed);
                    dst.store(now, Ordering::Relaxed);
                    if let Some(j) = jobs.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&id) {
                        j.restored_bytes = now;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            })
        };
        let mut log = Vec::new();
        let result = crate::restore::execute(
            paths,
            config,
            crate::restore::Job {
                snapshot,
                archive: &sel.archive,
                target: &sub,
                patterns: &patterns,
                key: crate::client::RestoreKey::Auto,
                staging_dir: None,
            },
            requester,
            &progress,
            &mut log,
        );
        monitor_stop.store(true, Ordering::Relaxed);
        let _ = monitor.join();
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
    UrlPath(id): UrlPath<String>,
) -> ApiResult<Json<RestoreJob>> {
    let jobs = app.restores.lock().unwrap_or_else(|e| e.into_inner());
    let mut job = jobs
        .get(&id)
        .cloned()
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "unknown restore"))?;
    if job.state == "running" {
        job.restored_bytes = job.restored.load(Ordering::Relaxed);
    }
    Ok(Json(job))
}

async fn cancel_run(
    AxState(app): AxState<App>,
    UrlPath(id): UrlPath<String>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    if !is_valid_run_id(&id) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "unknown run",
        ));
    }
    let paths = app.paths.clone();
    let found = blocking(move || {
        let text = std::fs::read_to_string(paths.runtime_dir.join("run.pid")).unwrap_or_default();
        let mut parts = text.split_whitespace();
        let (Some(pid), Some(run_id)) = (parts.next(), parts.next()) else {
            return Ok(false);
        };
        if run_id != id || crate::run::active_run(&paths)?.is_none_or(|r| r.id != id) {
            return Ok(false);
        }
        let pid: i32 = pid.parse()?;
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        )?;
        Ok(true)
    })
    .await?;
    if found {
        Ok((StatusCode::ACCEPTED, Json(json!({ "cancelling": true }))))
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no running backup with this id",
        ))
    }
}

#[derive(Deserialize)]
struct WakeQuery {
    wake: Option<bool>,
}

async fn pbs_check(
    AxState(app): AxState<App>,
    Query(q): Query<WakeQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let paths = app.paths.clone();
    let wake = q.wake.unwrap_or(false);
    let checks = blocking(move || {
        Ok(crate::check::run(
            &paths,
            &crate::check::CheckOptions { wake, login: true },
        ))
    })
    .await?;
    Ok(Json(json!({
        "checks": checks,
        "checked_at": crate::state::format_time(Utc::now()),
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
            "server": format!("{}:{}", config.pbs.server, config.pbs.port),
            "datastore": config.pbs.datastore,
            "namespace": config.pbs.namespace,
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
