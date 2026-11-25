//! Sweep execution: running each arm and recording its rows.

use super::*;

// ---------------------------------------------------------------------------
// One arm's run (§2.1 protocol): temp backend, preflight,
// tokenized depth padding, warm-up + measured runs, spec log
// collection, VRAM peak sampling.
// ---------------------------------------------------------------------------

pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(900);
pub(super) const FIXED_SEED: u32 = 42;

pub(super) struct ArmOutcomeData {
    pub(super) status: mb::ArmStatus,
    pub(super) detail: Option<String>,
    pub(super) runs: Vec<mb::RunRow>,
    pub(super) peak_vram_mib: Option<u64>,
    /// First measured run's output text (correctness control).
    pub(super) first_output: Option<String>,
    /// The sweep's llama build, learned from this arm's server.
    pub(super) llama_build: Option<String>,
    /// The library prompt's tokenized count, learned per sweep.
    pub(super) prompt_tokens: Option<u32>,
}

/// The log lines a request added to the temp backend's ring,
/// reduced to the newest window after `before` (the ring's
/// newest record before the request). Records come back
/// newest-first; matching the pre-request newest record inside
/// the post-request page bounds the new window.
pub(super) fn new_log_lines(
    sup: &manager_supervisor::Supervisor,
    temp_id: &str,
    before_newest: Option<&manager_supervisor::LogRecord>,
) -> Vec<String> {
    let result = sup.logs_query(temp_id, None, None, 4000);
    let mut lines: Vec<String> = Vec::new();
    for rec in &result.records {
        if let Some(b) = before_newest {
            if rec.ts == b.ts && rec.line == b.line {
                break;
            }
        }
        lines.push(rec.line.clone());
    }
    lines.reverse(); // chronological
    lines
}

pub(super) fn newest_log_record(
    sup: &manager_supervisor::Supervisor,
    temp_id: &str,
) -> Option<manager_supervisor::LogRecord> {
    sup.logs_query(temp_id, None, None, 1)
        .records
        .into_iter()
        .next()
}

/// Parse one request window's lines into a run row.
pub(super) fn run_row_from(
    sample: &crate::bench_client::Sample,
    lines: &[String],
    warmup: bool,
    prompt_tokens: Option<u32>,
) -> mb::RunRow {
    let mut row = mb::RunRow {
        warmup,
        prompt_tps: sample.prompt_per_second,
        gen_tps: sample.predicted_per_second,
        ttft_ms: sample.ttft_ms,
        prompt_tokens,
        generated_tokens: None,
        acceptance_rate: None,
        accepted: None,
        generated: None,
        mean_accepted_len: None,
        drafters: Vec::new(),
    };
    // The aggregate line is the request's own verdict; the
    // statistics lines attribute it per drafter (§1.5).
    for line in lines {
        if let Some(acc) = mb::parse_acceptance_line(line) {
            row.acceptance_rate = Some(acc.rate);
            row.accepted = Some(acc.accepted);
            row.generated = Some(acc.generated);
            row.mean_accepted_len = Some(acc.mean_len);
            row.generated_tokens = Some(acc.generated as u32);
        }
        if let Some(stats) = mb::parse_statistics_line(line) {
            if let Some(existing) = row
                .drafters
                .iter_mut()
                .find(|d: &&mut mb::DrafterAgg| d.kind == stats.kind)
            {
                existing.gen_tokens += stats.gen_tokens;
                existing.acc_tokens += stats.acc_tokens;
            } else {
                row.drafters.push(mb::DrafterAgg {
                    kind: stats.kind.clone(),
                    gen_tokens: stats.gen_tokens,
                    acc_tokens: stats.acc_tokens,
                });
            }
        }
    }
    row
}

/// Stops the VRAM sampler when the arm ends, however it ends (a
/// cancelled arm's future is dropped).
struct StopOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// MiB in use across the GPUs a backend is placed on.
async fn placed_used_mib(state: &AppState, b: &BackendConfig) -> Option<u64> {
    let store = state.telemetry().await;
    let samples = crate::lock_telemetry(&store).snapshot();
    used_on_placed(b, &samples)
}

fn used_on_placed(b: &BackendConfig, samples: &[crate::GpuSample]) -> Option<u64> {
    let placed = match crate::backend_gpu_visibility(b) {
        crate::GpuVisibility::Gpus(indices) => indices,
        crate::GpuVisibility::CpuOnly => return None,
        crate::GpuVisibility::Undetermined => samples.iter().map(|s| s.index).collect(),
    };
    let used: Vec<u64> = samples
        .iter()
        .filter(|s| placed.contains(&s.index) && !s.down)
        .map(|s| s.vram_used_mib)
        .collect();
    (!used.is_empty()).then(|| used.iter().sum())
}

/// Immutable per-sweep context the arm runner needs.
pub(super) struct SweepCtx {
    pub(super) cfg: AppConfig,
    pub(super) target: BackendConfig,
    pub(super) model_id: String,
    pub(super) temperature: f32,
    pub(super) draft_path: Option<String>,
    pub(super) draft_device_arg: Option<String>,
}

pub(super) async fn set_progress(
    state: &AppState,
    arm_index: usize,
    arm_count: usize,
    phase: &str,
    run: usize,
) {
    if let Ok(mut slot) = state.bench.job.try_lock() {
        if let Some(job) = slot.as_mut() {
            job.progress.arm_index = arm_index;
            job.progress.arm_count = arm_count;
            job.progress.phase = phase.to_string();
            job.progress.run = run;
        }
    }
}

/// Save one sweep record into the results store (replace by id
/// or append).
pub(super) fn save_sweep(state: &AppState, record: &mb::SweepRecord) {
    let mut results = load_results(state);
    if let Some(existing) = results.sweeps.iter_mut().find(|s| s.id == record.id) {
        *existing = record.clone();
    } else {
        results.sweeps.push(record.clone());
    }
    save_results(state, &results);
}

/// Run one arm end to end. Every failure is data (an
/// ArmOutcomeData with a non-Measured status), never an Err:
/// the sweep records it and moves on. Cleanup is the CALLER's
/// job — it lives outside this future so cancellation cannot
/// skip it.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_arm(
    state: &AppState,
    ctx: &SweepCtx,
    sweep_id: &str,
    arm_index: usize,
    arm_count: usize,
    spec: &mb::ArmSpec,
    raw_flags: &str,
    port: u16,
    prompt_body: &str,
) -> ArmOutcomeData {
    let mut out = ArmOutcomeData {
        status: mb::ArmStatus::Failed,
        detail: None,
        runs: Vec::new(),
        peak_vram_mib: None,
        first_output: None,
        llama_build: None,
        prompt_tokens: None,
    };
    let temp_id = format!("bench-{}", arm_index + 1);
    let mut temp = ctx.target.clone();
    temp.id = temp_id.clone();
    temp.port = port;
    temp.flags.n_ctx = spec.n_ctx;
    // The bench backend runs at the most verbose log level the
    // app exposes: the per-drafter TRACE statistics (§1.5) are
    // the combined arms' attribution source.
    temp.raw_flags = Some(format!("{raw_flags} --verbose"));
    let mut temp_cfg = ctx.cfg.clone();
    temp_cfg.backends.push(temp.clone());

    // -- Start preflight (§6.3): the same gate every app-mediated
    // start passes, evaluated against the cloned config carrying
    // the temp backend. Its verdicts carry the numbers.
    if let Err(e) =
        crate::optimize_experiments::preflight_after_release(state, &temp_cfg, &temp).await
    {
        out.status = mb::ArmStatus::PreflightFailed;
        out.detail = Some(format!("start preflight refused this arm: {e}"));
        return out;
    }

    // -- Journal before the spawn, so a crash from here on
    // leaves a sweep record.
    {
        let mut journal = load_journal(state);
        journal.add_active(ActiveBench {
            temp_id: temp_id.clone(),
            pid: None,
            port,
            sweep_id: sweep_id.to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
        });
        save_journal(state, &journal);
    }

    // The device memory in use before the spawn: where the driver
    // reports no per-process figure (WDDM), the arm's peak is the
    // rise over this.
    let used_before = placed_used_mib(state, &temp).await;

    // -- Register the spec directly and spawn.
    let pid: Option<u32> = {
        let store = state.telemetry().await;
        let gpus = crate::lock_telemetry(&store).inventory();
        let spec_proc = crate::backend_spec(&ctx.cfg, &temp, &gpus);
        let mut sup = state.supervisor().await.lock().await;
        sup.register_spec(spec_proc);
        if let Err(e) = sup.spawn_backend(&temp_id).await {
            out.detail = Some(format!("temp backend {temp_id} failed to start: {e:?}"));
            return out;
        }
        sup.status()
            .into_iter()
            .find(|s| s.id == temp_id)
            .and_then(|s| s.pid)
    };
    if let Some(pid) = pid {
        let mut journal = load_journal(state);
        journal.set_active_pid(&temp_id, pid);
        save_journal(state, &journal);
    }

    // -- Wait healthy on the existing probe.
    if let Err(e) = crate::optimize_experiments::wait_healthy(state, &temp_id, port).await {
        out.detail = Some(e);
        return out;
    }
    let base_url = format!("http://127.0.0.1:{port}");
    out.llama_build = manager_supervisor::server_build_info(&base_url).await;

    // -- The measured prompt: the library prompt padded to the
    // arm's depth with DISTINCT shipped library text (§2.1 —
    // never repeated filler, which would falsify n-gram
    // acceptance). Depth 0, or a prompt already at depth, runs
    // unpadded.
    let library_tokens = tokenize_count(&base_url, prompt_body).await;
    out.prompt_tokens = library_tokens;
    let measured_prompt = match library_tokens {
        Some(have) if spec.depth > have => {
            let need = spec.depth - have;
            let mut pad = String::new();
            let mut est: u32 = 0;
            let blocks = mb::padding_blocks();
            let mut bi = 0usize;
            while est < need && !blocks.is_empty() {
                let block = blocks[bi % blocks.len()];
                // The padding estimate is a character heuristic;
                // the run rows carry the backend's own prompt
                // rate, and the depth is the target the padding
                // aims at, not a claim about the exact fill.
                est += (block.len() as u32) / 4;
                pad.push_str(block);
                pad.push_str("\n\n");
                bi += 1;
            }
            format!("{pad}{prompt_body}")
        }
        _ => prompt_body.to_string(),
    };
    let measured_tokens = tokenize_count(&base_url, &measured_prompt).await;

    // -- VRAM peak sampling for the arm's duration (§5.1).
    let peak = Arc::new(std::sync::Mutex::new(Option::<u64>::None));
    let sampler_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _stop_sampler = StopOnDrop(Arc::clone(&sampler_stop));
    let sampler = if let Some(pid) = pid {
        let store = state.telemetry().await;
        let peak = Arc::clone(&peak);
        let stop = Arc::clone(&sampler_stop);
        let placed = temp.clone();
        Some(tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                {
                    let guard = crate::lock_telemetry(&store);
                    let total: u64 = guard
                        .last_processes()
                        .iter()
                        .filter(|p| p.pid == pid)
                        .filter_map(|p| p.vram_mib)
                        .sum();
                    let total = if total > 0 {
                        total
                    } else {
                        used_on_placed(&placed, &guard.snapshot())
                            .zip(used_before)
                            .map(|(now, before)| now.saturating_sub(before))
                            .unwrap_or(0)
                    };
                    if total > 0 {
                        let mut slot = peak.lock().unwrap_or_else(|e| e.into_inner());
                        *slot = Some(slot.map(|p| p.max(total)).unwrap_or(total));
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }))
    } else {
        None
    };

    // -- Warm-up (discarded from the medians; its row is kept,
    // flagged), then the measured runs. The spec-log window for
    // each request is the ring's growth across that request.
    let mut first_output: Option<String> = None;
    let mut failure: Option<String> = None;
    let phases: Vec<(bool, u32, &str)> = std::iter::once((true, mb::WARMUP_MAX_TOKENS, "warming"))
        .chain((0..mb::MEASURED_RUNS).map(|_| (false, mb::MEASURED_MAX_TOKENS, "measuring")))
        .collect();
    for (step, (warmup, cap, phase)) in phases.iter().enumerate() {
        set_progress(
            state,
            arm_index,
            arm_count,
            phase,
            if *warmup { 0 } else { step },
        )
        .await;
        let before = {
            let sup = state.supervisor().await.lock().await;
            newest_log_record(&sup, &temp_id)
        };
        match crate::bench_client::complete(
            &base_url,
            &ctx.model_id,
            &measured_prompt,
            ctx.temperature,
            *cap,
            Some(FIXED_SEED),
            REQUEST_TIMEOUT,
        )
        .await
        {
            Ok(sample) => {
                let lines = {
                    let sup = state.supervisor().await.lock().await;
                    new_log_lines(&sup, &temp_id, before.as_ref())
                };
                let row = run_row_from(&sample, &lines, *warmup, measured_tokens);
                if !*warmup && first_output.is_none() {
                    first_output = Some(sample.content.clone());
                }
                out.runs.push(row);
            }
            Err(e) => {
                failure = Some(format!(
                    "{} failed: {e}",
                    if *warmup { "warm-up" } else { "benchmark run" }
                ));
                break;
            }
        }
    }
    sampler_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(handle) = sampler {
        let _ = handle.await;
    }
    out.peak_vram_mib = *peak.lock().unwrap_or_else(|e| e.into_inner());
    out.first_output = first_output;
    match failure {
        Some(f) => {
            out.status = mb::ArmStatus::Failed;
            out.detail = Some(f);
        }
        None => {
            out.status = mb::ArmStatus::Measured;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The sweep executor: arms in order, cancellation between and
// within arms (cleanup always outside the arm future), the
// correctness control, and the record kept current on disk.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_sweep(
    state: &Arc<AppState>,
    ctx: &SweepCtx,
    record: &mut mb::SweepRecord,
    outcomes: Vec<mb::ArmOutcome>,
    prompt_body: &str,
    correctness_control: bool,
    rx: &mut tokio::sync::oneshot::Receiver<()>,
) -> bool {
    let arm_count = outcomes.len();
    let mut used_ports = crate::optimize_experiments::occupied_ports(&ctx.cfg);
    let mut cancelled = false;
    let mut baseline_output: Option<String> = None;
    let mut stop_note: Option<String> = None;
    for (index, outcome) in outcomes.iter().enumerate() {
        if cancelled || stop_note.is_some() {
            let arm_rec = &mut record.arms[index];
            if arm_rec.status == mb::ArmStatus::Pending {
                arm_rec.status = mb::ArmStatus::Cancelled;
                arm_rec.detail = Some(
                    stop_note
                        .clone()
                        .unwrap_or_else(|| "sweep cancelled by the operator".to_string()),
                );
            }
            continue;
        }
        match outcome {
            mb::ArmOutcome::Pruned { reason, .. } => {
                let arm_rec = &mut record.arms[index];
                arm_rec.status = mb::ArmStatus::Pruned;
                arm_rec.detail = Some(reason.clone());
                save_sweep(state, record);
            }
            mb::ArmOutcome::Ready(spec) => {
                let raw_flags = mb::arm_raw_flags(
                    ctx.target.raw_flags.as_deref(),
                    spec,
                    ctx.draft_path.as_deref(),
                    ctx.draft_device_arg.as_deref(),
                );
                {
                    let arm_rec = &mut record.arms[index];
                    arm_rec.raw_flags = raw_flags.clone();
                    arm_rec.status = mb::ArmStatus::Running;
                    arm_rec.started_ms = Some(now_ms());
                }
                save_sweep(state, record);
                set_progress(state, index, arm_count, "launching", 0).await;
                let port = manager_optimize::lowest_free_port(
                    &used_ports,
                    ctx.cfg.gateway.port,
                    ctx.cfg.postgres.port,
                );
                used_ports.insert(port);
                let temp_id = format!("bench-{}", index + 1);
                let arm_future = run_arm(
                    state,
                    ctx,
                    &record.id,
                    index,
                    arm_count,
                    spec,
                    &raw_flags,
                    port,
                    prompt_body,
                );
                let data = tokio::select! {
                    _ = &mut *rx => {
                        cancelled = true;
                        None
                    }
                    data = arm_future => Some(data),
                };
                // Cleanup is outside the arm future: it runs on
                // completion, failure, and cancellation alike.
                cleanup_bench_temp(state, &temp_id).await;
                let arm_rec = &mut record.arms[index];
                arm_rec.ended_ms = Some(now_ms());
                match data {
                    None => {
                        arm_rec.status = mb::ArmStatus::Cancelled;
                        arm_rec.detail = Some("sweep cancelled by the operator".to_string());
                        record.partial = true;
                    }
                    Some(data) => {
                        arm_rec.status = data.status;
                        arm_rec.detail = data.detail;
                        arm_rec.runs = data.runs;
                        arm_rec.peak_vram_mib = data.peak_vram_mib;
                        if !arm_rec.runs.is_empty() {
                            arm_rec.summary = Some(mb::summarize(&arm_rec.runs));
                        }
                        if record.llama_build.is_empty() {
                            if let Some(build) = &data.llama_build {
                                record.llama_build = build.clone();
                            }
                        }
                        if record.prompt_tokens.is_none() {
                            record.prompt_tokens = data.prompt_tokens;
                        }
                        if index == record.baseline_arm_index {
                            baseline_output = data.first_output.clone();
                        }
                        // Correctness control (§2.1): a spec
                        // arm whose temperature-0 output
                        // diverges from the baseline's stops
                        // the sweep; the divergence is stated
                        // on the record, never hidden.
                        if correctness_control
                            && ctx.temperature == 0.0
                            && spec.spec != mb::SpecKind::None
                            && index != record.baseline_arm_index
                            && arm_rec.status == mb::ArmStatus::Measured
                        {
                            if let (Some(base), Some(out_text)) =
                                (&baseline_output, &data.first_output)
                            {
                                let overlap = word_overlap(base, out_text);
                                if overlap < 0.5 {
                                    let note = format!(
                                        "correctness control: arm {} output diverged from the baseline (word overlap {:.0}% < 50%); sweep stopped",
                                        index + 1,
                                        overlap * 100.0
                                    );
                                    arm_rec.detail = Some(match arm_rec.detail.take() {
                                        Some(d) => format!("{d} {note}"),
                                        None => note.clone(),
                                    });
                                    stop_note = Some(note);
                                    record.partial = true;
                                }
                            }
                        }
                    }
                }
                save_sweep(state, record);
            }
        }
    }
    // Comparability keys (§3.3): one key per sweep — the
    // held-constant conditions (model identity, runner, prompt,
    // temperature, llama build) are sweep-level facts. Set once
    // the build is known from a launched arm; when no arm ever
    // launched, the key stays empty and the record says so by
    // absence rather than by a fabricated build string.
    if !record.llama_build.is_empty() {
        let key = mb::comparability_key(
            &record.model,
            &record.prompt_sha256,
            record.temperature,
            &record.llama_build,
        );
        for arm in record.arms.iter_mut() {
            arm.comparability_key = key.clone();
        }
    }
    if cancelled {
        record.partial = true;
    }
    record.status = mb::final_status(&record.arms, record.partial).to_string();
    record.ended_ms = Some(now_ms());
    save_sweep(state, record);
    cancelled
}
