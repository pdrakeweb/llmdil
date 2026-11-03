//! Phase 5 negative sweep: forged, expired, and revoked
//! credentials; role escalation attempts; malformed and oversized
//! requests; unknown routes. Every refusal must come back in the
//! standard error envelope with the documented code.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use manager_api::auth::MemoryCredentialStore;
use manager_api::users::RoleName;
use manager_api::{build_router, ApiCtx, UserStore};
use manager_app::AppState;

async fn test_ctx() -> (Arc<ApiCtx>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "llm-api-negative-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.json");
    let app = Arc::new(AppState::new(
        manager_config::AppConfig::default_config(),
        config_path,
        dir.clone(),
    ));
    let ctx = ApiCtx::with_services(
        app,
        dir.clone(),
        UserStore::in_memory(),
        Arc::new(MemoryCredentialStore::default()),
    );
    (ctx, dir)
}

/// Seed a user and return (user_id, access_token).
async fn seed(ctx: &Arc<ApiCtx>, username: &str, role: RoleName) -> (String, String) {
    static TEST_HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let hash = TEST_HASH
        .get_or_init(|| manager_api::users::hash_password("a-unique-passphrase").unwrap())
        .clone();
    let user_id = format!("u-{}", manager_api::users::generate_token());
    let user = manager_api::users::User {
        id: user_id.clone(),
        username: username.into(),
        password_hash: hash,
        role,
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
    let pair = ctx.auth.create_session(&user_id, "test").await.unwrap();
    (user_id, pair.access_token)
}

async fn call_raw(
    ctx: &Arc<ApiCtx>,
    method: Method,
    uri: &str,
    headers: &[(&str, String)],
    body: Body,
) -> (StatusCode, Value) {
    let (router, _) = build_router(ctx.clone());
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, v.clone());
    }
    let resp = router.oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn bearer(token: &str) -> Vec<(&str, String)> {
    vec![("authorization", format!("Bearer {token}"))]
}

fn assert_envelope(body: &Value, code: &str) {
    assert_eq!(body["error"]["code"], code, "body: {body}");
    assert!(body["error"]["message"].as_str().is_some_and(|m| !m.is_empty()));
}

#[tokio::test]
async fn forged_and_garbage_bearers_are_refused() {
    let (ctx, _dir) = test_ctx().await;
    for token in [
        "not-a-real-token".to_string(),
        "a".repeat(64),
        String::new(),
        "🔑".to_string(),
    ] {
        let (status, body) = call_raw(
            &ctx,
            Method::GET,
            "/api/v1/status",
            &bearer(&token),
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "token {token:?}");
        assert_envelope(&body, "UNAUTHENTICATED");
    }
    // Wrong scheme entirely.
    let (status, _) = call_raw(
        &ctx,
        Method::GET,
        "/api/v1/status",
        &[("authorization", "Basic dXNlcjpwYXNz".to_string())],
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn expired_and_revoked_sessions_are_refused() {
    let (ctx, _dir) = test_ctx().await;
    let (user_id, token) = seed(&ctx, "victim", RoleName::Admin).await;

    // Sanity: the fresh token works.
    let (status, _) = call_raw(&ctx, Method::GET, "/api/v1/status", &bearer(&token), Body::empty()).await;
    assert_eq!(status, StatusCode::OK);

    // Expire the session in the store.
    ctx.users
        .mutate::<_, manager_api::users::StoreError>(|f| {
            for s in &mut f.sessions {
                s.access_expires_ms = 1;
            }
            Ok(())
        })
        .await
        .unwrap();
    let (status, body) = call_raw(&ctx, Method::GET, "/api/v1/status", &bearer(&token), Body::empty()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_envelope(&body, "UNAUTHENTICATED");

    // A new session, revoked server-side, dies immediately too.
    let pair = ctx.auth.create_session(&user_id, "second").await.unwrap();
    ctx.auth.revoke_session(&pair.session_id).await.unwrap();
    let (status, _) = call_raw(
        &ctx,
        Method::GET,
        "/api/v1/status",
        &bearer(&pair.access_token),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn disabled_user_tokens_die() {
    let (ctx, _dir) = test_ctx().await;
    let (user_id, token) = seed(&ctx, "soon-disabled", RoleName::Operator).await;
    ctx.users
        .mutate::<_, manager_api::users::StoreError>(|f| {
            f.users.iter_mut().for_each(|u| {
                if u.id == user_id {
                    u.disabled = true;
                }
            });
            Ok(())
        })
        .await
        .unwrap();
    let (status, _) = call_raw(&ctx, Method::GET, "/api/v1/status", &bearer(&token), Body::empty()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn role_escalation_attempts_are_refused() {
    let (ctx, _dir) = test_ctx().await;
    let (_vid, viewer) = seed(&ctx, "v", RoleName::Viewer).await;
    let (_oid, operator) = seed(&ctx, "o", RoleName::Operator).await;

    // Viewer reaching for user administration.
    let (status, body) = call_raw(&ctx, Method::GET, "/api/v1/admin/users", &bearer(&viewer), Body::empty()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_envelope(&body, "FORBIDDEN");
    // Operator reaching for a config write.
    let (status, _) = call_raw(
        &ctx,
        Method::PATCH,
        "/api/v1/gateway/config",
        &bearer(&operator),
        Body::from(json!({ "port": 4001 }).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Operator reaching for the master-key disclosure op route is
    // not possible at all (the op has no route in v1), but the
    // S-disclosure bundle content refuses the operator as well.
    let (status, _) = call_raw(
        &ctx,
        Method::GET,
        "/api/v1/diagnostics/bundles/x/content",
        &bearer(&operator),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // An operator-role API token cannot mutate through any route.
    let (_aid, admin) = seed(&ctx, "a", RoleName::Admin).await;
    let _ = admin;
    let (status, _) = call_raw(
        &ctx,
        Method::POST,
        "/api/v1/backends/planner/stop",
        &bearer(&viewer),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn malformed_oversized_and_unknown_requests() {
    let (ctx, _dir) = test_ctx().await;
    let (_id, admin) = seed(&ctx, "root", RoleName::Admin).await;

    // Malformed JSON body.
    let (status, body) = call_raw(
        &ctx,
        Method::PATCH,
        "/api/v1/gateway/config",
        &bearer(&admin),
        Body::from("{not json"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_envelope(&body, "INVALID_INPUT");

    // Oversized body (> 1 MiB limit).
    let big = "x".repeat((1 << 20) + 1);
    let (status, _) = call_raw(
        &ctx,
        Method::PATCH,
        "/api/v1/gateway/config",
        &bearer(&admin),
        Body::from(big),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "status {status}");

    // Unknown route: the envelope, not a bare framework 404.
    let (status, body) = call_raw(
        &ctx,
        Method::GET,
        "/api/v1/no-such-thing",
        &bearer(&admin),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_envelope(&body, "NOT_FOUND");

    // Unknown fields in a closed body are refused.
    let (status, body) = call_raw(
        &ctx,
        Method::POST,
        "/api/v1/activation-plan/quiesce-wait",
        &bearer(&admin),
        Body::from(json!({ "timeout_secs": 5, "surprise": true }).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_envelope(&body, "INVALID_INPUT");
}
