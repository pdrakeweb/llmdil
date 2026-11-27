use super::*;
use crate::test_support::*;
use manager_config::ColdTier;
use sha2::Digest;

fn sha_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A state over a temp data dir with one cold tier (a sibling temp
/// dir: same volume, which only the settings operation refuses) and
/// model `m1` at `models\M1.gguf` pinned to `content`'s hash.
fn tiered_state(content: &[u8]) -> (Arc<AppState>, PathBuf, PathBuf) {
    let mut cfg = test_cfg();
    register_test_model(&mut cfg, "m1", "models/M1.gguf");
    cfg.models.last_mut().unwrap().sha256 = sha_hex(content);
    let cold = unique_test_dir("llm-mgr-cold");
    std::fs::create_dir_all(&cold).unwrap();
    cfg.model_storage.cold_tiers = vec![ColdTier {
        path: cold.clone(),
        label: "test".into(),
    }];
    cfg.model_storage.copy_rate_mib_s = 0;
    let (state, dir) = advanced_test_state(cfg);
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(dir.join("models").join("M1.gguf"), content).unwrap();
    (state, dir, cold)
}

async fn wait_idle(state: &AppState) {
    for _ in 0..200 {
        if lock(&state.model_storage.active).is_none() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("move did not finish");
}

fn last_move(state: &AppState) -> MoveRecord {
    with_state(state, |s| s.moves.last().cloned().expect("a move record"))
}

#[test]
fn logical_paths_map_back_from_any_tier() {
    let mut cfg = test_cfg();
    cfg.model_storage.cold_tiers = vec![ColdTier {
        path: PathBuf::from(if cfg!(windows) { r"E:\cold" } else { "/cold" }),
        label: String::new(),
    }];
    let hot = cfg.data_dir.join("models").join("X.gguf");
    let cold = cfg.model_storage.cold_tiers[0].path.join("X.gguf");
    assert_eq!(
        logical_for_path(&cfg, &hot),
        Some(PathBuf::from("models").join("X.gguf"))
    );
    assert_eq!(
        logical_for_path(&cfg, &cold),
        Some(PathBuf::from("models").join("X.gguf"))
    );
    assert_eq!(
        logical_for_path(&cfg, &cfg.data_dir.join("other.gguf")),
        None
    );
}

#[test]
fn spec_paths_include_drafts_in_both_flag_forms() {
    let args: Vec<String> = [
        "-m",
        "/a.gguf",
        "--port",
        "1",
        "--spec-draft-model",
        "/d.gguf",
        "-md=/e.gguf",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(
        spec_model_paths(&args),
        vec![
            PathBuf::from("/a.gguf"),
            PathBuf::from("/d.gguf"),
            PathBuf::from("/e.gguf")
        ]
    );
}

#[test]
fn pins_come_from_enabled_backends_raw_flags_and_keep_hot() {
    let mut cfg = test_cfg();
    register_test_model(&mut cfg, "m1", "models/M1.gguf");
    register_test_model(&mut cfg, "draft", "models/D.gguf");
    let logical = PathBuf::from("models/M1.gguf");
    assert_eq!(pin_for(&cfg, Some("m1"), &logical), None);
    let b = cfg.backends.iter_mut().find(|b| b.id == "coder").unwrap();
    b.model_file = PathBuf::from(r"models\M1.gguf");
    b.enabled = false;
    assert_eq!(
        pin_for(&cfg, Some("m1"), &logical),
        None,
        "a disabled backend does not pin"
    );
    cfg.backends
        .iter_mut()
        .find(|b| b.id == "coder")
        .unwrap()
        .enabled = true;
    assert_eq!(
        pin_for(&cfg, Some("m1"), &logical),
        Some(PinReason::Backend("coder".into()))
    );
    let draft_abs = cfg.data_dir.join("models").join("D.gguf");
    cfg.backends
        .iter_mut()
        .find(|b| b.id == "coder")
        .unwrap()
        .raw_flags = Some(format!("--spec-draft-model {}", draft_abs.display()));
    assert_eq!(
        pin_for(&cfg, Some("draft"), Path::new("models/D.gguf")),
        Some(PinReason::RawFlags("coder".into()))
    );
    cfg.model_storage.keep_hot = vec![r"models\Orphan.gguf".into()];
    assert_eq!(
        pin_for(&cfg, None, Path::new("models/orphan.gguf")),
        Some(PinReason::KeepHot)
    );
}

#[tokio::test]
async fn demote_promote_and_instant_re_demote_keep_the_gate_meaningful() {
    let content = b"gguf bytes for the tier round trip".to_vec();
    let (state, dir, cold) = tiered_state(&content);
    let sink = test_sink();
    let hot_file = dir.join("models").join("M1.gguf");
    let cold_file = cold.join("M1.gguf");

    // Demote: copy hot -> cold (verified against the pin), delete hot.
    op_model_storage_demote(&state, &sink, "m1".into())
        .await
        .unwrap();
    wait_idle(&state).await;
    let mv = last_move(&state);
    assert_eq!(mv.outcome, MoveOutcome::Done, "{mv:?}");
    assert!(!mv.instant);
    assert!(!hot_file.exists());
    assert_eq!(std::fs::read(&cold_file).unwrap(), content);
    {
        let cfg = state.config.read().await;
        let m = cfg.models.iter().find(|m| m.id == "m1").unwrap();
        assert!(
            m.verified_at.is_some(),
            "a verified move re-stamps verified_at"
        );
        assert_eq!(
            m.verified_sha256.as_deref(),
            Some(sha_hex(&content).as_str())
        );
        // The launch path now resolves to the cold copy.
        let mut b = cfg.backends[0].clone();
        b.model_file = PathBuf::from("models/M1.gguf");
        let spec = crate::backend_spec(&cfg, &b, &[]);
        let i = spec.args.iter().position(|a| a == "-m").unwrap();
        assert_eq!(PathBuf::from(&spec.args[i + 1]), cold_file);
    }
    // list_models reports the tier.
    let listed = crate::op_list_models(&state, &sink).await.unwrap();
    let row = listed["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "m1")
        .unwrap()
        .clone();
    assert_eq!(row["on_disk"], true);
    assert_eq!(row["storage"]["tier"], "cold");
    assert_eq!(row["storage"]["archived"], true);

    // Promote: copy cold -> hot; the cold copy stays as the archive.
    op_model_storage_promote(&state, &sink, "m1".into())
        .await
        .unwrap();
    wait_idle(&state).await;
    assert_eq!(last_move(&state).outcome, MoveOutcome::Done);
    assert_eq!(std::fs::read(&hot_file).unwrap(), content);
    assert!(cold_file.exists(), "promotion keeps the cold archive");

    // Re-demote: the archive is intact, so it is an instant delete.
    op_model_storage_demote(&state, &sink, "m1".into())
        .await
        .unwrap();
    wait_idle(&state).await;
    let mv = last_move(&state);
    assert_eq!(mv.outcome, MoveOutcome::Done);
    assert!(mv.instant, "{mv:?}");
    assert!(!hot_file.exists());
    assert!(cold_file.exists());
}

#[tokio::test]
async fn a_source_that_fails_its_pin_is_kept_and_gated() {
    let (state, dir, cold) = tiered_state(b"original bytes");
    std::fs::write(dir.join("models").join("M1.gguf"), b"corrupted bytes").unwrap();
    let sink = test_sink();
    op_model_storage_demote(&state, &sink, "m1".into())
        .await
        .unwrap();
    wait_idle(&state).await;
    let mv = last_move(&state);
    assert_eq!(mv.outcome, MoveOutcome::Failed);
    assert!(dir.join("models").join("M1.gguf").exists(), "source kept");
    assert!(!cold.join("M1.gguf").exists());
    assert!(!cold.join("M1.gguf.partial").exists());
    let outcome = state.downloads.outcomes.lock().await.get("m1").cloned();
    assert!(outcome.map(|o| o.verification_failure).unwrap_or(false));
}

#[tokio::test]
async fn unregistered_files_move_by_logical_path() {
    let (state, dir, cold) = tiered_state(b"x");
    std::fs::write(dir.join("models").join("Orphan.gguf"), b"orphan bytes").unwrap();
    let sink = test_sink();
    let status = op_model_storage_status(&state, &sink).await.unwrap();
    assert!(status["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["registered"] == false && f["tier"] == "hot"));
    op_model_storage_demote(&state, &sink, r"models\Orphan.gguf".into())
        .await
        .unwrap();
    wait_idle(&state).await;
    assert_eq!(last_move(&state).outcome, MoveOutcome::Done);
    assert_eq!(
        std::fs::read(cold.join("Orphan.gguf")).unwrap(),
        b"orphan bytes"
    );
    assert!(!dir.join("models").join("Orphan.gguf").exists());
}

#[tokio::test]
async fn pinned_models_refuse_manual_demotion() {
    let (state, _dir, _cold) = tiered_state(b"x");
    {
        let mut cfg = state.config.write().await;
        let b = cfg.backends.iter_mut().find(|b| b.id == "coder").unwrap();
        b.model_file = PathBuf::from("models/M1.gguf");
        b.enabled = true;
    }
    let err = op_model_storage_demote(&state, &test_sink(), "m1".into())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("coder"), "{err}");
    assert!(lock(&state.model_storage.active).is_none());
}

#[tokio::test]
async fn start_preflight_names_an_unavailable_cold_tier() {
    let (state, dir, _cold) = tiered_state(b"x");
    std::fs::remove_file(dir.join("models").join("M1.gguf")).unwrap();
    std::fs::create_dir_all(dir.join("llamacpp")).unwrap();
    let exe = if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    };
    std::fs::write(dir.join("llamacpp").join(exe), b"").unwrap();
    let missing = unique_test_dir("llm-mgr-cold-gone");
    let cfg = {
        let mut cfg = state.config.write().await;
        cfg.model_storage.cold_tiers[0].path = missing.clone();
        cfg.clone()
    };
    let mut b = cfg
        .backends
        .iter()
        .find(|b| b.id == "coder")
        .unwrap()
        .clone();
    b.model_file = PathBuf::from("models/M1.gguf");
    b.enabled = true;
    b.raw_flags = None;
    let err = crate::backend_start_preflight_for(&state, &cfg, &b, false)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not available"), "{err}");
    assert!(err.contains(&missing.display().to_string()), "{err}");
}

#[tokio::test]
async fn delete_removes_every_tier_copy() {
    let (state, dir, cold) = tiered_state(b"x");
    std::fs::write(cold.join("M1.gguf"), b"x").unwrap();
    let r = crate::op_model_delete(&state, &test_sink(), "m1".into())
        .await
        .unwrap();
    assert_eq!(r["deleted"], true);
    assert!(!dir.join("models").join("M1.gguf").exists());
    assert!(!cold.join("M1.gguf").exists());
}

#[tokio::test]
async fn downloads_land_where_the_file_is_else_in_the_download_tier() {
    let (state, dir, cold) = tiered_state(b"x");
    let logical = PathBuf::from("models/M1.gguf");
    let mut cfg = state.config.read().await.clone();
    assert_eq!(
        download_destination(&cfg, &logical),
        dir.join("models").join("M1.gguf")
    );
    std::fs::remove_file(dir.join("models").join("M1.gguf")).unwrap();
    cfg.model_storage.download_tier = manager_config::DownloadTier::Cold;
    assert_eq!(download_destination(&cfg, &logical), cold.join("M1.gguf"));
    cfg.model_storage.download_tier = manager_config::DownloadTier::Hot;
    assert_eq!(
        download_destination(&cfg, &logical),
        dir.join("models").join("M1.gguf")
    );
}

#[tokio::test]
async fn settings_refuse_a_cold_tier_on_the_data_volume_and_a_dropped_tier_with_files() {
    let (state, _dir, cold) = tiered_state(b"x");
    let cfg = state.config.read().await.clone();
    // Same volume as the temp data dir.
    let mut next = cfg.model_storage.clone();
    next.cold_tiers[0].path = unique_test_dir("llm-mgr-cold-same");
    let err = check_storage_paths(&cfg, &next).unwrap_err().to_string();
    assert!(err.contains("same volume"), "{err}");
    // Dropping a tier that still holds a model file.
    std::fs::write(cold.join("Kept.gguf"), b"k").unwrap();
    let mut next = cfg.model_storage.clone();
    next.cold_tiers.clear();
    let err = check_storage_paths(&cfg, &next).unwrap_err().to_string();
    assert!(err.contains("still holds"), "{err}");
}

#[tokio::test]
async fn a_spawn_records_a_start_of_its_model() {
    let (state, _dir, _cold) = tiered_state(b"x");
    let cfg = state.config.read().await.clone();
    let mut b = cfg.backends[0].clone();
    b.id = "bench-1".into();
    b.model_file = PathBuf::from("models/M1.gguf");
    {
        let mut sup = state.supervisor().await.lock().await;
        sup.register_spec(crate::backend_spec(&cfg, &b, &[]));
    }
    note_process_start(&state, "bench-1").await;
    let st = last_started(&state, Path::new("models/M1.gguf"));
    assert!(st.is_some());
    assert_eq!(
        with_state(&state, |s| s.recent_starts(
            Path::new("models/M1.gguf"),
            Utc::now(),
            14
        )),
        1
    );
}

#[tokio::test]
async fn startup_sweeps_partials_and_marks_interrupted_moves() {
    let (state, dir, cold) = tiered_state(b"x");
    std::fs::write(cold.join("M1.gguf.partial"), b"half").unwrap();
    std::fs::write(dir.join("models").join("Z.gguf.partial"), b"half").unwrap();
    let mut s = StorageState::default();
    s.push_move(MoveRecord {
        id: "mv-1".into(),
        logical: PathBuf::from("models/M1.gguf"),
        kind: "demote".into(),
        from: "hot".into(),
        to: "cold".into(),
        reason: "idle".into(),
        started_at: Utc::now(),
        finished_at: None,
        outcome: MoveOutcome::Running,
        error: None,
        bytes: 1,
        instant: false,
    });
    s.save(&dir.join(STATE_FILE)).unwrap();
    startup(&state).await;
    assert!(!cold.join("M1.gguf.partial").exists());
    assert!(!dir.join("models").join("Z.gguf.partial").exists());
    assert_eq!(last_move(&state).outcome, MoveOutcome::Interrupted);
}

#[tokio::test]
async fn a_tier_dropped_from_the_config_is_reported_while_its_files_remain() {
    // The 2026-11-26 incident: a build without tiered storage saved the
    // config and the cold tier's section vanished, so the demoted models
    // looked missing with nothing saying why.
    let content = b"gguf bytes for the lost tier".to_vec();
    let (state, _dir, cold) = tiered_state(&content);
    let sink = test_sink();
    op_model_storage_demote(&state, &sink, "m1".into())
        .await
        .unwrap();
    wait_idle(&state).await;
    assert_eq!(last_move(&state).outcome, MoveOutcome::Done);

    let status = op_model_storage_status(&state, &sink).await.unwrap();
    assert_eq!(
        status["lost_tiers"],
        json!([]),
        "a configured tier is not lost"
    );

    state.config.write().await.model_storage.cold_tiers.clear();
    let status = op_model_storage_status(&state, &sink).await.unwrap();
    let lost = status["lost_tiers"].as_array().unwrap();
    assert_eq!(lost.len(), 1, "{status}");
    assert_eq!(
        path_key(Path::new(lost[0]["path"].as_str().unwrap())),
        path_key(&cold)
    );
    assert_eq!(lost[0]["files"], json!(1));
    assert_eq!(lost[0]["bytes"], json!(content.len()));

    // Once the files are gone (a tier emptied on purpose), no warning.
    std::fs::remove_file(cold.join("M1.gguf")).unwrap();
    let status = op_model_storage_status(&state, &sink).await.unwrap();
    assert_eq!(status["lost_tiers"], json!([]));
}

#[test]
fn archive_roots_strip_the_relative_name() {
    let root = unique_test_dir("llm-mgr-root");
    let p = root.join("sub").join("X.gguf");
    assert_eq!(
        root_of(&p, &PathBuf::from("sub").join("x.GGUF")),
        Some(root.clone())
    );
    assert_eq!(root_of(&p, Path::new("Y.gguf")), None);
}
