//! Measuring and estimating backends, job cancel, and plan item
//! state / outcome / validate / discard operations.

use super::*;

/// Run one benchmark prompt against a backend: an optional
/// discarded warm-up, then `runs` measured completions,
/// reduced to the median-bearing [`BenchResult`]. Shared by
/// the full measurement, the abbreviated estimate (tracker
/// #56), and experiment estimate passes. A warm-up failure and
/// a run failure surface identically — the callers' existing
/// error text names the stage that wraps this helper.
pub(crate) async fn benchmark_one_prompt(
    base_url: &str,
    model: &str,
    prompt: &BenchmarkPrompt,
    warm_up: bool,
    runs: usize,
    timeout: Duration,
) -> Result<BenchResult, String> {
    let run = || {
        crate::bench_client::complete(
            base_url,
            model,
            prompt.text,
            BENCHMARK_TEMPERATURE,
            prompt.max_tokens,
            None,
            timeout,
        )
    };
    if warm_up {
        run().await.map_err(|e| format!("warm-up: {e}"))?;
    }
    let mut samples = Vec::new();
    for _ in 0..runs {
        samples.push(run().await?.bench_run());
    }
    Ok(BenchResult::from_runs(samples))
}

/// A resolved benchmark-job target (see [`benchmark_target`]).
pub(super) struct BenchmarkTarget {
    pub(super) backend: manager_config::BackendConfig,
    pub(super) fingerprint: String,
    pub(super) model_id: String,
    /// The resolved fingerprint inputs: the v2 entry's
    /// recorded snapshot and hash-or-size identity.
    pub(super) inputs: MeasurementFingerprintInputs,
    /// Driver versions of the fingerprint's GPU set, from the
    /// telemetry snapshot the inputs were resolved against.
    pub(super) driver_versions: Vec<String>,
}

/// Resolve the target of a benchmark job (measurement or
/// estimate): the backend's config, its current measurement
/// fingerprint, its model id, and the resolved fingerprint
/// inputs (the v2 entry's recorded snapshot and identity).
/// Benchmark jobs run only against a running backend — neither
/// starts one.
pub(super) async fn benchmark_target(
    state: &AppState,
    backend_id: &str,
    job_noun: &str,
) -> Result<BenchmarkTarget, ApiError> {
    let cfg = state.config.read().await.clone();
    let backend = cfg
        .backends
        .iter()
        .find(|b| b.id == backend_id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {backend_id}")))?
        .clone();
    let gathered = gather(state, &cfg).await;
    if gathered.running.get(backend_id).copied() != Some(true) {
        return Err(ApiError::InvalidInput(format!(
            "backend '{backend_id}' is not running; {job_noun} run against a running backend"
        )));
    }
    let fingerprint = gathered
        .fingerprints
        .get(backend_id)
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("backend {backend_id}")))?;
    let model_id = crate::model_for_backend(&cfg, &backend)
        .map(|m| m.id.clone())
        .unwrap_or_else(|| {
            backend
                .model_file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| backend.id.clone())
        });
    let inputs = gathered
        .input_sets
        .get(backend_id)
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("backend {backend_id}")))?;
    let driver_versions = driver_versions_for(&inputs, &gathered.gpu_driver_versions);
    Ok(BenchmarkTarget {
        backend,
        fingerprint,
        model_id,
        inputs,
        driver_versions,
    })
}

/// Measure one running backend (Phase 5a, §A.1): the
/// standardized benchmark — one discarded warm-up then three
/// measured runs per prompt, short then long, sequentially —
/// against the backend's loopback port, under the shared
/// GPU-job slot with scoped cancellation. The result is
/// appended to the measurement store under the backend's
/// config fingerprint (store v2 append semantics — lookup
/// takes the newest); the backend is never started, stopped,
/// or reconfigured.
pub async fn op_optimize_measure_backend(
    state: &std::sync::Arc<crate::AppState>,
    _sink: &std::sync::Arc<dyn crate::EventSink>,
    backend_id: String,
) -> crate::ApiResult {
    {
        let slot = state.optimize.job_abort.lock().await;
        if slot.is_some() {
            return Err(ApiError::InvalidInput(
                "a measurement or experiment job is already running".to_string(),
            ));
        }
    }
    // Mutual exclusion (tracker #70): claim this backend for a
    // benchmark before any work starts — a consultation running
    // on the same backend refuses the start, naming the running
    // operation, instead of the figures being taken under
    // contention. The guard releases the record on every exit
    // below (completion, failure, cancellation, save failure).
    let _benchmark_op_guard =
        OptimizeOpGuard::claim(state, &backend_id, OptimizeOpFamily::Benchmark, "measure")?;
    let target = benchmark_target(state, &backend_id, "measurements").await?;
    let backend = target.backend.clone();
    let fingerprint = target.fingerprint.clone();
    let model_id = target.model_id.clone();
    opt_log1(
        state,
        &format!(
            "measurement start: backend '{backend_id}' model '{model_id}' fingerprint {fingerprint}"
        ),
    );
    let task_id = state
        .record_task(
            "optimize-measure",
            json!({ "backend": backend_id, "fingerprint": fingerprint }),
        )
        .await;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    {
        let mut slot = state.optimize.job_abort.lock().await;
        if slot.is_some() {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "error": "a measurement or experiment job is already running" }),
                )
                .await;
            return Err(ApiError::InvalidInput(
                "a measurement or experiment job is already running".to_string(),
            ));
        }
        *slot = Some(tx);
    }
    state
        .tasks
        .controls
        .set_canceller(&task_id, crate::tasks::TaskCanceller::OptimizeJob);
    let job_started_ms = record_job_start(state, "measure", &backend_id).await;
    // Loopback by construction, as with consultations (§3).
    let base_url = format!("http://127.0.0.1:{}", backend.port);
    let run_model = model_id.clone();
    let run_backend = backend_id.clone();
    let run_fingerprint = fingerprint.clone();
    // The v2 provenance the entry records (design §4.2): the
    // readable inputs snapshot, the hash-or-size identity the
    // fingerprint used, the model file name, and the driver
    // versions of the fingerprint's GPU set.
    let run_snapshot = MeasurementInputs::from_fingerprint_inputs(&target.inputs);
    let (run_sha256, run_size_bytes) = entry_identity(&target.inputs);
    let run_model_file = target
        .backend
        .model_file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    let run_drivers = target.driver_versions.clone();
    let log_state = state.clone();
    let run = async move {
        let mut results: Vec<BenchResult> = Vec::new();
        let prompts = benchmark_prompts();
        let prompt_count = prompts.len();
        for (idx, prompt) in prompts.iter().enumerate() {
            // Per-prompt progress (tracker #69): a slow run is
            // observable in the Optimize log while it happens,
            // not only at its terminal line.
            opt_log1(
                &log_state,
                &format!(
                    "measurement prompt {}/{} ({}): start (backend '{}', warm-up plus {} measured runs)",
                    idx + 1,
                    prompt_count,
                    prompt.name,
                    run_backend,
                    MEASURED_RUNS
                ),
            );
            // Warm-up, discarded (§A.1): first-run effects after
            // a model load are not the configuration's steady
            // state.
            let result = benchmark_one_prompt(
                &base_url,
                &run_model,
                prompt,
                true,
                MEASURED_RUNS,
                MEASURE_RUN_TIMEOUT,
            )
            .await?;
            opt_log1(
                &log_state,
                &format!(
                    "measurement prompt {}/{} ({}): finished (backend '{}'): generation {} tok/s, prompt {} tok/s, TTFT {} ms",
                    idx + 1,
                    prompt_count,
                    prompt.name,
                    run_backend,
                    result.predicted_per_second.map(|x| format!("{x:.1}")).unwrap_or_else(|| "n/a".to_string()),
                    result.prompt_per_second.map(|x| format!("{x:.1}")).unwrap_or_else(|| "n/a".to_string()),
                    result.ttft_ms.map(|x| format!("{x:.1}")).unwrap_or_else(|| "n/a".to_string())
                ),
            );
            results.push(result);
        }
        // The server's build string, recorded when obtainable
        // (§4.2); its absence never fails the measurement.
        let llama_build = manager_supervisor::server_build_info(&base_url).await;
        let mut iter = results.into_iter();
        Ok::<MeasurementEntry, String>(MeasurementEntry {
            fingerprint: run_fingerprint,
            backend_id: run_backend,
            model_id: run_model,
            measured_at: chrono::Utc::now().to_rfc3339(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            short: iter
                .next()
                .unwrap_or_else(|| BenchResult::from_runs(Vec::new())),
            long: iter
                .next()
                .unwrap_or_else(|| BenchResult::from_runs(Vec::new())),
            source: Some(MeasurementSource::Benchmark),
            origin_backend_id: None,
            model_file: run_model_file,
            model_sha256: run_sha256,
            model_size_bytes: run_size_bytes,
            inputs: Some(run_snapshot),
            driver_version: run_drivers,
            llama_build,
            residency: None,
            peak_temp_c: None,
        })
    };
    let outcome = tokio::select! {
        res = run => Some(res),
        _ = rx => None,
    };
    {
        let mut slot = state.optimize.job_abort.lock().await;
        *slot = None;
    }
    let entry = match outcome {
        None => {
            opt_log1(
                state,
                &format!("measurement cancelled (backend '{backend_id}')"),
            );
            record_job_end(state, job_started_ms).await;
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Aborted,
                    json!({ "backend": backend_id }),
                )
                .await;
            return Err(ApiError::InvalidInput("measurement cancelled".to_string()));
        }
        Some(Err(e)) => {
            opt_log1(
                state,
                &format!("measurement failed: backend '{backend_id}': {e}"),
            );
            record_job_failure(state, job_started_ms, format!("measurement failed: {e}")).await;
            state
                .finish_task(&task_id, TaskStatus::Failed, json!({ "error": e.clone() }))
                .await;
            return Err(ApiError::InvalidInput(format!("measurement failed: {e}")));
        }
        Some(Ok(entry)) => entry,
    };
    {
        let rate = |v: Option<f64>| {
            v.map(|x| format!("{x:.1}"))
                .unwrap_or_else(|| "n/a".to_string())
        };
        opt_log1(
            state,
            &format!(
                "measurement result: backend '{}' model '{}': generation {} tok/s (short), {} tok/s (long); prompt {} tok/s; TTFT {} ms",
                entry.backend_id,
                entry.model_id,
                rate(entry.short.predicted_per_second),
                rate(entry.long.predicted_per_second),
                rate(entry.long.prompt_per_second.or(entry.short.prompt_per_second)),
                rate(entry.short.ttft_ms.or(entry.long.ttft_ms))
            ),
        );
    }
    let mut store = MeasurementStore::load(&measurements_path(state));
    store.upsert(entry);
    if let Err(e) = store.save(&measurements_path(state)) {
        record_job_failure(state, job_started_ms, format!("measurement failed: {e}")).await;
        return Err(ApiError::InvalidInput(e));
    }
    record_job_success(state, job_started_ms).await;
    state
        .finish_task(
            &task_id,
            TaskStatus::Done,
            json!({ "backend": backend_id, "fingerprint": fingerprint }),
        )
        .await;
    state_payload(state).await
}

/// Estimate one running backend (tracker #56): the
/// abbreviated benchmark — the same two standardized prompts,
/// one run each, no separate warm-up, reduced generation
/// budgets ([`estimate_prompts`]) — under the same shared
/// GPU-job slot and cancellation discipline as a measurement.
/// The backend is already running, so each prompt's single run
/// doubles as its warm-up; the figures are directional, which
/// is why they are stored as an estimate under their own
/// basis: [`MeasurementStore::upsert_estimate`] never touches
/// a measurement under the same fingerprint.
pub async fn op_optimize_estimate_backend(
    state: &std::sync::Arc<crate::AppState>,
    _sink: &std::sync::Arc<dyn crate::EventSink>,
    backend_id: String,
) -> crate::ApiResult {
    {
        let slot = state.optimize.job_abort.lock().await;
        if slot.is_some() {
            return Err(ApiError::InvalidInput(
                "a measurement or experiment job is already running".to_string(),
            ));
        }
    }
    // Mutual exclusion (tracker #70), as with measurements: an
    // estimate is a benchmark — a consultation running on the
    // same backend refuses its start, naming the running
    // operation (the Oct 20, 2026 case was an Estimate started
    // during a consultation on 'coder'). The guard releases the
    // record on every exit below.
    let _benchmark_op_guard =
        OptimizeOpGuard::claim(state, &backend_id, OptimizeOpFamily::Benchmark, "estimate")?;
    let target = benchmark_target(state, &backend_id, "estimates").await?;
    let backend = target.backend.clone();
    let fingerprint = target.fingerprint.clone();
    let model_id = target.model_id.clone();
    opt_log1(
        state,
        &format!(
            "estimate start: backend '{backend_id}' model '{model_id}' fingerprint {fingerprint} (abbreviated profile: one run per prompt, no warm-up, reduced generation budgets)"
        ),
    );
    let task_id = state
        .record_task(
            "optimize-estimate",
            json!({ "backend": backend_id, "fingerprint": fingerprint }),
        )
        .await;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    {
        let mut slot = state.optimize.job_abort.lock().await;
        if slot.is_some() {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "error": "a measurement or experiment job is already running" }),
                )
                .await;
            return Err(ApiError::InvalidInput(
                "a measurement or experiment job is already running".to_string(),
            ));
        }
        *slot = Some(tx);
    }
    state
        .tasks
        .controls
        .set_canceller(&task_id, crate::tasks::TaskCanceller::OptimizeJob);
    let job_started_ms = record_job_start(state, "estimate", &backend_id).await;
    // Loopback by construction, as with measurements (§3).
    let base_url = format!("http://127.0.0.1:{}", backend.port);
    let run_model = model_id.clone();
    let run_backend = backend_id.clone();
    let run_fingerprint = fingerprint.clone();
    let log_state = state.clone();
    let run = async move {
        let mut results: Vec<BenchResult> = Vec::new();
        let prompts = estimate_prompts();
        let prompt_count = prompts.len();
        for (idx, prompt) in prompts.iter().enumerate() {
            // Per-prompt progress (tracker #69), as with
            // measurements: a slow (fully CPU-offloaded) run is
            // observable while it happens.
            opt_log1(
                &log_state,
                &format!(
                    "estimate prompt {}/{} ({}): start (backend '{}', one run, no warm-up)",
                    idx + 1,
                    prompt_count,
                    prompt.name,
                    run_backend
                ),
            );
            let result = benchmark_one_prompt(
                &base_url,
                &run_model,
                prompt,
                false,
                ESTIMATE_RUNS,
                MEASURE_RUN_TIMEOUT,
            )
            .await?;
            opt_log1(
                &log_state,
                &format!(
                    "estimate prompt {}/{} ({}): finished (backend '{}'): generation {} tok/s, prompt {} tok/s, TTFT {} ms",
                    idx + 1,
                    prompt_count,
                    prompt.name,
                    run_backend,
                    result.predicted_per_second.map(|x| format!("{x:.1}")).unwrap_or_else(|| "n/a".to_string()),
                    result.prompt_per_second.map(|x| format!("{x:.1}")).unwrap_or_else(|| "n/a".to_string()),
                    result.ttft_ms.map(|x| format!("{x:.1}")).unwrap_or_else(|| "n/a".to_string())
                ),
            );
            results.push(result);
        }
        let mut iter = results.into_iter();
        Ok::<EstimateEntry, String>(EstimateEntry {
            fingerprint: run_fingerprint,
            backend_id: run_backend,
            model_id: run_model,
            estimated_at: chrono::Utc::now().to_rfc3339(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            short: iter
                .next()
                .unwrap_or_else(|| BenchResult::from_runs(Vec::new())),
            long: iter
                .next()
                .unwrap_or_else(|| BenchResult::from_runs(Vec::new())),
        })
    };
    let outcome = tokio::select! {
        res = run => Some(res),
        _ = rx => None,
    };
    {
        let mut slot = state.optimize.job_abort.lock().await;
        *slot = None;
    }
    let entry = match outcome {
        None => {
            opt_log1(
                state,
                &format!("estimate cancelled (backend '{backend_id}')"),
            );
            record_job_end(state, job_started_ms).await;
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Aborted,
                    json!({ "backend": backend_id }),
                )
                .await;
            return Err(ApiError::InvalidInput("estimate cancelled".to_string()));
        }
        Some(Err(e)) => {
            opt_log1(
                state,
                &format!("estimate failed: backend '{backend_id}': {e}"),
            );
            record_job_failure(state, job_started_ms, format!("estimate failed: {e}")).await;
            state
                .finish_task(&task_id, TaskStatus::Failed, json!({ "error": e.clone() }))
                .await;
            return Err(ApiError::InvalidInput(format!("estimate failed: {e}")));
        }
        Some(Ok(entry)) => entry,
    };
    {
        let rate = |v: Option<f64>| {
            v.map(|x| format!("{x:.1}"))
                .unwrap_or_else(|| "n/a".to_string())
        };
        opt_log1(
            state,
            &format!(
                "estimate result: backend '{}' model '{}': generation {} tok/s (short), {} tok/s (long); prompt {} tok/s; TTFT {} ms (estimate)",
                entry.backend_id,
                entry.model_id,
                rate(entry.short.predicted_per_second),
                rate(entry.long.predicted_per_second),
                rate(entry.long.prompt_per_second.or(entry.short.prompt_per_second)),
                rate(entry.short.ttft_ms.or(entry.long.ttft_ms))
            ),
        );
    }
    let mut store = MeasurementStore::load(&measurements_path(state));
    store.upsert_estimate(entry);
    if let Err(e) = store.save(&measurements_path(state)) {
        record_job_failure(state, job_started_ms, format!("estimate failed: {e}")).await;
        return Err(ApiError::InvalidInput(e));
    }
    record_job_success(state, job_started_ms).await;
    state
        .finish_task(
            &task_id,
            TaskStatus::Done,
            json!({ "backend": backend_id, "fingerprint": fingerprint }),
        )
        .await;
    state_payload(state).await
}

/// Cancel the running GPU-bound Optimize job (a measurement,
/// or an experiment batch in a later sub-phase): fires the
/// shared job slot's abort handle, which drops the job's
/// in-flight benchmark future.
pub async fn op_optimize_cancel_job(
    state: &std::sync::Arc<crate::AppState>,
    _sink: &std::sync::Arc<dyn crate::EventSink>,
) -> crate::ApiResult {
    let sender = {
        let mut slot = state.optimize.job_abort.lock().await;
        slot.take()
    };
    match sender {
        Some(tx) => {
            let _ = tx.send(());
            // Mutual exclusion (tracker #70): as with a cancelled
            // consultation, the cancel clears the benchmark's
            // per-backend record now, so a consultation may start
            // on that backend immediately.
            clear_running_ops(state, OptimizeOpFamily::Benchmark);
            Ok(json!({ "cancelled": true }))
        }
        None => Ok(json!({ "cancelled": false })),
    }
}

/// Set one plan item's tri-state decision (§5.2) and persist it.
pub async fn op_optimize_set_item_state(
    state: &std::sync::Arc<crate::AppState>,
    _sink: &std::sync::Arc<dyn crate::EventSink>,
    item_id: usize,
    decision: String,
) -> crate::ApiResult {
    let decision = match decision.as_str() {
        "accepted" => Decision::Accepted,
        "denied" => Decision::Denied,
        "undecided" => Decision::Undecided,
        other => {
            return Err(ApiError::InvalidInput(format!(
                "unknown decision '{other}'"
            )))
        }
    };
    let mut file = load_plan_file_current(state).await;
    let plan = file
        .plan
        .as_mut()
        .ok_or_else(|| ApiError::NotFound("no plan".to_string()))?;
    let item = plan
        .items
        .iter_mut()
        .find(|i| i.id == item_id)
        .ok_or_else(|| ApiError::NotFound(format!("plan item {item_id}")))?;
    if item.kind.is_advice() {
        return Err(ApiError::InvalidInput(
            "advice items have no decision".to_string(),
        ));
    }
    item.decision = decision;
    if decision == Decision::Denied {
        let fp = item.fingerprint.clone();
        if !file.denied_fingerprints.contains(&fp) {
            file.denied_fingerprints.push(fp);
        }
    }
    opt_log1(
        state,
        &format!(
            "item {item_id} decision -> {decision:?} ({:?} {})",
            item.kind,
            item.target.as_deref().unwrap_or("-")
        ),
    );
    save_plan_file(state, &file).map_err(ApiError::InvalidInput)?;
    state_payload(state).await
}

/// Record an apply outcome on one item (Phase 4 bookkeeping,
/// §5.2): the UI orchestrator reports each item as it executes.
pub async fn op_optimize_set_item_outcome(
    state: &std::sync::Arc<crate::AppState>,
    _sink: &std::sync::Arc<dyn crate::EventSink>,
    item_id: usize,
    outcome: String,
    error: Option<String>,
) -> crate::ApiResult {
    let outcome = match outcome.as_str() {
        "applied" => manager_optimize::ApplyOutcome::Applied,
        "failed" => manager_optimize::ApplyOutcome::Failed,
        "not_attempted" => manager_optimize::ApplyOutcome::NotAttempted,
        other => return Err(ApiError::InvalidInput(format!("unknown outcome '{other}'"))),
    };
    let mut file = load_plan_file_current(state).await;
    let plan = file
        .plan
        .as_mut()
        .ok_or_else(|| ApiError::NotFound("no plan".to_string()))?;
    let item = plan
        .items
        .iter_mut()
        .find(|i| i.id == item_id)
        .ok_or_else(|| ApiError::NotFound(format!("plan item {item_id}")))?;
    let apply_error = if outcome == manager_optimize::ApplyOutcome::Failed {
        error
    } else {
        None
    };
    // The server-visible apply trail (tracker #52): the UI
    // orchestrates apply through the mutation commands, so these
    // outcome records are where per-item apply results land.
    opt_log1(
        state,
        &format!(
            "apply outcome: item {item_id} ({:?} {}) -> {outcome:?}{}",
            item.kind,
            item.target.as_deref().unwrap_or("-"),
            apply_error
                .as_ref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        ),
    );
    item.apply_outcome = Some(outcome);
    item.apply_error = apply_error;
    save_plan_file(state, &file).map_err(ApiError::InvalidInput)?;
    state_payload(state).await
}

/// Re-validate the stored plan against live state. Validation
/// states are derived on every state read, so this simply
/// returns the refreshed payload, with the plan's configuration
/// hash brought up to date: re-validating IS the answer to "the
/// configuration changed since this plan".
pub async fn op_optimize_validate_plan(
    state: &std::sync::Arc<crate::AppState>,
    _sink: &std::sync::Arc<dyn crate::EventSink>,
) -> crate::ApiResult {
    let current = {
        let cfg = state.config.read().await;
        current_config_hash(&cfg)
    };
    let mut file = load_plan_file_current(state).await;
    if let Some(plan) = file.plan.as_mut().filter(|p| p.config_hash != current) {
        plan.config_hash = current;
        save_plan_file(state, &file).map_err(ApiError::InvalidInput)?;
    }
    state_payload(state).await
}

/// Discard the stored plan and its decisions (§1.2). Denial
/// memory is kept: those were explicit operator decisions.
pub async fn op_optimize_discard_plan(
    state: &std::sync::Arc<crate::AppState>,
    _sink: &std::sync::Arc<dyn crate::EventSink>,
) -> crate::ApiResult {
    let mut file = load_plan_file(state);
    file.plan = None;
    file.sample = false;
    save_plan_file(state, &file).map_err(ApiError::InvalidInput)?;
    state_payload(state).await
}
