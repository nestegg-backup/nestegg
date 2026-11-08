//! `nestegg ui`: serves the web UI on loopback and forwards `/api/v1/*` to
//! the daemon socket.
//!
//! Security model
//! - Binds to 127.0.0.1 only, random port by default.
//! - A random session token is required. The first request carries it as
//!   `?token=`; the server answers with an HttpOnly, SameSite=Strict cookie.
//! - The Host header must name the loopback address (DNS rebinding).
//! - The browser is opened through a private redirect file, so the token
//!   never appears in another process' command line.
//! - Authorization against the daemon is done by the Unix socket permissions
//!   of the user running `nestegg ui` (group `nestegg`).

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use hyper_util::rt::TokioIo;
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../ui/dist/"]
#[allow_missing = true]
struct Assets;

const COOKIE: &str = "nestegg_session";

#[derive(Clone)]
struct Ui {
    token: String,
    port: u16,
    socket: PathBuf,
    dev_proxy: Option<String>,
}

pub struct UiOptions {
    pub port: u16,
    pub open_browser: bool,
    pub dev_proxy: Option<String>,
    pub socket: PathBuf,
}

pub fn new_token() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub async fn serve(options: UiOptions) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], options.port)))
        .await
        .context("cannot bind 127.0.0.1")?;
    let port = listener.local_addr()?.port();
    let ui = Ui {
        token: new_token(),
        port,
        socket: options.socket.clone(),
        dev_proxy: options.dev_proxy.clone(),
    };
    let url = format!("http://127.0.0.1:{port}/?token={}", ui.token);
    if !options.socket.exists() {
        eprintln!(
            "warning: daemon socket {} not found - is nestegg.service running?",
            options.socket.display()
        );
    }
    println!("nestegg UI: {url}");
    println!("(only reachable from this computer; press Ctrl+C to stop)");
    if options.open_browser {
        if let Err(e) = open_browser(&url, port) {
            eprintln!("could not open a browser automatically: {e:#}");
        }
    }
    let app = router(ui);
    axum::serve(listener, app)
        .with_graceful_shutdown(crate::daemon::shutdown_signal())
        .await?;
    Ok(())
}

fn router(ui: Ui) -> Router {
    Router::new().fallback(handle).with_state(ui)
}

async fn handle(State(ui): State<Ui>, req: Request) -> Response {
    if !host_allowed(req.headers(), ui.port) {
        return (StatusCode::MISDIRECTED_REQUEST, "invalid Host header").into_response();
    }
    // Token login.
    if let Some(token) = query_token(req.uri()) {
        if constant_time_eq(token.as_bytes(), ui.token.as_bytes()) {
            let mut resp = Redirect::to("/").into_response();
            let cookie = format!("{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/", ui.token);
            if let Ok(v) = HeaderValue::from_str(&cookie) {
                resp.headers_mut().insert(header::SET_COOKIE, v);
            }
            return resp;
        }
        return unauthorized();
    }
    if !authorized(req.headers(), &ui.token) {
        return unauthorized();
    }
    if req.method() != Method::GET
        && req.method() != Method::HEAD
        && !origin_allowed(req.headers(), ui.port)
    {
        return (StatusCode::FORBIDDEN, "cross-origin request rejected").into_response();
    }
    let path = req.uri().path().to_string();
    let mut resp = if path.starts_with("/api/") {
        forward_to_socket(&ui.socket, req).await
    } else if let Some(dev) = &ui.dev_proxy {
        forward_to_dev(dev, req).await
    } else {
        static_file(&path)
    };
    if ui.dev_proxy.is_none() {
        let headers = resp.headers_mut();
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
                 frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
            ),
        );
    }
    let headers = resp.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    resp
}

fn host_allowed(headers: &HeaderMap, port: u16) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
}

fn origin_allowed(headers: &HeaderMap, port: u16) -> bool {
    match headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) {
        None => true,
        Some(origin) => {
            origin == format!("http://127.0.0.1:{port}")
                || origin == format!("http://localhost:{port}")
        }
    }
}

fn query_token(uri: &Uri) -> Option<String> {
    uri.query()?
        .split('&')
        .find_map(|kv| kv.strip_prefix("token="))
        .map(String::from)
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    if let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        && let Some(bearer) = auth.strip_prefix("Bearer ")
    {
        return constant_time_eq(bearer.as_bytes(), token.as_bytes());
    }
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| {
            kv.trim()
                .strip_prefix(&format!("{COOKIE}="))
                .map(String::from)
        })
        .any(|value| constant_time_eq(value.as_bytes(), token.as_bytes()))
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "Not authorized. Open the address printed by `nestegg ui` in your terminal.\n",
    )
        .into_response()
}

fn static_file(path: &str) -> Response {
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    if let Some(file) = Assets::get(rel) {
        return asset_response(rel, file.data.into_owned());
    }
    // Single-page app: unknown paths without extension get index.html.
    if !rel.contains('.')
        && let Some(index) = Assets::get("index.html")
    {
        return asset_response("index.html", index.data.into_owned());
    }
    if rel == "index.html" {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            PLACEHOLDER,
        )
            .into_response();
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn asset_response(name: &str, data: Vec<u8>) -> Response {
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let cache = if name == "index.html" {
        "no-cache"
    } else {
        "public, max-age=3600"
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime.as_ref().to_string()),
            (header::CACHE_CONTROL, cache.to_string()),
        ],
        data,
    )
        .into_response()
}

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>nestegg</title>
<meta name="viewport" content="width=device-width, initial-scale=1"></head>
<body style="font-family: system-ui, sans-serif; max-width: 40rem; margin: 3rem auto; padding: 0 1rem">
<h1>nestegg</h1>
<p>The web interface is not built into this binary yet.</p>
<p>The API is available: <a href="api/v1/status">api/v1/status</a></p>
</body></html>"#;

fn bad_gateway(message: String) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        axum::Json(serde_json::json!({ "code": "internal", "message": message })),
    )
        .into_response()
}

async fn forward_to_socket(socket: &std::path::Path, req: Request) -> Response {
    let stream = match tokio::net::UnixStream::connect(socket).await {
        Ok(s) => s,
        Err(e) => {
            let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
                " (is your user in the 'nestegg' group? log out and in again after adding it)"
            } else {
                " (is nestegg.service running?)"
            };
            return bad_gateway(format!("cannot connect to daemon: {e}{hint}"));
        }
    };
    send_http1(TokioIo::new(stream), req, "localhost").await
}

async fn forward_to_dev(base: &str, req: Request) -> Response {
    let authority = base
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let stream = match tokio::net::TcpStream::connect(&authority).await {
        Ok(s) => s,
        Err(e) => return bad_gateway(format!("cannot reach dev server {base}: {e}")),
    };
    send_http1(TokioIo::new(stream), req, &authority).await
}

async fn send_http1<I>(io: I, req: Request, host: &str) -> Response
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let (mut sender, conn) = match hyper::client::conn::http1::handshake::<_, Body>(io).await {
        Ok(v) => v,
        Err(e) => return bad_gateway(format!("handshake failed: {e}")),
    };
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let (mut parts, body) = req.into_parts();
    // Never forward the session cookie or token to the backend.
    parts.headers.remove(header::COOKIE);
    parts.headers.remove(header::AUTHORIZATION);
    if let Ok(v) = HeaderValue::from_str(host) {
        parts.headers.insert(header::HOST, v);
    }
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    parts.uri = match path_and_query.parse() {
        Ok(u) => u,
        Err(_) => return bad_gateway("invalid URI".into()),
    };
    let upstream = Request::from_parts(parts, body);
    match sender.send_request(upstream).await {
        Ok(resp) => resp.map(Body::new),
        Err(e) => bad_gateway(format!("backend request failed: {e}")),
    }
}

/// Open the browser via a private redirect file in `$XDG_RUNTIME_DIR`.
fn open_browser(url: &str, port: u16) -> Result<()> {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        anyhow::bail!("no graphical session");
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("XDG_RUNTIME_DIR not set")?;
    let file = dir.join(format!("nestegg-ui-{port}.html"));
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"0; url={url}\">\
         <title>nestegg</title><a href=\"{url}\">Open nestegg</a>"
    );
    crate::secrets::write_private_file(&file, html.as_bytes())?;
    std::process::Command::new("xdg-open")
        .arg(&file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("cannot run xdg-open")?;
    // Remove the redirect file after the browser had time to read it.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let _ = std::fs::remove_file(file);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn ui() -> Ui {
        Ui {
            token: "abc123".into(),
            port: 4242,
            socket: "/nonexistent/api.sock".into(),
            dev_proxy: None,
        }
    }

    fn get(path: &str) -> axum::http::request::Builder {
        HttpRequest::builder()
            .uri(path)
            .header(header::HOST, "127.0.0.1:4242")
    }

    #[tokio::test]
    async fn rejects_without_token() {
        let resp = router(ui())
            .oneshot(get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn token_login_sets_cookie() {
        let resp = router(ui())
            .oneshot(get("/?token=abc123").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let cookie = resp.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.contains("nestegg_session=abc123"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
    }

    #[tokio::test]
    async fn wrong_token_rejected() {
        let resp = router(ui())
            .oneshot(get("/?token=nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cookie_grants_access_and_sets_security_headers() {
        let resp = router(ui())
            .oneshot(
                get("/")
                    .header(header::COOKIE, "nestegg_session=abc123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().contains_key(header::CONTENT_SECURITY_POLICY));
        assert_eq!(resp.headers()[header::X_FRAME_OPTIONS], "DENY");
    }

    #[tokio::test]
    async fn foreign_host_rejected() {
        let req = HttpRequest::builder()
            .uri("/?token=abc123")
            .header(header::HOST, "evil.example.com:4242")
            .body(Body::empty())
            .unwrap();
        let resp = router(ui()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::MISDIRECTED_REQUEST);
    }

    #[tokio::test]
    async fn cross_origin_post_rejected() {
        let req = get("/api/v1/runs")
            .method(Method::POST)
            .header(header::AUTHORIZATION, "Bearer abc123")
            .header(header::ORIGIN, "http://evil.example.com")
            .body(Body::empty())
            .unwrap();
        let resp = router(ui()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn api_without_daemon_is_bad_gateway() {
        let req = get("/api/v1/status")
            .header(header::AUTHORIZATION, "Bearer abc123")
            .body(Body::empty())
            .unwrap();
        let resp = router(ui()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn tokens_are_random_hex() {
        let a = new_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, new_token());
    }
}
