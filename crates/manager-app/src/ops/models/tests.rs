use super::*;
use crate::test_support::*;

#[tokio::test]
async fn model_assign_leaves_advanced_backends_flags_untouched() {
    let nemotron = manager_config::ModelConfig {
        id: "nemotron-3.5-lightning-30b".to_string(),
        gguf_path: PathBuf::from("models/nemotron.gguf"),
        quant: "Q4_K_M".to_string(),
        params_b: 30.0,
        sha256: "a".repeat(64),
        verified_at: None,
        verified_sha256: None,
        source_url: String::new(),
        assigned_backends: Vec::new(),
        enabled: true,
        display_name: None,
        moe_layers: None,
        default_offload_layers: None,
        mtp: None,
    };
    // Advanced: the assignment proceeds, but neither the
    // catalog raw-flags fill (the backend's blank flags would
    // take Nemotron's --n-cpu-moe 16 in structured mode) nor
    // the split application touches the backend's flags.
    let mut cfg = test_cfg();
    cfg.models.push(nemotron.clone());
    {
        let coder = cfg.backends.iter_mut().find(|b| b.id == "coder").unwrap();
        coder.flags_advanced = true;
        coder.raw_flags = Some("  ".to_string());
        coder.flags.tensor_split = Vec::new();
    }
    let (state, dir) = advanced_test_state(cfg);
    // Pin an empty telemetry store: op_model_assign skips its
    // split-vs-GPU-count check when no telemetry has polled (its
    // documented no-telemetry path). Against live host telemetry
    // the fixed [1.0] split is rejected on multi-GPU machines,
    // which is unrelated to the flags behavior under test.
    let _ = state
        .telemetry
        .set(Arc::new(std::sync::Mutex::new(TelemetryStore::new())));
    let sink = test_sink();
    let r = op_model_assign(
        &state,
        &sink,
        "nemotron-3.5-lightning-30b".to_string(),
        "coder".to_string(),
        vec![1.0],
    )
    .await
    .unwrap();
    assert_eq!(r["flags_untouched"], true);
    let saved = manager_config::load(&state.config_path).unwrap();
    let coder = saved.backends.iter().find(|b| b.id == "coder").unwrap();
    assert_eq!(coder.model_file, PathBuf::from("models/nemotron.gguf"));
    assert_eq!(coder.raw_flags.as_deref(), Some("  "));
    assert!(coder.flags.tensor_split.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
    // Structured control case: the same assignment fills the
    // catalog default and applies the split. (split_mode is
    // preset to Layer: a bare [1.0] split under mode none is
    // invalid config, unrelated to this test.)
    let mut cfg = test_cfg();
    cfg.models.push(nemotron);
    {
        let coder = cfg.backends.iter_mut().find(|b| b.id == "coder").unwrap();
        coder.raw_flags = Some("  ".to_string());
        coder.flags.tensor_split = Vec::new();
        coder.flags.split_mode = manager_config::SplitMode::Layer;
    }
    let (state, dir) = advanced_test_state(cfg);
    // Same empty-telemetry pin as the advanced case above.
    let _ = state
        .telemetry
        .set(Arc::new(std::sync::Mutex::new(TelemetryStore::new())));
    let r = op_model_assign(
        &state,
        &sink,
        "nemotron-3.5-lightning-30b".to_string(),
        "coder".to_string(),
        vec![1.0],
    )
    .await
    .unwrap();
    assert_eq!(r["flags_untouched"], false);
    let saved = manager_config::load(&state.config_path).unwrap();
    let coder = saved.backends.iter().find(|b| b.id == "coder").unwrap();
    assert_eq!(coder.raw_flags.as_deref(), Some("--n-cpu-moe 16"));
    assert_eq!(coder.flags.tensor_split, vec![1.0]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn model_assign_failure_leaves_config_and_journal_untouched() {
    // Code review B-4: op_model_assign used to mutate the live
    // config ahead of its fallible steps, so a failure after the
    // mutation point left the in-memory config half-migrated —
    // and the next unrelated save persisted it with no journal
    // entry. Here apply_placement rejects the split (sums to
    // 0.5) after model_file was set and the catalog raw-flags
    // fill was written: exactly the traced half-migration. The
    // op now mutates a clone and commits only after placement,
    // validation, journaling, and the save all succeed.
    let nemotron = manager_config::ModelConfig {
        id: "nemotron-3.5-lightning-30b".to_string(),
        gguf_path: PathBuf::from("models/nemotron.gguf"),
        quant: "Q4_K_M".to_string(),
        params_b: 30.0,
        sha256: "a".repeat(64),
        verified_at: None,
        verified_sha256: None,
        source_url: String::new(),
        assigned_backends: Vec::new(),
        enabled: true,
        display_name: None,
        moe_layers: None,
        default_offload_layers: None,
        mtp: None,
    };
    let mut cfg = test_cfg();
    cfg.models.push(nemotron);
    {
        let coder = cfg.backends.iter_mut().find(|b| b.id == "coder").unwrap();
        // Blank flags: the assignment would fill the catalog
        // default (--n-cpu-moe 16) before placement fails.
        coder.raw_flags = Some("  ".to_string());
        coder.flags.tensor_split = Vec::new();
        coder.flags.split_mode = manager_config::SplitMode::Layer;
    }
    let (state, dir) = advanced_test_state(cfg);
    // Hermetic telemetry (no GPUs): against live host telemetry
    // the [1.0] split is refused on multi-GPU machines, which
    // made this test flaky depending on poll timing.
    let _ = state
        .telemetry
        .set(Arc::new(std::sync::Mutex::new(TelemetryStore::new())));
    let sink = test_sink();
    let before = serde_json::to_value(state.config.read().await.clone()).unwrap();
    let r = op_model_assign(
        &state,
        &sink,
        "nemotron-3.5-lightning-30b".to_string(),
        "coder".to_string(),
        vec![0.5],
    )
    .await;
    assert!(r.is_err(), "a split summing to 0.5 must be refused: {r:?}");
    let after = serde_json::to_value(state.config.read().await.clone()).unwrap();
    assert_eq!(
        before, after,
        "a failed assign must leave the live config byte-identical"
    );
    assert!(
        activation::load_journal(&state.data_dir).entries.is_empty(),
        "a failed assign must not journal"
    );
    // The state is not wedged: a valid assign right after the
    // failure succeeds and persists in full.
    let r = op_model_assign(
        &state,
        &sink,
        "nemotron-3.5-lightning-30b".to_string(),
        "coder".to_string(),
        vec![1.0],
    )
    .await
    .unwrap();
    assert_eq!(r["persisted"], true);
    let saved = manager_config::load(&state.config_path).unwrap();
    let coder = saved.backends.iter().find(|b| b.id == "coder").unwrap();
    assert_eq!(coder.model_file, PathBuf::from("models/nemotron.gguf"));
    assert_eq!(coder.raw_flags.as_deref(), Some("--n-cpu-moe 16"));
    assert_eq!(coder.flags.tensor_split, vec![1.0]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn model_assign_to_unregistered_model_fails_and_leaves_prior_config_intact() {
    // Tracker #54 apply-dependency guard, execution layer:
    // the Optimize apply orchestrator executes a
    // backend.set_model through model_assign. An assign
    // naming a model that is not registered — the state a
    // dependent set_model is in when its chained model.add
    // was denied or failed — is refused, and the backend's
    // prior configuration (the model it was serving) is
    // left exactly intact: no live-config mutation, no
    // persisted change, no journal entry.
    let cfg = test_cfg();
    let (state, dir) = advanced_test_state(cfg);
    // Pin an empty telemetry store, as the other
    // model_assign tests do: with no GPU count polled, the
    // split check is skipped and the refusal under test is
    // the unregistered-model lookup itself.
    let _ = state
        .telemetry
        .set(Arc::new(std::sync::Mutex::new(TelemetryStore::new())));
    let sink = test_sink();
    let before = serde_json::to_value(state.config.read().await.clone()).unwrap();
    let prior_model_file = state
        .config
        .read()
        .await
        .backends
        .iter()
        .find(|b| b.id == "coder")
        .unwrap()
        .model_file
        .clone();
    let r = op_model_assign(
        &state,
        &sink,
        "model-that-was-never-registered".to_string(),
        "coder".to_string(),
        vec![],
    )
    .await;
    let err = r.expect_err("assigning an unregistered model must be refused");
    assert!(
        matches!(&err, ApiError::NotFound(m) if m.contains("model-that-was-never-registered")),
        "refusal must name the unregistered model: {err}"
    );
    let after_cfg = state.config.read().await.clone();
    assert_eq!(
        serde_json::to_value(after_cfg.clone()).unwrap(),
        before,
        "a refused assign must leave the live config byte-identical"
    );
    assert_eq!(
        after_cfg
            .backends
            .iter()
            .find(|b| b.id == "coder")
            .unwrap()
            .model_file,
        prior_model_file,
        "the backend keeps serving its prior model after a refused swap"
    );
    assert!(
        activation::load_journal(&state.data_dir).entries.is_empty(),
        "a refused assign must not journal"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// PS-1 (docs/code-review-v0.1.60.md): `op_download_model` must
/// not hold the `downloads` lock while it waits for
/// `config.write()`. `op_list_models` takes `config.read()` then
/// `downloads`, so holding both in the opposite order deadlocked the
/// app. The test parks the download op on the supervisor lock (it
/// then holds `downloads`), takes a config read guard the way
/// list_models does, releases the supervisor, and asserts the
/// `downloads` lock becomes available while the read guard is still
/// held — i.e. the op released it before asking for the write lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn download_model_releases_downloads_lock_before_config_write() {
    let mut cfg = test_cfg();
    register_test_model(&mut cfg, "deadlock-probe", "models/deadlock-probe.gguf");
    cfg.models.last_mut().unwrap().source_url =
        "https://127.0.0.1:1/deadlock-probe.gguf".to_string();
    let (state, dir) = advanced_test_state(cfg);
    // Seed the catalog first so the op's own seeding pass is a
    // no-op write that cannot interleave with the guards below.
    ensure_catalog_models(&state).await;
    let sup_guard = state.supervisor().await.lock().await;
    let op_state = state.clone();
    let op = tokio::spawn(async move {
        let sink = test_sink();
        op_download_model(&op_state, &sink, "deadlock-probe".to_string()).await
    });
    // Wait until the op holds `downloads` (it is parked on the
    // supervisor lock we hold).
    let mut parked = false;
    for _ in 0..200 {
        if state.downloads.try_lock().is_err() {
            parked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(parked, "download op never took the downloads lock");
    let read_guard = state.config.read().await;
    drop(sup_guard);
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), state.downloads.lock()).await;
    let released = got.is_ok();
    drop(got);
    drop(read_guard);
    let r = tokio::time::timeout(std::time::Duration::from_secs(10), op)
        .await
        .expect("download op finished")
        .unwrap();
    assert!(
        released,
        "op_download_model held `downloads` while waiting for config.write()"
    );
    assert!(r.is_ok(), "{r:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn model_add_entry_unknown_size_is_a_clean_error() {
    // Tracker #32: an unknown-size model used to be stored with
    // params_b 0.0, which fails config validation. The add must
    // now refuse up front, naming what is missing.
    let r = model_add_entry(
        "obscure-1b",
        "https://example.com/obscure.gguf",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("params_b"), "error names the gap: {msg}");
            assert!(msg.contains("obscure-1b"), "error names the model: {msg}");
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }
    // A non-positive explicit value counts as "not supplied".
    let r = model_add_entry(
        "obscure-1b",
        "https://example.com/obscure.gguf",
        None,
        None,
        Some(0.0),
        None,
        None,
        None,
        None,
    );
    assert!(matches!(r, Err(ApiError::InvalidInput(_))), "{r:?}");
}

#[test]
fn model_add_entry_uses_explicit_params() {
    // Known-size add: behavior unchanged from before the fix —
    // enabled, unpinned hash, file name from the URL, quant
    // defaulting to "unknown".
    let e = model_add_entry(
        "my-7b",
        "https://example.com/my-7b-q4.gguf",
        None,
        None,
        Some(7.5),
        None,
        None,
        None,
        None,
    )
    .expect("explicit params resolve");
    assert_eq!(e.id, "my-7b");
    assert_eq!(e.params_b, 7.5);
    assert_eq!(e.gguf_path, PathBuf::from("models").join("my-7b-q4.gguf"));
    assert_eq!(e.quant, "unknown");
    assert!(e.enabled);
    assert!(e.sha256.is_empty());
    // The built entry passes config validation (the defect was
    // an entry that could not).
    let mut cfg = test_cfg();
    cfg.models.push(e);
    manager_config::validate(&cfg).expect("added entry validates");
    // An explicit value wins over the catalog.
    let e = model_add_entry(
        "qwen3-8b",
        "https://example.com/x.gguf",
        None,
        None,
        Some(9.0),
        None,
        None,
        None,
        None,
    )
    .expect("explicit params resolve");
    assert_eq!(e.params_b, 9.0);
}

#[test]
fn model_add_entry_resolves_params_from_catalog() {
    // By id.
    let e = model_add_entry(
        "qwen3-8b",
        "https://example.com/x.gguf",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("catalog id resolves");
    assert_eq!(e.params_b, 8.0);
    // By exact source URL under an operator-chosen id.
    let e = model_add_entry(
        "my-qwen3-8b",
        "https://huggingface.co/Qwen/Qwen3-8B-GGUF/resolve/main/Qwen3-8B-Q4_K_M.gguf",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("catalog URL resolves");
    assert_eq!(e.params_b, 8.0);
}

#[test]
fn model_add_entry_stores_and_validates_the_moe_declaration() {
    let e = model_add_entry(
        "moe-27b",
        "https://example.com/moe.gguf",
        None,
        None,
        Some(27.0),
        Some(48),
        Some(16),
        None,
        None,
    )
    .expect("declared pair resolves");
    assert_eq!(e.moe_layers, Some(48));
    assert_eq!(e.default_offload_layers, Some(16));
    let mut cfg = test_cfg();
    cfg.models.push(e);
    manager_config::validate(&cfg).expect("declared entry validates");
    // The pair is atomic at the write path: half a
    // declaration, an over-range default, and a zero count
    // all refuse.
    for (layers, default) in [
        (Some(48), None),
        (None, Some(16)),
        (Some(48), Some(49)),
        (Some(0), Some(0)),
    ] {
        assert!(
            matches!(
                model_add_entry(
                    "moe-27b",
                    "https://example.com/moe.gguf",
                    None,
                    None,
                    Some(27.0),
                    layers,
                    default,
                    None,
                    None,
                ),
                Err(ApiError::InvalidInput(_))
            ),
            "{layers:?} / {default:?} must refuse"
        );
    }
}

#[test]
fn model_add_entry_stores_the_mtp_declaration() {
    let e = model_add_entry(
        "mtp-27b",
        "https://example.com/mtp.gguf",
        None,
        None,
        Some(27.0),
        None,
        None,
        Some(true),
        None,
    )
    .expect("declared entry resolves");
    assert_eq!(e.mtp, Some(true));
    let mut cfg = test_cfg();
    cfg.models.push(e);
    manager_config::validate(&cfg).expect("declared entry validates");
    // Undeclared stays undeclared (the field is omitted
    // from the stored config, not written as false).
    let e = model_add_entry(
        "dense-7b",
        "https://example.com/dense.gguf",
        None,
        None,
        Some(7.0),
        None,
        None,
        None,
        None,
    )
    .expect("undeclared entry resolves");
    assert_eq!(e.mtp, None);
}

#[test]
fn model_add_entry_stores_a_valid_sha256_pin() {
    // P2-D6: an HF-picked add passes the sibling's
    // lfs.sha256 and the record starts pinned, so the
    // download path verifies against it (download_spec_for
    // treats a real pin as authoritative).
    let pin = "b".repeat(64);
    let e = model_add_entry(
        "picked-7b",
        "https://huggingface.co/org/picked/resolve/main/picked-7b.gguf",
        None,
        None,
        Some(7.0),
        None,
        None,
        None,
        Some(pin.clone()),
    )
    .expect("pinned entry resolves");
    assert_eq!(e.sha256, pin);
    let mut cfg = test_cfg();
    cfg.models.push(e);
    manager_config::validate(&cfg).expect("pinned entry validates");
    // No pin supplied: the record starts unpinned,
    // exactly as before Part 2.
    let e = model_add_entry(
        "plain-7b",
        "https://example.com/plain.gguf",
        None,
        None,
        Some(7.0),
        None,
        None,
        None,
        None,
    )
    .expect("unpinned entry resolves");
    assert!(e.sha256.is_empty());
}

#[test]
fn model_add_entry_refuses_a_malformed_sha256() {
    // §P2.12: a malformed hash is refused at the add —
    // a short or non-hex "pin" would verify nothing.
    for bad in ["abc", "z".repeat(64).as_str(), "a".repeat(63).as_str()] {
        assert!(
            matches!(
                model_add_entry(
                    "picked-7b",
                    "https://example.com/picked.gguf",
                    None,
                    None,
                    Some(7.0),
                    None,
                    None,
                    None,
                    Some(bad.to_string()),
                ),
                Err(ApiError::InvalidInput(_))
            ),
            "malformed pin {bad:?} must refuse"
        );
    }
}

#[test]
fn model_add_entry_keeps_the_existing_input_rules() {
    assert!(matches!(
        model_add_entry(
            "  ",
            "https://example.com/x.gguf",
            None,
            None,
            Some(1.0),
            None,
            None,
            None,
            None
        ),
        Err(ApiError::InvalidInput(_))
    ));
    assert!(matches!(
        model_add_entry(
            "m",
            "http://example.com/x.gguf",
            None,
            None,
            Some(1.0),
            None,
            None,
            None,
            None
        ),
        Err(ApiError::InvalidInput(_))
    ));
    assert!(matches!(
        model_add_entry(
            "m",
            "https://example.com/x.gguf",
            Some("sub/dir.gguf".to_string()),
            None,
            Some(1.0),
            None,
            None,
            None,
            None,
        ),
        Err(ApiError::InvalidInput(_))
    ));
}

#[test]
fn model_add_save_step_never_persists_an_invalid_config() {
    // The command's persistence shape: push onto a CLONE, save
    // (which validates first), swap into state only on success.
    // With a hand-built invalid entry (params_b 0.0 — what the
    // old code produced), the save must fail and both the file
    // on disk and the live config must stay exactly as they were.
    let dir = std::env::temp_dir().join(format!("model-add-save-step-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let cfg = test_cfg();
    manager_config::save(&cfg, &path).expect("baseline config saves");
    let before = std::fs::read(&path).unwrap();

    let mut new_cfg = cfg.clone();
    new_cfg.models.push(manager_config::ModelConfig {
        id: "bad-0b".to_string(),
        gguf_path: PathBuf::from("models").join("bad.gguf"),
        quant: "unknown".to_string(),
        params_b: 0.0,
        sha256: String::new(),
        verified_at: None,
        verified_sha256: None,
        source_url: "https://example.com/bad.gguf".to_string(),
        assigned_backends: Vec::new(),
        enabled: true,
        display_name: None,
        moe_layers: None,
        default_offload_layers: None,
        mtp: None,
    });
    assert!(manager_config::save(&new_cfg, &path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before, "file untouched");
    assert!(
        !cfg.models.iter().any(|m| m.id == "bad-0b"),
        "live config untouched (the entry only ever existed on the clone)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
