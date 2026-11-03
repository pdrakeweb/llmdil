//! Phase 4 gate tests: asynchronous operation lifecycle (202 +
//! poll + abort), the activation apply as an operation, the
//! diagnostics bundle content endpoint, and the SSE event stream
//! over a live socket.

use std::sync::Arc;
use std::time::Duration;

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
        "llm-api-ops-{}",
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

/// Poll an operation until it leaves the running state.
async fn poll_terminal(ctx: &Arc<ApiCtx>, token: &str, id: &str) -> Value {
    for _ in 0..100 {
        let (status, record) = call(
            ctx,
            Method::GET,
            &format!("/api/v1/operations/{id}"),
            Some(token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        if record["status"] != "running" {
            return record;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("operation {id} never finished");
}

#[tokio::test]
async fn async_op_lifecycle_202_poll_terminal() {
    let (ctx, _dir) = test_ctx().await;
    let operator = seed_token(&ctx, "op", RoleName::Operator).await;

    // Starting an unknown backend: accepted, then fails in the
    // record (the synchronous call would have errored).
    let (status, record) = call(
        &ctx,
        Method::POST,
        "/api/v1/backends/ghost/start",
        Some(&operator),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{record}");
    assert_eq!(record["kind"], "start_backend");
    assert_eq!(record["status"], "running");
    let id = record["id"].as_str().unwrap().to_string();

    let done = poll_terminal(&ctx, &operator, &id).await;
    assert_eq!(done["status"], "failed");
    assert!(done["error"].as_str().is_some_and(|e| !e.is_empty()));
    assert!(done["finished_ms"].is_number());

    // The record appears in the merged operations list.
    let (status, list) = call(&ctx, Method::GET, "/api/v1/operations", Some(&operator), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["id"] == id));

    // A viewer's start attempt is refused by the guard, not queued.
    let viewer = seed_token(&ctx, "look", RoleName::Viewer).await;
    let (status, _) = call(
        &ctx,
        Method::POST,
        "/api/v1/backends/ghost/start",
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn abort_is_bookkeeping_and_sticks() {
    let (ctx, _dir) = test_ctx().await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    // The diagnostics export waits on telemetry, giving a running
    // window to abort in.
    let (status, record) = call(
        &ctx,
        Method::POST,
        "/api/v1/diagnostics/bundles",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = record["id"].as_str().unwrap().to_string();

    let (status, body) = call(
        &ctx,
        Method::POST,
        &format!("/api/v1/operations/{id}/abort"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);

    // Even after the underlying work finishes, the record stays
    // aborted (the app's bookkeeping semantics).
    tokio::time::sleep(Duration::from_secs(4)).await;
    let (status, record) = call(
        &ctx,
        Method::GET,
        &format!("/api/v1/operations/{id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(record["status"], "aborted");
}

#[tokio::test]
async fn activation_apply_runs_as_operation() {
    let (ctx, _dir) = test_ctx().await;
    let operator = seed_token(&ctx, "op", RoleName::Operator).await;
    let (status, record) = call(
        &ctx,
        Method::POST,
        "/api/v1/activation-plan/apply",
        Some(&operator),
        Some(json!({ "mode": "now" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{record}");
    assert_eq!(record["kind"], "activation_apply");
    let id = record["id"].as_str().unwrap().to_string();
    let done = poll_terminal(&ctx, &operator, &id).await;
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(done["detail"]["applied"], true);
}

#[tokio::test]
async fn bundle_content_round_trip() {
    let (ctx, _dir) = test_ctx().await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let operator = seed_token(&ctx, "op", RoleName::Operator).await;

    // Export is S-disclosure class: the operator is refused.
    let (status, _) = call(
        &ctx,
        Method::POST,
        "/api/v1/diagnostics/bundles",
        Some(&operator),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, record) = call(
        &ctx,
        Method::POST,
        "/api/v1/diagnostics/bundles",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = record["id"].as_str().unwrap().to_string();
    let done = poll_terminal(&ctx, &admin, &id).await;
    assert_eq!(done["status"], "done", "{done}");
    let bundle_id = done["detail"]["bundle_id"]
        .as_str()
        .expect("bundle id in detail")
        .to_string();

    // Content: the zip bytes, admin only.
    let (router, _) = build_router(ctx.clone());
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/api/v1/diagnostics/bundles/{bundle_id}/content"))
        .header("authorization", format!("Bearer {operator}"))
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let (router, _) = build_router(ctx.clone());
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/api/v1/diagnostics/bundles/{bundle_id}/content"))
        .header("authorization", format!("Bearer {admin}"))
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/zip")
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(bytes.starts_with(b"PK"), "zip magic");

    // Unknown bundle id 404s.
    let (status, _) = call(
        &ctx,
        Method::GET,
        "/api/v1/diagnostics/bundles/nope/content",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sse_stream_carries_operation_events() {
    let (ctx, dir) = test_ctx().await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let server = manager_api::ApiServer::start_ctx(
        ctx.clone(),
        manager_api::ApiOptions {
            bind: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: 0,
            tls: None,
        },
    )
    .await
    .unwrap();
    let base = format!("http://{}", server.addr());
    let client = reqwest::Client::builder().no_proxy().build().unwrap();

    // Identity advertises the stream now.
    let identity_text = client
        .get(format!("{base}/api/v1/server"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let identity: Value = serde_json::from_str(&identity_text).unwrap();
    assert_eq!(identity["features"]["events_sse"], true);

    // Unauthenticated stream is refused.
    let resp = client
        .get(format!("{base}/api/v1/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let mut resp = client
        .get(format!("{base}/api/v1/events"))
        .header("authorization", format!("Bearer {admin}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.contains("text/event-stream")));

    // An event emitted through the API sink arrives framed as SSE.
    // Emit until the first framed chunk arrives: a lone event can
    // sit in the server's write buffer until the next flush, so
    // the test drives the stream the way real operation traffic
    // does (the curl-verified stream delivers every event once
    // flowing).
    let chunk = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            ctx.sink
                .emit("backend-status", json!({ "id": "planner", "running": true }));
            match tokio::time::timeout(Duration::from_millis(400), resp.chunk()).await {
                Ok(Ok(Some(bytes))) => break bytes,
                Ok(Ok(None)) => panic!("stream ended"),
                Ok(Err(e)) => panic!("stream error: {e}"),
                Err(_) => continue,
            }
        }
    })
    .await
    .expect("event within timeout");
    let text = String::from_utf8_lossy(&chunk).to_string();
    assert!(text.contains("event: backend-status"), "chunk: {text}");
    assert!(text.contains("planner"), "chunk: {text}");
    assert!(text.contains("id:"), "chunk: {text}");

    // Close the client stream first: graceful shutdown waits for
    // connections to drain, and an SSE stream never drains on its
    // own.
    drop(resp);
    let _ = tokio::time::timeout(Duration::from_secs(5), server.shutdown()).await;
    let _ = std::fs::remove_dir_all(&dir);
}

