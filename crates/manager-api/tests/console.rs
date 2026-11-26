//! Debug console over REST: the API layer fills `caller` from the
//! authenticated principal and the peer address (a client-supplied
//! `caller` is overwritten), process-running commands are refused for
//! non-loopback peers, reads are not, and the audit log names the
//! principal.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use manager_api::auth::MemoryCredentialStore;
use manager_api::users::{RoleName, UserStore};
use manager_api::{build_router, ApiCtx, PeerAddr};
use manager_app::AppState;
use serde_json::json;
use tower::ServiceExt;

async fn token(ctx: &Arc<ApiCtx>, role: RoleName, name: &str) -> String {
    static TEST_HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let hash = TEST_HASH
        .get_or_init(|| manager_api::users::hash_password("a-unique-passphrase").unwrap())
        .clone();
    let user_id = format!("u-{}", manager_api::users::generate_token());
    let user = manager_api::users::User {
        id: user_id.clone(),
        username: name.into(),
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

fn test_ctx(tag: &str) -> (Arc<ApiCtx>, std::path::PathBuf) {
    let dir =
        std::env::temp_dir().join(format!("manager-api-console-{tag}-{}", std::process::id()));
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

fn post(uri: &str, token: &str, body: serde_json::Value, peer: Option<&str>) -> Request<Body> {
    let mut req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    if let Some(p) = peer {
        let addr: SocketAddr = p.parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(PeerAddr(addr)));
    }
    req
}

async fn send(ctx: &Arc<ApiCtx>, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let (router, _) = build_router(ctx.clone());
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn remote_peers_cannot_run_processes_even_with_a_spoofed_caller() {
    let (ctx, dir) = test_ctx("remote");
    let t = token(&ctx, RoleName::Admin, "claude").await;
    // A client-supplied caller claiming to be the local desktop is
    // overwritten by the API layer.
    let spoof = json!({ "kind": "ui", "loopback": true, "role": "admin", "actor": "desktop" });
    let (s, v) = send(
        &ctx,
        post(
            "/api/v1/console/runs",
            &t,
            json!({ "recipe": "nvidia-smi-snapshot", "caller": spoof }),
            Some("10.1.2.3:5555"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"]["code"], "FORBIDDEN");
    let (s, _) = send(
        &ctx,
        post(
            "/api/v1/console/exec",
            &t,
            json!({ "line": "stop planner" }),
            Some("10.1.2.3:5555"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Reads work from anywhere.
    let (s, v) = send(
        &ctx,
        post(
            "/api/v1/console/exec",
            &t,
            json!({ "line": "recipes" }),
            Some("10.1.2.3:5555"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["ok"], true);
    assert!(v["text"].as_str().unwrap().contains("llama-bench-depth"));
    let audit = std::fs::read_to_string(dir.join("logs").join("admin-api-audit.jsonl")).unwrap();
    let refused = audit
        .lines()
        .find(|l| l.contains("\"action\":\"console.exec\"") && l.contains("refused: non-loopback"))
        .expect("refusal audited");
    assert!(
        refused.contains("\"actor\":\"claude\"") && refused.contains("10.1.2.3"),
        "{refused}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn loopback_peers_pass_the_gate_and_roles_still_apply() {
    let (ctx, dir) = test_ctx("loopback");
    let admin = token(&ctx, RoleName::Admin, "claude").await;
    // Past the gate: an unknown backend is a command error (200, ok:false), not a 403.
    let (s, v) = send(
        &ctx,
        post(
            "/api/v1/console/exec",
            &admin,
            json!({ "line": "stop no-such-backend" }),
            Some("127.0.0.1:5555"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().contains("no-such-backend"));
    // A viewer may read but not run.
    let viewer = token(&ctx, RoleName::Viewer, "ro").await;
    let (s, _) = send(
        &ctx,
        post(
            "/api/v1/console/exec",
            &viewer,
            json!({ "line": "run nvidia-smi-snapshot" }),
            Some("127.0.0.1:5555"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = send(
        &ctx,
        post(
            "/api/v1/console/exec",
            &viewer,
            json!({ "line": "help" }),
            Some("127.0.0.1:5555"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // The raw shell stays off for the API.
    let (s, _) = send(
        &ctx,
        post(
            "/api/v1/console/shell/s1",
            &admin,
            json!({}),
            Some("127.0.0.1:5555"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Import parses (never runs) and returns the reviewed draft.
    let (s, v) = send(
        &ctx,
        post("/api/v1/console/import", &admin, json!({ "text": "$env:CUDA_LAUNCH_BLOCKING=1\nnvidia-smi -q -d MEMORY\nnvidia-smi -q -d UTILIZATION" }), Some("127.0.0.1:5555")),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["recipe"]["steps"][0]["variants"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let _ = std::fs::remove_dir_all(&dir);
}
