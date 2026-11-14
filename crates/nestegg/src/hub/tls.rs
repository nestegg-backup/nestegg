use std::io::{BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn parse_fingerprint(s: &str) -> Result<Vec<u8>> {
    let s = s.replace(':', "");
    ensure!(
        s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()),
        "certificate fingerprint must be SHA-256 hex"
    );
    (0..64)
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).context("invalid fingerprint"))
        .collect()
}

pub fn hex_fingerprint(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[derive(Debug)]
struct Pinned {
    fingerprint: Vec<u8>,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}
impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        if bool::from(
            Sha256::digest(end.as_ref())
                .as_slice()
                .ct_eq(&self.fingerprint),
        ) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "PBS certificate fingerprint mismatch".into(),
            ))
        }
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

pub fn client(fingerprint: Option<&str>) -> Result<Arc<rustls::ClientConfig>> {
    let provider = provider();
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?;
    let c = if let Some(fp) = fingerprint {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pinned {
                fingerprint: parse_fingerprint(fp)?,
                algorithms: provider.signature_verification_algorithms,
            }))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        let certs = rustls_native_certs::load_native_certs();
        for cert in certs.certs {
            roots.add(cert)?;
        }
        ensure!(!roots.is_empty(), "no system certificate roots available");
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    Ok(Arc::new(c))
}

pub fn server(cert: &Path, key: &Path) -> Result<Arc<rustls::ServerConfig>> {
    let certs = rustls_pemfile::certs(&mut BufReader::new(std::fs::File::open(cert)?))
        .collect::<std::io::Result<Vec<_>>>()?;
    let secret = if let Ok(dir) = std::env::var("CREDENTIALS_DIRECTORY") {
        std::fs::read(Path::new(&dir).join("hub-tls-key"))?
    } else {
        std::fs::read(key)?
    };
    let secret = crate::secrets::Secret::new(secret);
    let key = rustls_pemfile::private_key(&mut BufReader::new(secret.as_bytes()))?
        .context("no TLS private key")?;
    Ok(Arc::new(
        rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(certs, key)?,
    ))
}

fn openssl(args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new("openssl")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("run openssl")?;
    child
        .stdin
        .take()
        .context("openssl stdin")?
        .write_all(input)?;
    let out = child.wait_with_output()?;
    ensure!(out.status.success(), "openssl public key conversion failed");
    Ok(out.stdout)
}

pub fn public_der(pem: &[u8]) -> Result<Vec<u8>> {
    openssl(&["pkey", "-pubin", "-outform", "DER"], pem)
}
pub fn pin(certificate: &Path) -> Result<String> {
    let public = openssl(&["x509", "-pubkey", "-noout"], &std::fs::read(certificate)?)?;
    Ok(format!(
        "sha256//{}",
        STANDARD.encode(Sha256::digest(public_der(&public)?))
    ))
}

/// Fetching a certificate never authenticates a server or sends credentials.
/// The caller must show and explicitly confirm the resulting fingerprint.
pub fn discover(server: &str, port: u16) -> Result<String> {
    let mut child = Command::new("timeout")
        .args(["10", "openssl", "s_client", "-connect"])
        .arg(format!("{server}:{port}"))
        .args(["-servername", server, "-showcerts"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut out = Vec::new();
    use std::io::Read;
    child
        .stdout
        .take()
        .context("TLS discovery stdout")?
        .take(1024 * 1024)
        .read_to_end(&mut out)?;
    let _ = child.wait();
    let cert = rustls_pemfile::certs(&mut BufReader::new(out.as_slice()))
        .next()
        .context("PBS sent no certificate")??;
    Ok(hex_fingerprint(cert.as_ref()))
}

pub fn create_certificate(certificate: &Path, key: &Path, url: &str) -> Result<()> {
    let host = url::Url::parse(url)?
        .host_str()
        .context("URL host")?
        .to_string();
    let san = if host.parse::<std::net::IpAddr>().is_ok() {
        format!("IP:{host}")
    } else {
        format!("DNS:{host}")
    };
    let status = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:3072",
            "-nodes",
            "-days",
            "3650",
            "-subj",
            "/CN=nestegg hub",
            "-addext",
        ])
        .arg(format!("subjectAltName={san}"))
        .arg("-keyout")
        .arg(key)
        .arg("-out")
        .arg(certificate)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    ensure!(status.success(), "could not create hub TLS certificate");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(key, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
