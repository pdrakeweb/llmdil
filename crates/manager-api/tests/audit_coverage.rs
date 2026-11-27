//! SEC-13 (docs/code-review-v0.1.60.md): the TOTP login step and
//! account lockouts reach the audit log.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use manager_api::auth::{base32_encode, totp_at, CredentialStore, MemoryCredentialStore};
use manager_api::users::{now_ms, RoleName, UserStore};
use manager_api::{build_router, ApiCtx};
use manager_app::AppState;
use tower::ServiceExt;

const PW: &str = "a-unique-passphrase";

fn post(uri: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

async fn json_of(resp: axum::response::Response) -> (StatusCode, serde_json::Value) {
    let st = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (st, serde_json::from_slice(&bytes).unwrap_or_default())
}

#[tokio::test]
async fn totp_login_step_is_audited() {
    let dir = std::env::temp_dir().join(format!(
        "manager-api-audit-coverage-{}-{}",
        std::process::id(),
        manager_api::users::generate_token()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = manager_config::AppConfig::default_config();
    cfg.data_dir = dir.clone();
    let app = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
    let creds = Arc::new(MemoryCredentialStore::default());
    let ctx = ApiCtx::with_services(app, dir.clone(), UserStore::in_memory(), creds.clone());

    let user = ctx
        .auth
        .create_user("tina", PW, RoleName::Operator)
        .await
        .unwrap();
    let secret = b"0123456789abcdefghij".to_vec();
    creds
        .write(&format!("llmdil/totp/{}", user.id), &base32_encode(&secret))
        .await
        .unwrap();
    let uid = user.id.clone();
    ctx.users
        .mutate::<_, manager_api::users::StoreError>(|f| {
            f.users
                .iter_mut()
                .find(|u| u.id == uid)
                .unwrap()
                .totp_enabled = true;
            Ok(())
        })
        .await
        .unwrap();
    let (router, _) = build_router(ctx.clone());

    let login_body = format!(r#"{{"username": "tina", "password": "{PW}"}}"#);
    let challenge = |router: axum::Router, body: String| async move {
        let (st, v) = json_of(
            router
                .oneshot(post("/api/v1/auth/login", body))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["totp_required"], true);
        v["challenge_id"].as_str().unwrap().to_string()
    };

    // A wrong code: 401, audited under the challenged account.
    let now = now_ms() / 1000;
    let valid: Vec<u32> = (-1..=1).map(|d| totp_at(&secret, now + d * 30)).collect();
    let wrong = (0..1_000_000u32).find(|c| !valid.contains(c)).unwrap();
    let cid = challenge(router.clone(), login_body.clone()).await;
    let (st, _) = json_of(
        router
            .clone()
            .oneshot(post(
                "/api/v1/auth/totp",
                format!(r#"{{"challenge_id": "{cid}", "code": "{wrong:06}"}}"#),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // The right code: tokens, audited as ok.
    let cid = challenge(router.clone(), login_body.clone()).await;
    let code = totp_at(&secret, now_ms() / 1000);
    let (st, v) = json_of(
        router
            .clone()
            .oneshot(post(
                "/api/v1/auth/totp",
                format!(r#"{{"challenge_id": "{cid}", "code": "{code:06}"}}"#),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    // An unknown challenge: audited with an unknown actor.
    let (st, _) = json_of(
        router
            .clone()
            .oneshot(post(
                "/api/v1/auth/totp",
                r#"{"challenge_id": "nope", "code": "000000"}"#.to_string(),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    let totp: Vec<(String, String)> = ctx
        .audit
        .read_recent(0, 100)
        .into_iter()
        .filter(|e| e["action"] == "POST /api/v1/auth/totp")
        .map(|e| {
            (
                e["actor"].as_str().unwrap().to_string(),
                e["result"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    // Newest first.
    assert_eq!(
        totp,
        vec![
            ("unknown".to_string(), "invalid_credentials".to_string()),
            ("tina".to_string(), "ok".to_string()),
            ("tina".to_string(), "invalid_credentials".to_string()),
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}
