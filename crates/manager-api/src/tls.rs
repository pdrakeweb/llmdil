//! Self-signed TLS material for the API listener.
//!
//! The certificate is generated at first enable and stored under
//! `<data_dir>/admin-api/tls/`; clients pin its SHA-256 fingerprint
//! (shown as a QR payload by the desktop, phase 2). SANs at this
//! phase cover loopback development: `localhost`, 127.0.0.1, ::1.
//! The phase-2 enablement flow regenerates with the machine's
//! hostname and LAN/Tailscale addresses, and moves the private key
//! into Windows Credential Manager; the files here are the phase-1
//! development form.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::Digest;

/// Certificate, key, and the pinning fingerprint (hex SHA-256 of the
/// certificate DER).
#[derive(Debug, Clone)]
pub struct TlsMaterial {
    pub cert_pem: String,
    pub key_pem: String,
    pub fingerprint_sha256: String,
}

fn tls_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("admin-api").join("tls")
}

/// The names a certificate should cover: DNS names (hostname,
/// `localhost`) and IP addresses (loopback, the LAN address, the
/// Tailscale address when present). Clients pin the fingerprint,
/// so SAN changes force a re-pair — regeneration is an explicit
/// enable-time decision, not a silent drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertSans {
    pub dns: Vec<String>,
    pub ips: Vec<std::net::IpAddr>,
}

impl CertSans {
    /// Loopback development set (phase 1).
    pub fn loopback() -> Self {
        Self {
            dns: vec!["localhost".to_string()],
            ips: vec![
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            ],
        }
    }

    fn encode(&self) -> String {
        let mut parts: Vec<String> = self.dns.iter().map(|d| format!("dns:{d}")).collect();
        parts.extend(self.ips.iter().map(|i| format!("ip:{i}")));
        parts.sort();
        parts.join(",")
    }
}

/// Load the stored certificate if it covers `sans`, otherwise
/// generate a fresh one covering them (fingerprint changes, so
/// paired clients must re-pair — the intended consequence).
pub fn ensure_cert(data_dir: &Path, sans: &CertSans) -> io::Result<TlsMaterial> {
    let dir = tls_dir(data_dir);
    let sans_path = dir.join("sans.txt");
    if sans_path.exists()
        && std::fs::read_to_string(&sans_path)
            .map(|t| t.trim() == sans.encode())
            .unwrap_or(false)
    {
        return load_or_generate(data_dir);
    }
    let material = generate_with_sans(sans)?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("server.crt"), &material.cert_pem)?;
    std::fs::write(dir.join("server.key"), &material.key_pem)?;
    std::fs::write(dir.join("fingerprint.txt"), &material.fingerprint_sha256)?;
    std::fs::write(&sans_path, sans.encode())?;
    Ok(material)
}

/// Replace the stored certificate with a fresh one covering
/// `sans` (certificate rotation from the desktop settings). The
/// fingerprint changes, so paired clients must re-pair.
pub fn regenerate(data_dir: &Path, sans: &CertSans) -> io::Result<TlsMaterial> {
    let dir = tls_dir(data_dir);
    let material = generate_with_sans(sans)?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("server.crt"), &material.cert_pem)?;
    std::fs::write(dir.join("server.key"), &material.key_pem)?;
    std::fs::write(dir.join("fingerprint.txt"), &material.fingerprint_sha256)?;
    std::fs::write(dir.join("sans.txt"), sans.encode())?;
    Ok(material)
}

fn generate_with_sans(sans: &CertSans) -> io::Result<TlsMaterial> {
    let mut params =
        rcgen::CertificateParams::new(sans.dns.clone()).map_err(io::Error::other)?;
    for ip in &sans.ips {
        params.subject_alt_names.push(rcgen::SanType::IpAddress(*ip));
    }
    params.distinguished_name = {
        let mut dn = rcgen::DistinguishedName::new();
        dn.push(rcgen::DnType::CommonName, "LLMdil Admin API");
        dn
    };
    let key_pair = rcgen::KeyPair::generate().map_err(io::Error::other)?;
    let cert = params.self_signed(&key_pair).map_err(io::Error::other)?;
    Ok(TlsMaterial {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
        fingerprint_sha256: hex_sha256(cert.der()),
    })
}

/// The stored certificate's fingerprint, if a certificate exists.
/// Never generates (status display must not create state).
pub fn stored_fingerprint(data_dir: &Path) -> Option<String> {
    let path = tls_dir(data_dir).join("fingerprint.txt");
    std::fs::read_to_string(path)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Load the stored certificate, generating a fresh self-signed one
/// on first use.
pub fn load_or_generate(data_dir: &Path) -> io::Result<TlsMaterial> {
    let dir = tls_dir(data_dir);
    let cert_path = dir.join("server.crt");
    let key_path = dir.join("server.key");
    let fp_path = dir.join("fingerprint.txt");
    if cert_path.exists() && key_path.exists() && fp_path.exists() {
        return Ok(TlsMaterial {
            cert_pem: std::fs::read_to_string(&cert_path)?,
            key_pem: std::fs::read_to_string(&key_path)?,
            fingerprint_sha256: std::fs::read_to_string(&fp_path)?.trim().to_string(),
        });
    }
    let material = generate()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&cert_path, &material.cert_pem)?;
    std::fs::write(&key_path, &material.key_pem)?;
    std::fs::write(&fp_path, &material.fingerprint_sha256)?;
    Ok(material)
}

fn generate() -> io::Result<TlsMaterial> {
    generate_with_sans(&CertSans::loopback())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = sha2::Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the rustls server config for the listener.
pub fn server_config(material: &TlsMaterial) -> io::Result<Arc<rustls::ServerConfig>> {
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut material.cert_pem.as_bytes())
            .collect::<Result<_, _>>()
            .map_err(io::Error::other)?;
    let key = rustls_pemfile::private_key(&mut material.key_pem.as_bytes())
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("no private key in TLS material"))?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(io::Error::other)?;
    Ok(Arc::new(config))
}

/// Longest a client may take to complete the TLS handshake.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Most handshakes in flight at once; beyond this the listener stops
/// accepting until one finishes or times out.
const MAX_PENDING_HANDSHAKES: usize = 64;

type Handshake = Option<(
    tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    std::net::SocketAddr,
)>;

/// An axum listener that terminates TLS per connection.
///
/// Handshakes run as concurrent tasks, each bounded by
/// [`HANDSHAKE_TIMEOUT`], and `accept` yields whichever completes first
/// (finding SEC-07, `docs/code-review-v0.1.60.md`). axum calls
/// `accept` one connection at a time, so a handshake awaited inline
/// let a single client that opened TCP and never sent a ClientHello
/// block every other connection indefinitely — an unauthenticated DoS
/// from the LAN or tailnet.
pub struct TlsListener {
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    handshakes: tokio::task::JoinSet<Handshake>,
}

impl TlsListener {
    pub fn new(listener: tokio::net::TcpListener, config: Arc<rustls::ServerConfig>) -> Self {
        Self {
            listener,
            acceptor: tokio_rustls::TlsAcceptor::from(config),
            handshakes: tokio::task::JoinSet::new(),
        }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let can_accept = self.handshakes.len() < MAX_PENDING_HANDSHAKES;
            tokio::select! {
                Some(done) = self.handshakes.join_next(), if !self.handshakes.is_empty() => {
                    // A failed or timed-out handshake (port scan,
                    // plaintext probe, idle socket) drops that
                    // connection only, never the listener.
                    if let Ok(Some(conn)) = done {
                        return conn;
                    }
                }
                res = self.listener.accept(), if can_accept => match res {
                    Ok((stream, addr)) => {
                        let acceptor = self.acceptor.clone();
                        self.handshakes.spawn(async move {
                            match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream))
                                .await
                            {
                                Ok(Ok(tls)) => Some((tls, addr)),
                                _ => None,
                            }
                        });
                    }
                    Err(_) => {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                },
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SEC-07: an idle TCP connection that never sends a ClientHello
    /// must not stop the listener from completing other handshakes.
    #[tokio::test]
    async fn idle_connection_does_not_block_other_handshakes() {
        let dir = std::env::temp_dir().join(format!(
            "manager-api-tls-idle-{}-{}",
            std::process::id(),
            crate::users::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let m = load_or_generate(&dir).unwrap();
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let listener = TlsListener::new(tcp, server_config(&m).unwrap());
        let app = axum::Router::new().route("/", axum::routing::get(|| async { "ok" }));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        // The idle client: connects, sends nothing, stays open.
        let _idle = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        let got = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.get(format!("https://{addr}/")).send(),
        )
        .await;
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
        let resp = got
            .expect("TLS request blocked behind an idle connection")
            .unwrap();
        assert_eq!(resp.text().await.unwrap(), "ok");
    }

    #[test]
    fn generated_material_is_self_consistent() {
        let dir = std::env::temp_dir().join(format!("manager-api-tls-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let m = load_or_generate(&dir).unwrap();
        assert!(m.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(m.key_pem.contains("PRIVATE KEY"));
        assert_eq!(m.fingerprint_sha256.len(), 64);
        // Second load returns the stored material.
        let m2 = load_or_generate(&dir).unwrap();
        assert_eq!(m.fingerprint_sha256, m2.fingerprint_sha256);
        assert!(server_config(&m).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
