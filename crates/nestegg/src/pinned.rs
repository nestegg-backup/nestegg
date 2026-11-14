//! Pin-only HTTPS for the computer's hub connection. No CA or hostname trust.
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use x509_cert::der::{Decode, Encode};

#[derive(Debug, thiserror::Error)]
#[error("hub request was not sent")]
pub struct NotSent;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(240);

pub fn pin_der(spki: &[u8]) -> String {
    format!("sha256//{}", STANDARD.encode(Sha256::digest(spki)))
}

pub fn certificate_pin(certificate: &[u8]) -> Result<String> {
    let cert = x509_cert::Certificate::from_der(certificate).context("invalid hub certificate")?;
    Ok(pin_der(
        &cert.tbs_certificate.subject_public_key_info.to_der()?,
    ))
}

pub fn validate_pin(pin: &str) -> Result<()> {
    let raw = pin
        .strip_prefix("sha256//")
        .context("the hub pin must start with sha256//; copy the complete command from the hub")?;
    let bytes = STANDARD
        .decode(raw)
        .context("invalid hub pin; copy the complete command from the hub")?;
    if bytes.len() != 32 || STANDARD.encode(&bytes) != raw {
        bail!("invalid hub pin; copy the complete command from the hub");
    }
    Ok(())
}

pub fn compare_pin(expected: &str, actual: &str) -> Result<()> {
    validate_pin(expected)?;
    if expected != actual {
        bail!(
            "the hub's public key does not match: expected {expected}, received {actual}. Stop and check the address and pin with the hub administrator; do not accept a new pin without checking it"
        );
    }
    Ok(())
}

#[derive(Debug)]
struct PinVerifier(String);
impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        certificate_pin(cert.as_ref())
            .and_then(|actual| compare_pin(&self.0, &actual))
            .map_err(|e| rustls::Error::General(e.to_string()))?;
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            signature,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            signature,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub origin: String,
    host: String,
    port: u16,
    authority: String,
}
impl Endpoint {
    pub fn parse(url: &str) -> Result<Self> {
        let uri: hyper::Uri = url
            .parse()
            .context("invalid hub address; copy its HTTPS address from the hub")?;
        let authority = uri
            .authority()
            .context("the hub address needs a host name")?;
        if uri.scheme_str() != Some("https")
            || authority.as_str().contains('@')
            || uri
                .path_and_query()
                .is_some_and(|p| !matches!(p.as_str(), "" | "/"))
            || url.contains('#')
        {
            bail!(
                "use the hub's HTTPS address, without a user name, path, query or fragment (for example https://hub.example.com:8443)"
            );
        }
        if authority.as_str().ends_with(':')
            || authority
                .port()
                .is_some_and(|p| p.as_str().parse::<u16>().is_err())
        {
            bail!("the hub port must be between 1 and 65535; copy the address from the hub");
        }
        if authority.port_u16() == Some(0) {
            bail!("the hub port must be between 1 and 65535");
        }
        Ok(Self {
            origin: format!("https://{authority}"),
            host: authority.host().trim_matches(['[', ']']).into(),
            port: authority.port_u16().unwrap_or(443),
            authority: authority.to_string(),
        })
    }

    /// Bounded request/response. Redirects are never followed and response
    /// text is never included in errors: it could contain credentials.
    pub async fn post(
        &self,
        pin: &str,
        route: &str,
        credential: Option<&[u8]>,
        body: Vec<u8>,
    ) -> Result<(u16, Vec<u8>)> {
        validate_pin(pin)?;
        let sent = std::sync::atomic::AtomicBool::new(false);
        let result = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.post_inner(pin, route, credential, body, &sent),
        )
        .await
        .context("the hub took too long to answer; check that it is running and try again")
        .and_then(|r| r);
        result.map_err(|e| {
            if sent.load(std::sync::atomic::Ordering::Relaxed) {
                e
            } else {
                e.context(NotSent)
            }
        })
    }
    async fn post_inner(
        &self,
        pin: &str,
        route: &str,
        credential: Option<&[u8]>,
        body: Vec<u8>,
        sent: &std::sync::atomic::AtomicBool,
    ) -> Result<(u16, Vec<u8>)> {
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinVerifier(pin.into())))
        .with_no_client_auth();
        let socket = tokio::net::TcpStream::connect((self.host.as_str(), self.port)).await
            .context("cannot reach the hub; check the address and network, and that the hub is switched on")?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from(self.host.clone())?, socket)
            .await
            .context("cannot establish a secure connection to the hub")?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tls)).await?;
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        // Abort the driver on every exit, including cancellation/timeout.
        struct Driver(tokio::task::JoinHandle<()>);
        impl Drop for Driver {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _driver = Driver(driver);
        let mut request = hyper::Request::post(format!("/api/hub/v1{route}"))
            .header("host", &self.authority)
            .header("content-type", "application/json");
        if let Some(credential) = credential {
            let mut value = b"Bearer ".to_vec();
            value.extend_from_slice(credential);
            let mut header = hyper::header::HeaderValue::from_bytes(&value)
                .context("invalid stored machine credential; leave and join again")?;
            header.set_sensitive(true);
            request = request.header("authorization", header);
        }
        sent.store(true, std::sync::atomic::Ordering::Relaxed);
        let response = sender
            .send_request(request.body(Full::new(Bytes::from(body)))?)
            .await
            .context("the hub connection was interrupted; check the hub and try again")?;
        let status = response.status().as_u16();
        let body = Limited::new(response.into_body(), 1024 * 1024)
            .collect()
            .await
            .map_err(|_| anyhow::anyhow!("the hub response was interrupted or too large"))?
            .to_bytes()
            .to_vec();
        Ok((status, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::PublicKeyData;
    #[test]
    fn pins_are_spki_sha256_and_comparison_names_both() {
        assert_eq!(
            pin_der(b"abc"),
            "sha256//ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0="
        );
        let a = pin_der(b"a");
        let b = pin_der(b"b");
        assert!(compare_pin(&a, &a).is_ok());
        let error = compare_pin(&a, &b).unwrap_err().to_string();
        assert!(error.contains(&a) && error.contains(&b));
        for invalid in [
            "",
            "sha256//abc",
            "sha256//AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert!(validate_pin(invalid).is_err());
        }
        let cert =
            rcgen::generate_simple_self_signed(vec!["unrelated.example.com".into()]).unwrap();
        assert_eq!(
            certificate_pin(cert.cert.der()).unwrap(),
            pin_der(&cert.signing_key.subject_public_key_info())
        );
    }
    #[test]
    fn endpoints_never_redirect_secrets() {
        assert_eq!(
            Endpoint::parse("https://hub.example.com:8443/")
                .unwrap()
                .origin,
            "https://hub.example.com:8443"
        );
        for invalid in [
            "http://hub.example.com",
            "https://alice@hub.example.com",
            "https://hub.example.com/path",
            "https://hub.example.com/?x",
            "https://hub.example.com/#x",
        ] {
            assert!(Endpoint::parse(invalid).is_err(), "{invalid}");
        }
    }
}
