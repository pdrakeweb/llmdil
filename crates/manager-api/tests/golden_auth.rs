//! Golden-payload tests for the admin API auth surface the Android
//! client and the CLI parse (admin API plan phases 3/4): the login
//! token pair, a login failure envelope, `/auth/me`, and the REST
//! rendering of `get_status`. Each golden records the HTTP status and
//! the normalised body; tokens, ids, session ids and wall-clock times
//! are replaced with type placeholders. Harness and regeneration:
//! `crates/manager-app/tests/support/golden.rs`
//! (`UPDATE_GOLDEN=1 cargo test -p manager-api --test golden_auth`).

#[path = "../../manager-app/tests/support/golden.rs"]
mod golden;

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use serde_json::{json, Value};
use tower::ServiceExt;

use golden::{assert_golden, Normalizer};
use manager_api::auth::MemoryCredentialStore;
use manager_api::users::{hash_password, now_ms, RoleName, StoreError, User, UserStore};
use manager_api::{build_router, ApiCtx};
use manager_app::AppState;

const PW: &str = "a-unique-golden-passphrase";

/// Every volatile field in the auth payloads.
const VOLATILE: &[&str] = &[
    "access_token",
    "refresh_token",
    "session_id",
    "access_expires_ms",
    "refresh_expires_ms",
    "id",
    "created_ms",
    "updated_ms",
    "version",
];

struct Fixture {
    ctx: Arc<ApiCtx>,
    dir: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn fixture() -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "manager-api-golden-{}-{}",
        std::process::id(),
        manager_api::users::generate_token()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = manager_config::AppConfig::default_config();
    cfg.data_dir = dir.clone();
    let app = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
    let ctx = ApiCtx::with_services(
        app,
        dir.clone(),
        UserStore::in_memory(),
        Arc::new(MemoryCredentialStore::default()),
    );
    let user = User {
        id: format!("u-{}", manager_api::users::generate_token()),
        username: "operator".into(),
        password_hash: hash_password(PW).unwrap(),
        role: RoleName::Operator,
        disabled: false,
        must_change_password: false,
        totp_enabled: false,
        recovery_code_hashes: vec![],
        last_totp_step: None,
        created_ms: now_ms(),
        updated_ms: now_ms(),
    };
    ctx.users
        .mutate::<_, StoreError>(|f| {
            f.users.push(user);
            Ok(())
        })
        .await
        .unwrap();
    Fixture { ctx, dir }
}

/// One request through the full router; the golden records the
/// status code beside the body.
async fn call(
    fx: &Fixture,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Value {
    let (router, _) = build_router(fx.ctx.clone());
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    json!({
        "status": status,
        "body": serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
    })
}

async fn login(fx: &Fixture) -> Value {
    call(
        fx,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({ "username": "operator", "password": PW, "device_name": "golden-test" })),
    )
    .await
}

fn check(fx: &Fixture, name: &str, mut v: Value) {
    Normalizer::keys(VOLATILE)
        .path(&fx.dir, "<DATA_DIR>")
        .apply(&mut v);
    assert_golden(env!("CARGO_MANIFEST_DIR"), name, &v);
}

#[tokio::test]
async fn golden_auth_login_ok() {
    let fx = fixture().await;
    let v = login(&fx).await;
    check(&fx, "auth_login_ok", v);
}

#[tokio::test]
async fn golden_auth_login_invalid_credentials() {
    let fx = fixture().await;
    let v = call(
        &fx,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({ "username": "operator", "password": "not-the-password" })),
    )
    .await;
    check(&fx, "auth_login_invalid_credentials", v);
}

#[tokio::test]
async fn golden_auth_me() {
    let fx = fixture().await;
    let token = login(&fx).await["body"]["access_token"]
        .as_str()
        .expect("login returns an access token")
        .to_string();
    let v = call(&fx, "GET", "/api/v1/auth/me", Some(&token), None).await;
    check(&fx, "auth_me", v);
}

#[tokio::test]
async fn golden_auth_me_unauthenticated() {
    let fx = fixture().await;
    let v = call(&fx, "GET", "/api/v1/auth/me", None, None).await;
    check(&fx, "auth_me_unauthenticated", v);
}

#[tokio::test]
async fn golden_rest_status() {
    let fx = fixture().await;
    let token = login(&fx).await["body"]["access_token"]
        .as_str()
        .expect("login returns an access token")
        .to_string();
    let v = call(&fx, "GET", "/api/v1/status", Some(&token), None).await;
    check(&fx, "rest_status", v);
}
