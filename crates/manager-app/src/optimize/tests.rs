use super::*;

/// Deep-discovery Part 1 (§P1.10/§P1.14): a pre-deep-pass
/// candidates file (no `funnel`) still loads, and the
/// payload carries the funnel beside the items when a
/// deep pass wrote one — including a partial pass that
/// kept nothing.
#[test]
fn candidates_file_funnel_is_additive() {
    let old: CandidatesFile =
        serde_json::from_str(r#"{"directive":"d","retrieved_at":"t","candidates":[]}"#).unwrap();
    assert!(old.funnel.is_none());
    // No candidates and no funnel: still the null payload.
    assert_eq!(candidates_json(&old), Value::Null);

    let funnel = DiscoveryFunnel {
        queries_run: 3,
        pool_repos: 6,
        repos_fetched: 4,
        kept: 0,
        partial: true,
        partial_reason: Some("rate_limited".to_string()),
        ..Default::default()
    };
    let file = CandidatesFile {
        directive: "d".to_string(),
        retrieved_at: "t".to_string(),
        candidates: Vec::new(),
        funnel: Some(funnel.clone()),
    };
    let payload = candidates_json(&file);
    assert_eq!(payload["items"], json!([]));
    assert_eq!(payload["funnel"]["pool_repos"], json!(6));
    assert_eq!(payload["funnel"]["partial"], json!(true));
    assert_eq!(payload["funnel"]["partial_reason"], json!("rate_limited"));
    // The stored file round-trips with the funnel intact.
    let text = serde_json::to_string(&file).unwrap();
    let back: CandidatesFile = serde_json::from_str(&text).unwrap();
    assert_eq!(back.funnel, Some(funnel));
    // The log line carries every counter.
    let line = funnel_log_line(&back.funnel.unwrap());
    assert!(line.contains("pool_repos=6"), "{line}");
    assert!(line.contains("partial_reason=rate_limited"), "{line}");
}

fn bench(gen: f64) -> BenchResult {
    BenchResult {
        prompt_per_second: Some(30.0),
        predicted_per_second: Some(gen),
        ttft_ms: Some(120.0),
        runs: Vec::new(),
    }
}

fn entry(fingerprint: String, gen: f64) -> MeasurementEntry {
    MeasurementEntry {
        fingerprint,
        backend_id: "planner".to_string(),
        model_id: "model".to_string(),
        measured_at: "2026-10-19T00:00:00Z".to_string(),
        app_version: "0.1.41".to_string(),
        short: bench(gen),
        long: bench(gen),
        source: None,
        origin_backend_id: None,
        model_file: None,
        model_sha256: None,
        model_size_bytes: None,
        inputs: None,
        driver_version: Vec::new(),
        llama_build: None,
        residency: None,
        peak_temp_c: None,
    }
}

fn item(kind: PlanKind, target: &str, change: Value) -> PlanItem {
    PlanItem {
        id: 1,
        fingerprint: "item".to_string(),
        kind,
        target: Some(target.to_string()),
        change,
        advice_text: None,
        rationale: String::new(),
        expected_effect: String::new(),
        confidence: manager_optimize::Confidence::Low,
        decision: Decision::Undecided,
        previously_denied: false,
        provision_for_experiment: None,
        apply_outcome: None,
        apply_error: None,
    }
}

/// Design §9: a flag-changing item's validation carries the
/// exact-match measurement of the configuration the item
/// produces — not of the configuration it replaces.
#[test]
fn plan_item_predicted_resolves_the_resulting_configuration() {
    let cfg = AppConfig::default_config();
    let saved = cfg
        .backends
        .iter()
        .find(|b| b.id == "planner")
        .expect("default config seeds planner");
    // The configuration a set_context item would produce.
    let mut merged = saved.clone();
    merged.flags.n_ctx = 16384;
    let fp = measurement_fingerprint(&fingerprint_inputs(&cfg, &merged, &[]));
    let mut store = MeasurementStore::load(&PathBuf::from(
        "/nonexistent-m2-test/optimize-measurements.json",
    ));
    store.upsert(entry(fp, 22.5));

    let it = item(
        PlanKind::BackendSetContext,
        "planner",
        json!({ "n_ctx": 16384 }),
    );
    let p = plan_item_predicted(&cfg, &it, &store, &[])
        .expect("the resulting configuration has an exact measurement");
    assert_eq!(p["predicted_per_second"], json!(22.5));

    // A different destination configuration has no
    // measurement and renders nothing.
    let it = item(
        PlanKind::BackendSetContext,
        "planner",
        json!({ "n_ctx": 8192 }),
    );
    assert!(plan_item_predicted(&cfg, &it, &store, &[]).is_none());

    // Kinds that do not change the measured configuration
    // never carry a prediction.
    let it = item(
        PlanKind::BackendSetEnabled,
        "planner",
        json!({ "enabled": true }),
    );
    assert!(plan_item_predicted(&cfg, &it, &store, &[]).is_none());
    let it = item(PlanKind::Advice, "planner", json!({}));
    assert!(plan_item_predicted(&cfg, &it, &store, &[]).is_none());
}

// ------------------------------------------------------------------
// Tracker #70: consultation/benchmark mutual exclusion.
// ------------------------------------------------------------------

fn mutex_test_state() -> std::sync::Arc<AppState> {
    let cfg = AppConfig::default_config();
    std::sync::Arc::new(AppState::new(
        cfg,
        PathBuf::from("/nonexistent-mutex-test/config.json"),
        PathBuf::from("/nonexistent-mutex-test"),
    ))
}

fn mutex_sink() -> std::sync::Arc<dyn crate::EventSink> {
    std::sync::Arc::new(crate::NoopSink)
}

fn running_ops_empty(state: &AppState) -> bool {
    state.optimize.running_ops.lock().unwrap().is_empty()
}

/// Tracker #70, scope test 1: a benchmark (Measure and
/// Estimate alike) is refused while a consultation runs on
/// the same backend, with the refusal naming the running
/// operation; a benchmark on a DIFFERENT backend is not
/// refused for contention (it proceeds to the ordinary
/// not-running refusal in this fixture, where no backend
/// runs).
#[tokio::test]
async fn benchmark_refused_during_consultation() {
    let state = mutex_test_state();
    let sink = mutex_sink();
    let _consult = OptimizeOpGuard::claim(
        &state,
        "coder",
        OptimizeOpFamily::Consultation,
        "consultation",
    )
    .expect("the first claim on a backend succeeds");

    let r = op_optimize_measure_backend(&state, &sink, "coder".to_string()).await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("cannot start a measurement"), "{msg}");
            assert!(msg.contains("a consultation is already running"), "{msg}");
            assert!(msg.contains("'coder'"), "{msg}");
            assert!(msg.contains("started"), "{msg}");
        }
        other => panic!("expected the mutual-exclusion refusal, got {other:?}"),
    }
    let r = op_optimize_estimate_backend(&state, &sink, "coder".to_string()).await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("cannot start an estimate"), "{msg}");
            assert!(msg.contains("a consultation is already running"), "{msg}");
        }
        other => panic!("expected the mutual-exclusion refusal, got {other:?}"),
    }
    // The eval-baseline benchmark path refuses the same way.
    let r =
        crate::optimize_eval::op_optimize_run_baseline(&state, &sink, "coder".to_string()).await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("a consultation is already running"), "{msg}");
        }
        other => panic!("expected the mutual-exclusion refusal, got {other:?}"),
    }
    // Different backend: no contention refusal.
    let r = op_optimize_measure_backend(&state, &sink, "planner".to_string()).await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(!msg.contains("already running on that backend"), "{msg}");
            assert!(msg.contains("not running"), "{msg}");
        }
        other => panic!("expected the not-running refusal, got {other:?}"),
    }
    // The refused starts claimed nothing of their own.
    assert_eq!(
        state.optimize.running_ops.lock().unwrap().len(),
        1,
        "only the consultation's record remains"
    );
}

/// Tracker #70, scope test 2: a consultation is refused
/// while a benchmark runs on the same backend, with the
/// refusal naming the running benchmark kind.
#[tokio::test]
async fn consultation_refused_during_benchmark() {
    let state = mutex_test_state();
    let sink = mutex_sink();
    let _bench = OptimizeOpGuard::claim(&state, "coder", OptimizeOpFamily::Benchmark, "measure")
        .expect("the first claim on a backend succeeds");

    let r = op_optimize_consult(
        &state,
        &sink,
        Some("coder".to_string()),
        Some(false),
        None,
        None,
    )
    .await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("cannot start a consultation"), "{msg}");
            assert!(msg.contains("a measurement is already running"), "{msg}");
            assert!(msg.contains("'coder'"), "{msg}");
        }
        other => panic!("expected the mutual-exclusion refusal, got {other:?}"),
    }
    // The refused consultation claimed nothing of its own.
    assert_eq!(state.optimize.running_ops.lock().unwrap().len(), 1);
}

/// Tracker #70, scope test 3: once the running operation
/// completes (its guard drops, as on every terminal path),
/// the other family may start on the same backend — and a
/// completed benchmark likewise frees the backend for a
/// consultation claim.
#[tokio::test]
async fn start_allowed_after_running_op_completes() {
    let state = mutex_test_state();
    {
        let _consult = OptimizeOpGuard::claim(
            &state,
            "coder",
            OptimizeOpFamily::Consultation,
            "consultation",
        )
        .unwrap();
        assert!(
            try_claim_optimize_op(&state, "coder", OptimizeOpFamily::Benchmark, "measure").is_err()
        );
    } // consultation completes: guard drop releases the record
    assert!(running_ops_empty(&state));
    let bench = OptimizeOpGuard::claim(&state, "coder", OptimizeOpFamily::Benchmark, "measure")
        .expect("a benchmark may start after the consultation completes");
    assert!(try_claim_optimize_op(
        &state,
        "coder",
        OptimizeOpFamily::Consultation,
        "consultation"
    )
    .is_err());
    drop(bench); // benchmark completes
    assert!(running_ops_empty(&state));
    let _consult = OptimizeOpGuard::claim(
        &state,
        "coder",
        OptimizeOpFamily::Consultation,
        "consultation",
    )
    .expect("a consultation may start after the benchmark completes");
}

/// Tracker #70, scope test 4 (the Oct 20, 2026 case): a
/// consultation cancelled by the operator clears its record
/// through the cancel command, so a benchmark may start on
/// the same backend immediately; the same holds for a
/// cancelled benchmark and a following consultation. A stale
/// release from the cancelled operation must not clear a
/// newer claim on the backend.
#[tokio::test]
async fn start_allowed_after_cancel() {
    let state = mutex_test_state();
    let sink = mutex_sink();
    // A live consultation on 'coder': its running-op record
    // plus the abort sender the cancel command fires.
    let consult = OptimizeOpGuard::claim(
        &state,
        "coder",
        OptimizeOpFamily::Consultation,
        "consultation",
    )
    .unwrap();
    let consult_claim = {
        let ops = state.optimize.running_ops.lock().unwrap();
        ops.get("coder").unwrap().claim_id
    };
    let (tx, _rx) = tokio::sync::oneshot::channel::<()>();
    *state.optimize.consult_abort.lock().await = Some(tx);
    let r = op_optimize_cancel_consult(&state, &sink).await.unwrap();
    assert_eq!(r["cancelled"], json!(true));
    assert!(
        running_ops_empty(&state),
        "the cancel clears the consultation's record at once"
    );
    // The benchmark the operator wanted to run can now claim.
    let bench = OptimizeOpGuard::claim(&state, "coder", OptimizeOpFamily::Benchmark, "estimate")
        .expect("an estimate may start after the consultation is cancelled");
    // The cancelled consultation's late release (its guard
    // dropping as its future unwinds) must not clear the
    // newer benchmark claim.
    drop(consult);
    release_optimize_op(&state, "coder", consult_claim);
    assert_eq!(state.optimize.running_ops.lock().unwrap().len(), 1);
    drop(bench);
    assert!(running_ops_empty(&state));

    // Symmetric: a cancelled benchmark (the shared job
    // cancel) frees its backend for a consultation.
    let _bench =
        OptimizeOpGuard::claim(&state, "coder", OptimizeOpFamily::Benchmark, "measure").unwrap();
    let (tx, _rx) = tokio::sync::oneshot::channel::<()>();
    *state.optimize.job_abort.lock().await = Some(tx);
    let r = op_optimize_cancel_job(&state, &sink).await.unwrap();
    assert_eq!(r["cancelled"], json!(true));
    assert!(running_ops_empty(&state));
    let _consult = OptimizeOpGuard::claim(
        &state,
        "coder",
        OptimizeOpFamily::Consultation,
        "consultation",
    )
    .expect("a consultation may start after the benchmark is cancelled");
}

/// The refusal names the running operation's kind AND its
/// elapsed time (the start the record — and the tracker #69
/// job-state payload — carries), not just the family.
#[test]
fn contention_refusal_names_kind_and_elapsed() {
    let state = mutex_test_state();
    let started = chrono::Utc::now().timestamp_millis() - 125_000;
    state.optimize.running_ops.lock().unwrap().insert(
        "coder".to_string(),
        OptimizeRunningOp {
            family: OptimizeOpFamily::Consultation,
            kind: "consultation".to_string(),
            backend_id: "coder".to_string(),
            started_unix_ms: started,
            claim_id: 999,
        },
    );
    let err = try_claim_optimize_op(&state, "coder", OptimizeOpFamily::Benchmark, "estimate")
        .expect_err("a running consultation refuses a benchmark claim");
    match err {
        ApiError::InvalidInput(msg) => {
            assert!(msg.contains("a consultation is already running"), "{msg}");
            assert!(msg.contains("started 2m 05s ago"), "{msg}");
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

/// Tracker #69: the `job` payload value carries the job
/// kind, backend, and start time — the fields a page
/// instance that did not start the job needs to render the
/// true in-progress state — and is null when no
/// Estimate/Measure job is running.
#[test]
fn job_json_carries_kind_backend_and_start() {
    assert_eq!(job_json(&None), Value::Null);
    let job = crate::OptimizeJobInfo {
        kind: "estimate".to_string(),
        backend_id: "coder".to_string(),
        started_unix_ms: 1729000000000,
    };
    let v = job_json(&Some(job));
    assert_eq!(v["kind"], json!("estimate"));
    assert_eq!(v["backend_id"], json!("coder"));
    assert_eq!(v["started_unix_ms"], json!(1729000000000i64));
}

fn job_test_state() -> std::sync::Arc<AppState> {
    let cfg = AppConfig::default_config();
    std::sync::Arc::new(AppState::new(
        cfg,
        PathBuf::from("/nonexistent-job-test/config.json"),
        PathBuf::from("/nonexistent-job-test"),
    ))
}

/// Tracker #69: the job record lifecycle follows the
/// consultation record's discipline — a start records the
/// descriptor and clears any previous failure, a failure
/// clears the descriptor and records the error for the
/// payload, a success clears both, and a cancellation
/// clears the descriptor without recording a failure.
#[tokio::test]
async fn job_record_lifecycle() {
    let state = job_test_state();
    let started = record_job_start(&state, "estimate", "coder").await;
    {
        let job = state.optimize.job.lock().await.clone();
        let job = job.expect("a started job has a descriptor");
        assert_eq!(job.kind, "estimate");
        assert_eq!(job.backend_id, "coder");
        assert!(job.started_unix_ms > 0);
        assert!(state.optimize.job_error.lock().await.is_none());
    }
    record_job_failure(&state, started, "estimate failed: boom".to_string()).await;
    assert!(state.optimize.job.lock().await.is_none());
    {
        let (msg, at) = state
            .optimize
            .job_error
            .lock()
            .await
            .clone()
            .expect("a failed job records its error");
        assert_eq!(msg, "estimate failed: boom");
        assert!(at > 0);
    }
    // A new start supersedes the recorded failure.
    let started2 = record_job_start(&state, "measure", "planner").await;
    assert!(state.optimize.job_error.lock().await.is_none());
    assert_eq!(
        state
            .optimize
            .job
            .lock()
            .await
            .as_ref()
            .map(|j| j.kind.as_str()),
        Some("measure")
    );
    record_job_success(&state, started2).await;
    assert!(state.optimize.job.lock().await.is_none());
    assert!(state.optimize.job_error.lock().await.is_none());
    // Cancellation clears the descriptor, records no error.
    let started3 = record_job_start(&state, "measure", "planner").await;
    record_job_end(&state, started3).await;
    assert!(state.optimize.job.lock().await.is_none());
    assert!(state.optimize.job_error.lock().await.is_none());
    // A stale job's terminal records (a cancelled job's late
    // unwind, tracker #70's cancel-then-restart case) must
    // not clear a newer job's descriptor or record a failure
    // over it.
    let stale = record_job_start(&state, "measure", "planner").await;
    let current = record_job_start(&state, "estimate", "coder").await;
    assert!(current > stale, "job start tokens are strictly increasing");
    record_job_end(&state, stale).await;
    record_job_failure(&state, stale, "stale failure".to_string()).await;
    record_job_success(&state, stale).await;
    {
        let job = state.optimize.job.lock().await.clone();
        let job = job.expect("the newer job's descriptor survives stale terminal records");
        assert_eq!(job.kind, "estimate");
        assert_eq!(job.started_unix_ms, current);
        assert!(state.optimize.job_error.lock().await.is_none());
    }
    record_job_success(&state, current).await;
    assert!(state.optimize.job.lock().await.is_none());
}

#[test]
fn plan_staleness_ignores_bookkeeping() {
    let mut base = AppConfig::default_config();
    base.models.push(manager_config::ModelConfig {
        id: "m".to_string(),
        gguf_path: PathBuf::from("models/m.gguf"),
        quant: "Q4_K_M".to_string(),
        params_b: 7.0,
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
    });
    let hash = current_config_hash(&base);
    assert_eq!(hash, current_config_hash(&base.clone()));
    // Verification stamps, probes and UI state are not a change.
    let mut cfg = base.clone();
    cfg.models[0].verified_at = Some(chrono::Utc::now());
    cfg.models[0].verified_sha256 = Some("ab".repeat(32));
    cfg.backends[0].tool_call_probe = Some(manager_config::ToolCallProbe {
        outcome: manager_config::ToolCallProbeOutcome::Verified,
        at_ms: 1,
    });
    cfg.setup_completed = !cfg.setup_completed;
    cfg.optimize_purpose = "coding".to_string();
    assert_eq!(hash, current_config_hash(&cfg));
    // A serving change is.
    let mut cfg = base.clone();
    cfg.backends[0].flags.n_ctx += 1024;
    assert_ne!(hash, current_config_hash(&cfg));
}
