use super::*;

fn backend(raw_flags: Option<&str>) -> BackendConfig {
    BackendConfig {
        id: "bench-target".to_string(),
        enabled: true,
        model_file: PathBuf::from("models/target.gguf"),
        port: 8090,
        flags: manager_config::ServerFlags {
            n_ctx: 65536,
            n_batch: 512,
            tensor_split: Vec::new(),
            split_mode: manager_config::SplitMode::None,
        },
        raw_flags: raw_flags.map(|s| s.to_string()),
        flags_advanced: false,
        restart_policy: manager_config::RestartPolicy {
            max_retries: 5,
            backoff_base_secs: 2,
        },
        overrides_global: false,
        tool_call_probe: None,
        purpose: String::new(),
    }
}

#[test]
fn base_arm_captures_the_targets_offload_position() {
    // The coder pattern: full expert offload is recorded on
    // the base arm, so every arm snapshot names the position
    // it runs at.
    let spec = base_arm_spec(&backend(Some("-ot exps=CPU --parallel 1")));
    assert_eq!(spec.offload_expr.as_deref(), Some("-ot exps=CPU"));
    assert_eq!(spec.n_ctx, 65536);
    // The partial pattern records its layer count.
    let spec = base_arm_spec(&backend(Some("--n-cpu-moe 24")));
    assert_eq!(spec.offload_expr.as_deref(), Some("--n-cpu-moe 24"));
}

#[test]
fn base_arm_canonicalizes_offload_spellings() {
    // Every spelling the decoder recognizes lands on the one
    // canonical expression for its position.
    let spec = base_arm_spec(&backend(Some("--cpu-moe")));
    assert_eq!(spec.offload_expr.as_deref(), Some("-ot exps=CPU"));
    let spec = base_arm_spec(&backend(Some("-ncmoe=46")));
    assert_eq!(spec.offload_expr.as_deref(), Some("--n-cpu-moe 46"));
}

#[test]
fn base_arm_without_offload_carries_no_expression() {
    let spec = base_arm_spec(&backend(Some("--parallel 1")));
    assert_eq!(spec.offload_expr, None);
    let spec = base_arm_spec(&backend(None));
    assert_eq!(spec.offload_expr, None);
    // A non-expert `-ot` assignment is not expert offload.
    let spec = base_arm_spec(&backend(Some("-ot blk.0.attn=CPU")));
    assert_eq!(spec.offload_expr, None);
}

fn placed_backend(id: &str, tensor_split: Vec<f32>, raw_flags: Option<&str>) -> BackendConfig {
    let mut b = backend(raw_flags);
    b.id = id.to_string();
    b.flags.tensor_split = tensor_split;
    b
}

fn cfg_with(backends: Vec<BackendConfig>) -> AppConfig {
    let mut cfg = AppConfig::default_config();
    cfg.backends = backends;
    cfg
}

fn running(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

#[test]
fn interlock_cross_gpu_raw_placed_backend_does_not_conflict() {
    // Pete's case (Oct 23, 2026): the target is resident
    // entirely on GPU 0; coder runs on GPU 1 only, its
    // placement stated in raw flags, and it CPU-offloads its
    // experts. Neither rule binds: the GPUs are disjoint, and
    // the CPU-offload rule binds only when the TARGET
    // CPU-offloads (§6.2). Before the bench-local placement
    // reading, backend_gpu_visibility reported coder
    // Undetermined and the interlock flagged it against every
    // sweep.
    let target = placed_backend("planner", vec![1.0], Some("--parallel 1"));
    let coder = placed_backend("coder", Vec::new(), Some("--tensor-split 0,1 -ot exps=CPU"));
    let cfg = cfg_with(vec![target.clone(), coder]);
    let conflicts = interlock_conflicts(&cfg, &running(&["coder"]), &target);
    assert!(conflicts.is_empty(), "conflicts: {conflicts:?}");
}

#[test]
fn interlock_shared_gpu_and_running_target_conflict() {
    // An unsplit backend sits on GPU 0 by default and shares
    // it with a GPU-0 target; the running target itself
    // always conflicts (§6.1).
    let target = placed_backend("planner", Vec::new(), None);
    let other = placed_backend("chat", Vec::new(), None);
    let cfg = cfg_with(vec![target.clone(), other]);
    let conflicts = interlock_conflicts(&cfg, &running(&["chat", "planner"]), &target);
    assert_eq!(conflicts, vec!["planner".to_string(), "chat".to_string()]);
}

#[test]
fn interlock_cpu_offload_target_conflicts_with_cpu_offload_backend() {
    // Disjoint GPUs, but the target CPU-offloads, so a
    // running CPU-offload backend shares the CPU expert
    // resource and conflicts (§3.4/§6.2) — while a running
    // dense backend on the other GPU does not.
    let target = placed_backend("coder-target", vec![1.0, 0.0], Some("-ot exps=CPU"));
    let moe = placed_backend("planner-moe", vec![0.0, 1.0], Some("--n-cpu-moe 24"));
    let dense = placed_backend("planner", vec![0.0, 1.0], Some("--parallel 1"));
    let cfg = cfg_with(vec![target.clone(), moe, dense]);
    let conflicts = interlock_conflicts(&cfg, &running(&["planner-moe", "planner"]), &target);
    assert_eq!(conflicts, vec!["planner-moe".to_string()]);
}

#[test]
fn interlock_unparseable_placement_still_conflicts() {
    // A device selection that does not parse stays
    // Undetermined, and Undetermined conflicts with
    // everything — the conservative reading is kept for the
    // genuinely unknown case.
    let target = placed_backend("planner", vec![1.0], None);
    let odd = placed_backend("odd", Vec::new(), Some("--device banana"));
    let cfg = cfg_with(vec![target.clone(), odd]);
    let conflicts = interlock_conflicts(&cfg, &running(&["odd"]), &target);
    assert_eq!(conflicts, vec!["odd".to_string()]);
}

#[test]
fn temperature_must_be_in_the_sampling_range() {
    for ok in [0.0, 0.6, 2.0] {
        assert!(check_temperature(ok).is_ok(), "{ok}");
    }
    for bad in [-0.1, 2.1, 5.0, f32::NAN] {
        assert!(check_temperature(bad).is_err(), "{bad}");
    }
}

#[test]
fn offload_expressions_place_tensors_only() {
    for ok in [
        "",
        "-ot exps=CPU",
        "-ot=exps=CPU,blk.0.attn=CPU",
        "--override-tensor exps=CPU",
        "--n-cpu-moe 24",
        "-ncmoe=46",
        "--cpu-moe",
    ] {
        assert!(check_offload_expr(ok).is_ok(), "{ok}");
    }
    for bad in [
        "--log-file x",
        "-ot exps=CPU --path x",
        "--n-cpu-moe",
        "-ot --api-key=k",
        "24",
        "--cpu-moe=1",
    ] {
        assert!(check_offload_expr(bad).is_err(), "{bad}");
    }
}

fn bench_state(name: &str) -> (Arc<AppState>, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "llm-mgr-bench-{name}-{}-{}",
        std::process::id(),
        now_ms()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = cfg_with(vec![placed_backend("planner", Vec::new(), None)]);
    cfg.data_dir = dir.clone();
    let state = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
    (state, dir)
}

/// A start answers at once with the sweep id and the task record, the
/// sweep runs on its own, and its end is visible in the results and on
/// the task (here: nothing can launch, so the sweep fails).
#[tokio::test]
async fn a_started_sweep_answers_at_once_and_settles_in_the_background() {
    let (state, dir) = bench_state("start");
    let sink: Arc<dyn crate::EventSink> = Arc::new(crate::NoopSink);
    let prompt_id = load_prompts(&state).entries[0].id.clone();
    let start = |expr: &str| {
        json!({
            "backend_id": "planner",
            "prompt_id": prompt_id,
            "dimensions": [[{ "offload_expr": expr }]],
        })
    };
    // Refusals stay errors of the call itself.
    let bad = op_bench_run_start(&state, &sink, start("--log-file x")).await;
    assert!(matches!(bad, Err(ApiError::InvalidInput(_))), "{bad:?}");
    // One sweep at a time.
    *state.bench.job.lock().await = Some(BenchJob {
        sweep_id: "other".to_string(),
        progress: BenchProgress {
            arm_index: 0,
            arm_count: 1,
            phase: "launching".to_string(),
            run: 0,
            started_unix_ms: now_ms(),
        },
    });
    let second = op_bench_run_start(&state, &sink, start("")).await;
    assert!(matches!(second, Err(ApiError::Busy(_))), "{second:?}");
    *state.bench.job.lock().await = None;
    let r = op_bench_run_start(&state, &sink, start("")).await.unwrap();
    let sweep_id = r["sweep_id"].as_str().unwrap().to_string();
    let task_id = r["id"].as_str().unwrap().to_string();
    assert_eq!(r["kind"], "bench-run");
    assert_eq!(r["status"], "running");
    assert_eq!(r["detail"]["sweep_id"], sweep_id.as_str());
    assert!(r["sweeps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["id"] == sweep_id.as_str()));
    for _ in 0..600 {
        if state.bench.job.lock().await.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let list = op_bench_results_list(&state, &sink).await.unwrap();
    let sweep = list["sweeps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == sweep_id.as_str())
        .unwrap();
    assert_eq!(sweep["status"], "failed", "{sweep}");
    // The slot is released just before the task settles.
    let mut status = Value::Null;
    for _ in 0..50 {
        let tasks = crate::op_tasks_list(&state, &sink).await.unwrap();
        status = tasks
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == task_id.as_str())
            .unwrap()["status"]
            .clone();
        if status != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status, "done");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A prompt saved with only the fields a person writes gets the rest,
/// and the answer names it.
#[tokio::test]
async fn a_prompt_needs_only_name_purpose_and_body() {
    let (state, dir) = bench_state("prompt");
    let sink: Arc<dyn crate::EventSink> = Arc::new(crate::NoopSink);
    let r = op_bench_prompt_save(
        &state,
        &sink,
        json!({ "name": "QA one", "purpose": "general", "body": "text" }),
    )
    .await
    .unwrap();
    assert_eq!(r["id"], "qa-one");
    let entry = r["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "qa-one")
        .unwrap();
    assert_eq!(entry["origin"], "user");
    assert_eq!(entry["sha256"].as_str().unwrap().len(), 64);
    let _ = std::fs::remove_dir_all(&dir);
}
