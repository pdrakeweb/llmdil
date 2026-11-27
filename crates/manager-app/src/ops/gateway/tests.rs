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

/// The bind host a test switches to: whichever loopback/any value
/// the configuration does not already carry.
async fn other_bind_host(state: &AppState) -> &'static str {
    if state.config.read().await.gateway.bind_host == "127.0.0.1" {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    }
}

/// PS-11 (docs/code-review-v0.1.60.md): a journaled edit must not
/// wait on the supervisor lock while it holds `config.write()` —
/// a backend start holds the supervisor for up to ~10 s, and a
/// parked config writer queues every status/list poll behind it.
/// The test holds the supervisor (as a start would), runs a
/// journaling op, and asserts the config stays readable the whole
/// time the op is parked; then it releases the supervisor and
/// checks the op journaled and saved as before (record-before-save).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journaled_edit_does_not_hold_config_write_while_waiting_on_supervisor() {
    let (state, dir) = advanced_test_state(test_cfg());
    let new_host = other_bind_host(&state).await;
    let sup_guard = state.supervisor().await.lock().await;
    let op_state = state.clone();
    let op = tokio::spawn(async move {
        op_gateway_set_bind_host(&op_state, &test_sink(), new_host.to_string()).await
    });
    // For 500 ms the op is parked on the supervisor we hold; the
    // config must never be write-locked meanwhile.
    for _ in 0..50 {
        assert!(
            state.config.try_read().is_ok(),
            "the journaling op holds config.write() while waiting on the supervisor"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!op.is_finished(), "the op must be parked on the supervisor");
    // A config read (what every status/list poll takes) completes
    // promptly while the supervisor is still held.
    let read = tokio::time::timeout(std::time::Duration::from_secs(1), state.config.read()).await;
    assert!(
        read.is_ok(),
        "config.read() queued behind the journaling op"
    );
    drop(read);
    drop(sup_guard);
    let r = tokio::time::timeout(std::time::Duration::from_secs(10), op)
        .await
        .expect("op finished")
        .unwrap();
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(state.config.read().await.gateway.bind_host, new_host);
    let journal = activation::load_journal(&dir);
    assert_eq!(journal.entries.len(), 1, "the change is journaled");
    assert_eq!(journal.entries[0].kind, "gateway_bind");
    let _ = std::fs::remove_dir_all(&dir);
}

/// PS-11 race: a config change to the liveness inputs (here a
/// backend's port) that lands between the journal op's liveness
/// snapshot and its write lock makes the op re-gather against the
/// configuration it actually writes. The op completes, and the
/// concurrent change survives (the op's `pre` is the locked config).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journaled_edit_regathers_liveness_when_backends_change_meanwhile() {
    let (state, dir) = advanced_test_state(test_cfg());
    let new_host = other_bind_host(&state).await;
    let sup_guard = state.supervisor().await.lock().await;
    let op_state = state.clone();
    let op = tokio::spawn(async move {
        op_gateway_set_bind_host(&op_state, &test_sink(), new_host.to_string()).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!op.is_finished(), "the op must be parked on the supervisor");
    // Not blocked: the op holds no config guard while parked.
    let new_port = {
        let mut cfg = tokio::time::timeout(std::time::Duration::from_secs(1), state.config.write())
            .await
            .expect("config writable while the op is parked");
        let b = cfg
            .backends
            .first_mut()
            .expect("default config has backends");
        b.port += 1000;
        b.port
    };
    drop(sup_guard);
    let r = tokio::time::timeout(std::time::Duration::from_secs(10), op)
        .await
        .expect("op finished")
        .unwrap();
    assert!(r.is_ok(), "{r:?}");
    let cfg = state.config.read().await;
    assert_eq!(cfg.gateway.bind_host, new_host);
    assert_eq!(
        cfg.backends.first().unwrap().port,
        new_port,
        "the concurrent change survives"
    );
    drop(cfg);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The enabled default fleet with a recorded successful gateway start:
/// the generated files and the spawn record on disk.
async fn started_state() -> (Arc<AppState>, PathBuf) {
    let (state, dir) = advanced_test_state(test_cfg());
    ensure_catalog_models(&state).await;
    {
        let mut cfg = state.config.write().await;
        cfg.gateway.port = 40093;
        cfg.gateway.bind_host = "127.0.0.1".to_string();
    }
    let cfg = state.config.read().await.clone();
    record_start(&dir, &cfg);
    (state, dir)
}

fn record_start(dir: &std::path::Path, cfg: &AppConfig) {
    let armed = manager_wizard::config_with_armed_gateway_mcp_servers(cfg, |_| false);
    let yaml = manager_gateway::generate_config_yaml(&armed).unwrap();
    std::fs::create_dir_all(dir.join("gateway")).unwrap();
    std::fs::write(dir.join("gateway").join("config.yaml"), &yaml).unwrap();
    let fp = manager_wizard::gateway_spawn_fingerprint(cfg, false, None, None, &[]);
    std::fs::write(
        manager_wizard::gateway_spawn_state_path(dir),
        manager_wizard::gateway_spawn_state_json(&fp, &yaml, Some("1.111.0")),
    )
    .unwrap();
}

/// A16/B2/C10b: a port save is a pending gateway change whose summary
/// names it; an unusable port is refused in plain words.
#[tokio::test]
async fn a_port_change_is_journaled_and_a_bad_port_is_refused_plainly() {
    let (state, dir) = started_state().await;
    let sink = test_sink();
    for bad in [json!(0), json!(70000), json!(-1), json!("4000")] {
        let e = op_update_gateway_config(&state, &sink, json!({ "port": bad }))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("from 1 to 65535"), "{e}");
    }
    let taken = state.config.read().await.backends[0].port;
    let e = op_update_gateway_config(&state, &sink, json!({ "port": taken }))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("is used by both the gateway and"), "{e}");
    assert!(!e.contains("validation failed"), "{e}");
    assert!(activation::load_journal(&dir).entries.is_empty());

    op_update_gateway_config(&state, &sink, json!({ "port": 40094 }))
        .await
        .unwrap();
    let journal = activation::load_journal(&dir);
    assert_eq!(journal.entries.len(), 1);
    assert_eq!(journal.entries[0].scope, "gateway");
    assert_eq!(
        journal.entries[0].summary,
        "Gateway settings changed: port (40093 → 40094)"
    );
    // Back to the running port: nothing is left to apply.
    op_update_gateway_config(&state, &sink, json!({ "port": 40093 }))
        .await
        .unwrap();
    assert!(activation::load_journal(&dir).entries.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// B1/B2: moving a backend names it, and moving it back leaves nothing
/// pending.
#[tokio::test]
async fn moving_a_backend_and_back_leaves_no_pending_entry() {
    let (state, dir) = started_state().await;
    let sink = test_sink();
    let mv = |group: &str| {
        op_gateway_set_backend_group(&state, &sink, "coder".to_string(), Some(group.to_string()))
    };
    mv("planner").await.unwrap();
    let journal = activation::load_journal(&dir);
    assert_eq!(journal.entries.len(), 1);
    assert_eq!(journal.entries[0].targets, ["coder"]);
    assert!(journal.entries[0].summary.contains("`coder`"));
    mv("coder").await.unwrap();
    assert!(activation::load_journal(&dir).entries.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// B6: the reserved cloud-tier name and an over-long name are refused,
/// on create and on rename; stored names keep working.
#[tokio::test]
async fn reserved_and_overlong_group_names_are_refused() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    let e = op_gateway_create_group(&state, &sink, "openrouter".to_string())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("reserved"), "{e}");
    let e = op_gateway_create_group(&state, &sink, "g".repeat(60))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("limit is 48"), "{e}");
    let mut groups = serde_json::to_value(&state.config.read().await.gateway.groups).unwrap();
    groups[0]["name"] = json!("OpenRouter");
    let e = op_update_gateway_config(&state, &sink, json!({ "groups": groups }))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("reserved"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Answer every connection with a minimal HTTP reply, like a running
/// gateway's liveliness endpoint.
fn stub_gateway() -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { return };
            let _ = s.read(&mut [0u8; 1024]);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        }
    });
    port
}

/// A16/A17/B5: with another port saved, the status is judged at the
/// running gateway's port, reports both, describes the applied config,
/// and the plan announces the administrator prompt.
#[tokio::test]
async fn status_describes_the_running_gateway_after_a_port_save() {
    let (state, dir) = started_state().await;
    let sink = test_sink();
    let running = stub_gateway();
    {
        let mut cfg = state.config.write().await;
        cfg.gateway.port = running;
        cfg.gateway.bind_host = "0.0.0.0".to_string();
        record_start(&dir, &cfg);
    }
    let before = op_gateway_status(&state, &sink).await.unwrap();
    assert_eq!(before["running_port"], json!(running));
    assert_eq!(before["port_pending"], false);

    op_update_gateway_config(&state, &sink, json!({ "port": 40095 }))
        .await
        .unwrap();
    let mut groups = serde_json::to_value(&state.config.read().await.gateway.groups).unwrap();
    groups[0]["strategy"] = json!("least-busy");
    op_update_gateway_config(&state, &sink, json!({ "groups": groups }))
        .await
        .unwrap();
    // A probe of the saved port (the monitor's) answered "down".
    state.lifecycle.set_probe(lifecycle::GATEWAY, false);
    let v = op_gateway_status(&state, &sink).await.unwrap();
    assert_eq!(v["up"], true, "{v}");
    assert_eq!(v["port"], 40095);
    assert_eq!(v["running_port"], json!(running));
    assert_eq!(v["port_pending"], true);
    assert_eq!(v["activation_changes"], 2, "{v}");
    assert_eq!(v["groups"], before["groups"]);
    assert_eq!(v["effective_strategy"], before["effective_strategy"]);
    assert_eq!(v["saved_effective_strategy"], "least-busy");
    let plan = op_gateway_activation_plan(&state, &sink).await.unwrap();
    assert_eq!(plan["gateway_up"], true, "{plan}");
    assert_eq!(plan["changes"].as_array().unwrap().len(), 2, "{plan}");
    assert_eq!(plan["drift"], false);
    assert_eq!(plan["files_differ"], true);
    assert_eq!(plan["admin_approval_required"], true);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A18: cancelling an edit the gateway's files do not carry leaves them
/// alone and no drift behind — also with a Gateway MCP server the
/// running gateway was started without (unarmed).
#[tokio::test]
async fn cancelling_a_backend_edit_creates_no_gateway_drift() {
    let (state, dir) = started_state().await;
    let sink = test_sink();
    let yaml_path = dir.join("gateway").join("config.yaml");
    // Another spelling of the same content: a rewrite would be visible.
    let yaml = std::fs::read_to_string(&yaml_path).unwrap() + "\n";
    std::fs::write(&yaml_path, &yaml).unwrap();
    state.config.write().await.gateway.port = stub_gateway();
    record_start(&dir, &*state.config.read().await);
    std::fs::write(&yaml_path, &yaml).unwrap();
    let pre = state.config.read().await.clone();
    let mut post = pre.clone();
    post.gateway.request_log_len += 1;
    activation::record_change(
        &dir,
        &pre,
        &post,
        "gateway_config",
        "edit".to_string(),
        "gateway",
        Vec::new(),
        &HashMap::new(),
        &Default::default(),
        &Default::default(),
    )
    .unwrap();
    *state.config.write().await = post;
    let r = op_gateway_activation_cancel(&state, &sink).await.unwrap();
    assert_eq!(r["cancelled"], true, "{r}");
    assert_eq!(r["yaml_regenerated"], true, "{r}");
    assert_eq!(std::fs::read_to_string(&yaml_path).unwrap(), yaml);
    let cfg = state.config.read().await.clone();
    assert!(!activation::config_drift(&cfg, &dir, |_| false));
    let _ = std::fs::remove_dir_all(&dir);
}
