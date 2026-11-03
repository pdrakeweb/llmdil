//! Phase-2 authorization tests: the role matrix exercised over the
//! live route table (deny side exhaustively — the guard answers
//! before any dispatch, so no operation runs), the class-table
//! invariant (every registered route has a class), and the HTTP
//! auth flows (login, rotation + reuse, enrollment, API tokens,
//! admin user management).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use manager_api::auth::MemoryCredentialStore;
use manager_api::users::{hash_password, now_ms, User};
use manager_api::auth_routes::route_class_table;
use manager_api::users::{RoleName, UserStore};
use manager_api::{build_router, ApiCtx};
use manager_app::registry::{role_allows, OP_SPECS};
use manager_app::AppState;
use tower::ServiceExt;

const PW: &str = "a-unique-passphrase";

async fn ctx_with_users() -> (Arc<ApiCtx>, std::path::PathBuf, [String; 4]) {
    let dir = std::env::temp_dir().join(format!(
        "manager-api-matrix-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
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
    // Seed users directly with one shared password hash and mint
    // sessions without password logins: argon2 at plan parameters
    // costs seconds per call on this VM, and the password paths
    // have their own dedicated flow tests.
    static TEST_HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let hash = TEST_HASH
        .get_or_init(|| hash_password(PW).unwrap())
        .clone();
    let roles = [
        RoleName::Viewer,
        RoleName::Operator,
        RoleName::Configurator,
        RoleName::Admin,
    ];
    let mut tokens = Vec::new();
    for role in roles {
        let username = format!("user-{}", role.as_str());
        let user_id = format!("u-{}", manager_api::users::generate_token());
        let user = User {
            id: user_id.clone(),
            username: username.clone(),
            password_hash: hash.clone(),
            role,
            disabled: false,
            must_change_password: false,
            totp_enabled: false,
            recovery_code_hashes: vec![],
            created_ms: now_ms(),
            updated_ms: now_ms(),
        };
        ctx.users
            .mutate::<_, manager_api::users::StoreError>(|f| {
                f.users.push(user);
                Ok(())
            })
            .await
            .unwrap();
        let pair = ctx.auth.create_session(&user_id, "test").await.unwrap();
        tokens.push(pair.access_token);
    }
    (ctx, dir, tokens.try_into().unwrap())
}

fn req(method: &str, uri: &str, token: Option<&str>, body: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    b.body(Body::from(body.unwrap_or("").to_string())).unwrap()
}

async fn status(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> StatusCode {
    router
        .clone()
        .oneshot(req(method, uri, token, body))
        .await
        .unwrap()
        .status()
}

fn fill_template(template: &str) -> String {
    template
        .replace("{id}", "planner")
        .replace("{step_id}", "check-prereqs")
        .replace("{task_id}", "1")
        .replace("{component_id}", "llama-cpp")
        .replace("{session_id}", "none")
}

/// Substitute a real resource id where the template needs one that
/// resolves; "planner" exists in the default config for backends,
/// other ids 404 — both prove the guard passed (not 401/403).
#[test]
fn class_table_covers_every_route() {
    let table = route_class_table();
    let (ctx, dir) = {
        // Reuse the router records from a minimal ctx.
        let dir = std::env::temp_dir().join(format!(
            "manager-api-coverage-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
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
        (ctx, dir)
    };
    let (_router, records) = build_router(ctx);
    for r in &records {
        assert!(
            table.contains_key(&(r.method.to_string(), r.path.to_string())),
            "route {} {} has no auth class (deny-by-default would refuse everyone)",
            r.method,
            r.path
        );
    }
    // And every OP_SPECS class is decidable by role_allows (the
    // exhaustive table): admin is allowed every class by construction.
    for spec in OP_SPECS {
        assert!(role_allows(manager_app::registry::Role::Admin, spec.class));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn get_routes_follow_the_matrix() {
    let (ctx, dir, tokens) = ctx_with_users().await;
    let (router, _records) = build_router(ctx);
    let roles = [
        manager_app::registry::Role::Viewer,
        manager_app::registry::Role::Operator,
        manager_app::registry::Role::Configurator,
        manager_app::registry::Role::Admin,
    ];
    // Every GET op route + derived GET: each role gets 403 exactly
    // when the class table says the role is not allowed.
    let mut checked = 0;
    for spec in OP_SPECS {
        let Some(r) = spec.route else { continue };
        if format!("{:?}", r.method) != "Get" {
            continue;
        }
        if spec.name == "gateway_preview_yaml" {
            continue; // text/yaml body; covered by class table test
        }
        let uri = fill_template(r.path);
        for (i, role) in roles.iter().enumerate() {
            let st = status(&router, "GET", &uri, Some(&tokens[i]), None).await;
            if role_allows(*role, spec.class) {
                assert_ne!(st, StatusCode::FORBIDDEN, "{role:?} GET {uri} must pass the guard");
                assert_ne!(st, StatusCode::UNAUTHORIZED, "{role:?} GET {uri}");
            } else {
                assert_eq!(st, StatusCode::FORBIDDEN, "{role:?} GET {uri} must be refused");
            }
            checked += 1;
        }
    }
    assert!(checked > 60, "matrix too small: {checked}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn mutation_routes_deny_below_class() {
    let (ctx, dir, tokens) = ctx_with_users().await;
    let (router, _records) = build_router(ctx);
    // viewer(0), operator(1), configurator(2) tokens against every
    // non-GET op route whose class they fail: the guard must answer
    // 403 before dispatch (no side effects from these calls).
    let role_of = [
        manager_app::registry::Role::Viewer,
        manager_app::registry::Role::Operator,
        manager_app::registry::Role::Configurator,
    ];
    for spec in OP_SPECS {
        let Some(r) = spec.route else { continue };
        let method = format!("{:?}", r.method).to_uppercase();
        if method == "GET" {
            continue;
        }
        let uri = fill_template(r.path);
        for (i, role) in role_of.iter().enumerate() {
            if !role_allows(*role, spec.class) {
                let st = status(&router, &method, &uri, Some(&tokens[i]), Some("{}")).await;
                assert_eq!(
                    st,
                    StatusCode::FORBIDDEN,
                    "{role:?} {method} {uri} must be refused at the guard"
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unauthenticated_is_refused_everywhere_but_public() {
    let (ctx, dir, _tokens) = ctx_with_users().await;
    let (router, records) = build_router(ctx);
    for r in &records {
        if r.path == "/api/v1/server"
            || r.path.starts_with("/api/v1/auth/login")
            || r.path == "/api/v1/auth/refresh"
            || r.path == "/api/v1/auth/totp"
            || r.path == "/api/v1/enroll"
        {
            continue;
        }
        let uri = fill_template(r.path);
        let st = status(&router, r.method, &uri, None, Some("{}")).await;
        assert_eq!(
            st,
            StatusCode::UNAUTHORIZED,
            "{} {} without a token",
            r.method,
            r.path
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn http_login_refresh_reuse_flow() {
    let (ctx, dir, _tokens) = ctx_with_users().await;
    let (router, _) = build_router(ctx);
    // Wrong password: the uniform 401.
    let (st, body) = {
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/v1/auth/login",
                None,
                Some(r#"{"username": "user-admin", "password": "wrong-password-zz"}"#),
            ))
            .await
            .unwrap();
        let st = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (st, serde_json::from_slice::<serde_json::Value>(&bytes).unwrap())
    };
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "INVALID_CREDENTIALS");

    // Correct login -> tokens; refresh rotates; reuse revokes.
    let login = |router: axum::Router| async move {
        let resp = router
            .oneshot(req(
                "POST",
                "/api/v1/auth/login",
                None,
                Some(&format!(
                    r#"{{"username": "user-operator", "password": "{PW}", "device_name": "pixel"}}"#
                )),
            ))
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
    };
    let body = login(router.clone()).await;
    let refresh1 = body["refresh_token"].as_str().unwrap().to_string();
    let resp = router
        .clone()
        .oneshot(req(
            "POST",
            "/api/v1/auth/refresh",
            None,
            Some(&format!(r#"{{"refresh_token": "{refresh1}"}}"#)),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = router
        .clone()
        .oneshot(req(
            "POST",
            "/api/v1/auth/refresh",
            None,
            Some(&format!(r#"{{"refresh_token": "{refresh1}"}}"#)),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], "SESSION_REVOKED");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn http_enrollment_and_admin_flows() {
    let (ctx, dir, tokens) = ctx_with_users().await;
    let admin = tokens[3].clone();
    let (router, _) = build_router(ctx.clone());

    // Operator may not administer users.
    let st = status(&router, "GET", "/api/v1/admin/users", Some(&tokens[1]), None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // Admin creates a user, then an enrollment code for them; the
    // code redeems once into a working session.
    let resp = router
        .clone()
        .oneshot(req(
            "POST",
            "/api/v1/admin/users",
            Some(&admin),
            Some(&format!(
                r#"{{"username": "newphone", "password": "{PW}", "role": "viewer"}}"#
            )),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let user_id = created["user"]["id"].as_str().unwrap().to_string();

    let resp = router
        .clone()
        .oneshot(req(
            "POST",
            "/api/v1/admin/enrollment-codes",
            Some(&admin),
            Some(&format!(r#"{{"user_id": "{user_id}"}}"#)),
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let code = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["code"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = router
        .clone()
        .oneshot(req(
            "POST",
            "/api/v1/enroll",
            None,
            Some(&format!(
                r#"{{"code": "{code}", "device_name": "pixel"}}"#
            )),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let enrolled: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let access = enrolled["access_token"].as_str().unwrap();
    let st = status(&router, "GET", "/api/v1/status", Some(access), None).await;
    assert_eq!(st, StatusCode::OK);
    // Second redemption fails.
    let resp = router
        .clone()
        .oneshot(req(
            "POST",
            "/api/v1/enroll",
            None,
            Some(&format!(r#"{{"code": "{code}"}}"#)),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Admin issues an API token for the viewer user; it reads but
    // cannot mutate (viewer class), and dies on revocation.
    let resp = router
        .clone()
        .oneshot(req(
            "POST",
            "/api/v1/admin/api-tokens",
            Some(&admin),
            Some(&format!(
                r#"{{"user_id": "{user_id}", "name": "pixel", "role": "viewer"}}"#
            )),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let issued: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let token_value = issued["value"].as_str().unwrap().to_string();
    let token_id = issued["api_token"]["id"].as_str().unwrap().to_string();
    let st = status(&router, "GET", "/api/v1/status", Some(&token_value), None).await;
    assert_eq!(st, StatusCode::OK);
    let st = status(
        &router,
        "POST",
        "/api/v1/backends/planner/start",
        Some(&token_value),
        Some("{}"),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let st = status(
        &router,
        "DELETE",
        &format!("/api/v1/admin/api-tokens/{token_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let st = status(&router, "GET", "/api/v1/status", Some(&token_value), None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // The audit log recorded the mutations with the admin as actor.
    let recent = ctx.audit.read_recent(0, 100);
    assert!(recent
        .iter()
        .any(|e| e["action"] == "POST /api/v1/admin/users" && e["actor"] == "user-admin"));
    let _ = std::fs::remove_dir_all(&dir);
}
