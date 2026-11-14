//! PBS management API over rustls + hyper. Refusal messages are bounded and
//! redact submitted passwords and authentication headers before leaving here.
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use axum::body::{Body, to_bytes};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio_rustls::TlsConnector;

use super::config::Pbs;

#[derive(Debug, thiserror::Error)]
#[error("PBS refused the request (HTTP {0}): {1}")]
pub struct Refused(pub u16, pub String);

pub struct Client {
    config: Pbs,
    tls: Arc<rustls::ClientConfig>,
    ticket: Option<String>,
    csrf: Option<String>,
}

pub fn segment(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
fn form(values: &[(&str, String)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(values.iter().map(|(k, v)| (*k, v.as_str())))
        .finish()
}

impl Client {
    pub fn new(config: &Pbs) -> Result<Self> {
        Ok(Self {
            config: config.clone(),
            tls: super::tls::client(config.fingerprint.as_deref())?,
            ticket: None,
            csrf: None,
        })
    }
    pub async fn login(&mut self, password: &[u8]) -> Result<()> {
        let password = std::str::from_utf8(password).context("PBS password must be UTF-8")?;
        let v = self
            .request(
                Method::POST,
                "/access/ticket",
                &[
                    ("username", self.config.user.clone()),
                    ("password", password.to_string()),
                ],
            )
            .await?;
        self.ticket = Some(
            v["ticket"]
                .as_str()
                .context("PBS returned no ticket")?
                .into(),
        );
        self.csrf = Some(
            v["CSRFPreventionToken"]
                .as_str()
                .context("PBS returned no CSRF token")?
                .into(),
        );
        Ok(())
    }
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        fields: &[(&str, String)],
    ) -> Result<Value> {
        tokio::time::timeout(
            Duration::from_secs(15),
            self.request_inner(method, path, fields),
        )
        .await
        .context("PBS request timed out")?
    }
    async fn request_inner(
        &self,
        method: Method,
        path: &str,
        fields: &[(&str, String)],
    ) -> Result<Value> {
        let host = self.config.server.trim_matches(['[', ']']);
        let stream = tokio::net::TcpStream::connect((host, self.config.port))
            .await
            .context("PBS unavailable")?;
        let name = rustls::pki_types::ServerName::try_from(host.to_string())
            .context("invalid PBS server name")?;
        let stream = TlsConnector::from(self.tls.clone())
            .connect(name, stream)
            .await
            .context("PBS TLS handshake failed")?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        struct Driver(tokio::task::JoinHandle<()>);
        impl Drop for Driver {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _driver = Driver(driver);
        let query = form(fields);
        let uri = if method == Method::GET && !query.is_empty() {
            format!("/api2/json{path}?{query}")
        } else {
            format!("/api2/json{path}")
        };
        let body = if method == Method::GET {
            String::new()
        } else {
            query
        };
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(
                "Host",
                format!("{}:{}", self.config.server, self.config.port),
            )
            .header("Content-Type", "application/x-www-form-urlencoded");
        if let Some(ticket) = &self.ticket {
            req = req.header("Cookie", format!("PBSAuthCookie={ticket}"));
        }
        if let Some(csrf) = &self.csrf {
            req = req.header("CSRFPreventionToken", csrf);
        }
        let response = sender
            .send_request(req.body(Body::from(body))?)
            .await
            .context("PBS request failed")?;
        let status = response.status();
        let bytes = to_bytes(Body::new(response.into_body()), 4 * 1024 * 1024)
            .await
            .context("PBS response too large or incomplete")?;
        if !status.is_success() {
            let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let message = value["message"]
                .as_str()
                .unwrap_or_else(|| status.canonical_reason().unwrap_or("PBS refused request"));
            let mut message = message.to_string();
            for secret in fields
                .iter()
                .filter(|(k, _)| *k == "password")
                .map(|(_, v)| v.as_str())
                .chain(self.ticket.as_deref())
                .chain(self.csrf.as_deref())
            {
                if !secret.is_empty() {
                    message = message
                        .replace(secret, "<redacted>")
                        .replace(&segment(secret), "<redacted>");
                }
            }
            let message: String = message
                .chars()
                .filter(|c| !c.is_control())
                .take(1024)
                .collect();
            return Err(Refused(status.as_u16(), message).into());
        }
        let mut v: Value = serde_json::from_slice(&bytes).context("PBS response is not JSON")?;
        Ok(v["data"].take())
    }
    fn token_path(&self, name: &str) -> String {
        format!(
            "/access/users/{}/token/{}",
            segment(&self.config.user),
            segment(name)
        )
    }
    pub async fn create_token(&self, name: &str) -> Result<(String, String)> {
        let v = self
            .request(Method::POST, &self.token_path(name), &[])
            .await?;
        let id = v["tokenid"]
            .as_str()
            .context("PBS returned no token id")?
            .to_string();
        ensure!(
            id == format!("{}!{name}", self.config.user),
            "PBS returned unexpected token id"
        );
        Ok((
            id,
            v["value"]
                .as_str()
                .context("PBS returned no token secret")?
                .into(),
        ))
    }
    pub async fn token_exists(&self, name: &str) -> Result<bool> {
        let tokens = self
            .request(
                Method::GET,
                &format!("/access/users/{}/token", segment(&self.config.user)),
                &[],
            )
            .await?;
        Ok(tokens
            .as_array()
            .context("PBS token list is not an array")?
            .iter()
            .any(|t| {
                t["tokenname"] == name || t["tokenid"] == format!("{}!{name}", self.config.user)
            }))
    }
    pub async fn delete_token(&self, name: &str) -> Result<()> {
        if !self.token_exists(name).await? {
            return Ok(());
        }
        match self
            .request(Method::DELETE, &self.token_path(name), &[])
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.downcast_ref::<Refused>().is_some_and(|r| r.0 == 404) => Ok(()),
            Err(e) => Err(e),
        }
    }
    pub fn acl_path(&self) -> String {
        let mut path = format!("/datastore/{}", self.config.datastore);
        if let Some(ns) = &self.config.namespace {
            path.push('/');
            path.push_str(ns);
        }
        path
    }
    pub async fn acl(&self, auth_id: &str) -> Result<()> {
        self.request(
            Method::PUT,
            "/access/acl",
            &[
                ("path", self.acl_path()),
                ("role", "DatastoreBackup".into()),
                ("auth-id", auth_id.into()),
            ],
        )
        .await?;
        Ok(())
    }
    fn store_path(&self, suffix: &str) -> String {
        format!(
            "/admin/datastore/{}/{suffix}",
            segment(&self.config.datastore)
        )
    }
    fn namespace_fields(&self) -> Vec<(&'static str, String)> {
        self.config
            .namespace
            .as_ref()
            .map(|ns| vec![("ns", ns.clone())])
            .unwrap_or_default()
    }
    pub async fn groups(&self) -> Result<Vec<Value>> {
        let v = self
            .request(
                Method::GET,
                &self.store_path("groups"),
                &self.namespace_fields(),
            )
            .await?;
        Ok(v.as_array().context("PBS groups are not an array")?.clone())
    }
    pub async fn overview(&self) -> Result<Vec<Value>> {
        let mut groups = self.groups().await?;
        for group in &mut groups {
            let mut fields = self.namespace_fields();
            fields.push((
                "backup-type",
                group["backup-type"].as_str().unwrap_or("host").into(),
            ));
            fields.push((
                "backup-id",
                group["backup-id"]
                    .as_str()
                    .context("group missing backup id")?
                    .into(),
            ));
            let snapshots = self
                .request(Method::GET, &self.store_path("snapshots"), &fields)
                .await?;
            let snapshots = snapshots
                .as_array()
                .context("PBS snapshots are not an array")?;
            group["backup-count"] = json!(snapshots.len());
            if let Some(latest) = snapshots
                .iter()
                .max_by_key(|s| s["backup-time"].as_i64().unwrap_or(0))
            {
                group["last-backup"] = latest["backup-time"].clone();
                group["last_snapshot_verified"] = match latest["verification"]["state"].as_str() {
                    Some("ok") => json!(true),
                    Some("failed") => json!(false),
                    _ => Value::Null,
                };
                fields.push(("backup-time", latest["backup-time"].to_string()));
                let notes = self
                    .request(Method::GET, &self.store_path("notes"), &fields)
                    .await?;
                group["report"] = notes
                    .as_str()
                    .or_else(|| notes["notes"].as_str())
                    .and_then(crate::run::parse_report)
                    .filter(Value::is_object)
                    .unwrap_or(Value::Null);
            }
        }
        Ok(groups)
    }
    pub async fn prepare(&self, user: &str, password: &[u8]) -> Result<()> {
        let password = std::str::from_utf8(password)?;
        let existing = self.request(Method::GET, "/access/users", &[]).await?;
        let user_exists = existing
            .as_array()
            .context("PBS user list")?
            .iter()
            .any(|v| v["userid"] == user);
        // Never reset an existing user's password. Its current password must
        // authenticate and its explicit ACLs must fit this one namespace.
        if user_exists {
            let mut target = self.config.clone();
            target.user = user.into();
            let mut probe = Self::new(&target)?;
            probe.login(password.as_bytes()).await.context("existing PBS user does not match the supplied/stored password; provide --pbs-password-file; refusing to reset it")?;
        }
        let acls = self.request(Method::GET, "/access/acl", &[]).await?;
        for acl in acls.as_array().context("PBS ACL list")? {
            let principal = acl["ugid"].as_str().or_else(|| acl["auth-id"].as_str());
            if principal == Some(user) {
                let role = acl["roleid"].as_str().or_else(|| acl["role"].as_str());
                ensure!(
                    acl["path"] == self.acl_path() && role == Some("DatastoreBackup"),
                    "existing PBS user has an ACL outside the household namespace or a different role; refusing to repurpose it"
                );
            }
        }
        if let Some(namespace) = &self.config.namespace {
            let mut parent = String::new();
            for part in namespace.split('/') {
                let list = self
                    .request(
                        Method::GET,
                        &self.store_path("namespace"),
                        &[("parent", parent.clone()), ("max-depth", "1".into())],
                    )
                    .await?;
                let wanted = if parent.is_empty() {
                    part.into()
                } else {
                    format!("{parent}/{part}")
                };
                if !list
                    .as_array()
                    .context("PBS namespace list")?
                    .iter()
                    .any(|v| v["ns"] == wanted)
                {
                    self.request(
                        Method::POST,
                        &self.store_path("namespace"),
                        &[("parent", parent.clone()), ("name", part.into())],
                    )
                    .await?;
                }
                parent = wanted;
            }
        }
        if !user_exists {
            self.request(
                Method::POST,
                "/access/users",
                &[
                    ("userid", user.into()),
                    ("password", password.into()),
                    ("comment", "nestegg hub".into()),
                ],
            )
            .await?;
        }
        self.acl(user).await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::task::JoinHandle;

    pub struct Fake {
        pub config: Pbs,
        pub calls: Arc<Mutex<Vec<(Method, String, String)>>>,
        pub gate: Arc<tokio::sync::Semaphore>,
        pub refuse_acl: Arc<AtomicBool>,
        pub token: Arc<AtomicBool>,
        pub with_snapshot: Arc<AtomicBool>,
        task: JoinHandle<()>,
        _dir: tempfile::TempDir,
    }
    impl Drop for Fake {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl Fake {
        pub async fn start() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cert = dir.path().join("cert.pem");
            let key = dir.path().join("key.pem");
            super::super::tls::create_certificate(&cert, &key, "https://localhost").unwrap();
            let der = rustls_pemfile::certs(&mut std::io::BufReader::new(
                std::fs::File::open(&cert).unwrap(),
            ))
            .next()
            .unwrap()
            .unwrap();
            let tls = super::super::tls::server(&cert, &key).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let refuse_acl = Arc::new(AtomicBool::new(false));
            let token = Arc::new(AtomicBool::new(false));
            let with_snapshot = Arc::new(AtomicBool::new(false));
            let show_snapshot = with_snapshot.clone();
            let (record, refuse, has_token) = (calls.clone(), refuse_acl.clone(), token.clone());
            let gate = Arc::new(tokio::sync::Semaphore::new(1));
            let request_gate = gate.clone();
            let task = tokio::spawn(async move {
                let acceptor = tokio_rustls::TlsAcceptor::from(tls);
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let acceptor = acceptor.clone();
                    let request_gate = request_gate.clone();
                    let show_snapshot = show_snapshot.clone();
                    let (record, refuse, has_token) =
                        (record.clone(), refuse.clone(), has_token.clone());
                    tokio::spawn(async move {
                        let Ok(stream) = acceptor.accept(stream).await else {
                            return;
                        };
                        let svc = hyper::service::service_fn(
                            move |req: Request<hyper::body::Incoming>| {
                                let show_snapshot = show_snapshot.clone();
                                let request_gate = request_gate.clone();
                                let (record, refuse, has_token) =
                                    (record.clone(), refuse.clone(), has_token.clone());
                                async move {
                                    let path = req.uri().path().to_string();
                                    let method = req.method().clone();
                                    let cookie = req
                                        .headers()
                                        .get("cookie")
                                        .and_then(|h| h.to_str().ok())
                                        .unwrap_or("")
                                        .to_string();
                                    let csrf = req
                                        .headers()
                                        .get("CSRFPreventionToken")
                                        .and_then(|h| h.to_str().ok())
                                        .unwrap_or("")
                                        .to_string();
                                    let body =
                                        to_bytes(Body::new(req.into_body()), 16384).await.unwrap();
                                    record.lock().unwrap().push((
                                        method.clone(),
                                        path.clone(),
                                        String::from_utf8(body.to_vec()).unwrap(),
                                    ));
                                    let _permit = request_gate.acquire().await.unwrap();
                                    let mut status = hyper::StatusCode::OK;
                                    let data = if path.ends_with("/access/ticket") {
                                        json!({"ticket":"test-ticket","CSRFPreventionToken":"test-csrf"})
                                    } else {
                                        assert_eq!(cookie, "PBSAuthCookie=test-ticket");
                                        assert_eq!(csrf, "test-csrf");
                                        if path.ends_with("/groups") {
                                            if show_snapshot.load(Ordering::SeqCst) {
                                                json!([{"backup-type":"host","backup-id":"desktop","owner":"backup@pbs!desktop"}])
                                            } else {
                                                json!([])
                                            }
                                        } else if path.ends_with("/snapshots") {
                                            json!([{"backup-time":1700000000,"verification":{"state":"failed"}}])
                                        } else if path.ends_with("/notes") {
                                            json!("nestegg test\n\n{\"state\":\"ok\"}")
                                        } else if path.ends_with("/token") {
                                            if has_token.load(Ordering::SeqCst) {
                                                json!([{"tokenid":"backup@pbs!desktop"}])
                                            } else {
                                                json!([])
                                            }
                                        } else if path.contains("/token/") && method == Method::POST
                                        {
                                            has_token.store(true, Ordering::SeqCst);
                                            json!({"tokenid":"backup@pbs!desktop","value":"test-only-token-value"})
                                        } else if path.contains("/token/")
                                            && method == Method::DELETE
                                        {
                                            has_token.store(false, Ordering::SeqCst);
                                            Value::Null
                                        } else if path.ends_with("/access/acl")
                                            && refuse.load(Ordering::SeqCst)
                                        {
                                            status = hyper::StatusCode::FORBIDDEN;
                                            Value::Null
                                        } else {
                                            Value::Null
                                        }
                                    };
                                    Ok::<_, std::convert::Infallible>(
                                        (status, axum::Json(json!({"data":data}))).into_response(),
                                    )
                                }
                            },
                        );
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), svc)
                            .await;
                    });
                }
            });
            Self {
                config: Pbs {
                    server: "127.0.0.1".into(),
                    port,
                    datastore: "store1".into(),
                    namespace: Some("household".into()),
                    user: "backup@pbs".into(),
                    fingerprint: Some(super::super::tls::hex_fingerprint(der.as_ref())),
                },
                calls,
                gate,
                refuse_acl,
                token,
                with_snapshot,
                task,
                _dir: dir,
            }
        }
    }
    #[tokio::test]
    async fn management_api_authentication_forms_and_tls_pin() {
        let fake = Fake::start().await;
        let mut client = Client::new(&fake.config).unwrap();
        client.login(b"test password & reserved").await.unwrap();
        let (id, secret) = client.create_token("desktop").await.unwrap();
        assert_eq!(id, "backup@pbs!desktop");
        assert_eq!(secret, "test-only-token-value");
        client.acl(&id).await.unwrap();
        assert!(client.groups().await.unwrap().is_empty());
        fake.with_snapshot.store(true, Ordering::SeqCst);
        let groups = client.overview().await.unwrap();
        assert_eq!(groups[0]["report"]["state"], "ok");
        assert_eq!(groups[0]["last_snapshot_verified"], false);
        assert_eq!(groups[0]["backup-count"], 1);
        client.delete_token("desktop").await.unwrap();
        {
            let calls = fake.calls.lock().unwrap();
            assert!(calls[0].2.contains("password=test+password+%26+reserved"));
            assert!(calls.iter().any(|(_, p, b)| p.ends_with("/access/acl")
                && b.contains("path=%2Fdatastore%2Fstore1%2Fhousehold")
                && b.contains("auth-id=backup%40pbs%21desktop")));
        }
        let mut wrong = fake.config.clone();
        wrong.fingerprint = Some("00".repeat(32));
        assert!(
            Client::new(&wrong)
                .unwrap()
                .login(b"not sent")
                .await
                .is_err()
        );
        assert_eq!(
            fake.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, p, _)| p.ends_with("/access/ticket"))
                .count(),
            1,
            "wrong pin must fail before sending credentials"
        );
    }
}
