//! SEC-03 (docs/code-review-v0.1.60.md): after an admin password
//! reset the account is flagged must-change, and until it changes
//! the password every route except change-password, logout and
//! `/auth/me` answers 403 `PASSWORD_CHANGE_REQUIRED` — for sessions
//! and API tokens alike, with refresh refused too.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use manager_api::auth::{
    base32_encode, totp_at, CredentialStore, MemoryCredentialStore, MUST_CHANGE_SESSION_TTL_MS,
};
use manager_api::users::{hash_password, now_ms, RoleName, StoreError, User, UserStore};
use manager_api::{build_router, ApiCtx};
use manager_app::AppState;

const PW: &str = "a-unique-passphrase";
const NEW_PW: &str = "a-brand-new-passphrase";

struct Fixture {
    ctx: Arc<ApiCtx>,
    creds: Arc<MemoryCredentialStore>,
    dir: std::path::PathBuf,
    admin_token: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn fixture(tag: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "manager-api-mustchange-{tag}-{}-{}",
        std::process::id(),
        manager_api::users::generate_token()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = manager_config::AppConfig::default_config();
    cfg.data_dir = dir.clone();
    let app = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
    let creds = Arc::new(MemoryCredentialStore::default());
    let ctx = ApiCtx::with_services(app, dir.clone(), UserStore::in_memory(), creds.clone());
    let admin_id = seed_user(&ctx, "root", RoleName::Admin).await;
    let admin_token = ctx
        .auth
        .create_session(&admin_id, "test")
        .await
        .unwrap()
        .access_token;
    Fixture {
        ctx,
        creds,
        dir,
        admin_token,
    }
}

async fn seed_user(ctx: &Arc<ApiCtx>, username: &str, role: RoleName) -> String {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let hash = HASH.get_or_init(|| hash_password(PW).unwrap()).clone();
    let id = format!("u-{}", manager_api::users::generate_token());
    let user = User {
        id: id.clone(),
        username: username.into(),
        password_hash: hash,
        role,
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
    id
}

async fn call(
    ctx: &Arc<ApiCtx>,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (router, _) = build_router(ctx.clone());
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
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn reset(fx: &Fixture, user_id: &str) -> String {
    let (status, body) = call(
        &fx.ctx,
        "POST",
        &format!("/api/v1/admin/users/{user_id}/reset-password"),
        Some(&fx.admin_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["temporary_password"].as_str().unwrap().to_string()
}

fn assert_change_required(status: StatusCode, body: &Value) {
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "PASSWORD_CHANGE_REQUIRED", "{body}");
}

#[tokio::test]
async fn reset_user_is_confined_to_change_password_until_it_changes() {
    let fx = fixture("flow").await;
    let user_id = seed_user(&fx.ctx, "pat", RoleName::Operator).await;
    let old = fx
        .ctx
        .auth
        .create_session(&user_id, "laptop")
        .await
        .unwrap();
    let temp = reset(&fx, &user_id).await;
    // The reset revoked the pre-existing session.
    let (status, _) = call(
        &fx.ctx,
        "GET",
        "/api/v1/status",
        Some(&old.access_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The temporary password logs in to a short, flagged session.
    let before = now_ms();
    let (status, login) = call(
        &fx.ctx,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({ "username": "pat", "password": temp })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{login}");
    assert_eq!(login["must_change_password"], true);
    assert_eq!(login["user"]["must_change_password"], true);
    let refresh_exp = login["refresh_expires_ms"].as_i64().unwrap();
    assert!(refresh_exp <= now_ms() + MUST_CHANGE_SESSION_TTL_MS);
    assert!(refresh_exp >= before + MUST_CHANGE_SESSION_TTL_MS - 5_000);
    let access = login["access_token"].as_str().unwrap().to_string();
    let refresh = login["refresh_token"].as_str().unwrap().to_string();

    // /auth/me answers and reports the flag (HEAD resolves as GET too).
    let (status, me) = call(&fx.ctx, "GET", "/api/v1/auth/me", Some(&access), None).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["must_change_password"], true);
    assert_eq!(me["user"]["must_change_password"], true);

    // Everything else is refused with the new code.
    let (status, body) = call(&fx.ctx, "GET", "/api/v1/status", Some(&access), None).await;
    assert_change_required(status, &body);
    let (status, body) = call(&fx.ctx, "GET", "/api/v1/auth/sessions", Some(&access), None).await;
    assert_change_required(status, &body);
    let (status, body) = call(&fx.ctx, "HEAD", "/api/v1/status", Some(&access), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Refresh (a public route) is refused too, without rotating.
    let (status, body) = call(
        &fx.ctx,
        "POST",
        "/api/v1/auth/refresh",
        None,
        Some(json!({ "refresh_token": refresh })),
    )
    .await;
    assert_change_required(status, &body);

    // Every refusal is audited.
    let refusals: Vec<Value> = fx
        .ctx
        .audit
        .read_recent(0, 100)
        .into_iter()
        .filter(|e| e["result"] == "password_change_required")
        .collect();
    assert!(refusals.len() >= 4, "{refusals:?}");
    assert!(refusals.iter().all(|e| e["actor"] == "pat"), "{refusals:?}");
    assert!(refusals
        .iter()
        .any(|e| e["action"] == "POST /api/v1/auth/refresh"));
    assert!(refusals.iter().any(|e| e["action"] == "GET /api/v1/status"));

    // Change the password: the same session keeps working, unconfined.
    let (status, body) = call(
        &fx.ctx,
        "POST",
        "/api/v1/auth/password",
        Some(&access),
        Some(json!({ "current_password": temp, "new_password": NEW_PW })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(&fx.ctx, "GET", "/api/v1/status", Some(&access), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, me) = call(&fx.ctx, "GET", "/api/v1/auth/me", Some(&access), None).await;
    assert_eq!(me["must_change_password"], false);
    // And refresh now rotates, back to a normal session lifetime.
    let (status, body) = call(
        &fx.ctx,
        "POST",
        "/api/v1/auth/refresh",
        None,
        Some(json!({ "refresh_token": refresh })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["must_change_password"], false);
    assert!(body["refresh_expires_ms"].as_i64().unwrap() > now_ms() + MUST_CHANGE_SESSION_TTL_MS);
}

#[tokio::test]
async fn logout_is_open_to_a_flagged_session() {
    let fx = fixture("logout").await;
    let user_id = seed_user(&fx.ctx, "lou", RoleName::Viewer).await;
    fx.ctx.auth.admin_reset_password(&user_id).await.unwrap();
    let pair = fx.ctx.auth.create_session(&user_id, "phone").await.unwrap();
    let (status, body) = call(
        &fx.ctx,
        "POST",
        "/api/v1/auth/logout",
        Some(&pair.access_token),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = call(
        &fx.ctx,
        "GET",
        "/api/v1/auth/me",
        Some(&pair.access_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn reset_revokes_api_tokens_and_flagged_tokens_are_confined() {
    let fx = fixture("tokens").await;
    let user_id = seed_user(&fx.ctx, "bot", RoleName::Operator).await;
    let mint = |name: &'static str| {
        let ctx = fx.ctx.clone();
        let admin = fx.admin_token.clone();
        let uid = user_id.clone();
        async move {
            let (status, body) = call(
                &ctx,
                "POST",
                "/api/v1/admin/api-tokens",
                Some(&admin),
                Some(json!({ "user_id": uid, "name": name, "role": "viewer" })),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            body["value"].as_str().unwrap().to_string()
        }
    };
    let before_reset = mint("ci").await;
    let (status, _) = call(&fx.ctx, "GET", "/api/v1/status", Some(&before_reset), None).await;
    assert_eq!(status, StatusCode::OK);

    reset(&fx, &user_id).await;
    // The reset revoked the existing token outright.
    let (status, _) = call(&fx.ctx, "GET", "/api/v1/status", Some(&before_reset), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let revoked = fx
        .ctx
        .users
        .read(|f| {
            f.api_tokens
                .iter()
                .all(|t| t.user_id != user_id || t.revoked)
        })
        .await;
    assert!(revoked);

    // A token issued while the owner is still flagged is confined.
    let during = mint("ci-2").await;
    let (status, body) = call(&fx.ctx, "GET", "/api/v1/status", Some(&during), None).await;
    assert_change_required(status, &body);
}

#[tokio::test]
async fn totp_user_completes_second_factor_then_changes_password() {
    let fx = fixture("totp").await;
    let user_id = seed_user(&fx.ctx, "tia", RoleName::Operator).await;
    let secret = b"0123456789abcdefghij".to_vec();
    fx.creds
        .write(&format!("llmdil/totp/{user_id}"), &base32_encode(&secret))
        .await
        .unwrap();
    let uid = user_id.clone();
    fx.ctx
        .users
        .mutate::<_, StoreError>(|f| {
            f.users
                .iter_mut()
                .find(|u| u.id == uid)
                .unwrap()
                .totp_enabled = true;
            Ok(())
        })
        .await
        .unwrap();
    let temp = reset(&fx, &user_id).await;

    let (status, body) = call(
        &fx.ctx,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({ "username": "tia", "password": temp })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["totp_required"], true);
    let code = format!("{:06}", totp_at(&secret, now_ms() / 1000));
    let (status, tokens) = call(
        &fx.ctx,
        "POST",
        "/api/v1/auth/totp",
        None,
        Some(json!({ "challenge_id": body["challenge_id"], "code": code })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    assert_eq!(tokens["must_change_password"], true);
    let access = tokens["access_token"].as_str().unwrap().to_string();

    let (status, body) = call(&fx.ctx, "GET", "/api/v1/models", Some(&access), None).await;
    assert_change_required(status, &body);
    let (status, body) = call(
        &fx.ctx,
        "POST",
        "/api/v1/auth/password",
        Some(&access),
        Some(json!({ "current_password": temp, "new_password": NEW_PW })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(&fx.ctx, "GET", "/api/v1/status", Some(&access), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
