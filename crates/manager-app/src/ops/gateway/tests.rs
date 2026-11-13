use super::*;
use crate::test_support::*;

/// Capability audit §13.2: the effective global routing strategy
/// is computed server-side by the generator's rule and returned on
/// `get_gateway_config` (additive field), so the UI never
/// re-implements it.
#[tokio::test]
async fn get_gateway_config_reports_effective_strategy() {
    let cfg = AppConfig::default_config();
    let expected = manager_gateway::effective_global_strategy(&cfg);
    let (state, _dir) = advanced_test_state(cfg);
    let v = op_get_gateway_config(&state, &test_sink()).await.unwrap();
    assert_eq!(
        v["effective_strategy"],
        serde_json::to_value(expected).unwrap()
    );
    if let Some(s) = expected {
        // Same kebab-case spelling as each group's `strategy`.
        let groups = v["groups"].as_array().unwrap();
        assert!(groups
            .iter()
            .any(|g| g["strategy"] == serde_json::to_value(s).unwrap()));
    }

    // No group would be generated: null, not a guess.
    let mut empty = AppConfig::default_config();
    empty.gateway.groups.clear();
    let (state, _dir) = advanced_test_state(empty);
    let v = op_get_gateway_config(&state, &test_sink()).await.unwrap();
    assert!(v["effective_strategy"].is_null());
    assert!(
        v.get("effective_strategy").is_some(),
        "field is always present"
    );
}

/// PS-6 (docs/code-review-v0.1.60.md): an invalid gateway patch is
/// refused before it can journal a phantom activation-plan entry.
#[tokio::test]
async fn invalid_gateway_patch_journals_nothing() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    // Seeding enables the default fleet, so the groups render into
    // the gateway YAML and a change to them is journal-worthy.
    ensure_catalog_models(&state).await;
    let mut groups = serde_json::to_value(&state.config.read().await.gateway.groups).unwrap();
    groups[0]["fallbacks"] = json!(["no-such-backend"]);
    let patch = json!({ "groups": groups });
    assert!(op_update_gateway_config(&state, &sink, patch)
        .await
        .is_err());
    let journal = activation::load_journal(&state.data_dir);
    assert!(journal.entries.is_empty(), "{:?}", journal.entries.len());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Groups page membership (predicted-throughput design
/// §8.5): the move joins exactly one group and leaves every
/// other; the response names the group it left.
#[tokio::test]
async fn gateway_set_backend_group_moves_membership() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    let r = op_gateway_set_backend_group(
        &state,
        &sink,
        "coder".to_string(),
        Some("planner".to_string()),
    )
    .await
    .unwrap();
    assert_eq!(r["changed"], json!(true));
    assert_eq!(r["group"], json!("planner"));
    assert_eq!(r["removed_from"], json!(["coder"]));
    let cfg = state.config.read().await;
    let memberships: Vec<&str> = cfg
        .gateway
        .groups
        .iter()
        .filter(|g| g.members.iter().any(|m| m == "coder"))
        .map(|g| g.name.as_str())
        .collect();
    assert_eq!(memberships, vec!["planner"]);
    drop(cfg);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A move onto the lane the backend solely occupies — and an
/// ungroup of an already-ungrouped backend — succeed without
/// a write (design §8.4: drops are idempotent).
#[tokio::test]
async fn gateway_set_backend_group_noop_drops_do_not_write() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    let r = op_gateway_set_backend_group(
        &state,
        &sink,
        "coder".to_string(),
        Some("coder".to_string()),
    )
    .await
    .unwrap();
    assert_eq!(r["changed"], json!(false));
    assert_eq!(r["removed_from"], json!([]));
    // Ungroup once (a real change), then again (a no-op).
    let r = op_gateway_set_backend_group(&state, &sink, "coder".to_string(), None)
        .await
        .unwrap();
    assert_eq!(r["changed"], json!(true));
    assert_eq!(r["group"], Value::Null);
    let r = op_gateway_set_backend_group(&state, &sink, "coder".to_string(), None)
        .await
        .unwrap();
    assert_eq!(r["changed"], json!(false));
    let cfg = state.config.read().await;
    assert!(!cfg
        .gateway
        .groups
        .iter()
        .any(|g| g.members.iter().any(|m| m == "coder")));
    drop(cfg);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A hand-edited config can list one backend in several
/// groups; a move collapses it to the single target group
/// and reports every group it was removed from (design §8.3).
#[tokio::test]
async fn gateway_set_backend_group_collapses_multi_membership() {
    let mut cfg = test_cfg();
    // coder is seeded into group "coder"; add it to "planner" too.
    cfg.gateway
        .groups
        .iter_mut()
        .find(|g| g.name == "planner")
        .unwrap()
        .members
        .push("coder".to_string());
    let (state, dir) = advanced_test_state(cfg);
    let sink = test_sink();
    let r = op_gateway_set_backend_group(
        &state,
        &sink,
        "coder".to_string(),
        Some("tool-runner".to_string()),
    )
    .await
    .unwrap();
    assert_eq!(r["changed"], json!(true));
    let mut removed: Vec<String> = serde_json::from_value(r["removed_from"].clone()).unwrap();
    removed.sort();
    assert_eq!(removed, vec!["coder".to_string(), "planner".to_string()]);
    let cfg = state.config.read().await;
    let memberships: Vec<&str> = cfg
        .gateway
        .groups
        .iter()
        .filter(|g| g.members.iter().any(|m| m == "coder"))
        .map(|g| g.name.as_str())
        .collect();
    assert_eq!(memberships, vec!["tool-runner"]);
    drop(cfg);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Unknown backends and groups are NotFound; an empty group
/// string is refused rather than read as ungroup.
#[tokio::test]
async fn gateway_set_backend_group_rejects_unknowns() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    let r = op_gateway_set_backend_group(
        &state,
        &sink,
        "nope".to_string(),
        Some("planner".to_string()),
    )
    .await;
    assert!(matches!(r, Err(ApiError::NotFound(_))), "{r:?}");
    let r =
        op_gateway_set_backend_group(&state, &sink, "coder".to_string(), Some("nope".to_string()))
            .await;
    assert!(matches!(r, Err(ApiError::NotFound(_))), "{r:?}");
    let r =
        op_gateway_set_backend_group(&state, &sink, "coder".to_string(), Some("  ".to_string()))
            .await;
    assert!(matches!(r, Err(ApiError::InvalidInput(_))), "{r:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Group lifecycle (design §8.5/D11): create makes an empty
/// group under the slug rule and refuses duplicates; delete
/// refuses a non-empty group with its member count and
/// deletes an empty one.
#[tokio::test]
async fn gateway_group_create_and_delete() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    let r = op_gateway_create_group(&state, &sink, "batch-jobs".to_string())
        .await
        .unwrap();
    assert_eq!(r["name"], json!("batch-jobs"));
    {
        let cfg = state.config.read().await;
        let g = cfg
            .gateway
            .groups
            .iter()
            .find(|g| g.name == "batch-jobs")
            .unwrap();
        assert!(g.members.is_empty());
        assert_eq!(g.strategy, manager_config::RoutingStrategy::SimpleShuffle);
    }
    let r = op_gateway_create_group(&state, &sink, "batch-jobs".to_string()).await;
    assert!(matches!(r, Err(ApiError::InvalidInput(_))), "{r:?}");
    let r = op_gateway_create_group(&state, &sink, "Bad Name".to_string()).await;
    assert!(matches!(r, Err(ApiError::InvalidInput(_))), "{r:?}");
    // Non-empty delete is refused, naming the count.
    let r = op_gateway_delete_group(&state, &sink, "coder".to_string()).await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("not empty"), "{msg}");
            assert!(msg.contains('1'), "{msg}");
        }
        other => panic!("expected the not-empty refusal, got {other:?}"),
    }
    let r = op_gateway_delete_group(&state, &sink, "batch-jobs".to_string())
        .await
        .unwrap();
    assert_eq!(r["deleted"], json!(true));
    let r = op_gateway_delete_group(&state, &sink, "batch-jobs".to_string()).await;
    assert!(matches!(r, Err(ApiError::NotFound(_))), "{r:?}");
    let cfg = state.config.read().await;
    assert!(!cfg.gateway.groups.iter().any(|g| g.name == "batch-jobs"));
    drop(cfg);
    let _ = std::fs::remove_dir_all(&dir);
}

/// PS-6: a journaling command whose config save fails after the
/// entry was recorded removes exactly that entry — an earlier
/// entry and a standing offload override written by another
/// command stay.
#[tokio::test]
async fn failed_save_rolls_back_only_its_journal_entry() {
    let dir = unique_test_dir("llm-mgr-ps6-rollback");
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = test_cfg();
    cfg.data_dir = dir.clone();
    // The config path is a directory: save's backup rotation
    // cannot copy it, so every save fails after journaling.
    let config_path = dir.join("config-is-a-directory");
    std::fs::create_dir_all(&config_path).unwrap();
    let state = Arc::new(AppState::new(cfg.clone(), config_path, dir.clone()));
    let mut earlier_post = cfg.clone();
    earlier_post.gateway.port += 1;
    let configured: std::collections::HashSet<String> =
        cfg.backends.iter().map(|b| b.id.clone()).collect();
    let earlier = activation::record_change(
        &dir,
        &cfg,
        &earlier_post,
        "gateway_config",
        "earlier change".to_string(),
        "gateway",
        Vec::new(),
        &std::collections::HashMap::new(),
        &configured,
        &std::collections::HashSet::new(),
    )
    .unwrap();
    activation::record_offload_override(&dir, "coder", 24, vec![(8, 9)]).unwrap();
    let host_before = state.config.read().await.gateway.bind_host.clone();
    let new_host = if host_before == "127.0.0.1" {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    };
    let r = op_gateway_set_bind_host(&state, &test_sink(), new_host.to_string()).await;
    assert!(r.is_err(), "the save must fail: {r:?}");
    assert_eq!(
        state.config.read().await.gateway.bind_host,
        host_before,
        "a failed save leaves the live config unchanged"
    );
    let journal = activation::load_journal(&dir);
    assert_eq!(
        journal
            .entries
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        vec![earlier.as_str()],
        "only the failed command's entry is rolled back"
    );
    assert!(
        activation::offload_override_covers(&journal, "coder", 24, &[(8, 9)]),
        "the other command's offload override survives the rollback"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
