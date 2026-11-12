//! Self-signed TLS material for the API listener.
//!
//! The certificate is generated at first enable and stored under
//! `<data_dir>/admin-api/tls/`; clients pin its SHA-256 fingerprint
//! (shown as a QR payload by the desktop). Later starts load the
//! stored key and certificate ([`ensure_cert`], SEC-08): a new key is
//! generated only when the material is missing, corrupt, mismatched
//! or near expiry, never because an address changed, and the SANs
//! change only through the desktop's explicit Regenerate
//! ([`regenerate`]). Address drift is reported by [`san_drift`].

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

    /// Inverse of [`Self::encode`]; `None` for anything that is not a
    /// well-formed, non-empty SAN list (a corrupt `sans.txt`).
    fn decode(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let mut sans = CertSans {
            dns: Vec::new(),
            ips: Vec::new(),
        };
        for part in text.split(',') {
            if let Some(d) = part.strip_prefix("dns:") {
                if d.is_empty() {
                    return None;
                }
                sans.dns.push(d.to_string());
            } else if let Some(ip) = part.strip_prefix("ip:") {
                sans.ips.push(ip.parse().ok()?);
            } else {
                return None;
            }
        }
        Some(sans)
    }
}

/// Certificates expiring within this window are replaced at startup.
/// rcgen issues them with its default validity (to 4096-01-01), so in
/// practice this fires only for material from elsewhere.
pub const CERT_RENEW_WINDOW_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Why the certificate the listener uses is the one it is (SEC-08).
/// Generation happens only on the explicit branches below — never
/// because the machine's addresses changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertSource {
    /// The stored key and certificate were loaded unchanged.
    Loaded,
    /// No certificate or key on disk (first enable): generated.
    GeneratedMissing,
    /// The stored key does not belong to the stored certificate.
    RegeneratedKeyMismatch,
    /// The stored certificate is expired or within
    /// [`CERT_RENEW_WINDOW_MS`] of expiry.
    RegeneratedExpiring,
    /// The certificate, key or `sans.txt` was unreadable or corrupt.
    RegeneratedCorrupt,
    /// Replaced on operator request (the desktop's Regenerate action).
    RegeneratedByOperator,
}

impl CertSource {
    /// Stable wire spelling for the Admin API status.
    pub fn as_str(self) -> &'static str {
        match self {
            CertSource::Loaded => "loaded",
            CertSource::GeneratedMissing => "generated_missing",
            CertSource::RegeneratedKeyMismatch => "regenerated_key_mismatch",
            CertSource::RegeneratedExpiring => "regenerated_expiring",
            CertSource::RegeneratedCorrupt => "regenerated_corrupt",
            CertSource::RegeneratedByOperator => "regenerated_by_operator",
        }
    }

    /// Whether this outcome replaced the key (and so the fingerprint).
    pub fn is_new_key(self) -> bool {
        self != CertSource::Loaded
    }
}

/// The material [`ensure_cert`] settled on, why, and (for a
/// regeneration) what it found wrong.
#[derive(Debug, Clone)]
pub struct CertLoad {
    pub material: TlsMaterial,
    pub source: CertSource,
    pub detail: Option<String>,
}

/// What a stored certificate covers and until when.
#[derive(Debug, Clone)]
pub struct StoredCert {
    pub sans: CertSans,
    pub not_after_ms: i64,
    pub fingerprint_sha256: String,
}

/// Parse a PEM certificate: its SANs, expiry and fingerprint.
fn parse_cert(cert_pem: &str) -> Result<StoredCert, String> {
    let der = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .next()
        .ok_or("no certificate in server.crt")?
        .map_err(|e| format!("server.crt: {e}"))?;
    let params = rcgen::CertificateParams::from_ca_cert_pem(cert_pem)
        .map_err(|e| format!("server.crt: {e}"))?;
    let mut sans = CertSans {
        dns: Vec::new(),
        ips: Vec::new(),
    };
    for san in &params.subject_alt_names {
        match san {
            rcgen::SanType::DnsName(d) => sans.dns.push(d.as_str().to_string()),
            rcgen::SanType::IpAddress(ip) => sans.ips.push(*ip),
            _ => {}
        }
    }
    let not_after_ms =
        i64::try_from(params.not_after.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX);
    Ok(StoredCert {
        sans,
        not_after_ms,
        fingerprint_sha256: hex_sha256(&der),
    })
}

/// `Ok(true)` when `key_pem` is the private key of `cert_pem`.
fn key_matches_cert(cert_pem: &str, key_pem: &str) -> Result<bool, String> {
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_pem.as_bytes())
            .collect::<Result<_, _>>()
            .map_err(|e| format!("server.crt: {e}"))?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .map_err(|e| format!("server.key: {e}"))?
        .ok_or("no private key in server.key")?;
    let signer = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| format!("server.key: {e}"))?;
    match rustls::sign::CertifiedKey::new(certs, signer).keys_match() {
        Ok(()) => Ok(true),
        Err(rustls::Error::InconsistentKeys(rustls::InconsistentKeys::KeyMismatch)) => Ok(false),
        Err(e) => Err(format!("cannot compare key and certificate: {e}")),
    }
}

/// Write a full set of material; `sans.txt` records the SANs actually
/// in the new certificate.
fn store_material(data_dir: &Path, material: &TlsMaterial, sans: &CertSans) -> io::Result<()> {
    let dir = tls_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("server.crt"), &material.cert_pem)?;
    std::fs::write(dir.join("server.key"), &material.key_pem)?;
    std::fs::write(dir.join("fingerprint.txt"), &material.fingerprint_sha256)?;
    std::fs::write(dir.join("sans.txt"), sans.encode())?;
    Ok(())
}

/// The listener's certificate at startup (SEC-08). Loads the stored
/// key and certificate and reuses them; `sans` is used only when a new
/// certificate must be generated, and that happens only on explicit,
/// logged branches: material missing, corrupt or unreadable (including
/// `sans.txt`), key/certificate mismatch, or expiry within
/// [`CERT_RENEW_WINDOW_MS`]. A machine address that moved (DHCP, a
/// Tailscale that came up late) is NOT a reason: clients pin the
/// fingerprint and do not check hostnames, so a fresh key would only
/// break every pairing. Address drift is reported by [`san_drift`]; the
/// SANs change only through [`regenerate`]. `sans.txt` is rewritten to
/// the stored certificate's own SANs when it disagrees.
pub fn ensure_cert(data_dir: &Path, sans: &CertSans) -> io::Result<CertLoad> {
    let dir = tls_dir(data_dir);
    let cert_path = dir.join("server.crt");
    let key_path = dir.join("server.key");
    let regenerate_for = |source: CertSource, detail: String| -> io::Result<CertLoad> {
        eprintln!(
            "admin api tls: generating a new certificate ({}): {detail}",
            source.as_str()
        );
        let material = generate_with_sans(sans)?;
        store_material(data_dir, &material, sans)?;
        Ok(CertLoad {
            material,
            source,
            detail: Some(detail),
        })
    };
    if !cert_path.exists() || !key_path.exists() {
        let missing = if cert_path.exists() {
            "server.key"
        } else {
            "server.crt"
        };
        return regenerate_for(
            CertSource::GeneratedMissing,
            format!("{missing} is missing"),
        );
    }
    let (cert_pem, key_pem) = match (
        std::fs::read_to_string(&cert_path),
        std::fs::read_to_string(&key_path),
    ) {
        (Ok(c), Ok(k)) => (c, k),
        (Err(e), _) | (_, Err(e)) => {
            return regenerate_for(
                CertSource::RegeneratedCorrupt,
                format!("cannot read the stored material: {e}"),
            )
        }
    };
    let stored = match parse_cert(&cert_pem) {
        Ok(s) => s,
        Err(e) => return regenerate_for(CertSource::RegeneratedCorrupt, e),
    };
    match key_matches_cert(&cert_pem, &key_pem) {
        Ok(true) => {}
        Ok(false) => {
            return regenerate_for(
                CertSource::RegeneratedKeyMismatch,
                "server.key does not belong to server.crt".to_string(),
            )
        }
        Err(e) => return regenerate_for(CertSource::RegeneratedCorrupt, e),
    }
    let sans_path = dir.join("sans.txt");
    match std::fs::read_to_string(&sans_path) {
        Ok(text) => match CertSans::decode(&text) {
            Some(recorded) if recorded.encode() == stored.sans.encode() => {}
            Some(_) => {
                eprintln!(
                    "admin api tls: sans.txt disagreed with the stored certificate; rewritten from it"
                );
                std::fs::write(&sans_path, stored.sans.encode())?;
            }
            None => {
                return regenerate_for(
                    CertSource::RegeneratedCorrupt,
                    "sans.txt is corrupt".to_string(),
                )
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!("admin api tls: sans.txt missing; written from the stored certificate");
            std::fs::write(&sans_path, stored.sans.encode())?;
        }
        Err(e) => {
            return regenerate_for(
                CertSource::RegeneratedCorrupt,
                format!("sans.txt is unreadable: {e}"),
            )
        }
    }
    let now = crate::users::now_ms();
    if stored.not_after_ms - now <= CERT_RENEW_WINDOW_MS {
        let detail = if stored.not_after_ms <= now {
            "the certificate has expired".to_string()
        } else {
            format!(
                "the certificate expires in {} day(s)",
                (stored.not_after_ms - now) / (24 * 60 * 60 * 1000)
            )
        };
        return regenerate_for(CertSource::RegeneratedExpiring, detail);
    }
    // The fingerprint file is derived data (status and pairing read
    // it); keep it in step with the certificate actually served.
    let fp_path = dir.join("fingerprint.txt");
    let recorded_fp = std::fs::read_to_string(&fp_path).unwrap_or_default();
    if recorded_fp.trim() != stored.fingerprint_sha256 {
        std::fs::write(&fp_path, &stored.fingerprint_sha256)?;
    }
    Ok(CertLoad {
        material: TlsMaterial {
            cert_pem,
            key_pem,
            fingerprint_sha256: stored.fingerprint_sha256,
        },
        source: CertSource::Loaded,
        detail: None,
    })
}

/// The stored certificate's SANs, expiry and fingerprint; `None` when
/// there is no readable certificate. Never generates.
pub fn stored_cert(data_dir: &Path) -> Option<StoredCert> {
    let pem = std::fs::read_to_string(tls_dir(data_dir).join("server.crt")).ok()?;
    parse_cert(&pem).ok()
}

/// Informational SAN drift (SEC-08): the addresses in `current` that
/// the stored certificate does not list. Empty when there is no drift
/// or no certificate. Drift never triggers regeneration — pinned
/// clients ignore SANs — and is shown beside the Regenerate action.
pub fn san_drift(data_dir: &Path, current: &CertSans) -> Vec<std::net::IpAddr> {
    let Some(stored) = stored_cert(data_dir) else {
        return Vec::new();
    };
    current
        .ips
        .iter()
        .filter(|ip| !stored.sans.ips.contains(ip))
        .copied()
        .collect()
}

/// Replace the stored certificate with a fresh one covering
/// `sans` (certificate rotation from the desktop settings). The
/// fingerprint changes, so paired clients must re-pair.
pub fn regenerate(data_dir: &Path, sans: &CertSans) -> io::Result<TlsMaterial> {
    let material = generate_with_sans(sans)?;
    store_material(data_dir, &material, sans)?;
    Ok(material)
}

fn generate_with_sans(sans: &CertSans) -> io::Result<TlsMaterial> {
    let mut params = rcgen::CertificateParams::new(sans.dns.clone()).map_err(io::Error::other)?;
    for ip in &sans.ips {
        params
            .subject_alt_names
            .push(rcgen::SanType::IpAddress(*ip));
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
/// `HANDSHAKE_TIMEOUT` (10 s), and `accept` yields whichever completes first
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
    /// Wrap a bound TCP listener; no handshake work starts until `accept`.
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

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "manager-api-tls-{tag}-{}-{}",
            std::process::id(),
            crate::users::generate_token()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sans_with(ip: &str) -> CertSans {
        let mut s = CertSans::loopback();
        s.ips.push(ip.parse().unwrap());
        s
    }

    fn read(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(tls_dir(dir).join(name)).unwrap()
    }

    /// Store a certificate (and its key) valid until `not_after_ms`.
    fn store_with_expiry(dir: &Path, sans: &CertSans, not_after_ms: i64) {
        let mut params = rcgen::CertificateParams::new(sans.dns.clone()).unwrap();
        for ip in &sans.ips {
            params
                .subject_alt_names
                .push(rcgen::SanType::IpAddress(*ip));
        }
        params.not_before = rcgen::date_time_ymd(2000, 1, 1);
        params.not_after = rcgen::date_time_ymd(1970, 1, 1)
            + std::time::Duration::from_millis(not_after_ms as u64);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let material = TlsMaterial {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
            fingerprint_sha256: hex_sha256(cert.der()),
        };
        store_material(dir, &material, sans).unwrap();
    }

    /// SEC-08: first start generates; later starts load the same
    /// material, even when the machine's addresses moved — that is
    /// reported as drift instead, and sans.txt keeps the cert's SANs.
    #[test]
    fn missing_generates_then_drift_keeps_the_key() {
        let dir = temp_dir("drift");
        let first = ensure_cert(&dir, &sans_with("192.168.1.10")).unwrap();
        assert_eq!(first.source, CertSource::GeneratedMissing);
        assert_eq!(
            read(&dir, "sans.txt"),
            sans_with("192.168.1.10").encode(),
            "sans.txt records the SANs in the certificate"
        );
        assert_eq!(
            stored_fingerprint(&dir).unwrap(),
            first.material.fingerprint_sha256
        );
        // Same addresses: loaded.
        let again = ensure_cert(&dir, &sans_with("192.168.1.10")).unwrap();
        assert_eq!(again.source, CertSource::Loaded);
        assert_eq!(
            again.material.fingerprint_sha256,
            first.material.fingerprint_sha256
        );
        // DHCP moved the LAN address: still loaded, same key.
        let moved = sans_with("192.168.1.77");
        let after = ensure_cert(&dir, &moved).unwrap();
        assert_eq!(after.source, CertSource::Loaded);
        assert_eq!(
            after.material.fingerprint_sha256,
            first.material.fingerprint_sha256
        );
        assert_eq!(after.material.key_pem, first.material.key_pem);
        assert_eq!(read(&dir, "sans.txt"), sans_with("192.168.1.10").encode());
        assert_eq!(
            san_drift(&dir, &moved),
            vec!["192.168.1.77".parse::<std::net::IpAddr>().unwrap()]
        );
        assert!(san_drift(&dir, &sans_with("192.168.1.10")).is_empty());
        // Only the explicit regenerate picks up the new address.
        let rotated = regenerate(&dir, &moved).unwrap();
        assert_ne!(
            rotated.fingerprint_sha256,
            first.material.fingerprint_sha256
        );
        assert!(san_drift(&dir, &moved).is_empty());
        assert_eq!(read(&dir, "sans.txt"), moved.encode());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_key_generates() {
        let dir = temp_dir("nokey");
        let first = ensure_cert(&dir, &CertSans::loopback()).unwrap();
        std::fs::remove_file(tls_dir(&dir).join("server.key")).unwrap();
        let next = ensure_cert(&dir, &CertSans::loopback()).unwrap();
        assert_eq!(next.source, CertSource::GeneratedMissing);
        assert_ne!(
            next.material.fingerprint_sha256,
            first.material.fingerprint_sha256
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_mismatch_regenerates() {
        let dir = temp_dir("mismatch");
        let first = ensure_cert(&dir, &CertSans::loopback()).unwrap();
        let other = rcgen::KeyPair::generate().unwrap();
        std::fs::write(tls_dir(&dir).join("server.key"), other.serialize_pem()).unwrap();
        let next = ensure_cert(&dir, &CertSans::loopback()).unwrap();
        assert_eq!(next.source, CertSource::RegeneratedKeyMismatch);
        assert_ne!(
            next.material.fingerprint_sha256,
            first.material.fingerprint_sha256
        );
        assert!(key_matches_cert(&next.material.cert_pem, &next.material.key_pem).unwrap());
        assert_eq!(
            ensure_cert(&dir, &CertSans::loopback()).unwrap().source,
            CertSource::Loaded
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_or_expiring_regenerates() {
        let now = crate::users::now_ms();
        let day = 24 * 60 * 60 * 1000;
        for (tag, not_after) in [("expiring", now + 10 * day), ("expired", now - day)] {
            let dir = temp_dir(tag);
            store_with_expiry(&dir, &CertSans::loopback(), not_after);
            let old_fp = read(&dir, "fingerprint.txt");
            let next = ensure_cert(&dir, &CertSans::loopback()).unwrap();
            assert_eq!(next.source, CertSource::RegeneratedExpiring, "{tag}");
            assert_ne!(next.material.fingerprint_sha256, old_fp, "{tag}");
            let _ = std::fs::remove_dir_all(&dir);
        }
        // Comfortably outside the window: loaded.
        let dir = temp_dir("valid");
        store_with_expiry(&dir, &CertSans::loopback(), now + 90 * day);
        assert_eq!(
            ensure_cert(&dir, &CertSans::loopback()).unwrap().source,
            CertSource::Loaded
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_material_regenerates_and_missing_sans_is_rewritten() {
        let dir = temp_dir("corrupt");
        let first = ensure_cert(&dir, &sans_with("10.0.0.5")).unwrap();
        // sans.txt missing: rewritten from the certificate, key kept.
        std::fs::remove_file(tls_dir(&dir).join("sans.txt")).unwrap();
        let next = ensure_cert(&dir, &CertSans::loopback()).unwrap();
        assert_eq!(next.source, CertSource::Loaded);
        assert_eq!(read(&dir, "sans.txt"), sans_with("10.0.0.5").encode());
        // sans.txt well-formed but wrong: rewritten, key kept.
        std::fs::write(tls_dir(&dir).join("sans.txt"), "dns:elsewhere").unwrap();
        let next = ensure_cert(&dir, &CertSans::loopback()).unwrap();
        assert_eq!(next.source, CertSource::Loaded);
        assert_eq!(
            next.material.fingerprint_sha256,
            first.material.fingerprint_sha256
        );
        assert_eq!(read(&dir, "sans.txt"), sans_with("10.0.0.5").encode());
        // sans.txt corrupt: regenerated.
        std::fs::write(tls_dir(&dir).join("sans.txt"), "garbage,ip:not-an-ip").unwrap();
        let next = ensure_cert(&dir, &CertSans::loopback()).unwrap();
        assert_eq!(next.source, CertSource::RegeneratedCorrupt);
        // Certificate corrupt: regenerated.
        std::fs::write(tls_dir(&dir).join("server.crt"), "not a certificate").unwrap();
        let next = ensure_cert(&dir, &CertSans::loopback()).unwrap();
        assert_eq!(next.source, CertSource::RegeneratedCorrupt);
        assert_eq!(
            ensure_cert(&dir, &CertSans::loopback()).unwrap().source,
            CertSource::Loaded
        );
        let _ = std::fs::remove_dir_all(&dir);
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
