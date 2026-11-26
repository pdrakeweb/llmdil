use super::*;

#[tokio::test]
async fn detect_hardware_reports_gpus() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus());
    let ctx = ctx_with(ops, RunMode::Install);
    let step = DetectHardware;
    assert!(step.is_check());
    assert!(step.prerequisites().is_empty());
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done);
    assert!(out.message.contains("3 GPU(s)"), "{}", out.message);
    assert_eq!(out.log_tail.len(), 3);
    assert!(out.log_tail[1].contains("sm_60"));
}

#[tokio::test]
async fn detect_hardware_fails_without_gpus() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(vec![]);
    let ctx = ctx_with(ops, RunMode::Install);
    let out = DetectHardware.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("no NVIDIA GPUs"));
}

#[test]
fn detect_hardware_offers_driver_download_url() {
    assert_eq!(
        DetectHardware.help_url(),
        Some(crate::NVIDIA_DRIVER_DOWNLOAD_URL)
    );
    assert!(crate::NVIDIA_DRIVER_DOWNLOAD_URL.starts_with("https://"));
    let hint = DetectHardware.remediation_hint().unwrap();
    assert!(hint.contains(crate::NVIDIA_DRIVER_DOWNLOAD_URL));
}

#[tokio::test]
async fn detect_hardware_rejects_old_driver() {
    let ops = Arc::new(FakeSystemOps::new());
    let mut gpus = test_gpus();
    for g in gpus.iter_mut() {
        g.driver_version = "471.11".to_string();
    }
    ops.with_gpus(gpus);
    let ctx = ctx_with(ops, RunMode::Install);
    let out = DetectHardware.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("too old"), "{}", out.message);
}

#[tokio::test]
async fn detect_hardware_probe_error_fails() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpu_error("nvidia-smi not found");
    let ctx = ctx_with(ops, RunMode::Install);
    let out = DetectHardware.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
}

// --- InstallLlamaCpp ---

#[tokio::test]
async fn install_llamacpp_happy_path_and_transparency() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus());
    let pin = test_pin();
    script_pin_downloads(&ops, &pin);
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = InstallLlamaCpp::with_pin(pin.clone());

    // Transparency: planned commands equal what will run — BOTH
    // archive downloads, binaries first.
    let planned = step.planned_commands(&ctx);
    assert_eq!(planned.len(), 2);
    assert_eq!(planned[0].argv[0], "download");
    assert_eq!(planned[0].argv[1], pin.download_url());
    assert!(
        planned[0].argv[1].contains("cuda-12.4"),
        "{:?}",
        planned[0].argv
    );
    assert_eq!(planned[1].argv[0], "download");
    assert_eq!(planned[1].argv[1], pin.runtime_download_url());

    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(out.message.contains("b9999"));

    // Both archives downloaded + extracted; marker records both.
    let archive = PathBuf::from("/data/llamacpp").join(&pin.asset);
    let runtime_archive = PathBuf::from("/data/llamacpp").join(&pin.runtime.asset);
    assert!(ops.file_bytes(&archive).is_some());
    assert!(ops.file_bytes(&runtime_archive).is_some());
    assert_eq!(ops.downloads_made().len(), 2);
    let marker_bytes = ops
        .file_bytes(&PathBuf::from("/data/llamacpp/.installed.json"))
        .expect("marker written");
    let marker: serde_json::Value = serde_json::from_slice(&marker_bytes).expect("marker is JSON");
    assert_eq!(marker["tag"], "b9999");
    assert_eq!(marker["asset"], pin.asset);
    assert_eq!(marker["sha256"], pin.sha256);
    assert_eq!(marker["runtime_asset"], pin.runtime.asset);
    assert_eq!(marker["runtime_sha256"], pin.runtime.sha256);
    assert_eq!(ops.extractions().len(), 2);
    assert_eq!(ops.extractions()[0].0, archive);
    assert_eq!(ops.extractions()[1].0, runtime_archive);
}

#[tokio::test]
async fn install_llamacpp_skips_redownload_when_verified() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus());
    let pin = test_pin();
    let archive = PathBuf::from("/data/llamacpp").join(&pin.asset);
    ops.with_file(archive.clone(), b"fake-zip-bytes".to_vec());
    let runtime_archive = PathBuf::from("/data/llamacpp").join(&pin.runtime.asset);
    ops.with_file(runtime_archive, b"fake-cudart-bytes".to_vec());
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done);
    assert!(ops.downloads_made().is_empty(), "no download needed");
    assert!(out
        .log_tail
        .iter()
        .any(|l| l.contains("already present with matching checksum")));
}

#[tokio::test]
async fn install_llamacpp_redownloads_only_the_missing_archive() {
    // Binaries archive already verified on disk; the runtime archive
    // is missing, so exactly one download happens (the runtime one).
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus());
    let pin = test_pin();
    let archive = PathBuf::from("/data/llamacpp").join(&pin.asset);
    ops.with_file(archive, b"fake-zip-bytes".to_vec());
    script_pin_downloads(&ops, &pin);
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = InstallLlamaCpp::with_pin(pin.clone())
        .run(&ctx)
        .await
        .unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let made = ops.downloads_made();
    assert_eq!(made.len(), 1, "{made:?}");
    assert_eq!(made[0].0, pin.runtime_download_url());
}

#[tokio::test]
async fn install_llamacpp_fails_closed_without_pin() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus());
    let ctx = ctx_with(ops, RunMode::Install);
    let out = InstallLlamaCpp::unconfigured().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("no llama.cpp build pinned"));
    // The message must state the real cause (a packaging error in the
    // build) and must NOT direct the operator to configure
    // updates.components — that path cannot supply a pin.
    let detail = out.log_tail.join("\n");
    assert!(detail.contains("packaging error"), "{detail}");
    assert!(!detail.contains("configure updates.components"), "{detail}");
}

#[tokio::test]
async fn install_llamacpp_rejects_cuda13_on_pascal() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus()); // includes P100s
    let pin = LlamaCppPin {
        tag: "b9999".to_string(),
        asset: "llama-b9999-bin-win-cuda-13.0-x64.zip".to_string(),
        sha256: sha256_hex(b"x"),
        runtime: LlamaCppAsset {
            asset: "cudart-llama-bin-win-cuda-12.4-x64.zip".to_string(),
            sha256: sha256_hex(b"y"),
        },
    };
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("cuda-13"), "{}", out.message);
    assert!(ops.downloads_made().is_empty(), "no download attempted");
}

#[tokio::test]
async fn install_llamacpp_rejects_cuda13_runtime_on_pascal() {
    // The Pascal rule covers BOTH archives: a cuda-12.4 binaries
    // archive paired with a cuda-13 runtime archive is still refused,
    // and the error names the runtime asset.
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus()); // includes P100s
    let pin = LlamaCppPin {
        tag: "b9999".to_string(),
        asset: "llama-b9999-bin-win-cuda-12.4-x64.zip".to_string(),
        sha256: sha256_hex(b"x"),
        runtime: LlamaCppAsset {
            asset: "cudart-llama-bin-win-cuda-13.0-x64.zip".to_string(),
            sha256: sha256_hex(b"y"),
        },
    };
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("cuda-13"), "{}", out.message);
    assert!(
        out.message
            .contains("cudart-llama-bin-win-cuda-13.0-x64.zip"),
        "{}",
        out.message
    );
    assert!(ops.downloads_made().is_empty(), "no download attempted");
}

#[tokio::test]
async fn install_llamacpp_allows_cuda13_without_pascal() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(vec![test_gpus()[0].clone()]); // A4000 only
    let pin = LlamaCppPin {
        tag: "b9999".to_string(),
        asset: "llama-b9999-bin-win-cuda-13.0-x64.zip".to_string(),
        sha256: sha256_hex(b"fake-zip-bytes"),
        runtime: LlamaCppAsset {
            asset: "cudart-llama-bin-win-cuda-12.4-x64.zip".to_string(),
            sha256: sha256_hex(b"fake-cudart-bytes"),
        },
    };
    script_pin_downloads(&ops, &pin);
    let ctx = ctx_with(ops, RunMode::Install);
    let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done);
}

#[tokio::test]
async fn install_llamacpp_checksum_mismatch_fails() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus());
    let pin = test_pin();
    ops.with_download(&pin.download_url(), b"tampered-bytes".to_vec());
    let ctx = ctx_with(ops, RunMode::Install);
    let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("checksum mismatch"));
}

#[tokio::test]
async fn install_llamacpp_audit_reverifies_and_detects_tamper() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus());
    let pin = test_pin();
    script_pin_downloads(&ops, &pin);
    let install_ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = InstallLlamaCpp::with_pin(pin.clone());
    assert_eq!(step.run(&install_ctx).await.unwrap().state, StepState::Done);

    // Audit passes on the pristine install.
    let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
    let out = step.run(&audit_ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done);

    // Tamper with the archive: audit must fail with a remediation hint.
    let archive = PathBuf::from("/data/llamacpp").join(&pin.asset);
    ops.with_file(archive, b"tampered".to_vec());
    let out = step.run(&audit_ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("checksum mismatch"));
    assert!(step.remediation_hint().unwrap().contains("re-download"));
}

#[tokio::test]
async fn install_llamacpp_audit_fails_on_runtime_checksum_mismatch() {
    // Audit verifies BOTH archives: a tampered runtime archive fails
    // even when the binaries archive is pristine, and the message
    // names the runtime archive.
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus());
    let pin = test_pin();
    script_pin_downloads(&ops, &pin);
    let install_ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = InstallLlamaCpp::with_pin(pin.clone());
    assert_eq!(step.run(&install_ctx).await.unwrap().state, StepState::Done);

    let runtime_archive = PathBuf::from("/data/llamacpp").join(&pin.runtime.asset);
    ops.with_file(runtime_archive, b"tampered-cudart".to_vec());
    let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
    let out = step.run(&audit_ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("checksum mismatch"), "{}", out.message);
    assert!(out.message.contains(&pin.runtime.asset), "{}", out.message);
}

#[test]
fn default_pin_is_verified_b11658_cuda124_pair() {
    let pin = LlamaCppPin::default_pin();
    assert_eq!(pin.tag, "b11658");
    assert_eq!(pin.asset, "llama-b11658-bin-win-cuda-12.4-x64.zip");
    assert_eq!(
        pin.sha256,
        "4268543cfffee11e62dbe1fb4ee907da508c2a70de7b845ed7b338bf8cabd76d"
    );
    assert_eq!(pin.runtime.asset, "cudart-llama-bin-win-cuda-12.4-x64.zip");
    assert_eq!(
        pin.runtime.sha256,
        "8c79a9b226de4b3cacfd1f83d24f962d0773be79f1e7b75c6af4ded7e32ae1d6"
    );
    for sha in [&pin.sha256, &pin.runtime.sha256] {
        assert_eq!(sha.len(), 64, "{sha}");
        assert!(
            sha.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{sha}"
        );
    }
    for name in [&pin.asset, &pin.runtime.asset] {
        assert!(name.contains("cuda-12.4"), "{name}");
        assert!(!name.contains("cuda-13"), "{name}");
    }
    assert_eq!(
            pin.download_url(),
            "https://github.com/ggml-org/llama.cpp/releases/download/b11658/llama-b11658-bin-win-cuda-12.4-x64.zip"
        );
    assert_eq!(
            pin.runtime_download_url(),
            "https://github.com/ggml-org/llama.cpp/releases/download/b11658/cudart-llama-bin-win-cuda-12.4-x64.zip"
        );
    // Regression for the shipped defect: the default step list must
    // carry the pin (it previously shipped unconfigured and step 2
    // could never pass).
    let ctx = WizardContext::new(
        AppConfig::default_config(),
        PathBuf::from("/data"),
        RunMode::Install,
        Arc::new(FakeSystemOps::new()),
    );
    let step = default_steps()
        .into_iter()
        .find(|s| s.id() == STEP_INSTALL_LLAMACPP)
        .expect("llamacpp step present in default steps");
    let planned = step.planned_commands(&ctx);
    assert_eq!(planned.len(), 2, "{planned:?}");
    assert!(
        planned[0].argv[1].contains("b11658"),
        "{:?}",
        planned[0].argv
    );
    assert!(
        planned[1].argv[1].contains("cudart"),
        "{:?}",
        planned[1].argv
    );
}

/// Context whose config pins `updates.components.llamacpp` to the
/// given version (or leaves the component unpinned when `None`).
fn ctx_with_llamacpp_pin(
    ops: Arc<FakeSystemOps>,
    mode: RunMode,
    pinned_version: Option<&str>,
) -> WizardContext {
    let mut cfg = test_config_with_models();
    cfg.updates.components.insert(
        "llamacpp".to_string(),
        ComponentUpdate {
            pinned_version: pinned_version.map(str::to_string),
            auto_update: false,
            last_checked: None,
        },
    );
    WizardContext::new(cfg, PathBuf::from("/data"), mode, ops)
}

#[tokio::test]
async fn install_llamacpp_config_version_mismatch_fails_closed() {
    // The config names a version this build carries no verified pin
    // for: refused before any download, naming both versions.
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus());
    let pin = test_pin(); // built-in tag b9999
    script_pin_downloads(&ops, &pin);
    let ctx = ctx_with_llamacpp_pin(ops.clone(), RunMode::Install, Some("b1111"));
    let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("b1111"), "{}", out.message);
    assert!(out.message.contains("b9999"), "{}", out.message);
    assert!(
        ops.downloads_made().is_empty(),
        "refused before any download"
    );
}

#[tokio::test]
async fn install_llamacpp_config_version_match_proceeds() {
    // A config pin naming exactly the built-in tag is a no-op guard:
    // the step proceeds with the verified pin.
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus());
    let pin = test_pin(); // built-in tag b9999
    script_pin_downloads(&ops, &pin);
    let ctx = ctx_with_llamacpp_pin(ops.clone(), RunMode::Install, Some("b9999"));
    let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert_eq!(ops.downloads_made().len(), 2);
}

// --- ConfigureTensorSplit ---

#[tokio::test]
async fn compute_splits_sums_to_one() {
    let splits = compute_splits(&[16384, 16384, 16384]).unwrap();
    assert_eq!(splits.len(), 3);
    let sum: f32 = splits.iter().sum();
    assert!((sum - 1.0).abs() < 1e-6, "{splits:?}");
    // Uneven VRAM: proportional shares.
    let splits = compute_splits(&[24576, 8192]).unwrap();
    assert!((splits[0] - 0.75).abs() < 1e-6, "{splits:?}");
    assert!((splits[1] - 0.25).abs() < 1e-6);
    // Zero VRAM is an error, not a silent [0,0].
    assert!(compute_splits(&[0, 0]).is_err());
}

#[test]
fn compute_splits_never_negative_and_always_sums_to_one() {
    // Deterministic pseudo-random sweep: adversarial VRAM mixes over
    // 1..=40 GPUs must never produce a negative share or a bad sum.
    let mut seed: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        seed >> 33
    };
    for _ in 0..2000 {
        let n = 1 + (next() % 40) as usize;
        let vrams: Vec<u64> = (0..n).map(|_| 1024 + next() % 49152).collect();
        let splits = compute_splits(&vrams).unwrap();
        assert_eq!(splits.len(), n);
        for s in &splits {
            assert!(
                (0.0..=1.0).contains(s),
                "split out of range: {splits:?} from {vrams:?}"
            );
        }
        let sum: f32 = splits.iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-4,
            "sum != 1.0: {splits:?} from {vrams:?}"
        );
    }
    // Single GPU owns the whole split.
    assert_eq!(compute_splits(&[16384]).unwrap(), vec![1.0]);
    // Empty input is zero total VRAM, not a panic.
    assert!(compute_splits(&[]).is_err());
}

// --- assign_splits (pure) ---

fn gi_b(n: f64) -> u64 {
    (n * 1_073_741_824.0) as u64
}

#[test]
fn assign_splits_single_gpu_assigns_nothing() {
    let inputs = vec![SplitInput {
        model_bytes: gi_b(5.0),
        existing: None,
        assign: true,
    }];
    let out = assign_splits(&inputs, &[16384]);
    assert_eq!(out, vec![None]);
}

#[test]
fn assign_splits_fitting_model_goes_to_gpu_with_most_room() {
    // Accounting-only entry occupies 4 GB of GPU 0 (its unset
    // split means llama.cpp puts it there today); the assignable
    // 6 GB model no longer fits GPU 0's remaining placeable space
    // and must go entirely on GPU 1.
    let inputs = vec![
        SplitInput {
            model_bytes: gi_b(4.0),
            existing: None,
            assign: false,
        },
        SplitInput {
            model_bytes: gi_b(6.0),
            existing: None,
            assign: true,
        },
    ];
    let out = assign_splits(&inputs, &[8192, 16384]);
    assert_eq!(out[0], None);
    assert_eq!(out[1], Some((vec![0.0, 1.0], SplitMode::Layer)));
}

#[test]
fn assign_splits_oversized_model_spreads_proportionally() {
    let inputs = vec![SplitInput {
        model_bytes: gi_b(40.0),
        existing: None,
        assign: true,
    }];
    let out = assign_splits(&inputs, &[16384, 16384]);
    assert_eq!(out[0], Some((vec![0.5, 0.5], SplitMode::Layer)));
}

#[test]
fn assign_splits_keeps_valid_existing_and_deducts_it() {
    // Existing [0.5, 0.5] of a 20 GB model leaves ~5.0 GB
    // placeable on each 16 GiB GPU; the new 10 GB model fits
    // neither and spreads over the remaining room evenly.
    let inputs = vec![
        SplitInput {
            model_bytes: gi_b(20.0),
            existing: Some(vec![0.5, 0.5]),
            assign: false,
        },
        SplitInput {
            model_bytes: gi_b(10.0),
            existing: None,
            assign: true,
        },
    ];
    let out = assign_splits(&inputs, &[16384, 16384]);
    assert_eq!(out[0], Some((vec![0.5, 0.5], SplitMode::Layer)));
    assert_eq!(out[1], Some((vec![0.5, 0.5], SplitMode::Layer)));
}

#[test]
fn assign_splits_stale_length_existing_is_recomputed() {
    let inputs = vec![SplitInput {
        model_bytes: gi_b(5.0),
        existing: Some(vec![1.0]),
        assign: true,
    }];
    let out = assign_splits(&inputs, &[16384, 16384]);
    let (split, _) = out[0].clone().expect("assigned");
    assert_eq!(split.len(), 2);
    let sum: f32 = split.iter().sum();
    assert!((sum - 1.0).abs() < 1e-4);
}

#[test]
fn assign_splits_largest_first_packing() {
    // Two 10 GB models, two 16 GiB GPUs (14.4 GiB placeable
    // each): each model gets its own GPU rather than the second
    // being spread around the first's leftovers.
    let inputs = vec![
        SplitInput {
            model_bytes: gi_b(10.0),
            existing: None,
            assign: true,
        },
        SplitInput {
            model_bytes: gi_b(10.0),
            existing: None,
            assign: true,
        },
    ];
    let out = assign_splits(&inputs, &[16384, 16384]);
    assert_eq!(out[0], Some((vec![1.0, 0.0], SplitMode::Layer)));
    assert_eq!(out[1], Some((vec![0.0, 1.0], SplitMode::Layer)));
}

// --- ConfigureTensorSplit step ---

#[tokio::test]
async fn tensor_split_install_persists_splits_and_audit_validates() {
    let data_dir = stamp_test_dir("splits");
    let config_path = data_dir.join("config.json");
    let mut config = test_config_with_models();
    // The shared test config's backend model files do not resolve
    // to its two registered models (save() would reject it);
    // point each backend at one of them.
    for b in config.backends.iter_mut() {
        b.model_file = if b.id == "coder-fast" {
            PathBuf::from("models/qwen2.5-coder-7b.gguf")
        } else {
            PathBuf::from("models/qwen3-8b.gguf")
        };
    }
    // This test pins the split step's contract for backends that
    // need a split. Since the memory-requirement work (v0.1.31),
    // an unsplit backend is valid when its GPU-resident weights
    // fit GPU 0 or it is served by expert offload — so
    // the fixture neutralizes both escape hatches: no offload
    // flags, and the shared model carries the coder catalog id
    // (49 GB — fits no single test GPU).
    for b in config.backends.iter_mut() {
        b.raw_flags = None;
    }
    for m in config.models.iter_mut() {
        if m.gguf_path.ends_with("qwen3-8b.gguf") {
            m.id = "qwen3-coder-30b".to_string();
        }
    }
    manager_config::save(&config, &config_path).unwrap();
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus());
    let step = ConfigureTensorSplit;
    assert!(step
        .planned_commands(&WizardContext::new(
            config.clone(),
            data_dir.clone(),
            RunMode::Install,
            ops.clone(),
        ))
        .is_empty());

    // Audit against the unsplit config fails: this is the state
    // Pete's machine was in — backends configured, no placement.
    let audit_ctx = WizardContext::new(
        config.clone(),
        data_dir.clone(),
        RunMode::Audit,
        ops.clone(),
    );
    let out = step.run(&audit_ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(out.message.contains("no tensor split"), "{}", out.message);

    // Install assigns and persists into config.json.
    let install_ctx = WizardContext::new(
        config.clone(),
        data_dir.clone(),
        RunMode::Install,
        ops.clone(),
    );
    let out = step.run(&install_ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let saved = manager_config::load(&config_path).unwrap();
    manager_config::validate(&saved).unwrap();
    let enabled: Vec<_> = saved.backends.iter().filter(|b| b.enabled).collect();
    assert!(!enabled.is_empty());
    for b in &enabled {
        assert_eq!(b.flags.tensor_split.len(), 3, "{}: {:?}", b.id, b.flags);
        let sum: f32 = b.flags.tensor_split.iter().sum();
        assert!((sum - 1.0).abs() < 1e-3, "{} sums to {sum}", b.id);
    }

    // Audit against the persisted config passes.
    let audit_ctx = WizardContext::new(saved, data_dir.clone(), RunMode::Audit, ops.clone());
    let out = step.run(&audit_ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let _ = std::fs::remove_dir_all(&data_dir);
}

// --- DownloadModels ---

#[tokio::test]
async fn download_models_install_and_verify() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_download(
        "https://example.com/qwen3-8b.gguf",
        b"gguf-8b-bytes".to_vec(),
    );
    ops.with_download(
        "https://example.com/qwen2.5-coder-7b.gguf",
        b"gguf-7b-bytes".to_vec(),
    );
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = DownloadModels;
    assert_eq!(step.planned_commands(&ctx).len(), 2);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(out.message.contains("2/2"));
    assert_eq!(ops.downloads_made().len(), 2);
}

#[tokio::test]
async fn download_models_skips_disabled_models() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_download(
        "https://example.com/qwen3-8b.gguf",
        b"gguf-8b-bytes".to_vec(),
    );
    ops.with_download(
        "https://example.com/qwen2.5-coder-7b.gguf",
        b"gguf-7b-bytes".to_vec(),
    );
    let mut config = test_config_with_models();
    config.models[1].enabled = false;
    let ctx = WizardContext::new(
        config,
        PathBuf::from("/data"),
        RunMode::Install,
        ops.clone(),
    );
    let step = DownloadModels;
    // Planned commands cover enabled models only.
    assert_eq!(step.planned_commands(&ctx).len(), 1);
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(out.message.contains("1/1"), "{}", out.message);
    assert_eq!(ops.downloads_made().len(), 1);
}

#[tokio::test]
async fn download_models_none_enabled_is_a_no_op() {
    let ops = Arc::new(FakeSystemOps::new());
    let mut config = test_config_with_models();
    for m in &mut config.models {
        m.enabled = false;
    }
    let ctx = WizardContext::new(
        config,
        PathBuf::from("/data"),
        RunMode::Install,
        ops.clone(),
    );
    let out = DownloadModels.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(out.message.contains("no models enabled"), "{}", out.message);
    assert_eq!(ops.downloads_made().len(), 0);
}

#[tokio::test]
async fn download_models_missing_url_fails_closed() {
    let ops = Arc::new(FakeSystemOps::new());
    let mut config = test_config_with_models();
    config.models[0].source_url.clear();
    let ctx = WizardContext::new(config, PathBuf::from("/data"), RunMode::Install, ops);
    let out = DownloadModels.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("no source URL"));
}

#[tokio::test]
async fn download_models_audit_detects_corruption() {
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_download(
        "https://example.com/qwen3-8b.gguf",
        b"gguf-8b-bytes".to_vec(),
    );
    ops.with_download(
        "https://example.com/qwen2.5-coder-7b.gguf",
        b"gguf-7b-bytes".to_vec(),
    );
    let install_ctx = ctx_with(ops.clone(), RunMode::Install);
    let step = DownloadModels;
    assert_eq!(step.run(&install_ctx).await.unwrap().state, StepState::Done);

    let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
    assert_eq!(step.run(&audit_ctx).await.unwrap().state, StepState::Done);

    // Corrupt one file on disk.
    ops.with_file(
        PathBuf::from("/data/models/qwen3-8b.gguf"),
        b"corrupted".to_vec(),
    );
    let out = step.run(&audit_ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("checksum mismatch"));
    assert!(step.remediation_hint().unwrap().contains("re-download"));
    // Audit performed no downloads.
    assert_eq!(ops.downloads_made().len(), 2, "audit added no downloads");
}

// --- DownloadModels: verified_at stamping ---

/// Config fixture that passes `manager_config::save` validation.
/// The shared fixture enables every backend, but its backends point
/// at model files absent from the replaced models list, which
/// save() rejects. The stamp tests persist the config, so they
/// disable the backends (DownloadModels ignores backends).
fn stamp_test_config() -> AppConfig {
    let mut c = test_config_with_models();
    for b in c.backends.iter_mut() {
        b.enabled = false;
    }
    c
}

/// Unique real temp data dir for the stamp tests: the stamp
/// targets `<data_dir>/config.json` on the real filesystem.
fn stamp_test_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("llm-wiz-stamp-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn download_models_install_stamps_verified_at() {
    let data_dir = stamp_test_dir("install");
    let config_path = data_dir.join("config.json");
    // Nothing persisted yet (first run): the stamp falls back to
    // the context config and creates the persisted file.
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_download(
        "https://example.com/qwen3-8b.gguf",
        b"gguf-8b-bytes".to_vec(),
    );
    ops.with_download(
        "https://example.com/qwen2.5-coder-7b.gguf",
        b"gguf-7b-bytes".to_vec(),
    );
    let ctx = WizardContext::new(stamp_test_config(), data_dir.clone(), RunMode::Install, ops);
    let out = DownloadModels.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");

    let persisted = manager_config::load(&config_path).unwrap();
    assert_eq!(persisted.models.len(), 2);
    for m in &persisted.models {
        assert!(m.verified_at.is_some(), "{} verified but not stamped", m.id);
    }
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn download_models_audit_stamps_verified_at() {
    let data_dir = stamp_test_dir("audit");
    let config_path = data_dir.join("config.json");
    // A persisted config with no stamps yet: audit loads it as
    // the stamp base.
    let config = stamp_test_config();
    manager_config::save(&config, &config_path).unwrap();

    // Both model files already installed with matching bytes.
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_file(
        data_dir.join("models/qwen3-8b.gguf"),
        b"gguf-8b-bytes".to_vec(),
    );
    ops.with_file(
        data_dir.join("models/qwen2.5-coder-7b.gguf"),
        b"gguf-7b-bytes".to_vec(),
    );
    let ctx = WizardContext::new(config, data_dir.clone(), RunMode::Audit, ops);
    let out = DownloadModels.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(out.message.contains("2/2"), "{out:?}");

    let persisted = manager_config::load(&config_path).unwrap();
    for m in &persisted.models {
        assert!(m.verified_at.is_some(), "{} audited but not stamped", m.id);
    }
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn download_models_mismatch_is_not_stamped() {
    let data_dir = stamp_test_dir("mismatch");
    let config_path = data_dir.join("config.json");
    let config = stamp_test_config();
    manager_config::save(&config, &config_path).unwrap();

    let ops = Arc::new(FakeSystemOps::new());
    ops.with_download(
        "https://example.com/qwen3-8b.gguf",
        b"gguf-8b-bytes".to_vec(),
    );
    // The second model's bytes do not match its pinned hash.
    ops.with_download(
        "https://example.com/qwen2.5-coder-7b.gguf",
        b"tampered-bytes".to_vec(),
    );
    let ctx = WizardContext::new(config, data_dir.clone(), RunMode::Install, ops);
    let out = DownloadModels.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed);
    assert!(out.message.contains("checksum mismatch"), "{out:?}");

    let persisted = manager_config::load(&config_path).unwrap();
    let good = persisted
        .models
        .iter()
        .find(|m| m.id == "qwen3-8b")
        .unwrap();
    assert!(
        good.verified_at.is_some(),
        "model verified earlier in the same run is stamped"
    );
    let bad = persisted
        .models
        .iter()
        .find(|m| m.id == "qwen2.5-coder-7b")
        .unwrap();
    assert!(
        bad.verified_at.is_none(),
        "mismatching model must not be stamped"
    );
    let _ = std::fs::remove_dir_all(&data_dir);
}

// --- PS-4: never save a snapshot over an unloadable config.json ---

const GARBAGE_CONFIG: &[u8] = b"{ this is not json";

/// Fixture for the placement step: enabled backends whose model
/// needs a split on the three test GPUs (see
/// `tensor_split_install_persists_splits_and_audit_validates`).
fn split_test_config() -> AppConfig {
    let mut config = test_config_with_models();
    for b in config.backends.iter_mut() {
        b.model_file = if b.id == "coder-fast" {
            PathBuf::from("models/qwen2.5-coder-7b.gguf")
        } else {
            PathBuf::from("models/qwen3-8b.gguf")
        };
        b.raw_flags = None;
    }
    for m in config.models.iter_mut() {
        if m.gguf_path.ends_with("qwen3-8b.gguf") {
            m.id = "qwen3-coder-30b".to_string();
        }
    }
    config
}

#[tokio::test]
async fn download_models_unloadable_config_fails_and_is_not_overwritten() {
    let data_dir = stamp_test_dir("ps4-garbage");
    let config_path = data_dir.join("config.json");
    std::fs::write(&config_path, GARBAGE_CONFIG).unwrap();
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_download(
        "https://example.com/qwen3-8b.gguf",
        b"gguf-8b-bytes".to_vec(),
    );
    ops.with_download(
        "https://example.com/qwen2.5-coder-7b.gguf",
        b"gguf-7b-bytes".to_vec(),
    );
    let ctx = WizardContext::new(stamp_test_config(), data_dir.clone(), RunMode::Install, ops);
    let out = DownloadModels.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(
        out.message.contains("could not be loaded"),
        "{}",
        out.message
    );
    assert_eq!(
        std::fs::read(&config_path).unwrap(),
        GARBAGE_CONFIG,
        "the unloadable file must not be overwritten"
    );
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn tensor_split_unloadable_config_fails_and_is_not_overwritten() {
    let data_dir = stamp_test_dir("ps4-split-garbage");
    let config_path = data_dir.join("config.json");
    std::fs::write(&config_path, GARBAGE_CONFIG).unwrap();
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus());
    let ctx = WizardContext::new(split_test_config(), data_dir.clone(), RunMode::Install, ops);
    let out = ConfigureTensorSplit.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(
        out.message.contains("could not be loaded"),
        "{}",
        out.message
    );
    assert_eq!(std::fs::read(&config_path).unwrap(), GARBAGE_CONFIG);
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn tensor_split_return_to_caller_leaves_disk_alone() {
    let data_dir = stamp_test_dir("ps4-split-return");
    let config_path = data_dir.join("config.json");
    // Even an unloadable file is irrelevant: the step never reads
    // or writes config.json when the caller owns persistence.
    std::fs::write(&config_path, GARBAGE_CONFIG).unwrap();
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus());
    let config = split_test_config();
    let ctx = WizardContext::new(config.clone(), data_dir.clone(), RunMode::Install, ops)
        .with_config_edit_mode(ConfigEditMode::ReturnToCaller);
    let out = ConfigureTensorSplit.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let enabled = config.backends.iter().filter(|b| b.enabled).count();
    assert_eq!(out.config_edits.len(), enabled, "{:?}", out.config_edits);
    for edit in &out.config_edits {
        match edit {
            ConfigEdit::BackendPlacement { tensor_split, .. } => {
                assert_eq!(tensor_split.len(), 3)
            }
            other => panic!("unexpected edit {other:?}"),
        }
    }
    assert_eq!(std::fs::read(&config_path).unwrap(), GARBAGE_CONFIG);
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn download_models_return_to_caller_returns_stamps_without_saving() {
    let data_dir = stamp_test_dir("ps4-dl-return");
    let config_path = data_dir.join("config.json");
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_download(
        "https://example.com/qwen3-8b.gguf",
        b"gguf-8b-bytes".to_vec(),
    );
    // The second model fails verification: the first model's
    // stamp still rides back on the failed outcome.
    ops.with_download(
        "https://example.com/qwen2.5-coder-7b.gguf",
        b"tampered-bytes".to_vec(),
    );
    let ctx = WizardContext::new(stamp_test_config(), data_dir.clone(), RunMode::Install, ops)
        .with_config_edit_mode(ConfigEditMode::ReturnToCaller);
    let out = DownloadModels.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert_eq!(out.config_edits.len(), 1, "{:?}", out.config_edits);
    match &out.config_edits[0] {
        ConfigEdit::ModelVerified {
            model_id,
            verified_sha256,
            ..
        } => {
            assert_eq!(model_id, "qwen3-8b");
            assert!(!verified_sha256.is_empty());
        }
        other => panic!("unexpected edit {other:?}"),
    }
    assert!(!config_path.exists(), "the step must not write config.json");
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn tensor_split_absent_config_seeds_from_snapshot() {
    let data_dir = stamp_test_dir("ps4-split-fresh");
    let config_path = data_dir.join("config.json");
    let ops = Arc::new(FakeSystemOps::new());
    ops.with_gpus(test_gpus());
    let ctx = WizardContext::new(split_test_config(), data_dir.clone(), RunMode::Install, ops);
    let out = ConfigureTensorSplit.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    let saved = manager_config::load(&config_path).unwrap();
    assert!(saved
        .backends
        .iter()
        .filter(|b| b.enabled)
        .all(|b| !b.flags.tensor_split.is_empty()));
    let _ = std::fs::remove_dir_all(&data_dir);
}

// --- DownloadModels: audit verified-by-stamp skip ---

fn catalog_size(id: &str) -> u64 {
    manager_models::curated_models()
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.size_bytes)
        .unwrap()
}

/// An audit ctx whose only enabled model is qwen3-8b, with the
/// given stamp state and scripted on-disk length. The fake file
/// holds bytes whose hash does NOT match the pin, so an audit
/// pass proves the hash was skipped and a failure proves it ran.
fn audit_ctx_stamped(
    ops: &Arc<FakeSystemOps>,
    stamped: bool,
    scripted_len: Option<u64>,
) -> WizardContext {
    let mut config = test_config_with_models();
    config.models[1].enabled = false;
    if stamped {
        config.models[0].verified_at = Some(chrono::Utc::now());
        // The stamp is bound to the pin it verified against
        // (code-review 2.4): without this the skip cannot fire.
        config.models[0].verified_sha256 = Some(config.models[0].sha256.clone());
    }
    let dest = resolve_dest(&PathBuf::from("/data"), &config.models[0].gguf_path);
    ops.with_file(dest.clone(), b"not-the-pinned-bytes".to_vec());
    if let Some(len) = scripted_len {
        ops.with_file_len(dest, len);
    }
    WizardContext::new(config, PathBuf::from("/data"), RunMode::Audit, ops.clone())
}

#[tokio::test]
async fn download_models_audit_skips_hash_for_stamped_model_with_matching_length() {
    let ops = Arc::new(FakeSystemOps::new());
    let ctx = audit_ctx_stamped(&ops, true, Some(catalog_size("qwen3-8b")));
    let out = DownloadModels.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("verified by an earlier checksum pass")),
        "{out:?}"
    );
}

#[tokio::test]
async fn download_models_audit_hashes_when_length_differs_or_stamp_missing() {
    // Length differs from the catalog pin: the hash runs (and
    // fails — the fake bytes are not the pinned content).
    let ops = Arc::new(FakeSystemOps::new());
    let ctx = audit_ctx_stamped(&ops, true, Some(catalog_size("qwen3-8b") - 1));
    let out = DownloadModels.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Failed, "{out:?}");
    assert!(out.message.contains("checksum mismatch"), "{}", out.message);

    // No stamp: the hash runs even at the exact catalog length.
    let ops2 = Arc::new(FakeSystemOps::new());
    let ctx2 = audit_ctx_stamped(&ops2, false, Some(catalog_size("qwen3-8b")));
    let out2 = DownloadModels.run(&ctx2).await.unwrap();
    assert_eq!(out2.state, StepState::Failed, "{out2:?}");
    assert!(
        out2.message.contains("checksum mismatch"),
        "{}",
        out2.message
    );

    // Stamp against a DIFFERENT pin than the current one (the
    // catalog was re-pinned after the stamp): the skip must not
    // fire — the hash runs and fails (code-review 2.4).
    let ops3 = Arc::new(FakeSystemOps::new());
    let mut ctx3 = audit_ctx_stamped(&ops3, true, Some(catalog_size("qwen3-8b")));
    ctx3.config.models[0].verified_sha256 = Some("0".repeat(64));
    let out3 = DownloadModels.run(&ctx3).await.unwrap();
    assert_eq!(out3.state, StepState::Failed, "{out3:?}");
    assert!(
        out3.message.contains("checksum mismatch"),
        "{}",
        out3.message
    );
}

// --- StartGateway ---

// --- DownloadModels: tiered model storage ---

/// An install ctx whose only enabled model is qwen3-8b, with a cold
/// tier at /cold (reachable when `cold_reachable`).
fn install_ctx_with_cold_tier(ops: &Arc<FakeSystemOps>, cold_reachable: bool) -> WizardContext {
    let mut config = test_config_with_models();
    config.models[1].enabled = false;
    config.model_storage.cold_tiers = vec![manager_config::ColdTier {
        path: PathBuf::from("/cold"),
        label: String::new(),
    }];
    if cold_reachable {
        ops.with_file(PathBuf::from("/cold"), Vec::new());
    }
    WizardContext::new(
        config,
        PathBuf::from("/data"),
        RunMode::Install,
        ops.clone(),
    )
}

#[tokio::test]
async fn download_models_finds_a_cold_copy_instead_of_re_downloading() {
    let ops = Arc::new(FakeSystemOps::new());
    let ctx = install_ctx_with_cold_tier(&ops, true);
    let rel = manager_config::tier_relative(&ctx.config.models[0].gguf_path).unwrap();
    let cold_copy = PathBuf::from("/cold").join(rel);
    ops.with_file(cold_copy.clone(), b"bytes".to_vec());
    let _ = DownloadModels.run(&ctx).await.unwrap();
    assert!(
        ops.downloads_made().iter().all(|(_, dest)| dest
            != &resolve_dest(&PathBuf::from("/data"), &ctx.config.models[0].gguf_path)),
        "a model present on the cold tier must not be re-downloaded to hot: {:?}",
        ops.downloads_made()
    );
}

#[tokio::test]
async fn download_models_never_re_downloads_while_the_cold_tier_is_unreachable() {
    let ops = Arc::new(FakeSystemOps::new());
    let ctx = install_ctx_with_cold_tier(&ops, false);
    let out = DownloadModels.run(&ctx).await.unwrap();
    assert!(
        ops.downloads_made().is_empty(),
        "{:?}",
        ops.downloads_made()
    );
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.contains("cold tier is unavailable")),
        "{out:?}"
    );
}
