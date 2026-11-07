//! Transport plumbing: TCP connect (direct or via a front SOCKS5 proxy)
//! and a minimal HTTPS GET client built on tokio-rustls.
//!
//! Two TLS modes exist on purpose:
//! * **Verified** (Mozilla roots) for `vpngate.net` and DoH endpoints.
//! * **Accept-any** for VPNGate nodes: SoftEther nodes use self-signed
//!   certificates, so verification would always fail (the Python reference
//!   probe also disables verification). This is documented in the README.

use crate::config::FrontProxy;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

/// A TCP connector that optionally routes through a front SOCKS5 proxy.
#[derive(Clone, Debug)]
pub struct Connector {
    pub front: Option<FrontProxy>,
}

impl Connector {
    #[allow(dead_code)]
    pub fn new(front: Option<FrontProxy>) -> Self {
        Self { front }
    }

    pub fn from_config(cfg: &crate::config::Config) -> Self {
        if cfg.front_proxy.enabled {
            Self {
                front: Some(cfg.front_proxy.clone()),
            }
        } else {
            Self { front: None }
        }
    }

    /// TCP connect to `host:port`, direct or via the front SOCKS5 proxy.
    pub async fn connect_tcp(&self, host: &str, port: u16) -> std::io::Result<TcpStream> {
        match &self.front {
            Some(fp) => crate::socks5::socks5_connect(&fp.addr, host, port, fp.username.as_deref(), fp.password.as_deref()).await,
            None => TcpStream::connect((host, port)).await,
        }
    }
}

/// Build a verified rustls client config (Mozilla roots).
pub fn verified_tls_config() -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(cfg)
}

/// Build an accept-any rustls client config (self-signed VPNGate nodes).
/// Only used for the SSTP TLS layer, never for vpngate.net / DoH.
pub fn accept_any_tls_config() -> Arc<ClientConfig> {
    let cfg = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyVerifier))
        .with_no_client_auth();
    Arc::new(cfg)
}

#[derive(Debug)]
struct AcceptAnyVerifier;

impl tokio_rustls::rustls::client::danger::ServerCertVerifier for AcceptAnyVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
        _server_name: &tokio_rustls::rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error> {
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<tokio_rustls::rustls::client::danger::HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<tokio_rustls::rustls::client::danger::HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        vec![
            tokio_rustls::rustls::SignatureScheme::RSA_PKCS1_SHA256,
            tokio_rustls::rustls::SignatureScheme::RSA_PKCS1_SHA384,
            tokio_rustls::rustls::SignatureScheme::RSA_PKCS1_SHA512,
            tokio_rustls::rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            tokio_rustls::rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            tokio_rustls::rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
            tokio_rustls::rustls::SignatureScheme::RSA_PSS_SHA256,
            tokio_rustls::rustls::SignatureScheme::RSA_PSS_SHA384,
            tokio_rustls::rustls::SignatureScheme::RSA_PSS_SHA512,
            tokio_rustls::rustls::SignatureScheme::ED25519,
        ]
    }
}

/// Minimal HTTPS GET. `url` must be https://host[:port]/path.
/// Returns (status_code, body). Follows no redirects.
pub async fn https_get(
    connector: &Connector,
    url: &str,
    extra_headers: &[(&str, &str)],
    verify: bool,
) -> Result<(u16, Vec<u8>), String> {
    let rest = url.strip_prefix("https://").ok_or("https_get: only https:// URLs supported")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rfind(':') {
        Some(i) if authority[i + 1..].chars().all(|c| c.is_ascii_digit()) => {
            (&authority[..i], authority[i + 1..].parse::<u16>().map_err(|_| "bad port in URL")?)
        }
        _ => (authority, 443),
    };
    let tcp = connector
        .connect_tcp(host, port)
        .await
        .map_err(|e| format!("TCP connect to {}:{} failed: {}", host, port, e))?;
    let tls_cfg = if verify {
        verified_tls_config()
    } else {
        accept_any_tls_config()
    };
    let server_name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("bad server name {}: {}", host, e))?;
    let mut tls = TlsConnector::from(tls_cfg)
        .connect(server_name, tcp)
        .await
        .map_err(|e| format!("TLS handshake to {} failed: {}", host, e))?;
    let mut req = format!("GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: vpngate-client/0.1\r\n", path, authority);
    for (k, v) in extra_headers {
        req.push_str(&format!("{}: {}\r\n", k, v));
    }
    req.push_str("\r\n");
    tls.write_all(req.as_bytes())
        .await
        .map_err(|e| format!("HTTPS write failed: {}", e))?;
    let mut raw = Vec::new();
    tls.read_to_end(&mut raw)
        .await
        .map_err(|e| format!("HTTPS read failed: {}", e))?;
    // Split headers / body.
    let split = find_subslice(&raw, b"\r\n\r\n").ok_or("HTTPS response has no header terminator")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("cannot parse HTTP status line")?;
    let mut body = raw[split + 4..].to_vec();
    // Handle chunked transfer encoding (vpngate.net / DoH may use it).
    if head.to_lowercase().contains("transfer-encoding: chunked") {
        body = dechunk(&body)?;
    }
    Ok((status, body))
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn dechunk(body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let end = find_subslice(&body[pos..], b"\r\n").ok_or("bad chunked body")? + pos;
        let size_str = String::from_utf8_lossy(&body[pos..end]).trim().to_string();
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or(""), 16)
            .map_err(|_| "bad chunk size")?;
        if size == 0 {
            break;
        }
        let start = end + 2;
        if start + size > body.len() {
            return Err("chunk overruns body".to_string());
        }
        out.extend_from_slice(&body[start..start + size]);
        pos = start + size + 2;
    }
    Ok(out)
}
