use super::*;
use crate::test_support::*;

/// T-95: the test toast reports what is known and claims no delivery;
/// the app id it reads settings for is the shell's identifier.
#[tokio::test]
async fn test_notification_claims_no_delivery() {
    let (state, dir) = advanced_test_state(test_cfg());
    let v = op_notifications_send_test(&state, &test_sink())
        .await
        .unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["delivery_confirmed"], false);
    let conf =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../src-tauri/tauri.conf.json");
    let conf: Value = serde_json::from_str(&std::fs::read_to_string(conf).unwrap()).unwrap();
    assert_eq!(v["app_user_model_id"], conf["identifier"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// F2 (docs/code-review-v0.1.60.md): `get_status` carries the
/// startup config-load outcome once the shell records it, and
/// omits the field otherwise.
#[tokio::test]
async fn get_status_reports_config_load_outcome() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    let v = op_get_status(&state, &sink).await.unwrap();
    assert!(v.get("config_load").is_none(), "{v}");
    state.set_config_load_state(
        manager_config::ConfigSource::Defaults,
        Some("bad json".to_string()),
        &[dir.join("config.json.corrupt-1")],
    );
    let v = op_get_status(&state, &sink).await.unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["config_load"]["source"], "defaults");
    assert_eq!(v["config_load"]["error"], "bad json");
    assert_eq!(v["config_load"]["preserved"].as_array().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Code review B-3: the whole-config patch path may not
/// smuggle in a new backend in the reserved namespace either.
#[tokio::test]
async fn update_config_refuses_a_new_reserved_temp_backend() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    let mut v = serde_json::to_value(state.config.read().await.clone()).unwrap();
    let mut extra = v["backends"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == "coder")
        .unwrap()
        .clone();
    extra["id"] = json!("opt-exp-9");
    extra["port"] = json!(9999);
    extra["enabled"] = json!(false);
    v["backends"].as_array_mut().unwrap().push(extra);
    let patch = json!({ "backends": v["backends"].clone() });
    match op_update_config(&state, &sink, patch).await {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("reserved"), "{msg}");
            assert!(msg.contains("opt-exp-"), "{msg}");
        }
        other => panic!("expected the reservation refusal, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Code review B-3: the reservation guards creation only —
/// a config that already carries a reserved id (a pre-fix
/// leftover, which the startup sweep owns) is not newly
/// rejected by an unrelated patch.
#[tokio::test]
async fn update_config_leaves_a_preexisting_reserved_temp_backend_alone() {
    let mut cfg = test_cfg();
    let mut extra = cfg
        .backends
        .iter()
        .find(|b| b.id == "coder")
        .unwrap()
        .clone();
    extra.id = "opt-exp-9".to_string();
    extra.port = 9999;
    extra.enabled = false;
    cfg.backends.push(extra);
    let (state, dir) = advanced_test_state(cfg);
    let sink = test_sink();
    let r = op_update_config(
        &state,
        &sink,
        json!({ "supervision": { "mode": "process" } }),
    )
    .await
    .unwrap();
    assert_eq!(r["ok"], true);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Settings-pages sweep (Oct 19, 2026): every field the sweep
/// wired to a Settings/Gateway editor must round-trip through
/// its real save path — op call, save, reload from disk shows
/// the new value. Covers the dedicated ops (notifications,
/// telemetry interval, updates pin/auto), the generic config
/// patch (Postgres port, Admin API port, WinML, MXC policy
/// path), and the gateway config patch (port, OpenRouter tier,
/// group fallbacks).
#[tokio::test]
async fn settings_controls_round_trip_through_real_ops() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();

    // Notifications master: to false, and back to true.
    let notif = manager_config::NotificationConfig {
        enabled: false,
        ..Default::default()
    };
    op_notifications_set_config(&state, &sink, notif)
        .await
        .unwrap();
    let loaded = manager_config::load(&state.config_path).unwrap();
    assert!(!loaded.notifications.enabled);
    let notif = manager_config::NotificationConfig {
        enabled: true,
        ..Default::default()
    };
    op_notifications_set_config(&state, &sink, notif)
        .await
        .unwrap();
    let loaded = manager_config::load(&state.config_path).unwrap();
    assert!(loaded.notifications.enabled);

    // Supervision mode.
    use manager_config::SupervisionMode::{Process, Service};
    for mode in [Service, Process] {
        let r = op_settings_set_supervision(&state, &sink, mode)
            .await
            .unwrap();
        assert_eq!(r["persisted"], true);
        let loaded = manager_config::load(&state.config_path).unwrap();
        assert_eq!(loaded.supervision.mode, mode);
    }

    // Telemetry poll interval.
    op_telemetry_set_interval(&state, &sink, 2500)
        .await
        .unwrap();
    let loaded = manager_config::load(&state.config_path).unwrap();
    assert_eq!(loaded.telemetry.poll_interval_ms, 2500);

    // Generic config patch: Postgres port, Admin API port,
    // WinML enable + port + model, MXC policy path.
    op_update_config(&state, &sink, json!({ "postgres": { "port": 54330 } }))
        .await
        .unwrap();
    op_update_config(&state, &sink, json!({ "admin_api": { "port": 47822 } }))
        .await
        .unwrap();
    op_update_config(
        &state,
        &sink,
        json!({ "winml": { "enabled": true, "port": 8091, "model_id": "qwen3-8b" } }),
    )
    .await
    .unwrap();
    op_update_config(
        &state,
        &sink,
        json!({ "mxc": { "path": "/tmp/custom-policy.json" } }),
    )
    .await
    .unwrap();
    let loaded = manager_config::load(&state.config_path).unwrap();
    assert_eq!(loaded.postgres.port, 54330);
    assert_eq!(loaded.admin_api.port, 47822);
    assert!(loaded.winml.enabled);
    assert_eq!(loaded.winml.port, 8091);
    assert_eq!(loaded.winml.model_id.as_deref(), Some("qwen3-8b"));
    assert_eq!(loaded.mxc.path, PathBuf::from("/tmp/custom-policy.json"));

    // Component pin + auto-update, then unpin + re-enable.
    op_updates_pin(
        &state,
        &sink,
        "litellm".to_string(),
        Some("1.105.1".to_string()),
    )
    .await
    .unwrap();
    op_updates_set_auto(&state, &sink, "litellm".to_string(), false)
        .await
        .unwrap();
    let loaded = manager_config::load(&state.config_path).unwrap();
    let comp = loaded.updates.components.get("litellm").unwrap();
    assert_eq!(comp.pinned_version.as_deref(), Some("1.105.1"));
    assert!(!comp.auto_update);
    op_updates_pin(&state, &sink, "litellm".to_string(), None)
        .await
        .unwrap();
    // Nothing updates LiteLLM automatically: refused, not stored (B18).
    let err = op_updates_set_auto(&state, &sink, "litellm".to_string(), true)
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Unsupported(_)), "{err}");
    let loaded = manager_config::load(&state.config_path).unwrap();
    let comp = loaded.updates.components.get("litellm").unwrap();
    // LiteLLM is never left unpinned (finding F1,
    // docs/code-review-v0.1.60.md): the unpin persists, and the
    // load-time backfill restores the default pin.
    assert_eq!(
        comp.pinned_version.as_deref(),
        Some(manager_config::DEFAULT_LITELLM_PIN)
    );
    assert!(!comp.auto_update);
    // Unpin round-trips as None for components without a default pin.
    op_updates_pin(
        &state,
        &sink,
        "llamacpp".to_string(),
        Some(manager_wizard::steps::LlamaCppPin::default_pin().tag),
    )
    .await
    .unwrap();
    op_updates_pin(&state, &sink, "llamacpp".to_string(), None)
        .await
        .unwrap();
    let loaded = manager_config::load(&state.config_path).unwrap();
    let comp = loaded.updates.components.get("llamacpp").unwrap();
    assert_eq!(comp.pinned_version, None);

    // Gateway patch: port, the OpenRouter tier fields, and one
    // group's fallback chain (the full groups array rides
    // through in stored form, only the chain changes).
    op_update_gateway_config(&state, &sink, json!({ "port": 4001 }))
        .await
        .unwrap();
    op_update_gateway_config(
        &state,
        &sink,
        json!({ "openrouter": { "cred_ref": "openrouter-key", "daily_cap_usd": 12.5, "model_allowlist": ["openai/gpt-5"] } }),
    )
    .await
    .unwrap();
    let mut groups_patch = serde_json::to_value(&state.config.read().await.gateway.groups).unwrap();
    groups_patch.as_array_mut().unwrap()[0]["fallbacks"] = json!(["openrouter"]);
    op_update_gateway_config(&state, &sink, json!({ "groups": groups_patch }))
        .await
        .unwrap();
    let loaded = manager_config::load(&state.config_path).unwrap();
    assert_eq!(loaded.gateway.port, 4001);
    assert_eq!(
        loaded.gateway.openrouter.cred_ref.as_deref(),
        Some("openrouter-key")
    );
    assert_eq!(loaded.gateway.openrouter.daily_cap_usd, 12.5);
    assert_eq!(
        loaded.gateway.openrouter.model_allowlist,
        vec!["openai/gpt-5".to_string()]
    );
    assert_eq!(
        loaded.gateway.groups[0].fallbacks,
        vec!["openrouter".to_string()]
    );
    // The groups patch must not reset the stored purpose.
    assert_eq!(
        loaded.gateway.groups[0].purpose,
        test_cfg().gateway.groups[0].purpose
    );
    let _ = std::fs::remove_dir_all(&dir);
}
