//! Phase-1 surface tests: the registry↔router contract, the read
//! endpoints over a default config, the error envelope, redaction
//! structure, and live-socket smoke tests (plaintext loopback and
//! TLS with the generated self-signed certificate).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use manager_api::auth::MemoryCredentialStore;
use manager_api::users::{RoleName, UserStore};
use manager_api::{build_router, ApiCtx, ApiOptions, ApiServer, AUTH_ROUTES, DERIVED_ROUTES};
use manager_app::registry::OP_SPECS;
use manager_app::AppState;
use tower::ServiceExt;

async fn admin_token(ctx: &Arc<ApiCtx>) -> String {
    // Seed an admin directly and mint a session (password login has
    // its own flow tests; argon2 at plan parameters is seconds per
    // call and would dominate this suite).
    static TEST_HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let hash = TEST_HASH
        .get_or_init(|| manager_api::users::hash_password("a-unique-passphrase").unwrap())
        .clone();
    let user_id = format!("u-{}", manager_api::users::generate_token());
    let user = manager_api::users::User {
        id: user_id.clone(),
        username: "root".into(),
        password_hash: hash,
        role: RoleName::Admin,
        disabled: false,
        must_change_password: false,
        totp_enabled: false,
        recovery_code_hashes: vec![],
        created_ms: manager_api::users::now_ms(),
        updated_ms: manager_api::users::now_ms(),
    };
    ctx.users
        .mutate::<_, manager_api::users::StoreError>(|f| {
            f.users.push(user);
            Ok(())
        })
        .await
        .unwrap();
    ctx.auth
        .create_session(&user_id, "test")
        .await
        .unwrap()
        .access_token
}

fn get_auth(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn test_ctx(tag: &str) -> (Arc<ApiCtx>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("manager-api-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = manager_config::AppConfig::default_config();
    cfg.data_dir = dir.clone();
    let app = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
    (
        ApiCtx::with_services(
            app,
            dir.clone(),
            UserStore::in_memory(),
            Arc::new(MemoryCredentialStore::default()),
        ),
        dir,
    )
}

async fn body_json(resp: axum::response::Response) -> (StatusCode, serde_json::Value) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

// ---------------------------------------------------------------------------
// Contract: the live route table is exactly the registry's routes
// plus the enumerated derived routes — nothing more, nothing less.
// ---------------------------------------------------------------------------

#[test]
fn router_matches_registry() {
    let (ctx, dir) = test_ctx("contract");
    let (_router, records) = build_router(ctx);
    let method_of = |m: manager_app::registry::HttpMethod| match m {
        manager_app::registry::HttpMethod::Get => "GET",
        manager_app::registry::HttpMethod::Post => "POST",
        manager_app::registry::HttpMethod::Put => "PUT",
        manager_app::registry::HttpMethod::Patch => "PATCH",
        manager_app::registry::HttpMethod::Delete => "DELETE",
    };
    let mut expected: Vec<(String, String)> = OP_SPECS
        .iter()
        .filter_map(|s| s.route)
        .map(|r| (method_of(r.method).to_string(), r.path.to_string()))
        .collect();
    for (m, p) in DERIVED_ROUTES {
        expected.push((m.to_string(), p.to_string()));
    }
    for (m, p, _c) in AUTH_ROUTES {
        expected.push((m.to_string(), p.to_string()));
    }
    let mut actual: Vec<(String, String)> = records
        .iter()
        .map(|r| (r.method.to_string(), r.path.to_string()))
        .collect();
    expected.sort();
    actual.sort();
    assert_eq!(actual, expected);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Read surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn identity_and_status() {
    let (ctx, dir) = test_ctx("identity");
    let (router, _) = build_router(ctx.clone());
    let (status, v) = body_json(router.clone().oneshot(get("/api/v1/server")).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["api_version"], "1");
    assert!(v["deployment_id"].as_str().is_some_and(|s| !s.is_empty()));

    // /status without a credential is refused by the guard.
    let (status, _) = body_json(router.clone().oneshot(get("/api/v1/status")).await.unwrap()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let token = admin_token(&ctx).await;
    let (status, v) = body_json(router.oneshot(get_auth("/api/v1/status", &token)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(v.is_object());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn read_endpoints_respond() {
    let (ctx, dir) = test_ctx("reads");
    let (router, _) = build_router(ctx.clone());
    let token = admin_token(&ctx).await;
    for uri in [
        "/api/v1/config",
        "/api/v1/backends",
        "/api/v1/models",
        "/api/v1/gateway/status",
        "/api/v1/gateway/config",
        "/api/v1/gateway/groups",
        "/api/v1/activation-plan",
        "/api/v1/operations",
        "/api/v1/updates",
        "/api/v1/setup/steps",
        "/api/v1/setup/status",
        "/api/v1/mxc/policy",
        "/api/v1/logs/scopes",
    ] {
        let (status, _) = body_json(router.clone().oneshot(get_auth(uri, &token)).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK, "GET {uri}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn get_one_and_not_found_envelope() {
    let (ctx, dir) = test_ctx("getone");
    let (router, _) = build_router(ctx.clone());
    let token = admin_token(&ctx).await;
    // The default config seeds four backends; planner must resolve.
    let (status, v) = body_json(
        router
            .clone()
            .oneshot(get_auth("/api/v1/backends/planner", &token))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "planner");

    let (status, v) = body_json(router.oneshot(get_auth("/api/v1/backends/nope", &token)).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "NOT_FOUND");
    assert_eq!(v["error"]["kind"], "NotFound");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn logs_query_params_coerce() {
    let (ctx, dir) = test_ctx("logs");
    let (router, _) = build_router(ctx.clone());
    let token = admin_token(&ctx).await;
    let (status, v) = body_json(
        router
            .oneshot(get_auth("/api/v1/logs?scope=gateway&limit=5", &token))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(v["scopes"].is_array());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Validation + envelope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_body_field_rejected() {
    let (ctx, dir) = test_ctx("closed");
    let (router, _) = build_router(ctx.clone());
    let token = admin_token(&ctx).await;
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/backends/planner/test")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"prompt": "hi", "bogus": 1}"#))
        .unwrap();
    let (status, v) = body_json(router.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["code"], "INVALID_INPUT");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn malformed_json_rejected() {
    let (ctx, dir) = test_ctx("badjson");
    let (router, _) = build_router(ctx.clone());
    let token = admin_token(&ctx).await;
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/backends/planner/test")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let (status, v) = body_json(router.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["code"], "INVALID_INPUT");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn wrong_method_is_405() {
    let (ctx, dir) = test_ctx("method");
    let (router, _) = build_router(ctx);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/status")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Redaction structure: secret-bearing ops have no route; no read
// response carries key material.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn redaction_structure() {
    // The master-key operation is registered but explicitly routeless.
    let master = OP_SPECS
        .iter()
        .find(|s| s.name == "gateway_master_key")
        .unwrap();
    assert!(master.route.is_none());
    let update_config = OP_SPECS.iter().find(|s| s.name == "update_config").unwrap();
    assert!(update_config.route.is_none());

    let (ctx, dir) = test_ctx("redact");
    let (router, _) = build_router(ctx.clone());
    let token = admin_token(&ctx).await;
    for uri in ["/api/v1/config", "/api/v1/gateway/config", "/api/v1/status"] {
        let (status, v) = body_json(router.clone().oneshot(get_auth(uri, &token)).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK, "GET {uri}");
        let text = v.to_string();
        assert!(!text.contains("PRIVATE KEY"), "key material in {uri}");
        assert!(!text.contains("sk-"), "API key material in {uri}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Live sockets
// ---------------------------------------------------------------------------

fn test_app(dir: &std::path::Path) -> Arc<AppState> {
    let mut cfg = manager_config::AppConfig::default_config();
    cfg.data_dir = dir.to_path_buf();
    Arc::new(AppState::new(
        cfg,
        dir.join("config.json"),
        dir.to_path_buf(),
    ))
}

#[tokio::test]
async fn live_plaintext_loopback_smoke() {
    let (_ctx, dir) = test_ctx("live");
    let server = ApiServer::start(
        test_app(&dir),
        dir.clone(),
        ApiOptions {
            bind: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: 0,
            tls: None,
        },
    )
    .await
    .unwrap();
    let addr = server.addr();
    assert_ne!(addr.port(), 0);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /api/v1/server HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    assert!(raw.contains(" 200 OK"), "response: {raw:.120}");
    assert!(raw.contains("deployment_id"));
    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn plaintext_non_loopback_refused() {
    let (_ctx, dir) = test_ctx("plainbind");
    let err = ApiServer::start(
        test_app(&dir),
        dir.clone(),
        ApiOptions {
            bind: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            port: 0,
            tls: None,
        },
    )
    .await;
    assert!(err.is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn live_tls_smoke() {
    let (_ctx, dir) = test_ctx("livetls");
    let material = manager_api::tls::load_or_generate(&dir).unwrap();
    assert_eq!(material.fingerprint_sha256.len(), 64);
    let server = ApiServer::start(
        test_app(&dir),
        dir.clone(),
        ApiOptions {
            bind: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: 0,
            tls: Some(material),
        },
    )
    .await
    .unwrap();
    let addr = server.addr();

    // Permissive client verifier: the smoke asserts transport +
    // serving, not PKI (pinning is the client's fingerprint check).
    #[derive(Debug)]
    struct NoVerify;
    impl rustls::client::danger::ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![
                rustls::SignatureScheme::RSA_PKCS1_SHA256,
                rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
                rustls::SignatureScheme::RSA_PSS_SHA256,
                rustls::SignatureScheme::ED25519,
            ]
        }
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut tls = connector.connect(server_name, tcp).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    tls.write_all(b"GET /api/v1/server HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut raw = String::new();
    tls.read_to_string(&mut raw).await.unwrap();
    assert!(raw.contains("200"), "response: {raw:.120}");
    assert!(raw.contains("api_version"));
    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}
