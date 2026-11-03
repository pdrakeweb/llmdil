//! Certificate-fingerprint pinning for the Admin API's self-signed
//! certificate.
//!
//! Trust model: the pinned fingerprint IS the server identity. The
//! client compares the hex SHA-256 of the presented end-entity
//! certificate's DER bytes against the fingerprint the LLMdil UI
//! shows in its "Certificate SHA-256" row. A mismatch is a hard stop:
//! the connection aborts, never a warning the user clicks through.
//! Hostname verification is intentionally not performed — the API is
//! reached by LAN IP / Tailscale address with a self-signed cert,
//! and the fingerprint replaces hostname trust.
//!
//! Fingerprint matching alone is not authentication: a presented
//! certificate is public data. The verifier therefore also checks
//! the TLS CertificateVerify signature against the presented
//! certificate's public key (via rustls-webpki), proving the server
//! holds the private key for the pinned certificate. Skipping that
//! check would let anyone presenting the (public) certificate
//! impersonate the server.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, DigitallySignedStruct, Error as TlsError, SignatureScheme,
};
use webpki::EndEntityCert;
use sha2::{Digest, Sha256};

/// Parse a fingerprint as shown by the LLMdil UI: exactly 64 hex
/// characters (case-insensitive, surrounding whitespace ignored).
pub fn parse_fingerprint(text: &str) -> Result<[u8; 32], String> {
    let text = text.trim();
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "fingerprint must be exactly 64 hex characters, got {} characters",
            text.len()
        ));
    }
    let mut out = [0u8; 32];
    for (i, chunk) in text.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16).ok_or("bad hex")? as u8;
        let lo = (chunk[1] as char).to_digit(16).ok_or("bad hex")? as u8;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

/// Hex SHA-256 of DER bytes — the same computation the server uses
/// for its `fingerprint_sha256` (hex SHA-256 of the certificate DER).
pub fn fingerprint_of_der(der: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(der);
    hasher.finalize().into()
}

/// rustls server-certificate verifier that trusts exactly one
/// certificate: the one whose DER SHA-256 matches `expected`.
#[derive(Debug)]
pub struct FingerprintVerifier {
    expected: [u8; 32],
}

impl FingerprintVerifier {
    pub fn new(expected: [u8; 32]) -> Arc<Self> {
        Arc::new(Self { expected })
    }

    fn verify_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        // Look the scheme up in the crypto provider's own mapping
        // (one scheme -> the algorithms valid for it), instead of
        // hand-maintaining a table. An unmapped scheme fails closed.
        // Trying every mapped algorithm is sound: a signature valid
        // under any of them proves the server holds the private key
        // for the pinned certificate (TLS 1.2 tries them in
        // sequence; TLS 1.3 tries only the first — accepting a
        // valid one under either rule is never a forgery).
        let provider = CryptoProvider::get_default().ok_or(
            TlsError::InvalidCertificate(CertificateError::BadSignature),
        )?;
        let algs = provider
            .signature_verification_algorithms
            .mapping
            .iter()
            .find(|(scheme, _)| *scheme == dss.scheme)
            .map(|(_, algs)| *algs)
            .ok_or(TlsError::InvalidCertificate(
                CertificateError::BadSignature,
            ))?;
        let end_entity = EndEntityCert::try_from(cert).map_err(|_| {
            TlsError::InvalidCertificate(CertificateError::BadEncoding)
        })?;
        for alg in algs {
            if end_entity
                .verify_signature(*alg, message, dss.signature())
                .is_ok()
            {
                return Ok(HandshakeSignatureValid::assertion());
            }
        }
        Err(TlsError::InvalidCertificate(
            CertificateError::BadSignature,
        ))
    }
}

impl rustls::client::danger::ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        if fingerprint_of_der(end_entity.as_ref()) == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::InvalidCertificate(
                CertificateError::UnknownIssuer,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.verify_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.verify_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        // The provider's own scheme list: exactly the schemes
        // verify_signature can check. No provider means no schemes
        // (fail closed — no handshake can proceed).
        CryptoProvider::get_default()
            .map(|p| {
                p.signature_verification_algorithms
                    .supported_schemes()
            })
            .unwrap_or_default()
    }
}

/// Build a rustls client config that pins to `expected` and trusts
/// nothing else. The default crypto provider (ring, the only one
/// enabled in this workspace) supplies the TLS stack.
pub fn pinned_client_config(expected: [u8; 32]) -> rustls::ClientConfig {
    let provider = CryptoProvider::get_default().expect("crypto provider");
    rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .expect("protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(FingerprintVerifier::new(expected))
        .with_no_client_auth()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier;

    fn test_cert_der() -> Vec<u8> {
        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .expect("params");
        params.is_ca = rcgen::IsCa::NoCa;
        let key = rcgen::KeyPair::generate().expect("keypair");
        let cert = params.self_signed(&key).expect("self-signed cert");
        cert.der().to_vec()
    }

    #[test]
    fn fingerprint_parses_and_rejects() {
        let good = "ab".repeat(32);
        assert_eq!(parse_fingerprint(&good).unwrap(), [0xab; 32]);
        assert_eq!(
            parse_fingerprint(&"AB".repeat(32)).unwrap(),
            [0xab; 32]
        );
        assert!(parse_fingerprint("  ab  ").is_err());
        assert!(parse_fingerprint(&"ab".repeat(31)).is_err());
        assert!(parse_fingerprint(&"zz".repeat(32)).is_err());
        assert!(parse_fingerprint("").is_err());
    }

    #[test]
    fn verifier_accepts_matching_cert_and_rejects_other() {
        let der = test_cert_der();
        let fp = fingerprint_of_der(&der);
        assert_eq!(fp.len(), 32);
        let verifier = FingerprintVerifier::new(fp);
        let name = ServerName::try_from("localhost").expect("server name");
        let cert = CertificateDer::from(der.clone());
        assert!(verifier
            .verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
            .is_ok());
        let wrong = FingerprintVerifier::new([0u8; 32]);
        assert!(wrong
            .verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
            .is_err());
    }

    #[test]
    fn supported_schemes_come_from_provider() {
        // The verifier offers exactly the provider's schemes — no
        // hand-maintained table to drift.
        let schemes = FingerprintVerifier::new([0u8; 32]).supported_verify_schemes();
        assert!(schemes.contains(&SignatureScheme::ECDSA_NISTP256_SHA256));
        assert!(schemes.contains(&SignatureScheme::ED25519));
    }

    /// Run one real TLS handshake: a tokio-rustls server presenting
    /// a fresh self-signed cert, a client pinned to `fp`. Returns
    /// whether the handshake succeeded. This exercises the whole
    /// verifier, including the CertificateVerify signature check —
    /// not just the fingerprint comparison.
    async fn handshake_once(cert_der: &[u8], key_der: &[u8], fp: [u8; 32]) -> bool {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
        use std::sync::Arc;

        let server_cert = CertificateDer::from(cert_der.to_vec());
        let server_key = PrivateKeyDer::try_from(key_der.to_vec())
            .expect("private key der");
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![server_cert], server_key)
            .expect("server config");
        let acceptor =
            tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let addr = listener.local_addr().expect("local addr");
        let accept_task = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let _ = acceptor.accept(stream).await;
            }
        });
        let client_config = pinned_client_config(fp);
        let connector =
            tokio_rustls::TlsConnector::from(Arc::new(client_config));
        let stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect");
        let name = ServerName::try_from("localhost").expect("server name");
        let ok = connector.connect(name, stream).await.is_ok();
        let _ = accept_task.await;
        ok
    }

    #[tokio::test]
    async fn pinned_handshake_accepts_match_and_rejects_mismatch() {
        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .expect("params");
        params.is_ca = rcgen::IsCa::NoCa;
        let key = rcgen::KeyPair::generate().expect("keypair");
        let cert = params.self_signed(&key).expect("self-signed cert");
        let cert_der = cert.der().to_vec();
        let key_der = key.serialized_der();
        let fp = fingerprint_of_der(&cert_der);
        assert!(
            handshake_once(&cert_der, key_der, fp).await,
            "matching fingerprint must complete the handshake"
        );
        assert!(
            !handshake_once(&cert_der, key_der, [0u8; 32]).await,
            "mismatched fingerprint must abort the handshake"
        );
    }
}
