//! Regression tests for the defects and gaps listed in
//! docs/testing/integration-test-api.md (sections 16 and 17), over the
//! router.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use manager_api::auth::MemoryCredentialStore;
use manager_api::users::RoleName;
use manager_api::{build_router, ApiCtx, UserStore};
use manager_app::AppState;

async fn test_ctx(
    edit: impl FnOnce(&mut manager_config::AppConfig),
) -> (Arc<ApiCtx>, std::path::PathBuf) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("llm-api-defects-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = manager_config::AppConfig::default_config();
    cfg.data_dir = dir.clone();
    // Nothing answers here, whatever runs on the developer's machine.
    cfg.gateway.port = 40099;
    edit(&mut cfg);
    let app = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
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
        last_totp_step: None,
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

async fn call_text(
    ctx: &Arc<ApiCtx>,
    method: Method,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, String, String) {
    let (router, _) = build_router(ctx.clone());
    let req = Request::builder()
        .method(method)
        .uri(format!("/api/v1{path}"))
        .header("authorization", format!("Bearer {token}"));
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, ctype, String::from_utf8_lossy(&bytes).into_owned())
}

async fn call(
    ctx: &Arc<ApiCtx>,
    method: Method,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, _, text) = call_text(ctx, method, path, token, body).await;
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

#[track_caller]
fn assert_error(got: &(StatusCode, Value), status: StatusCode, code: &str) {
    assert_eq!(got.0, status, "{}", got.1);
    assert_eq!(got.1["error"]["code"], code, "{}", got.1);
}

/// Poll an operation until it leaves the running state.
async fn poll_terminal(ctx: &Arc<ApiCtx>, token: &str, id: &str) -> Value {
    for _ in 0..200 {
        let path = format!("/operations/{id}");
        let (status, record) = call(ctx, Method::GET, &path, token, None).await;
        assert_eq!(status, StatusCode::OK);
        if record["status"] != "running" {
            return record;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("operation {id} never finished");
}

/// A port that accepts connections and never answers.
fn silent_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming() {
            held.push(s);
        }
    });
    port
}

// ---------------------------------------------------------------------------
// Arguments: D03, D05, D20, G22
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_values_follow_the_parameter_type() {
    let (ctx, dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let logs = dir.join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join("wizard.log"),
        "[2026-01-01T00:00:00Z] port 8080\n\
         [2026-01-01T00:00:01Z] an inf line\n\
         [2026-01-01T00:00:02Z] other\n",
    )
    .unwrap();
    for (search, hits) in [("8080", 1), ("inf", 1), ("true", 0), ("nan", 0)] {
        let path = format!("/logs?scope=wizard&search={search}");
        let (status, v) = call(&ctx, Method::GET, &path, &admin, None).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["total"], hits, "search={search}: {v}");
    }
    for bad in ["limit=abc", "limit=-1", "limit=1.5", "since_ms=x"] {
        let path = format!("/logs?scope=wizard&{bad}");
        let got = call(&ctx, Method::GET, &path, &admin, None).await;
        assert_error(&got, StatusCode::BAD_REQUEST, "INVALID_INPUT");
    }
}

#[tokio::test]
async fn unknown_log_level_and_wrong_routing_group_are_400() {
    let (ctx, _dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let got = call(
        &ctx,
        Method::GET,
        "/logs?scope=all&level=loud",
        &admin,
        None,
    )
    .await;
    assert_error(&got, StatusCode::BAD_REQUEST, "INVALID_INPUT");
    let (status, _) = call(
        &ctx,
        Method::GET,
        "/logs?scope=all&level=warn",
        &admin,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for body in [json!({ "group": 5 }), json!({}), json!([])] {
        let path = "/gateway/routing/test";
        let got = call(&ctx, Method::POST, path, &admin, Some(body)).await;
        assert_error(&got, StatusCode::BAD_REQUEST, "INVALID_INPUT");
    }
}

#[tokio::test]
async fn async_routes_refuse_bad_arguments_before_queueing() {
    let (ctx, _dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let post = |path: &'static str, body: Value| {
        let (ctx, admin) = (ctx.clone(), admin.clone());
        async move { call(&ctx, Method::POST, path, &admin, Some(body)).await }
    };
    // Missing or wrongly typed arguments.
    for (path, body) in [
        ("/backends/planner/stop", json!({})),
        ("/backends/planner/stop", json!({ "graceful": "yes" })),
        ("/backends/planner/restart", json!({})),
        (
            "/backends/planner/start",
            json!({ "wait_ready_secs": "soon" }),
        ),
        ("/backends/planner/start", json!({ "wait_ready_secs": -1 })),
        ("/gateway/restart", json!({ "drain": 1 })),
        ("/models/no-pin/verify", json!({ "bogus": 1 })),
    ] {
        let got = post(path, body).await;
        assert_error(&got, StatusCode::BAD_REQUEST, "INVALID_INPUT");
    }
    // Unknown ids.
    for (path, body) in [
        ("/backends/ghost/start", json!({})),
        ("/backends/ghost/stop", json!({ "graceful": true })),
        ("/backends/ghost/restart", json!({ "graceful": true })),
        ("/models/ghost/download", json!({})),
        ("/models/ghost/download/resume", json!({})),
        ("/models/ghost/verify", json!({})),
        ("/setup/steps/ghost/run", json!({})),
        ("/setup/steps/ghost/retry", json!({})),
    ] {
        let got = post(path, body).await;
        assert_error(&got, StatusCode::NOT_FOUND, "NOT_FOUND");
    }
    let (_, list) = call(&ctx, Method::GET, "/operations", &admin, None).await;
    assert_eq!(list, json!([]), "nothing was queued");
}

#[tokio::test]
async fn a_failed_operation_carries_a_code() {
    let (ctx, _dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    // The model file does not exist: the start preflight refuses.
    let (status, record) = call(
        &ctx,
        Method::POST,
        "/backends/planner/start",
        &admin,
        Some(json!({ "wait_ready_secs": 5 })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{record}");
    assert!(record.get("error_code").is_some_and(Value::is_null));
    let done = poll_terminal(&ctx, &admin, record["id"].as_str().unwrap()).await;
    assert_eq!(done["status"], "failed", "{done}");
    let code = done["error_code"].as_str().expect("a code");
    assert!(
        ["SUPERVISOR", "INVALID_INPUT", "CONFIG"].contains(&code),
        "{done}"
    );
    assert!(done["error_kind"].is_string(), "{done}");
    assert!(done["error"].is_string(), "{done}");
}

// ---------------------------------------------------------------------------
// Operations: D02, D10, D18, D19
// ---------------------------------------------------------------------------

#[tokio::test]
async fn operations_are_newest_first_and_linked_to_their_task() {
    let (ctx, _dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let mut ids = Vec::new();
    for id in ["planner", "coder"] {
        let (status, record) = call(
            &ctx,
            Method::POST,
            &format!("/backends/{id}/stop"),
            &admin,
            Some(json!({ "graceful": true })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{record}");
        let op = record["id"].as_str().unwrap().to_string();
        let done = poll_terminal(&ctx, &admin, &op).await;
        assert_eq!(done["status"], "done", "{done}");
        ids.push(op);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let (_, list) = call(&ctx, Method::GET, "/operations", &admin, None).await;
    let rows = list.as_array().unwrap();
    let created: Vec<i64> = rows
        .iter()
        .map(|r| r["created_ms"].as_i64().unwrap())
        .collect();
    assert!(created.windows(2).all(|w| w[0] >= w[1]), "{list}");
    assert_eq!(rows.len(), 4, "{list}");
    assert!(
        rows[..2].iter().any(|r| r["id"] == ids[1].as_str()),
        "{list}"
    );
    for op in &ids {
        let api = rows.iter().find(|r| r["id"] == op.as_str()).unwrap();
        let task_id = api["task_id"].as_str().expect("linked task");
        let task = rows.iter().find(|r| r["id"] == task_id).unwrap();
        assert_eq!(task["kind"], "backend_stop");
        assert_eq!(task["operation_id"], op.as_str());
        for row in [api, task] {
            assert!(row["started_ms"].is_number(), "{row}");
            assert!(row["finished_ms"].is_number(), "{row}");
            assert!(row.get("error").is_some_and(Value::is_null), "{row}");
        }
        // Get-one shows the same row.
        let path = format!("/operations/{task_id}");
        let (_, one) = call(&ctx, Method::GET, &path, &admin, None).await;
        assert_eq!(&one, task);
    }
}

#[tokio::test]
async fn aborting_a_finished_operation_changes_nothing() {
    let (ctx, _dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let (_, record) = call(
        &ctx,
        Method::POST,
        "/backends/planner/stop",
        &admin,
        Some(json!({ "graceful": true })),
    )
    .await;
    let op = record["id"].as_str().unwrap().to_string();
    let done = poll_terminal(&ctx, &admin, &op).await;
    let path = format!("/operations/{op}/abort");
    let (status, v) = call(&ctx, Method::POST, &path, &admin, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["ok"], true);
    assert_eq!(v["status"], "done", "{v}");
    assert_eq!(v["already_finished"], true, "{v}");
    let path = format!("/operations/{op}");
    let (_, after) = call(&ctx, Method::GET, &path, &admin, None).await;
    assert_eq!(after, done);
}

/// A download: the operation runs until the transfer ended, and its
/// `202` id aborts the app's own task.
#[tokio::test]
async fn a_download_runs_until_it_ended_and_aborts_by_its_202_id() {
    let port = silent_port();
    let (ctx, _dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let (status, v) = call(
        &ctx,
        Method::POST,
        "/models",
        &admin,
        Some(json!({
            "id": "qa-slow",
            "source_url": format!("https://127.0.0.1:{port}/qa-slow.gguf"),
            "params_b": 1.0,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let start = "/models/qa-slow/download";
    let (status, record) = call(&ctx, Method::POST, start, &admin, Some(json!({}))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{record}");
    let op = record["id"].as_str().unwrap().to_string();
    let op_path = format!("/operations/{op}");
    let mut task_id = Value::Null;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_, r) = call(&ctx, Method::GET, &op_path, &admin, None).await;
        assert_eq!(r["status"], "running", "the transfer has not ended: {r}");
        task_id = r["task_id"].clone();
        if task_id.is_string() {
            break;
        }
    }
    let task_id = task_id.as_str().expect("linked task").to_string();
    // A second download of the same model is refused at once.
    let got = call(&ctx, Method::POST, start, &admin, Some(json!({}))).await;
    assert_error(&got, StatusCode::CONFLICT, "BUSY");

    let path = format!("/operations/{op}/abort");
    let (status, v) = call(&ctx, Method::POST, &path, &admin, None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "aborted", "{v}");
    for id in [&op, &task_id] {
        let r = poll_terminal(&ctx, &admin, id).await;
        assert_eq!(r["status"], "aborted", "{r}");
        assert!(r["finished_ms"].is_number(), "{r}");
    }
    let (_, m) = call(&ctx, Method::GET, "/models/qa-slow", &admin, None).await;
    assert_eq!(m["downloading"], false, "{m}");
}

#[tokio::test]
async fn only_admins_see_local_paths_in_operation_details() {
    let (ctx, dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let viewer = seed_token(&ctx, "look", RoleName::Viewer).await;
    let (_, record) = call(&ctx, Method::POST, "/diagnostics/bundles", &admin, None).await;
    let op = record["id"].as_str().unwrap().to_string();
    let done = poll_terminal(&ctx, &admin, &op).await;
    let root = dir.display().to_string();
    assert!(
        done["detail"]["path"].as_str().unwrap().contains(&root),
        "{done}"
    );
    let (_, list) = call(&ctx, Method::GET, "/operations", &viewer, None).await;
    let path = format!("/operations/{op}");
    let (_, one) = call(&ctx, Method::GET, &path, &viewer, None).await;
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == op.as_str())
        .unwrap()
        .clone();
    for v in [row, one] {
        let path = v["detail"]["path"].as_str().unwrap();
        assert!(path.starts_with("<data-dir>"), "{v}");
    }
}

// ---------------------------------------------------------------------------
// Configuration: D01, D06, D07, D08, D09, D11, D14
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restart_now_is_refused_while_stopped() {
    let (ctx, _dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let path = "/backends/planner/config";
    let (_, before) = call(&ctx, Method::GET, path, &admin, None).await;
    let body = json!({ "patch": { "n_ctx": 8192 }, "apply_mode": "RestartNow" });
    let got = call(&ctx, Method::PATCH, path, &admin, Some(body)).await;
    assert_error(&got, StatusCode::BAD_REQUEST, "INVALID_INPUT");
    let (_, after) = call(&ctx, Method::GET, path, &admin, None).await;
    assert_eq!(after, before, "nothing was saved");
    let (_, list) = call(&ctx, Method::GET, "/operations", &admin, None).await;
    assert_eq!(list, json!([]), "no task stays behind");
}

#[tokio::test]
async fn backend_patches_are_closed_and_checked() {
    let (ctx, _dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let patch_backend = |id: &'static str, patch: Value, mode: &'static str| {
        let (ctx, admin) = (ctx.clone(), admin.clone());
        async move {
            call(
                &ctx,
                Method::PATCH,
                &format!("/backends/{id}/config"),
                &admin,
                Some(json!({ "patch": patch, "apply_mode": mode })),
            )
            .await
        }
    };
    for patch in [
        json!({ "n_ctxx": 8192 }),
        json!({ "n_ctx": "8192" }),
        json!({ "n_ctx": -1 }),
        json!({ "n_ctx": 10 }),
        json!({ "n_ctx": 2_000_000 }),
        json!({ "n_batch": 0 }),
        json!({ "n_ctx": 2048, "n_batch": 4096 }),
        json!({ "port": 70000 }),
        json!({ "split_mode": "diagonal" }),
        json!({ "raw_flags": 5 }),
        json!("n_ctx"),
    ] {
        let got = patch_backend("planner", patch.clone(), "OnNextRestart").await;
        assert_eq!(got.0, StatusCode::BAD_REQUEST, "{patch}: {}", got.1);
        assert_error(&got, StatusCode::BAD_REQUEST, "INVALID_INPUT");
        let got = call(
            &ctx,
            Method::POST,
            "/backends/planner/config/preview-diff",
            &admin,
            Some(json!({ "patch": patch })),
        )
        .await;
        assert_eq!(got.0, StatusCode::BAD_REQUEST, "{patch}: {}", got.1);
    }
    let got = patch_backend("ghost", json!({ "n_ctx": 8192 }), "OnNextRestart").await;
    assert_error(&got, StatusCode::NOT_FOUND, "NOT_FOUND");
    let got = patch_backend("planner", json!({}), "Later").await;
    assert_error(&got, StatusCode::BAD_REQUEST, "INVALID_INPUT");

    // null clears the raw flags; other nulls leave the field alone.
    for (patch, want) in [
        (json!({ "raw_flags": "--threads 4" }), json!("--threads 4")),
        (json!({ "port": null }), json!("--threads 4")),
        (json!({ "raw_flags": null }), Value::Null),
    ] {
        let (status, v) = patch_backend("planner", patch, "OnNextRestart").await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let path = "/backends/planner/config";
        let (_, cfg) = call(&ctx, Method::GET, path, &admin, None).await;
        assert_eq!(cfg["raw_flags"], want, "{cfg}");
    }
}

#[tokio::test]
async fn gateway_patches_are_closed_and_checked() {
    let (ctx, _dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let path = "/gateway/config";
    let (_, before) = call(&ctx, Method::GET, path, &admin, None).await;
    for patch in [
        json!({ "request_log_length": 5 }),
        json!({ "typo": null }),
        json!({ "openrouter": { "enabld": true } }),
        json!({ "openrouter": { "overflow": { "queue": 1 } } }),
        json!({ "request_log_len": "many" }),
        json!({ "request_log_len": -1 }),
        json!({ "openrouter": { "enabled": "yes" } }),
        json!({ "groups": "none" }),
        json!({ "groups": [{
            "name": "x", "members": [], "strategy": "simple-shuffle",
            "fallbacks": [], "colour": 1,
        }] }),
        json!({ "effective_strategy": "least-busy" }),
        json!({ "bind_host": 5 }),
        // A bad field beside bind_host saves neither.
        json!({ "bind_host": "127.0.0.1", "typo": 1 }),
        json!([]),
    ] {
        let got = call(&ctx, Method::PATCH, path, &admin, Some(patch.clone())).await;
        assert_eq!(got.0, StatusCode::BAD_REQUEST, "{patch}: {}", got.1);
        assert_error(&got, StatusCode::BAD_REQUEST, "INVALID_INPUT");
    }
    let (_, after) = call(&ctx, Method::GET, path, &admin, None).await;
    assert_eq!(after, before);
    // What the read gives can be written back (computed group fields).
    let patch = json!({ "groups": before["groups"], "request_log_len": 50 });
    let (status, v) = call(&ctx, Method::PATCH, path, &admin, Some(patch)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn groups_follow_the_apps_rules_and_are_journaled_by_name() {
    let (ctx, dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    // Seeds the catalog fleet, so groups render into the gateway config.
    let _ = call(&ctx, Method::GET, "/models", &admin, None).await;
    let post = |body: Value| {
        let (ctx, admin) = (ctx.clone(), admin.clone());
        async move { call(&ctx, Method::POST, "/gateway/groups", &admin, Some(body)).await }
    };
    let long = "g".repeat(63);
    for name in ["Has Spaces", "openrouter", "", "-x", &long] {
        for body in [
            json!({ "name": name }),
            json!({ "name": name, "members": ["planner"] }),
        ] {
            let got = post(body).await;
            assert_error(&got, StatusCode::BAD_REQUEST, "INVALID_INPUT");
        }
    }
    let (status, v) = post(json!({ "name": "qa-empty" })).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["name"], "qa-empty");
    let got = post(json!({ "name": "qa-empty" })).await;
    assert_error(&got, StatusCode::BAD_REQUEST, "INVALID_INPUT");
    let (status, v) = post(json!({ "name": "qa-full", "members": ["planner"] })).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let (status, v) = call(
        &ctx,
        Method::PATCH,
        "/gateway/groups/qa-full",
        &admin,
        Some(json!({ "members": ["planner", "coder"], "strategy": "least-busy" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");

    let del = |path: &'static str| {
        let (ctx, admin) = (ctx.clone(), admin.clone());
        async move { call(&ctx, Method::DELETE, path, &admin, None).await }
    };
    let (status, _) = del("/gateway/groups/qa-empty").await;
    assert_eq!(status, StatusCode::OK);
    let got = del("/gateway/groups/qa-empty").await;
    assert_error(&got, StatusCode::NOT_FOUND, "NOT_FOUND");
    let got = del("/gateway/groups/qa-full").await;
    assert_error(&got, StatusCode::BAD_REQUEST, "INVALID_INPUT");
    let got = del("/gateway/groups/qa-full?cascade=maybe").await;
    assert_error(&got, StatusCode::BAD_REQUEST, "INVALID_INPUT");
    let (status, v) = del("/gateway/groups/qa-full?cascade=true").await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["deleted"], "qa-full");
    let got = del("/gateway/groups/qa-full?cascade=true").await;
    assert_error(&got, StatusCode::NOT_FOUND, "NOT_FOUND");

    let journal: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("activation-journal.json")).unwrap_or_default(),
    )
    .unwrap_or_default();
    let rows: Vec<(&str, Vec<&str>)> = journal["entries"]
        .as_array()
        .expect("journal entries")
        .iter()
        .map(|e| {
            let targets = e["targets"].as_array().unwrap();
            (
                e["summary"].as_str().unwrap(),
                targets.iter().filter_map(Value::as_str).collect(),
            )
        })
        .collect();
    assert!(!rows.is_empty());
    for (summary, targets) in &rows {
        assert!(summary.contains("`qa-"), "{rows:?}");
        assert!(targets.contains(&"planner"), "{rows:?}");
    }
    assert!(
        rows.iter()
            .any(|(s, t)| s.contains("members") && s.contains("strategy") && t.contains(&"coder")),
        "{rows:?}"
    );
}

#[tokio::test]
async fn telemetry_interval_is_bounded() {
    let (ctx, _dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let put = |ms: Value| {
        let (ctx, admin) = (ctx.clone(), admin.clone());
        async move {
            let body = Some(json!({ "ms": ms }));
            call(&ctx, Method::PUT, "/telemetry/interval", &admin, body).await
        }
    };
    for bad in [
        json!(1),
        json!(499),
        json!(60_001),
        json!(u64::MAX),
        json!("1"),
    ] {
        let got = put(bad).await;
        assert_error(&got, StatusCode::BAD_REQUEST, "INVALID_INPUT");
    }
    for good in [0, 500, 1000, 5000, 60_000] {
        let (status, v) = put(json!(good)).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["poll_interval_ms"], good);
    }
}

#[tokio::test]
async fn a_backend_test_does_not_block_saves() {
    let port = silent_port();
    let (ctx, _dir) = test_ctx(|c| {
        let planner = c.backends.iter_mut().find(|b| b.id == "planner");
        planner.unwrap().port = port;
    })
    .await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let (ctx2, admin2) = (ctx.clone(), admin.clone());
    let test = tokio::spawn(async move {
        let body = Some(json!({ "prompt": "hi" }));
        call(&ctx2, Method::POST, "/backends/planner/test", &admin2, body).await
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!test.is_finished(), "the model is still answering");
    let started = Instant::now();
    let body = Some(json!({ "ms": 1000 }));
    let (status, _) = call(&ctx, Method::PUT, "/telemetry/interval", &admin, body).await;
    assert_eq!(status, StatusCode::OK);
    let took = started.elapsed();
    assert!(took < Duration::from_secs(5), "{took:?}");
    test.abort();
}

// ---------------------------------------------------------------------------
// Gaps: G17, G18, G19, G20
// ---------------------------------------------------------------------------

#[tokio::test]
async fn generated_routing_file_is_readable() {
    let (ctx, _dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let viewer = seed_token(&ctx, "look", RoleName::Viewer).await;
    let path = "/gateway/generated-routing";
    // Seeds the catalog fleet: without a group nothing is generated.
    let _ = call(&ctx, Method::GET, "/models", &admin, None).await;
    let got = call(&ctx, Method::GET, path, &viewer, None).await;
    assert_error(&got, StatusCode::NOT_FOUND, "NOT_FOUND");
    let rule = json!({
        "id": "qa-rule", "description": "qa", "enabled": true, "source": "user",
        "kind": { "type": "context-overflow", "target_group": "planner" },
    });
    let patch = Some(json!({ "routing_rules": [rule] }));
    let (status, v) = call(&ctx, Method::PATCH, "/gateway/config", &admin, patch).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (status, ctype, text) = call_text(&ctx, Method::GET, path, &viewer, None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(ctype.starts_with("text/x-python"), "{ctype}");
    assert!(text.contains("planner"), "{text}");
}

#[tokio::test]
async fn last_update_check_and_mxc_runtime_are_readable() {
    let (ctx, _dir) = test_ctx(|_| ()).await;
    let viewer = seed_token(&ctx, "look", RoleName::Viewer).await;
    let (status, v) = call(&ctx, Method::GET, "/updates/components", &viewer, None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert!(v.get("checked_at").is_some(), "{v}");
    let (status, v) = call(&ctx, Method::GET, "/mxc/policy", &viewer, None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["runtime_installed"], false, "{v}");
}

#[tokio::test]
async fn snapshots_can_be_listed_read_and_deleted() {
    let (ctx, dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let operator = seed_token(&ctx, "op", RoleName::Operator).await;
    let viewer = seed_token(&ctx, "look", RoleName::Viewer).await;
    std::fs::write(dir.join("telemetry-snapshot-1700000000000.json"), "[]").unwrap();
    std::fs::write(dir.join("telemetry-snapshot-x.json"), "[]").unwrap();
    let base = "/telemetry/snapshots";
    let (status, saved) = call(&ctx, Method::POST, base, &operator, None).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let id = saved["id"].as_str().expect("an id").to_string();
    assert!(
        saved["path"].as_str().unwrap().starts_with("<data-dir>"),
        "{saved}"
    );
    let (status, list) = call(&ctx, Method::GET, base, &viewer, None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let rows = list["snapshots"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{list}");
    assert_eq!(rows[0]["id"], id.as_str(), "newest first: {list}");
    assert_eq!(rows[1]["created_ms"], 1_700_000_000_000_i64);
    assert!(!list.to_string().contains("telemetry-snapshot"), "{list}");
    let path = format!("{base}/1700000000000");
    let (status, one) = call(&ctx, Method::GET, &path, &viewer, None).await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(one["samples"], json!([]));
    for bad in ["nope", "..%2F..%2Fconfig", "x", "1"] {
        let path = format!("{base}/{bad}");
        let got = call(&ctx, Method::GET, &path, &viewer, None).await;
        assert_error(&got, StatusCode::NOT_FOUND, "NOT_FOUND");
    }
    let path = format!("{base}/{id}");
    let got = call(&ctx, Method::DELETE, &path, &operator, None).await;
    assert_error(&got, StatusCode::FORBIDDEN, "FORBIDDEN");
    let (status, v) = call(&ctx, Method::DELETE, &path, &admin, None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let got = call(&ctx, Method::DELETE, &path, &admin, None).await;
    assert_error(&got, StatusCode::NOT_FOUND, "NOT_FOUND");
}

// ---------------------------------------------------------------------------
// Audit: refusals and disclosures (auth-plan.md sections 4.3 and 6)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn refusals_and_disclosures_are_audited() {
    let (ctx, _dir) = test_ctx(|_| ()).await;
    let admin = seed_token(&ctx, "root", RoleName::Admin).await;
    let viewer = seed_token(&ctx, "look", RoleName::Viewer).await;
    for (method, path, token) in [
        (Method::DELETE, "/backends/planner", &viewer),
        (Method::GET, "/admin/users", &viewer),
        (Method::GET, "/backends/planner/launch-command", &admin),
        (Method::GET, "/diagnostics/bundles/nope/content", &admin),
        (Method::GET, "/status", &viewer),
    ] {
        let _ = call(&ctx, method, path, token, None).await;
    }
    let rows: Vec<(String, String, String)> = ctx
        .audit
        .read_recent(0, 100)
        .into_iter()
        .rev()
        .map(|e| {
            let s = |k: &str| e[k].as_str().unwrap_or("").to_string();
            (s("actor"), s("action"), s("result"))
        })
        .collect();
    let want = [
        ("look", "DELETE /api/v1/backends/{id}", Some("403")),
        ("look", "GET /api/v1/admin/users", Some("403")),
        ("root", "GET /api/v1/backends/{id}/launch-command", None),
        (
            "root",
            "GET /api/v1/diagnostics/bundles/{bundle_id}/content",
            Some("404"),
        ),
    ];
    assert_eq!(
        rows.len(),
        want.len(),
        "plain reads are not audited: {rows:?}"
    );
    for (row, (actor, action, result)) in rows.iter().zip(want) {
        assert_eq!(row.0, actor, "{rows:?}");
        assert_eq!(row.1, action, "{rows:?}");
        assert!(result.is_none_or(|r| row.2 == r), "{rows:?}");
    }
}
