use super::*;
use crate::test_support::*;

/// Code review B-3: user creation refuses the reserved
/// `opt-exp-*` experiment namespace, naming the reservation.
#[tokio::test]
async fn backend_add_refuses_reserved_temp_prefix() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    let r = op_backend_add(
        &state,
        &sink,
        "opt-exp-1".to_string(),
        "qwen3.8-27b".to_string(),
        9999,
        "planner".to_string(),
        false,
        true,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("reserved"), "{msg}");
            assert!(msg.contains("opt-exp-"), "{msg}");
        }
        other => panic!("expected the reservation refusal, got {other:?}"),
    }
    // Nothing was created.
    let cfg = state.config.read().await;
    assert!(!cfg.backends.iter().any(|b| b.id == "opt-exp-1"));
    drop(cfg);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn advanced_mode_flip_writes_no_flags_and_no_journal() {
    let mut cfg = test_cfg();
    // A deliberately non-canonical raw string (an `=` spelling
    // and doubled spacing): any canonicalizing writer would
    // rewrite it, so byte equality proves the flip wrote no
    // flags (tracker #60, requirement: entry performs no write).
    cfg.backends
        .iter_mut()
        .find(|b| b.id == "coder")
        .unwrap()
        .raw_flags = Some("--n-cpu-moe=46  --threads 8".to_string());
    let (state, dir) = advanced_test_state(cfg);
    let sink = test_sink();
    let r = op_backend_set_flags_mode(&state, &sink, "coder".to_string(), true)
        .await
        .unwrap();
    assert_eq!(r["flags_advanced"], true);
    assert_eq!(r["changed"], true);
    assert_eq!(r["raw_flags"], "--n-cpu-moe=46  --threads 8");
    // On disk: the string is byte-identical and the mode is on.
    let text = std::fs::read_to_string(dir.join("config.json")).unwrap();
    assert!(text.contains("--n-cpu-moe=46  --threads 8"), "{text}");
    assert!(text.contains("\"flags_advanced\": true"), "{text}");
    // A mode flip is not an activation change: no journal entry.
    assert!(activation::load_journal(&dir).entries.is_empty());
    // Re-flipping to the same state writes nothing.
    let r = op_backend_set_flags_mode(&state, &sink, "coder".to_string(), true)
        .await
        .unwrap();
    assert_eq!(r["changed"], false);
    // Leaving the mode also writes no flags.
    let r = op_backend_set_flags_mode(&state, &sink, "coder".to_string(), false)
        .await
        .unwrap();
    assert_eq!(r["raw_flags"], "--n-cpu-moe=46  --threads 8");
    let saved = manager_config::load(&state.config_path).unwrap();
    let coder = saved.backends.iter().find(|b| b.id == "coder").unwrap();
    assert!(!coder.flags_advanced);
    assert_eq!(
        coder.raw_flags.as_deref(),
        Some("--n-cpu-moe=46  --threads 8")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn raw_flags_save_refuses_an_override_comma_llama_server_would_split() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    op_backend_set_flags_mode(&state, &sink, "planner".to_string(), true)
        .await
        .unwrap();
    let err = op_backend_set_raw_flags(
        &state,
        &sink,
        "planner".to_string(),
        Some(r#"-ot "blk\.[0-9]{1,2}\.ffn_.*_exps=CPU""#.to_string()),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, ApiError::InvalidInput(m) if m.contains("alternation")),
        "{err}"
    );
    // Nothing was saved.
    let cfg = state.config.read().await.clone();
    let b = cfg.backends.iter().find(|b| b.id == "planner").unwrap();
    assert!(b.raw_flags.as_deref().is_none_or(|r| !r.contains("{1,2}")));
    // The alternation spelling saves.
    let r = op_backend_set_raw_flags(
        &state,
        &sink,
        "planner".to_string(),
        Some(r#"-ot "blk\.([0-9]|[1-9][0-9])\.ffn_.*_exps=CPU""#.to_string()),
    )
    .await
    .unwrap();
    assert_eq!(r["changed"], true);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn raw_flags_save_requires_mode_journals_and_warns_without_blocking() {
    let mut cfg = test_cfg();
    cfg.backends
        .iter_mut()
        .find(|b| b.id == "planner")
        .unwrap()
        .tool_call_probe = Some(manager_config::ToolCallProbe {
        outcome: manager_config::ToolCallProbeOutcome::Verified,
        at_ms: 1,
    });
    let (state, dir) = advanced_test_state(cfg);
    let sink = test_sink();
    // Structured mode: the dedicated save refuses — the
    // structured writers own the string there.
    let err = op_backend_set_raw_flags(
        &state,
        &sink,
        "planner".to_string(),
        Some("--parallel 1".to_string()),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, ApiError::InvalidInput(m) if m.contains("not in advanced flags mode")),
        "{err}"
    );
    op_backend_set_flags_mode(&state, &sink, "planner".to_string(), true)
        .await
        .unwrap();
    const RAW: &str = "--parallel 1 --spec-type draft-mtp --spec-draft-n-max 2 --threads 8";
    let r = op_backend_set_raw_flags(&state, &sink, "planner".to_string(), Some(RAW.to_string()))
        .await
        .unwrap();
    assert_eq!(r["changed"], true);
    assert_eq!(r["raw_flags"], RAW);
    assert!(r["warnings"].as_array().unwrap().is_empty());
    // Journaled as a backend-scope activation change, and the
    // tool-call probe was cleared by the flags change.
    let journal = activation::load_journal(&dir);
    let entries: Vec<_> = journal
        .entries
        .iter()
        .filter(|e| e.kind == "backend_set_raw_flags")
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].scope, "backend");
    assert_eq!(entries[0].targets, vec!["planner".to_string()]);
    let saved = manager_config::load(&state.config_path).unwrap();
    let planner = saved.backends.iter().find(|b| b.id == "planner").unwrap();
    assert_eq!(planner.raw_flags.as_deref(), Some(RAW));
    assert_eq!(planner.tool_call_probe, None);
    // A no-change save journals nothing new.
    let r = op_backend_set_raw_flags(&state, &sink, "planner".to_string(), Some(RAW.to_string()))
        .await
        .unwrap();
    assert_eq!(r["changed"], false);
    assert_eq!(
        activation::load_journal(&dir)
            .entries
            .iter()
            .filter(|e| e.kind == "backend_set_raw_flags")
            .count(),
        1
    );
    // Warn, never block: a malformed value and an unrecognized
    // flag both save, with the warnings on the response.
    let r = op_backend_set_raw_flags(
        &state,
        &sink,
        "planner".to_string(),
        Some("--parallel abc --bogus-flag 3".to_string()),
    )
    .await
    .unwrap();
    assert_eq!(r["changed"], true);
    let warnings = r["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 2);
    assert!(warnings.iter().any(|w| w["token"] == "--parallel"));
    assert!(warnings.iter().any(|w| w["token"] == "--bogus-flag"));
    // An all-whitespace save clears the flags entirely.
    let r = op_backend_set_raw_flags(
        &state,
        &sink,
        "planner".to_string(),
        Some("   ".to_string()),
    )
    .await
    .unwrap();
    assert_eq!(r["changed"], true);
    assert_eq!(r["raw_flags"], Value::Null);
    let saved = manager_config::load(&state.config_path).unwrap();
    assert_eq!(
        saved
            .backends
            .iter()
            .find(|b| b.id == "planner")
            .unwrap()
            .raw_flags,
        None
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn structured_writers_refused_in_advanced_mode() {
    let mut cfg = test_cfg();
    for id in ["planner", "coder"] {
        cfg.backends
            .iter_mut()
            .find(|b| b.id == id)
            .unwrap()
            .flags_advanced = true;
    }
    // Give planner an MTP-capable model so the MTP refusal is
    // distinguishable from the capability refusal.
    serve_model(
        &mut cfg,
        "planner",
        "qwen3.8-27b-mtp",
        "models/Qwen3.8-27B-MTP.gguf",
        Some("--parallel 1"),
    );
    let (state, dir) = advanced_test_state(cfg);
    let sink = test_sink();
    let refused = |r: Result<Value, ApiError>| match r {
        Err(ApiError::InvalidInput(m)) => m.contains("advanced flags mode"),
        other => panic!("expected the advanced-mode refusal, got {other:?}"),
    };
    assert!(refused(
        op_backend_update_flags(&state, &sink, "planner".to_string(), 32768, 512).await
    ));
    assert!(refused(
        op_backend_set_offload(&state, &sink, "coder".to_string(), 4, None).await
    ));
    assert!(refused(
        op_backend_set_mtp(&state, &sink, "planner".to_string(), Some(2)).await
    ));
    assert!(refused(
        op_placement_apply(
            &state,
            &sink,
            "qwen3.8-27b-mtp".to_string(),
            vec![1.0],
            Some("planner".to_string()),
        )
        .await
    ));
    // The config patch path refuses a flags-field patch...
    assert!(refused(
        op_backend_apply_config(
            &state,
            &sink,
            "planner".to_string(),
            json!({ "n_ctx": 4096 }),
            "OnNextRestart".to_string(),
        )
        .await
    ));
    // ...but a non-flags patch still applies in the mode.
    let r = op_backend_apply_config(
        &state,
        &sink,
        "planner".to_string(),
        json!({ "enabled": false }),
        "OnNextRestart".to_string(),
    )
    .await
    .unwrap();
    assert_eq!(r["persisted"], true);
    // update_config cannot rewrite an advanced backend's flags.
    let mut v = serde_json::to_value(state.config.read().await.clone()).unwrap();
    for b in v["backends"].as_array_mut().unwrap().iter_mut() {
        if b["id"] == "coder" {
            b["raw_flags"] = json!("--threads 4");
        }
    }
    let patch = json!({ "backends": v["backends"].clone() });
    assert!(refused(op_update_config(&state, &sink, patch).await));
    // A benign whole-config patch (the supervision toggle the
    // Settings view sends) still succeeds.
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

#[tokio::test]
async fn advanced_save_then_structured_decode_round_trip() {
    let mut cfg = test_cfg();
    serve_model(
        &mut cfg,
        "planner",
        "qwen3.8-27b-mtp",
        "models/Qwen3.8-27B-MTP.gguf",
        None,
    );
    cfg.backends
        .iter_mut()
        .find(|b| b.id == "planner")
        .unwrap()
        .flags_advanced = true;
    let (state, dir) = advanced_test_state(cfg);
    let sink = test_sink();
    // A mix of modeled flags (offload, MTP, parallel) and a
    // foreign flag no control models (--threads).
    const RAW: &str =
        "--parallel 1 --spec-type draft-mtp --spec-draft-n-max 2 --threads 8 -ot exps=CPU";
    let r = op_backend_set_raw_flags(&state, &sink, "planner".to_string(), Some(RAW.to_string()))
        .await
        .unwrap();
    assert_eq!(r["changed"], true);
    assert!(r["warnings"].as_array().unwrap().is_empty());
    // Leaving the mode writes nothing: the string survives
    // byte-for-byte, foreign tokens included.
    let r = op_backend_set_flags_mode(&state, &sink, "planner".to_string(), false)
        .await
        .unwrap();
    assert_eq!(r["raw_flags"], RAW);
    let saved = manager_config::load(&state.config_path).unwrap();
    let b = saved.backends.iter().find(|b| b.id == "planner").unwrap();
    assert!(!b.flags_advanced);
    assert_eq!(b.raw_flags.as_deref(), Some(RAW));
    // The existing decoders read the saved string exactly as
    // the structured form renders it.
    assert_eq!(
        manager_models::expert_offload(b.raw_flags.as_deref()),
        manager_models::ExpertOffload::AllExperts
    );
    assert_eq!(
        manager_models::mtp_draft(b.raw_flags.as_deref()),
        manager_models::MtpDraft::On(Some(2))
    );
    assert_eq!(
        manager_models::parallel_slots(b.raw_flags.as_deref()),
        Some(1)
    );
    let info = backend_mtp_info(&saved, b).expect("planner-mtp is MTP-capable");
    assert!(info.enabled);
    assert_eq!(info.n_max, Some(2));
    assert_eq!(info.parallel, Some(1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn validate_ctx_batch_matches_update_flags_rules() {
    assert!(validate_ctx_batch(32768, 512).is_ok());
    assert!(validate_ctx_batch(1024, 1024).is_ok());
    assert!(validate_ctx_batch(1_048_576, 1).is_ok());
    // Below the context floor, above the ceiling, a zero
    // batch, and a batch past the context all refuse.
    assert!(validate_ctx_batch(1023, 512).is_err());
    assert!(validate_ctx_batch(1_048_577, 512).is_err());
    assert!(validate_ctx_batch(32768, 0).is_err());
    assert!(validate_ctx_batch(2048, 4096).is_err());
}

#[test]
fn validate_parallel_slots_matches_set_parallel_rules() {
    assert!(validate_parallel_slots(1).is_ok());
    assert!(validate_parallel_slots(4).is_ok());
    assert!(validate_parallel_slots(1024).is_ok());
    // Zero and past the control's ceiling refuse, with the
    // range named in the message (the flags form renders the
    // same bounds).
    assert!(validate_parallel_slots(0).is_err());
    let err = validate_parallel_slots(1025).expect_err("past the ceiling refuses");
    assert!(
        err.to_string()
            .contains("parallel slots must be between 1 and 1024"),
        "unexpected wording: {err}"
    );
}

#[test]
fn split_for_gpu_selection_composes_even_splits() {
    let (split, mode) = split_for_gpu_selection(&[0, 1], &[0, 1]).unwrap();
    assert_eq!(split, vec![0.5, 0.5]);
    assert_eq!(mode, manager_config::SplitMode::Layer);
    let (split, _) = split_for_gpu_selection(&[0, 1], &[1]).unwrap();
    assert_eq!(split, vec![0.0, 1.0]);
    let (split, mode) = split_for_gpu_selection(&[0], &[0]).unwrap();
    assert_eq!(split, vec![1.0]);
    assert_eq!(mode, manager_config::SplitMode::Layer);
    // No telemetry, an empty selection, and an unknown GPU
    // all refuse (placement_apply's validation, mirrored).
    assert!(split_for_gpu_selection(&[], &[0]).is_err());
    assert!(split_for_gpu_selection(&[0, 1], &[]).is_err());
    assert!(split_for_gpu_selection(&[0, 1], &[2]).is_err());
}

#[test]
fn resolve_new_backend_offload_applies_creation_rules() {
    let mut cfg = test_cfg();
    register_test_model(
        &mut cfg,
        "qwen3-coder-30b",
        "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
    );
    // The backend creation would build: unsplit (places on GPU
    // 0), raw flags at the catalog default (full offload).
    let backend = uncreated_backend(
        "coder-2",
        "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
        catalog_default_raw_flags("qwen3-coder-30b"),
    );
    // GPU 0 is a Pascal card: partial positions are blocked.
    let mut gpus = test_gpus();
    gpus[0].compute_capability = (6, 0);
    let journal = activation::ActivationJournal::default();

    // A blocked position without an override refuses, naming
    // the snap target.
    let err = resolve_new_backend_offload(&cfg, &backend, &gpus, &journal, 46, false)
        .err()
        .expect("blocked position refuses");
    assert!(
        format!("{err}").contains("outside the verified range"),
        "unexpected refusal: {err}"
    );

    // The same position with a confirmed override resolves and
    // asks for the override to be journaled.
    let resolved = resolve_new_backend_offload(&cfg, &backend, &gpus, &journal, 46, true).unwrap();
    assert_eq!(resolved.raw_flags.as_deref(), Some("--n-cpu-moe 46"));
    assert!(resolved.record_override);
    assert_eq!(resolved.placed_ccs, vec![(6, 0)]);

    // The catalog default position resolves to the catalog
    // default flags, with no override to record.
    let resolved = resolve_new_backend_offload(&cfg, &backend, &gpus, &journal, 48, false).unwrap();
    assert_eq!(
        resolved.raw_flags,
        catalog_default_raw_flags("qwen3-coder-30b")
    );
    assert!(!resolved.record_override);

    // No offload clears the tokens entirely.
    let resolved = resolve_new_backend_offload(&cfg, &backend, &gpus, &journal, 0, false).unwrap();
    assert_eq!(resolved.raw_flags, None);

    // Past the model's MoE layer count refuses.
    assert!(resolve_new_backend_offload(&cfg, &backend, &gpus, &journal, 49, false).is_err());

    // A model with no catalog offload pin is not
    // offload-capable: any chosen position refuses.
    let mut cfg = test_cfg();
    register_test_model(&mut cfg, "qwen3-27b-planner", "models/planner.gguf");
    let backend = uncreated_backend("planner-2", "models/planner.gguf", None);
    let err = resolve_new_backend_offload(&cfg, &backend, &gpus, &journal, 1, false)
        .err()
        .expect("dense model refuses offload");
    assert!(
        format!("{err}").contains("does not support expert offload"),
        "unexpected refusal: {err}"
    );
}

#[test]
fn catalog_default_raw_flags_render_the_design_points() {
    assert_eq!(
        catalog_default_raw_flags("qwen3-coder-30b").as_deref(),
        Some("-ot exps=CPU")
    );
    assert_eq!(
        catalog_default_raw_flags("nemotron-3.5-lightning-30b").as_deref(),
        Some("--n-cpu-moe 16")
    );
    assert_eq!(catalog_default_raw_flags("qwen3-27b-planner"), None);
    assert_eq!(catalog_default_raw_flags("not-a-model"), None);
    // The seeded default fleet agrees with the catalog: the
    // coder backend's built-in raw flags are the catalog
    // default for its model.
    let cfg = test_cfg();
    let coder = backend(&cfg, "coder");
    assert_eq!(
        coder.raw_flags.as_deref(),
        catalog_default_raw_flags("qwen3-coder-30b").as_deref()
    );
}

#[test]
fn model_default_raw_flags_render_declared_defaults() {
    let mut cfg = test_cfg();
    register_test_model(&mut cfg, "user-moe-27b", "models/user-moe.gguf");
    let mut model = cfg.models.last().unwrap().clone();
    // No declaration: no default, exactly like a dense model.
    assert_eq!(model_default_raw_flags(&model), None);
    model.moe_layers = Some(48);
    model.default_offload_layers = Some(48);
    assert_eq!(
        model_default_raw_flags(&model).as_deref(),
        Some("-ot exps=CPU")
    );
    model.default_offload_layers = Some(16);
    assert_eq!(
        model_default_raw_flags(&model).as_deref(),
        Some("--n-cpu-moe 16")
    );
    // A declared default of 0 writes no tokens.
    model.default_offload_layers = Some(0);
    assert_eq!(model_default_raw_flags(&model), None);
    // Layers without a default cannot form a position.
    model.default_offload_layers = None;
    assert_eq!(model_default_raw_flags(&model), None);
}

#[test]
fn default_offload_never_lands_on_a_blocked_position() {
    let mut cfg = AppConfig::default_config();
    cfg.data_dir = std::env::temp_dir().join("llm-mgr-no-such-dir");
    let file = "models/nemotron.gguf";
    register_test_model(&mut cfg, "nemotron-3.5-lightning-30b", file);
    let default = catalog_default_raw_flags("nemotron-3.5-lightning-30b");
    let snapped = |cc: (u32, u32)| {
        let mut b = uncreated_backend("nemo", file, default.clone());
        let mut gpus = test_gpus();
        gpus[0].compute_capability = cc;
        snap_default_offload(&cfg, &mut b, &gpus);
        b.raw_flags
    };
    // The catalog default is a partial position: kept where it is
    // allowed, full offload on the P100.
    assert_eq!(snapped((8, 6)), default);
    assert_eq!(snapped((6, 0)).as_deref(), Some("-ot exps=CPU"));
}

/// D01: the restart of a RestartNow save is the normal restart, and its
/// task always settles (here it fails: there is no model file).
#[tokio::test]
async fn restart_now_settles_its_task() {
    let (state, dir) = advanced_test_state(test_cfg());
    let task_id = spawn_backend_restart(&state, &test_sink(), "planner").await;
    let settled = wait_task_settled(&state, &task_id);
    let task = tokio::time::timeout(Duration::from_secs(30), settled)
        .await
        .expect("settled")
        .expect("recorded");
    assert_eq!(task.kind, "backend_restart");
    assert_eq!(task.status, TaskStatus::Failed, "{:?}", task.detail);
    assert!(task.error.is_some());
    assert!(task.detail["error_kind"].is_string());
    let _ = std::fs::remove_dir_all(&dir);
}
