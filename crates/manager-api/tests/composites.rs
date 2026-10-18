//! Phase 3 gate tests: composite endpoints (model PATCH, gateway
//! group CRUD, gateway-config bind_host split, activation apply,
//! gateway start alias), the admin audit read, and concurrent
//! writers through the API surface.

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
        "llm-api-composites-{}",
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

async fn seed_token(ctx: &Arc<ApiCtx>, username: &str, role: RoleName) -> String {
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
    ctx.auth
        .create_session(&user_id, "test")
        .await
        .unwrap()
        .access_token
}

async fn call(
    ctx: &Arc<ApiCtx>,
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (router, _) = build_router(ctx.clone());
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn group_crud_round_trip() {
    let (ctx, _dir) = test_ctx().await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let viewer = seed_token(&ctx, "look", RoleName::Viewer).await;

    // Viewer cannot create groups (class C).
    let (status, _) = call(
        &ctx,
        Method::POST,
        "/api/v1/gateway/groups",
        Some(&viewer),
        Some(json!({ "name": "g1", "members": ["planner"], "strategy": "simple-shuffle" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = call(
        &ctx,
        Method::POST,
        "/api/v1/gateway/groups",
        Some(&admin),
        Some(json!({ "name": "g1", "members": ["planner"], "strategy": "simple-shuffle" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Duplicate name refused.
    let (status, _) = call(
        &ctx,
        Method::POST,
        "/api/v1/gateway/groups",
        Some(&admin),
        Some(json!({ "name": "g1", "members": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = call(
        &ctx,
        Method::GET,
        "/api/v1/gateway/groups",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["groups"]
        .as_array()
        .unwrap()
        .iter()
        .any(|g| g["name"] == "g1"));

    let (status, body) = call(
        &ctx,
        Method::PATCH,
        "/api/v1/gateway/groups/g1",
        Some(&admin),
        Some(json!({ "strategy": "least-busy" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &ctx,
        Method::GET,
        "/api/v1/gateway/config",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let g1 = body["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["name"] == "g1")
        .unwrap()
        .clone();
    assert_eq!(g1["strategy"], "least-busy");
    assert_eq!(g1["members"][0], "planner", "patch merges, not replaces");

    let (status, _) = call(
        &ctx,
        Method::DELETE,
        "/api/v1/gateway/groups/g1",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &ctx,
        Method::DELETE,
        "/api/v1/gateway/groups/g1",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn concurrent_group_creates_all_land() {
    let (ctx, _dir) = test_ctx().await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let (router, _) = build_router(ctx.clone());
    let mut handles = Vec::new();
    for i in 0..8 {
        let router = router.clone();
        let token = admin.clone();
        handles.push(tokio::spawn(async move {
            let req = Request::builder()
                .method(Method::POST)
                .uri("/api/v1/gateway/groups")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "name": format!("cg-{i}"), "members": ["planner"] }).to_string(),
                ))
                .unwrap();
            router.oneshot(req).await.unwrap().status()
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), StatusCode::CREATED);
    }
    let (status, body) = call(&ctx, Method::GET, "/api/v1/gateway/groups", Some(&admin), None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = body["groups"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|g| g["name"].as_str())
        .collect();
    for i in 0..8 {
        assert!(names.contains(&format!("cg-{i}").as_str()), "lost update: {names:?}");
    }
}

#[tokio::test]
async fn model_patch_dispatches_both_fields() {
    let (ctx, _dir) = test_ctx().await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    // Register a model to patch.
    let (status, _) = call(
        &ctx,
        Method::POST,
        "/api/v1/models",
        Some(&admin),
        Some(json!({
            "id": "patch-me",
            "source_url": "https://example.invalid/patch-me.gguf",
            "params_b": 7.0,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "model_add: {status}");

    let (status, body) = call(
        &ctx,
        Method::PATCH,
        "/api/v1/models/patch-me",
        Some(&admin),
        Some(json!({ "display_name": "Patched", "enabled": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], "patch-me");
    assert_eq!(body["enabled"], false);

    let (status, model) = call(&ctx, Method::GET, "/api/v1/models/patch-me", Some(&admin), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(model["enabled"], false);
    assert_eq!(model["name"], "Patched");

    // Unknown field and empty body are refused.
    let (status, _) = call(
        &ctx,
        Method::PATCH,
        "/api/v1/models/patch-me",
        Some(&admin),
        Some(json!({ "bogus": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &ctx,
        Method::PATCH,
        "/api/v1/models/ghost",
        Some(&admin),
        Some(json!({ "enabled": true })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn gateway_config_bind_host_split() {
    let (ctx, _dir) = test_ctx().await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let (status, body) = call(
        &ctx,
        Method::PATCH,
        "/api/v1/gateway/config",
        Some(&admin),
        Some(json!({ "bind_host": "0.0.0.0" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["bind_host"], "0.0.0.0");
    assert_eq!(body["restart_required"], true);
    let (status, cfg) = call(&ctx, Method::GET, "/api/v1/gateway/config", Some(&admin), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cfg["bind_host"], "0.0.0.0");

    // Not an IP -> the bind operation's validation answers.
    let (status, _) = call(
        &ctx,
        Method::PATCH,
        "/api/v1/gateway/config",
        Some(&admin),
        Some(json!({ "bind_host": "not-an-ip" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn activation_apply_on_empty_plan_and_role_gate() {
    let (ctx, _dir) = test_ctx().await;
    let operator = seed_token(&ctx, "op", RoleName::Operator).await;
    let viewer = seed_token(&ctx, "look", RoleName::Viewer).await;

    let (status, _) = call(
        &ctx,
        Method::POST,
        "/api/v1/activation-plan/apply",
        Some(&viewer),
        Some(json!({ "mode": "now" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "apply is class L");

    // Since phase 4 the apply runs as an operation: 202, then the
    // record's detail carries the composition's outcome.
    let (status, body) = call(
        &ctx,
        Method::POST,
        "/api/v1/activation-plan/apply",
        Some(&operator),
        Some(json!({ "mode": "now" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let op_id = body["id"].as_str().unwrap().to_string();
    let mut detail = Value::Null;
    for _ in 0..100 {
        let (status, record) = call(
            &ctx,
            Method::GET,
            &format!("/api/v1/operations/{op_id}"),
            Some(&operator),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        if record["status"] != "running" {
            assert_eq!(record["status"], "done", "{record}");
            detail = record["detail"].clone();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(detail["applied"], true);
    assert_eq!(detail["restarted_backends"], json!([]));

    let (status, _) = call(
        &ctx,
        Method::POST,
        "/api/v1/activation-plan/apply",
        Some(&operator),
        Some(json!({ "mode": "sideways" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn gateway_start_alias_is_admin_class() {
    let (ctx, _dir) = test_ctx().await;
    let operator = seed_token(&ctx, "op", RoleName::Operator).await;
    // The alias inherits the start-gateway step's class D: the
    // guard answers before any step machinery runs.
    let (status, _) = call(
        &ctx,
        Method::POST,
        "/api/v1/gateway/start",
        Some(&operator),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&ctx, Method::POST, "/api/v1/gateway/start", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_audit_readback() {
    let (ctx, _dir) = test_ctx().await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let operator = seed_token(&ctx, "op", RoleName::Operator).await;
    let (status, _) = call(
        &ctx,
        Method::POST,
        "/api/v1/gateway/groups",
        Some(&admin),
        Some(json!({ "name": "audited", "members": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(&ctx, Method::GET, "/api/v1/admin/audit", Some(&admin), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entries = body["entries"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e["action"] == "POST /api/v1/gateway/groups" && e["result"] == "201"),
        "audit entries: {entries:?}"
    );
    // Operator cannot read the audit log.
    let (status, _) = call(&ctx, Method::GET, "/api/v1/admin/audit", Some(&operator), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
