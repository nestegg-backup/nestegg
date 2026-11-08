//! `nestegg daemon`: local JSON API on a Unix socket (see `api/openapi.yaml`).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
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
    start: std::sync::Arc<std::sync::Mutex<()>>,
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
        .route("/api/v1/snapshots/{time}/browse", get(not_implemented))
        .route("/api/v1/restores", axum::routing::post(not_implemented))
        .route("/api/v1/restores/{id}", get(unknown_restore))
        .route("/api/v1/config", get(config))
        .fallback(|| async {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "unknown endpoint")
        })
        .with_state(App {
            paths,
            start: Default::default(),
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

async fn not_implemented() -> ApiError {
    ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        "not_implemented",
        "planned for v0.2",
    )
}

async fn unknown_restore() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "unknown restore")
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
    let app = router(paths);
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
