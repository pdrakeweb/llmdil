//! The routes added when the v1 exclusions were lifted (rest-api.md
//! §6.15, §6.19–§6.21): status codes, body rules, what a payload may
//! not contain, and that the event stream and the audit log see the
//! mutations.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use manager_api::auth::MemoryCredentialStore;
use manager_api::users::{now_ms, RoleName, User, UserStore};
use manager_api::{build_router, ApiCtx};
use manager_app::AppState;
use serde_json::{json, Value};
use tower::ServiceExt;

struct Api {
    ctx: Arc<ApiCtx>,
    router: axum::Router,
    token: String,
    dir: std::path::PathBuf,
}

impl Api {
    async fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("manager-api-lifted-{}-{n}", std::process::id()));
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
        ctx.users
            .mutate::<_, manager_api::users::StoreError>(|f| {
                f.users.push(User {
                    id: "u-root".into(),
                    username: "root".into(),
                    password_hash: String::new(),
                    role: RoleName::Admin,
                    disabled: false,
                    must_change_password: false,
                    totp_enabled: false,
                    recovery_code_hashes: vec![],
                    last_totp_step: None,
                    created_ms: now_ms(),
                    updated_ms: now_ms(),
                });
                Ok(())
            })
            .await
            .unwrap();
        let token = ctx
            .auth
            .create_session("u-root", "test")
            .await
            .unwrap()
            .access_token;
        let (router, _) = build_router(ctx.clone());
        Api {
            ctx,
            router,
            token,
            dir,
        }
    }

    async fn call(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let body = if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        };
        let req = Request::builder()
            .method(method)
            .uri(format!("/api/v1{path}"))
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-type", "application/json")
            .body(body)
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    /// Poll an operation to its end.
    async fn settled(&self, id: &str) -> Value {
        for _ in 0..600 {
            let (_, v) = self
                .call("GET", &format!("/operations/{id}"), Value::Null)
                .await;
            if v["status"] != "running" {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("operation {id} did not settle");
    }

    async fn sample_plan(&self) {
        let (st, op) = self
            .call("POST", "/optimize/consult", json!({ "sample": true }))
            .await;
        assert_eq!(st, StatusCode::ACCEPTED);
        let done = self.settled(op["id"].as_str().unwrap()).await;
        assert_eq!(done["status"], "done", "{done}");
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn reads_answer_without_local_paths_or_the_windows_account() {
    let api = Api::new().await;
    for path in [
        "/mcp/servers",
        "/mcp/preflight",
        "/services",
        "/log-receiver",
        "/optimize",
        "/bench/prompts",
        "/bench/results",
    ] {
        let (st, v) = api.call("GET", path, Value::Null).await;
        assert_eq!(st, StatusCode::OK, "GET {path}: {v}");
        assert!(
            !v.to_string()
                .contains(&*api.dir.file_name().unwrap().to_string_lossy()),
            "GET {path} names the data dir"
        );
    }
    let (_, v) = api.call("GET", "/services", Value::Null).await;
    assert!(v.get("account").is_none() && v.get("host_path").is_none());
    assert_eq!(v["mode"], "process");
    let (_, v) = api.call("GET", "/log-receiver", Value::Null).await;
    assert!(v.get("data_dir").is_none() && v.get("receiver_exe").is_none());
    assert!(v["lifecycle"].is_string());
}

#[tokio::test]
async fn settings_sections_are_closed_audited_and_announced() {
    let api = Api::new().await;
    let mut events = manager_app::event_bus::subscribe(&api.ctx.app);
    let (st, v) = api
        .call("PUT", "/settings/supervision", json!({ "mode": "service" }))
        .await;
    assert_eq!((st, &v["mode"]), (StatusCode::OK, &json!("service")));
    let changed = loop {
        let e = events.recv().await.unwrap();
        if e.event == "config-changed" {
            break e;
        }
    };
    assert_eq!(changed.payload["sections"], json!(["supervision"]));
    assert_eq!(changed.origin, manager_app::event_bus::EventOrigin::Api);
    let (st, _) = api
        .call("PUT", "/settings/supervision", json!({ "mode": "other" }))
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let prefs =
        json!({ "enabled": true, "operations": false, "failures": true, "activation": true });
    let (st, _) = api.call("PUT", "/notifications", prefs).await;
    assert_eq!(st, StatusCode::OK);
    let (_, cfg) = api.call("GET", "/config", Value::Null).await;
    assert_eq!(cfg["notifications"]["operations"], false);
    let (st, v) = api
        .call(
            "PUT",
            "/notifications",
            json!({ "enabled": true, "operation": true }),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("operation"));
    // No desktop shell in this process: the test says so.
    let (st, v) = api.call("POST", "/notifications/test", Value::Null).await;
    assert_eq!(st, StatusCode::NOT_IMPLEMENTED, "{v}");

    let audited: Vec<(String, String)> = api
        .ctx
        .audit
        .read_recent(0, 20)
        .iter()
        .map(|e| {
            (
                e["action"].as_str().unwrap().to_string(),
                e["result"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    for (action, result) in [
        ("PUT /api/v1/settings/supervision", "200"),
        ("PUT /api/v1/settings/supervision", "400"),
        ("PUT /api/v1/notifications", "200"),
        ("POST /api/v1/notifications/test", "501"),
    ] {
        assert!(
            audited.contains(&(action.to_string(), result.to_string())),
            "{action} {result}: {audited:?}"
        );
    }
}

#[tokio::test]
async fn previews_take_an_unsaved_position_and_unknown_fields_are_refused() {
    let api = Api::new().await;
    let (st, v) = api
        .call("POST", "/backends/preview", json!({ "model_id": "nope" }))
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
    let (st, _) = api
        .call(
            "POST",
            "/backends/preview",
            json!({ "model_id": "nope", "offload": 3 }),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = api
        .call(
            "POST",
            "/backends/nope/cache/preview",
            json!({ "offload_layers": 3, "gpu_indices": [0] }),
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// Path ids the operation declares as integers arrive as integers.
#[tokio::test]
async fn plan_items_are_addressed_by_number() {
    let api = Api::new().await;
    let (st, v) = api
        .call(
            "PUT",
            "/optimize/plan/items/0",
            json!({ "decision": "denied" }),
        )
        .await;
    assert_eq!(
        (st, &v["error"]["message"]),
        (StatusCode::NOT_FOUND, &json!("not found: no plan"))
    );
    api.sample_plan().await;
    let (_, state) = api.call("GET", "/optimize", Value::Null).await;
    let id = state["plan"]["items"][0]["id"].as_u64().unwrap();
    let (st, v) = api
        .call(
            "PUT",
            &format!("/optimize/plan/items/{id}"),
            json!({ "decision": "denied" }),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["plan"]["items"][0]["decision"], "denied", "{v}");
    let (st, _) = api
        .call(
            "PUT",
            "/optimize/plan/items/x",
            json!({ "decision": "denied" }),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = api
        .call(
            "PUT",
            "/optimize/plan/items/999",
            json!({ "decision": "denied" }),
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn consult_asks_before_it_replaces_a_plan() {
    let api = Api::new().await;
    api.sample_plan().await;
    for body in [
        json!({ "sample": true }),
        json!({ "sample": true, "replace": false }),
    ] {
        let (st, v) = api.call("POST", "/optimize/consult", body).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(v["error"]["code"], "PLAN_EXISTS");
    }
    let (st, op) = api
        .call(
            "POST",
            "/optimize/consult",
            json!({ "sample": true, "replace": true }),
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED);
    let done = api.settled(op["id"].as_str().unwrap()).await;
    assert_eq!(done["status"], "done", "{done}");
    let (st, v) = api.call("DELETE", "/optimize/plan", Value::Null).await;
    assert_eq!((st, &v["plan"]), (StatusCode::OK, &Value::Null));
    api.sample_plan().await;
}

/// A sweep start answers 202 with the sweep id and an id that
/// `/operations` knows; what can be refused is refused synchronously.
#[tokio::test]
async fn bench_runs_answer_at_once_and_settle_under_operations() {
    let api = Api::new().await;
    let (_, prompts) = api.call("GET", "/bench/prompts", Value::Null).await;
    let prompt_id = prompts["prompts"][0]["id"].as_str().unwrap().to_string();
    let (st, _) = api
        .call(
            "POST",
            "/bench/runs",
            json!({ "backend_id": "nope", "prompt_id": prompt_id, "preset": "kv" }),
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = api
        .call(
            "POST",
            "/bench/runs",
            json!({ "backend_id": "planner", "prompt_id": prompt_id,
                    "dimensions": [[{ "offload_expr": "--log-file x" }]] }),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, run) = api
        .call(
            "POST",
            "/bench/runs",
            json!({ "backend_id": "planner", "prompt_id": prompt_id, "dimensions": [[{}]] }),
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{run}");
    let sweep = run["sweep_id"].as_str().unwrap().to_string();
    let done = api.settled(run["id"].as_str().unwrap()).await;
    assert_eq!(done["detail"]["sweep_id"], sweep.as_str(), "{done}");
    let (st, v) = api
        .call("POST", &format!("/bench/runs/{sweep}/cancel"), Value::Null)
        .await;
    assert_eq!((st, &v["cancelled"]), (StatusCode::OK, &json!(false)));
    let (st, _) = api
        .call("DELETE", &format!("/bench/results/{sweep}"), Value::Null)
        .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = api
        .call("DELETE", &format!("/bench/results/{sweep}"), Value::Null)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bench_prompts_are_created_and_deleted() {
    let api = Api::new().await;
    let (st, v) = api
        .call(
            "POST",
            "/bench/prompts",
            json!({ "name": "QA", "purpose": "general", "body": "x" }),
        )
        .await;
    assert_eq!((st, &v["id"]), (StatusCode::OK, &json!("qa")), "{v}");
    for expected in [StatusCode::OK, StatusCode::NOT_FOUND] {
        let (st, _) = api.call("DELETE", "/bench/prompts/qa", Value::Null).await;
        assert_eq!(st, expected);
    }
}
