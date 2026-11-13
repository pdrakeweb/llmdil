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
