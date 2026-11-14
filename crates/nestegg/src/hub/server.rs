use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use hyper_util::rt::TokioIo;
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{
    config::Config,
    crypto, pbs,
    state::{self, Transaction},
};

#[derive(RustEmbed)]
#[folder = "$NESTEGG_UI_DIST"]
#[allow_missing = true]
struct Assets;

#[derive(Clone)]
struct Hub {
    config: Arc<Config>,
    pin: String,
    auth: Arc<Mutex<Auth>>,
}
#[derive(Default)]
struct Auth {
    sessions: HashMap<String, Session>,
    attempts: HashMap<std::net::IpAddr, (Instant, u32)>,
}
struct Session {
    last: Instant,
    generation: String,
}
const IDLE: Duration = Duration::from_secs(30 * 60);
const DELAY: Duration = Duration::from_millis(750);

impl Auth {
    fn session(&mut self, token: &str, generation: &str) -> bool {
        self.sessions
            .retain(|_, s| s.last.elapsed() < IDLE && s.generation == generation);
        if let Some(s) = self.sessions.get_mut(&crypto::hash(token)) {
            s.last = Instant::now();
            true
        } else {
            false
        }
    }
    fn limited(&mut self, ip: std::net::IpAddr) -> bool {
        self.attempts
            .retain(|_, (at, _)| at.elapsed() < Duration::from_secs(60));
        if self.attempts.len() >= 16384 && !self.attempts.contains_key(&ip) {
            return true;
        }
        let n = self.attempts.entry(ip).or_insert((Instant::now(), 0));
        n.1 += 1;
        n.1 > 5
    }
}

#[derive(Debug)]
pub(super) struct Error(StatusCode, &'static str, String);
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.1, self.2)
    }
}
impl std::error::Error for Error {}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"code":self.1,"message":self.2}))).into_response()
    }
}
fn error(status: StatusCode, code: &'static str, text: &str) -> Error {
    Error(status, code, text.into())
}
fn unauthorized() -> Error {
    error(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "Authentication required",
    )
}
fn bad() -> Error {
    error(StatusCode::BAD_REQUEST, "bad_request", "Invalid request")
}
fn internal(_: anyhow::Error) -> Error {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "pbs_unreachable",
        "Hub storage or TLS configuration unavailable; inspect the hub locally",
    )
}
fn upstream(e: anyhow::Error) -> Error {
    if e.downcast_ref::<pbs::Refused>().is_some() {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "pbs_refused",
            &e.to_string(),
        )
    } else {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "pbs_unreachable",
            "PBS cannot be reached or its certificate could not be verified",
        )
    }
}
fn cookie(req: &Request) -> String {
    req.headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.split(';')
                .map(str::trim)
                .find_map(|s| s.strip_prefix("nestegg_hub_session="))
        })
        .unwrap_or("")
        .into()
}
fn bearer(req: &Request) -> String {
    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("")
        .into()
}

pub async fn serve(config: Config) -> Result<()> {
    let tls = super::tls::server(&config.certificate, &config.private_key)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .context("bind hub HTTPS listener")?;
    let hub = Hub {
        pin: super::tls::pin(&config.certificate)?,
        config: Arc::new(config),
        auth: Arc::default(),
    };
    let app = Router::new().fallback(handle).with_state(hub);
    loop {
        tokio::select! {
            _ = crate::daemon::shutdown_signal() => return Ok(()),
            incoming = listener.accept() => {
                let (stream,address) = incoming?;
                let acceptor = acceptor.clone();
                let app = app.clone();
                tokio::spawn(async move {
                    let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_secs(10),acceptor.accept(stream)).await else { return; };
                    let service = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                        let app = app.clone();
                        req.extensions_mut().insert(address);
                        async move { app.oneshot(req.map(Body::new)).await }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().timer(hyper_util::rt::TokioTimer::new())
                        .header_read_timeout(Duration::from_secs(10)).serve_connection(TokioIo::new(stream),service).await;
                });
            }
        }
    }
}

async fn handle(State(hub): State<Hub>, req: Request) -> Response {
    let head = req.method() == Method::HEAD;
    let mut response = match dispatch(hub, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    };
    let h = response.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY,header::HeaderValue::from_static("default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        header::X_FRAME_OPTIONS,
        header::HeaderValue::from_static("DENY"),
    );
    h.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    if head {
        *response.body_mut() = Body::empty();
    }
    response
}

async fn dispatch(hub: Hub, req: Request) -> std::result::Result<Response, Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    if !path.starts_with("/api/") {
        if method != Method::GET && method != Method::HEAD {
            return Err(bad());
        }
        return tokio::task::spawn_blocking(move || static_file(&hub, &path))
            .await
            .map_err(|_| internal(anyhow::anyhow!("static request failed")))?;
    }
    // Browser mutations require the custom header, including login. Computer
    // endpoints use the code/Bearer credential and intentionally need no header.
    let computer = path == "/api/hub/v1/join" || path == "/api/hub/v1/machines/self/leave";
    if !matches!(method, Method::GET | Method::HEAD)
        && !computer
        && req
            .headers()
            .get("X-Nestegg-Hub")
            .and_then(|h| h.to_str().ok())
            != Some("1")
    {
        return Err(unauthorized());
    }
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let expected = url::Url::parse(&hub.config.url)
            .map(|u| u.origin().ascii_serialization())
            .map_err(|_| bad())?;
        if origin.to_str().ok() != Some(expected.as_str()) {
            return Err(unauthorized());
        }
    }
    let session = cookie(&req);
    let credential = bearer(&req);
    let address = req
        .extensions()
        .get::<SocketAddr>()
        .map(|s| s.ip())
        .unwrap_or(std::net::Ipv4Addr::LOCALHOST.into());
    let bytes = tokio::time::timeout(
        Duration::from_secs(10),
        to_bytes(req.into_body(), 16 * 1024),
    )
    .await
    .map_err(|_| bad())?
    .map_err(|_| bad())?;
    let body: Value = if bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&bytes).map_err(|_| bad())?
    };
    // File locks and password hashing are blocking. Keep them off Tokio's I/O
    // threads; the transaction spans provisioning and publishes only on success.
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        runtime.block_on(api(hub, method, path, session, credential, address, body))
    })
    .await
    .map_err(|_| internal(anyhow::anyhow!("request task failed")))?
}

async fn api(
    hub: Hub,
    method: Method,
    path: String,
    session: String,
    credential: String,
    address: std::net::IpAddr,
    body: Value,
) -> std::result::Result<Response, Error> {
    let mut t = Transaction::open(&hub.config.state_dir).map_err(internal)?;
    let logged_in = hub
        .auth
        .lock()
        .unwrap()
        .session(&session, &t.state.password_generation);
    let path = path
        .strip_prefix("/api/hub/v1")
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "not_found", "Unknown endpoint"))?;
    if path == "/session" {
        match method {
            Method::GET | Method::HEAD => {
                return Ok(Json(json!({"logged_in":logged_in})).into_response());
            }
            Method::DELETE => {
                hub.auth
                    .lock()
                    .unwrap()
                    .sessions
                    .remove(&crypto::hash(&session));
                return Ok((StatusCode::NO_CONTENT,[(header::SET_COOKIE,"nestegg_hub_session=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0")]).into_response());
            }
            Method::POST => {
                let limited = hub.auth.lock().unwrap().limited(address);
                if limited {
                    drop(t);
                    tokio::time::sleep(DELAY).await;
                    return Err(error(
                        StatusCode::TOO_MANY_REQUESTS,
                        "rate_limited",
                        "Try again later",
                    ));
                }
                let password = body["password"].as_str().ok_or_else(bad)?;
                if !crypto::password_matches(password.as_bytes(), &t.state.admin_hash) {
                    drop(t);
                    tokio::time::sleep(DELAY).await;
                    return Err(unauthorized());
                }
                let token = crypto::random_secret();
                let mut auth = hub.auth.lock().unwrap();
                if auth.sessions.len() >= 1024 {
                    auth.sessions.clear();
                }
                auth.sessions.insert(
                    crypto::hash(&token),
                    Session {
                        last: Instant::now(),
                        generation: t.state.password_generation.clone(),
                    },
                );
                auth.attempts.remove(&address);
                return Ok((
                    StatusCode::NO_CONTENT,
                    [(
                        header::SET_COOKIE,
                        format!(
                            "nestegg_hub_session={token}; Path=/; HttpOnly; Secure; SameSite=Strict"
                        ),
                    )],
                )
                    .into_response());
            }
            _ => return Err(bad()),
        }
    }
    if path == "/join" && method == Method::POST {
        return join(&hub, t, body).await;
    }
    if path == "/machines/self/leave" && method == Method::POST {
        let id = t
            .state
            .machines
            .iter()
            .find(|m| crypto::matches(&credential, &m.credential_digest))
            .map(|m| m.machine_id.clone())
            .ok_or_else(unauthorized)?;
        return remove(&hub, t, &id).await;
    }
    if !logged_in {
        return Err(unauthorized());
    }
    match (method,path) {
        (Method::GET|Method::HEAD,"/hub") => Ok(Json(hub_info(&hub,&t.state)).into_response()),
        (Method::GET|Method::HEAD,"/join-codes") => Ok(Json(json!({"codes": t.state.codes.iter().filter(|c| c.valid(&state::now())).map(|c| c.info()).collect::<Vec<_>>()})).into_response()),
        (Method::POST,"/join-codes") => {
            let note = body.get("note").map(|n| n.as_str().ok_or_else(bad)).transpose()?;
            let result = create_code(&hub.config,&hub.pin,&mut t,note)?;
            Ok((StatusCode::CREATED,Json(result)).into_response())
        }
        (Method::GET|Method::HEAD,"/machines") => {
            let reachable = match login(&hub.config).await {
                Ok(client) => match client.overview().await {
                    Ok(groups) => { t.state.pbs_groups = groups; t.state.pbs_read_at = Some(state::now()); t.save().map_err(internal)?; true }
                    Err(_) => false,
                }, Err(_) => false,
            };
            Ok(Json(machines(&t.state,reachable)).into_response())
        }
        (Method::DELETE,p) if p.starts_with("/machines/") => remove(&hub,t,p.trim_start_matches("/machines/")).await,
        (Method::DELETE,p) if p.starts_with("/join-codes/") => {
            let id = p.trim_start_matches("/join-codes/");
            let n = t.state.codes.len();
            t.state.codes.retain(|c| c.code_id != id);
            if n == t.state.codes.len() { return Err(error(StatusCode::NOT_FOUND,"not_found","Code not found")); }
            t.save().map_err(internal)?;
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        _ => Err(error(StatusCode::NOT_FOUND,"not_found","Unknown endpoint")),
    }
}

pub(super) fn create_code(
    config: &Config,
    pin: &str,
    t: &mut Transaction,
    note: Option<&str>,
) -> std::result::Result<Value, Error> {
    if t.state.master.proven_at.is_none() {
        return Err(error(
            StatusCode::CONFLICT,
            "master_key_unproven",
            "Prove the master key copy first",
        ));
    }
    if t.state.package.is_none() {
        return Err(error(
            StatusCode::CONFLICT,
            "no_package",
            "Add a nestegg package first",
        ));
    }
    if note.is_some_and(|s| s.chars().count() > 80) {
        return Err(bad());
    }
    let code = crypto::code();
    let info = state::Code {
        code_id: crypto::random_secret(),
        digest: crypto::hash(&code),
        note: note.map(str::to_string),
        created_at: state::now(),
        expires_at: (chrono::Utc::now() + chrono::Duration::minutes(30))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    let mut result = info.info();
    result["code"] = json!(code);
    result["command"] = json!(super::script::command(&config.url, pin, &code));
    t.state.codes.retain(|c| c.valid(&state::now()));
    t.state.codes.push(info);
    t.save().map_err(internal)?;
    Ok(result)
}

async fn login(config: &Config) -> Result<pbs::Client> {
    let password = config.password()?;
    let mut client = pbs::Client::new(&config.pbs)?;
    client.login(password.as_bytes()).await?;
    Ok(client)
}
async fn awake(config: &Config) -> Result<()> {
    let mut c = crate::config::example(&config.pbs.server, &config.pbs.datastore, &config.pbs.user);
    c.pbs.as_mut().context("PBS configuration")?.port = config.pbs.port;
    c.wake = config.wake.clone();
    ensure!(
        crate::wake::ensure_available(&c, |_| {})? != crate::wake::Availability::Unreachable,
        "PBS unavailable"
    );
    Ok(())
}
#[derive(Deserialize)]
struct Join {
    code: String,
    hostname: String,
    backup_id: Option<String>,
    #[serde(default)]
    adopt: bool,
    os: Option<String>,
    nestegg_version: Option<String>,
}
async fn join(hub: &Hub, mut t: Transaction, body: Value) -> std::result::Result<Response, Error> {
    let req: Join = serde_json::from_value(body).map_err(|_| bad())?;
    let now = state::now();
    // Scan every code and compare fixed-length hashes in constant time.
    let mut found = None;
    for (i, c) in t.state.codes.iter().enumerate() {
        if crypto::matches(&req.code, &c.digest) && c.valid(&now) {
            found = Some(i);
        }
    }
    let Some(index) = found else {
        drop(t);
        tokio::time::sleep(DELAY).await;
        return Err(error(
            StatusCode::FORBIDDEN,
            "invalid_code",
            "Invalid or expired code",
        ));
    };
    if t.state.master.proven_at.is_none() {
        return Err(error(
            StatusCode::CONFLICT,
            "master_key_unproven",
            "Prove master key first",
        ));
    }
    let name = req.backup_id.as_deref().unwrap_or(&req.hostname);
    if !super::config::valid_name(name)
        || req.hostname.is_empty()
        || req.hostname.len() > 255
        || req.os.as_ref().is_some_and(|s| s.len() > 256)
        || req.nestegg_version.as_ref().is_some_and(|s| s.len() > 128)
    {
        return Err(bad());
    }
    if t.state.machines.iter().any(|m| m.backup_id == name) {
        return Err(error(
            StatusCode::CONFLICT,
            "name_taken",
            "Computer name is already joined",
        ));
    }
    awake(&hub.config).await.map_err(upstream)?;
    let client = login(&hub.config).await.map_err(upstream)?;
    let groups = client.groups().await.map_err(upstream)?;
    let auth_id = format!("{}!{name}", hub.config.pbs.user);
    if let Some(group) = groups
        .iter()
        .find(|g| g["backup-type"] == "host" && g["backup-id"] == name)
    {
        if !req.adopt {
            return Err(error(
                StatusCode::CONFLICT,
                "name_exists_on_pbs",
                "Name already exists on PBS; explicit adoption required",
            ));
        }
        if group["owner"] != auth_id {
            return Err(error(
                StatusCode::CONFLICT,
                "name_taken",
                "Backup group is owned by another identity and cannot be adopted",
            ));
        }
    }
    // A durable intent permits safe compensation after a process crash or a
    // lost token response. Never delete an unrelated, pre-existing token.
    if t.state.pending_joins.iter().any(|n| n == name) {
        client.delete_token(name).await.map_err(upstream)?;
        t.state.pending_joins.retain(|n| n != name);
        t.save().map_err(internal)?;
    }
    if client.token_exists(name).await.map_err(upstream)? {
        return Err(error(
            StatusCode::CONFLICT,
            "name_taken",
            "A PBS token already uses this name",
        ));
    }
    t.state.pending_joins.push(name.into());
    t.save().map_err(internal)?;
    let (auth_id, secret) = client.create_token(name).await.map_err(upstream)?;
    if let Err(e) = client.acl(&auth_id).await {
        let _ = client.delete_token(name).await;
        return Err(upstream(e));
    }
    let machine_id = crypto::random_secret();
    let credential = crypto::random_secret();
    t.state.machines.push(state::Machine {
        machine_id: machine_id.clone(),
        backup_id: name.into(),
        hostname: req.hostname.clone(),
        os: req.os,
        nestegg_version: req.nestegg_version,
        joined_at: now,
        credential_digest: crypto::hash(&credential),
    });
    t.state.codes.remove(index);
    t.state.pending_joins.retain(|n| n != name);
    if let Err(e) = t.save() {
        let _ = client.delete_token(name).await;
        return Err(internal(e));
    }
    let c = &hub.config;
    Ok(Json(json!({"machine_id":machine_id,"backup_id":name,"machine_credential":credential,
        "pbs":{"server":c.pbs.server,"port":c.pbs.port,"datastore":c.pbs.datastore,"namespace":c.pbs.namespace,"fingerprint":c.pbs.fingerprint,"auth_id":auth_id,"secret":secret},
        "wake":c.wake.as_ref().map(|w|json!({"mac":w.mac,"broadcast":w.broadcast.to_string(),"timeout_seconds":w.timeout_seconds})),
        "master_key":{"public_key_pem":t.state.master.public_key_pem,"fingerprint":t.state.master.fingerprint,"proven_at":t.state.master.proven_at},
        "hub":{"name":c.name,"url":c.url,"pin":hub.pin}
    })).into_response())
}
async fn remove(hub: &Hub, mut t: Transaction, id: &str) -> std::result::Result<Response, Error> {
    let machine = t
        .state
        .machines
        .iter()
        .find(|m| m.machine_id == id)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "not_found", "Computer not found"))?;
    let client = login(&hub.config).await.map_err(upstream)?;
    client
        .delete_token(&machine.backup_id)
        .await
        .map_err(upstream)?;
    t.state.machines.retain(|m| m.machine_id != id);
    t.save().map_err(internal)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn hub_info(hub: &Hub, s: &state::State) -> Value {
    let c = &hub.config;
    json!({"name":c.name,"version":env!("CARGO_PKG_VERSION"),"url":c.url,"pin":hub.pin,
        "pbs":{"server":c.pbs.server,"port":c.pbs.port,"datastore":c.pbs.datastore,"namespace":c.pbs.namespace,"user":c.pbs.user,"wake_on_lan":c.wake.is_some()},
        "master_key":{"present":s.master.public_key_pem.is_some(),"proven":s.master.proven_at.is_some(),"proven_at":s.master.proven_at,"fingerprint":s.master.fingerprint},
        "package":{"available":s.package.is_some(),"version":s.package.as_ref().map(|p|&p.version),"architecture":s.package.as_ref().map(|p|&p.architecture)}})
}
fn timestamp(v: &Value) -> Value {
    v.as_i64()
        .and_then(|n| chrono::DateTime::from_timestamp(n, 0))
        .map(|d| json!(d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)))
        .unwrap_or(Value::Null)
}
fn machines(s: &state::State, reachable: bool) -> Value {
    let machines: Vec<Value> = s.machines.iter().map(|m| {
        let g = s.pbs_groups.iter().find(|g|g["backup-type"] == "host" && g["backup-id"] == m.backup_id);
        let empty = Value::Null;
        let g = g.unwrap_or(&empty);
        json!({"machine_id":m.machine_id,"backup_id":m.backup_id,"hostname":m.hostname,"os":m.os,"nestegg_version":m.nestegg_version,"joined_at":m.joined_at,"snapshot_count":g["backup-count"].as_u64().unwrap_or(0),"last_snapshot":timestamp(&g["last-backup"]),"last_snapshot_verified":g["last_snapshot_verified"],"report":g["report"]})
    }).collect();
    let unmanaged: Vec<Value> = s
        .pbs_groups
        .iter()
        .filter(|g| {
            g["backup-type"] == "host" && !s.machines.iter().any(|m| g["backup-id"] == m.backup_id)
        })
        .map(|g| json!({"backup_id":g["backup-id"],"last_snapshot":timestamp(&g["last-backup"])}))
        .collect();
    json!({"pbs_reachable":reachable,"pbs_read_at":s.pbs_read_at,"machines":machines,"unmanaged":unmanaged})
}
fn static_file(hub: &Hub, path: &str) -> std::result::Result<Response, Error> {
    if path == "/join.sh" {
        return Ok((
            [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
            super::script::render(&hub.config.url, &hub.pin).map_err(internal)?,
        )
            .into_response());
    }
    if path == "/pkg/nestegg.deb" || path == "/pkg/SHA256SUMS" {
        let t = Transaction::open(&hub.config.state_dir).map_err(internal)?;
        let pkg = t
            .state
            .package
            .ok_or_else(|| error(StatusCode::NOT_FOUND, "no_package", "No package available"))?;
        if path.ends_with("SHA256SUMS") {
            return Ok((
                [(header::CONTENT_TYPE, "text/plain")],
                format!("{}  nestegg.deb\n", pkg.sha256),
            )
                .into_response());
        }
        return Ok((
            [(
                header::CONTENT_TYPE,
                "application/vnd.debian.binary-package",
            )],
            std::fs::read(pkg.file).map_err(|e| internal(e.into()))?,
        )
            .into_response());
    }
    let asset = if path == "/" {
        "hub.html"
    } else {
        path.trim_start_matches('/')
    };
    if let Some(file) = Assets::get(asset) {
        return Ok((
            [(
                header::CONTENT_TYPE,
                mime_guess::from_path(asset)
                    .first_or_octet_stream()
                    .to_string(),
            )],
            file.data.into_owned(),
        )
            .into_response());
    }
    if path == "/" {
        return Ok(([(header::CONTENT_TYPE,"text/html; charset=utf-8")],"<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>nestegg hub</title><h1>nestegg hub</h1><p>The hub web interface is not built into this binary yet.</p></html>").into_response());
    }
    Err(error(StatusCode::NOT_FOUND, "not_found", "Not found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sessions_expire_and_password_change_revokes_them() {
        let mut a = Auth::default();
        a.sessions.insert(
            crypto::hash("test"),
            Session {
                last: Instant::now(),
                generation: "1".into(),
            },
        );
        assert!(!a.session("wrong", "1"));
        assert!(a.session("test", "1"));
        assert!(!a.session("test", "2"));
        a.sessions.insert(
            crypto::hash("test"),
            Session {
                last: Instant::now() - IDLE,
                generation: "2".into(),
            },
        );
        assert!(!a.session("test", "2"));
    }
    #[test]
    fn limits_are_per_address() {
        let mut a = Auth::default();
        let ip = "192.0.2.1".parse().unwrap();
        for _ in 0..5 {
            assert!(!a.limited(ip));
        }
        assert!(a.limited(ip));
        assert!(!a.limited("192.0.2.2".parse().unwrap()));
        a.attempts.get_mut(&ip).unwrap().0 -= Duration::from_secs(61);
        assert!(!a.limited(ip));
    }
}

#[cfg(test)]
mod api_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn fixture(fake: &pbs::tests::Fake) -> (tempfile::TempDir, Hub) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            name: "Home".into(),
            url: "https://hub.example.com:8443".into(),
            listen: "127.0.0.1:8443".into(),
            state_dir: dir.path().join("state"),
            credentials_dir: dir.path().join("credentials"),
            certificate: dir.path().join("tls.crt"),
            private_key: dir.path().join("tls.key"),
            pbs: fake.config.clone(),
            wake: None,
        };
        crate::secrets::create_private_dir(&config.credentials_dir).unwrap();
        crate::secrets::create_new_private_file(
            &config.credentials_dir.join("password"),
            b"test-only-password",
        )
        .unwrap();
        let mut t = Transaction::open(&config.state_dir).unwrap();
        t.state.admin_hash = crypto::password_hash(b"test-only-admin").unwrap();
        t.state.password_generation = "generation".into();
        t.state.master.public_key_pem = Some("test public key".into());
        t.state.master.fingerprint = Some("test fingerprint".into());
        t.state.master.proven_at = Some(state::now());
        t.state.package = Some(state::Package {
            version: "0.1.0".into(),
            architecture: "amd64".into(),
            sha256: "test digest".into(),
            file: dir.path().join("unused.deb"),
        });
        t.save().unwrap();
        (
            dir,
            Hub {
                config: Arc::new(config),
                pin: "sha256//test".into(),
                auth: Arc::default(),
            },
        )
    }
    async fn request(
        hub: &Hub,
        method: Method,
        path: &str,
        body: Value,
        cookie: Option<&str>,
        custom: bool,
    ) -> Response {
        let mut req = Request::builder().method(method).uri(path);
        if custom {
            req = req.header("X-Nestegg-Hub", "1");
        }
        if let Some(cookie) = cookie {
            req = req.header(header::COOKIE, cookie);
        }
        let app = Router::new().fallback(handle).with_state(hub.clone());
        app.oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
    }
    async fn body(response: Response) -> Value {
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap()
    }
    fn code(hub: &Hub) -> Value {
        let mut t = Transaction::open(&hub.config.state_dir).unwrap();
        create_code(&hub.config, &hub.pin, &mut t, None).unwrap()
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_join_keeps_code_and_retry_compensates_pending_token() {
        let fake = pbs::tests::Fake::start().await;
        let (_dir, hub) = fixture(&fake);
        let code = code(&hub);
        fake.refuse_acl.store(true, Ordering::SeqCst);
        let req = json!({"code":code["code"],"hostname":"desktop"});
        let failed = request(
            &hub,
            Method::POST,
            "/api/hub/v1/join",
            req.clone(),
            None,
            false,
        )
        .await;
        assert_eq!(failed.status(), StatusCode::SERVICE_UNAVAILABLE);
        let t = Transaction::open(&hub.config.state_dir).unwrap();
        assert_eq!(t.state.codes.len(), 1);
        assert!(t.state.machines.is_empty());
        assert_eq!(t.state.pending_joins, vec!["desktop"]);
        assert!(!fake.token.load(Ordering::SeqCst));
        drop(t);
        // Simulate a crash after PBS token creation but before response/save.
        fake.token.store(true, Ordering::SeqCst);
        fake.refuse_acl.store(false, Ordering::SeqCst);
        let joined = request(
            &hub,
            Method::POST,
            "/api/hub/v1/join",
            req.clone(),
            None,
            false,
        )
        .await;
        assert_eq!(joined.status(), StatusCode::OK);
        let joined = body(joined).await;
        let t = Transaction::open(&hub.config.state_dir).unwrap();
        assert!(t.state.codes.is_empty());
        assert!(t.state.pending_joins.is_empty());
        assert_eq!(t.state.machines.len(), 1);
        assert!(crypto::matches(
            joined["machine_credential"].as_str().unwrap(),
            &t.state.machines[0].credential_digest
        ));
        let disk = std::fs::read_to_string(hub.config.state_dir.join("state.json")).unwrap();
        assert!(!disk.contains(code["code"].as_str().unwrap()));
        assert!(!disk.contains(joined["machine_credential"].as_str().unwrap()));
        assert!(!disk.contains("test-only-token-value"));
        drop(t);
        let denied = request(&hub, Method::POST, "/api/hub/v1/join", req, None, false).await;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert_eq!(body(denied).await["code"], "invalid_code");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sessions_headers_csrf_and_login_failures() {
        let fake = pbs::tests::Fake::start().await;
        let (_dir, hub) = fixture(&fake);
        assert_eq!(
            request(&hub, Method::GET, "/api/hub/v1/hub", json!({}), None, false)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let login = json!({"password":"test-only-admin"});
        assert_eq!(
            request(
                &hub,
                Method::POST,
                "/api/hub/v1/session",
                login.clone(),
                None,
                false
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );
        let start = Instant::now();
        let wrong = request(
            &hub,
            Method::POST,
            "/api/hub/v1/session",
            json!({"password":"wrong"}),
            None,
            true,
        )
        .await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        assert!(start.elapsed() >= DELAY);
        let logged = request(&hub, Method::POST, "/api/hub/v1/session", login, None, true).await;
        assert_eq!(logged.status(), StatusCode::NO_CONTENT);
        let cookie = logged.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_string();
        for flag in ["HttpOnly", "Secure", "SameSite=Strict"] {
            assert!(cookie.contains(flag));
        }
        assert_eq!(
            request(
                &hub,
                Method::POST,
                "/api/hub/v1/join-codes",
                json!({}),
                Some(&cookie),
                false
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(
                &hub,
                Method::POST,
                "/api/hub/v1/join-codes",
                json!({}),
                Some(&cookie),
                true
            )
            .await
            .status(),
            StatusCode::CREATED
        );
        // Password command's generation change invalidates every old session.
        let mut t = Transaction::open(&hub.config.state_dir).unwrap();
        t.state.password_generation = "changed".into();
        t.save().unwrap();
        drop(t);
        assert_eq!(
            request(
                &hub,
                Method::GET,
                "/api/hub/v1/hub",
                json!({}),
                Some(&cookie),
                false
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn code_gates_expiry_revocation_and_security_headers() {
        let fake = pbs::tests::Fake::start().await;
        let (_dir, hub) = fixture(&fake);
        let mut t = Transaction::open(&hub.config.state_dir).unwrap();
        t.state.master.proven_at = None;
        assert_eq!(
            create_code(&hub.config, &hub.pin, &mut t, None)
                .unwrap_err()
                .1,
            "master_key_unproven"
        );
        t.state.master.proven_at = Some(state::now());
        t.state.package = None;
        assert_eq!(
            create_code(&hub.config, &hub.pin, &mut t, None)
                .unwrap_err()
                .1,
            "no_package"
        );
        drop(t);
        let expired = code(&hub);
        let mut t = Transaction::open(&hub.config.state_dir).unwrap();
        t.state.codes[0].expires_at = "2000-01-01T00:00:00Z".into();
        t.save().unwrap();
        drop(t);
        for value in [expired["code"].as_str().unwrap(), "unknown"] {
            let start = Instant::now();
            let response = request(
                &hub,
                Method::POST,
                "/api/hub/v1/join",
                json!({"code":value,"hostname":"desktop"}),
                None,
                false,
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert!(start.elapsed() >= DELAY);
            assert_eq!(body(response).await["code"], "invalid_code");
        }
        let response = request(&hub, Method::GET, "/", json!({}), None, false).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
        assert_eq!(response.headers()[header::X_FRAME_OPTIONS], "DENY");
        assert!(
            fake.calls.lock().unwrap().is_empty(),
            "invalid codes never contact PBS"
        );
    }
}
