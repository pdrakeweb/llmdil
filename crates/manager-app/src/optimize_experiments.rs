//! Optimize experiment runner (design addendum A.3, tracker
//! #28 Phase 5e): proposals, tri-state decisions, and the
//! dedicated batch runner.
//!
//! The runner is deliberately NOT built on `backend_add`: the
//! stock add always joins a gateway group, while a temp
//! experiment backend must live outside every group. The
//! runner therefore registers the temp backend and its
//! supervisor spec directly. The temp backend exists only in
//! an in-memory cloned configuration (for the existing
//! fit / memory-requirement machinery) and in the supervisor's
//! spec table — it never enters `config.json`, gateway config,
//! fallbacks, or routing.
//!
//! Cleanup is guaranteed structurally: the per-experiment work
//! runs as a future that cancellation (the shared job slot) or
//! the per-experiment timeout may drop at any await point, and
//! the cleanup step — stop, unregister the spec, remove the
//! journal's active record — runs AFTER that future completes
//! or is dropped, owned by the batch loop, which is never
//! itself selected against the abort handle. A startup sweep
//! (below) removes leftovers from an app crash mid-experiment.
//!
//! Budgets (§A.3, enforced here server-side): at most 3
//! experiments per batch, 30 minutes per experiment, 60
//! minutes per batch. A run never downloads: a candidate
//! whose file is not already on disk fails preflight, naming
//! the plan's register/download steps when it carries them
//! (provisioning is applied as plan items before the run),
//! else the Models view.
//!
//! Backends named in an accepted proposal's
//! `requires_stopping` are stopped only when the fit verdict
//! needs them stopped; those that were running when the batch
//! started are restarted after the batch, whatever its
//! outcome, and that is stated in each result's detail.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::json;

use manager_config::AppConfig;
use manager_optimize::{
    benchmark_prompts, compare, estimate_prompts, evaluate_fit, fit_verdict_text, is_temp_backend_id,
    lowest_free_port, resolve_candidate, temp_backend_config, temp_backend_id, ActiveExperiment,
    BenchResult, BenchRun, CandidateResolution, Decision, ExperimentEstimate, ExperimentJournal,
    ExperimentMeasured, ExperimentProposal, ExperimentResult, ExperimentStatus,
    MeasurementEntry, MeasurementFingerprintInputs, MeasurementStore, RegisteredModelFile,
    BATCH_TIMEOUT_SECS, BENCHMARK_TEMPERATURE, ESTIMATE_RUNS, EXPERIMENT_TIMEOUT_SECS, MEASURED_RUNS,
};
use manager_supervisor::{HealthProbe, HttpHealthProbe, ProcessState};

use crate::{ApiError, AppState, TaskStatus};

/// Per-run benchmark timeout, mirroring the measure command's
/// (the long prompt on a heavily offloaded backend is slow).
const EXP_RUN_TIMEOUT: Duration = Duration::from_secs(600);
/// Bounded wait for a temp backend to report healthy on its
/// `/health` probe after spawn. Model loads are minutes-scale;
/// the per-experiment budget caps the total anyway.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------------------
// Journal (<data_dir>/optimize-experiments.json)
// ---------------------------------------------------------------------------

fn journal_path(state: &AppState) -> PathBuf {
    state.data_dir.join("optimize-experiments.json")
}

fn load_journal(state: &AppState) -> ExperimentJournal {
    ExperimentJournal::load(&journal_path(state))
}

fn save_journal(state: &AppState, journal: &ExperimentJournal) {
    if let Err(e) = journal.save(&journal_path(state)) {
        eprintln!("optimize-experiments: journal save failed: {e}");
    }
}

// ---------------------------------------------------------------------------
// Startup sweep
// ---------------------------------------------------------------------------

/// Remove leftovers of experiments interrupted by an app
/// crash (design §A.3 step 5): for each active journal record,
/// terminate the recorded pid when it is alive and its image
/// is llama-server, following the gateway precedent —
/// `manager_wizard::ops::kill_process`, the same function
/// `stop_gateway` uses to terminate the pid recorded in
/// `gateway.pid`. Also defensively removes any `opt-exp-*`
/// entries from `config.backends` (they should never be
/// there: temp backends are never written to the config) and
/// clears the journal's active records. History is retained.
pub async fn startup_sweep(state: &AppState) {
    // Defensive config scrub first, so a leftover temp entry
    // can never be started or routed as a real backend.
    {
        let cfg = state.config.read().await.clone();
        if cfg.backends.iter().any(|b| is_temp_backend_id(&b.id)) {
            let mut new_cfg = cfg.clone();
            let removed: Vec<String> = new_cfg
                .backends
                .iter()
                .filter(|b| is_temp_backend_id(&b.id))
                .map(|b| b.id.clone())
                .collect();
            new_cfg.backends.retain(|b| !is_temp_backend_id(&b.id));
            for g in new_cfg.gateway.groups.iter_mut() {
                g.members.retain(|m| !removed.contains(m));
            }
            for m in new_cfg.models.iter_mut() {
                m.assigned_backends.retain(|b| !removed.contains(b));
            }
            if manager_config::save(&new_cfg, &state.config_path).is_ok() {
                *state.config.write().await = new_cfg;
                eprintln!(
                    "optimize-experiments: startup sweep removed leftover temp backends from config: {}",
                    removed.join(", ")
                );
            }
        }
    }
    let mut journal = load_journal(state);
    let active = std::mem::take(&mut journal.active);
    for record in &active {
        if let Some(pid) = record.pid {
            let alive = manager_supervisor::pid_alive(pid);
            let image = manager_supervisor::process_image_is(pid, "llama-server.exe")
                || manager_supervisor::process_image_is(pid, "llama-server");
            if alive && image {
                match manager_wizard::ops::kill_process(pid) {
                    Ok(()) => eprintln!(
                        "optimize-experiments: startup sweep terminated leftover {} (pid {pid})",
                        record.temp_id
                    ),
                    Err(e) => eprintln!(
                        "optimize-experiments: startup sweep could not terminate {} (pid {pid}): {e}",
                        record.temp_id
                    ),
                }
            }
        }
        // Belt and braces for an in-session leftover: if a spec
        // somehow exists under the temp id, stop + unregister.
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        if sup.spec(&record.temp_id).is_some() {
            let _ = sup.stop(&record.temp_id, false).await;
            sup.unregister_spec(&record.temp_id);
        }
    }
    if !active.is_empty() {
        save_journal(state, &journal);
    }
}

// ---------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------

/// Set one experiment proposal's tri-state decision (§A.3),
/// mirroring `optimize_set_item_state`, with the provisioning
/// cascade: the decision also lands on the plan items that
/// register and download the experiment's candidate model.
/// Denials accumulate into the plan file's denial memory,
/// keyed by fingerprint (the experiment's and any cascaded
/// item's), so an identical re-proposal in a later
/// consultation arrives pre-denied.
pub async fn op_optimize_set_experiment_state(
    state: &std::sync::Arc<crate::AppState>,
    _sink: &std::sync::Arc<dyn crate::EventSink>,
    experiment_id: usize,
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
    let mut file = crate::optimize::load_plan_file_current(state).await;
    let plan = file
        .plan
        .as_mut()
        .ok_or_else(|| ApiError::NotFound("no plan".to_string()))?;
    let candidate = plan
        .experiments
        .iter()
        .find(|e| e.id == experiment_id)
        .map(|e| e.candidate_model_id.clone())
        .ok_or_else(|| ApiError::NotFound(format!("experiment {experiment_id}")))?;
    // The decision cascades to the experiment's provisioning
    // items: accepting the experiment accepts the registration
    // and download steps it needs; denying it denies the steps
    // chained for it (never a shared authored registration).
    // The cascade lives in manager-optimize with the plan
    // types; denial memory records every fingerprint that
    // transitioned to denied, the experiment's included.
    let newly_denied =
        manager_optimize::apply_experiment_decision(plan, experiment_id, decision)
            .ok_or_else(|| ApiError::NotFound(format!("experiment {experiment_id}")))?;
    for fp in newly_denied {
        if !file.denied_fingerprints.contains(&fp) {
            file.denied_fingerprints.push(fp);
        }
    }
    crate::optimize::opt_log1(
        state,
        &format!(
            "experiment {experiment_id} (candidate {candidate}) decision -> {decision:?}"
        ),
    );
    crate::optimize::save_plan_file(state, &file).map_err(ApiError::InvalidInput)?;
    crate::optimize::state_payload(state).await
}

// ---------------------------------------------------------------------------
// The batch runner
// ---------------------------------------------------------------------------

/// What one batch pass does with each accepted proposal
/// (tracker #56): a full run (benchmark + optional eval,
/// recorded as an experiment result) or an estimate pass (the
/// abbreviated benchmark only, recorded on the proposal as an
/// estimate — never as a result).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunMode {
    Full,
    Estimate,
}

/// Per-batch mutable context the experiment runs share.
struct BatchCtx {
    consulted_backend: String,
    /// Backends running when the batch started.
    batch_running: HashSet<String>,
    /// requires_stopping backends this batch actually stopped
    /// and therefore owes a restart afterwards.
    stopped_to_restart: Vec<String>,
    /// Ports already handed to temp backends in this batch.
    used_ports: HashSet<u16>,
}

/// One experiment's outcome, before the batch loop wraps it in
/// an `ExperimentResult` (timestamps + identity are the loop's).
struct InnerOutcome {
    status: ExperimentStatus,
    detail: String,
    measured: Option<ExperimentMeasured>,
    eval: Option<manager_optimize::EvalRunRecord>,
    comparison: Option<manager_optimize::EvalComparison>,
    preflight: String,
    /// Estimate-pass figures (tracker #56); `Some` only for a
    /// completed estimate pass. The batch loop stamps the
    /// batch id and records it via `record_estimate`.
    estimate: Option<ExperimentEstimate>,
}

impl InnerOutcome {
    fn failed(detail: String, preflight: String) -> Self {
        InnerOutcome {
            status: ExperimentStatus::Failed,
            detail,
            measured: None,
            eval: None,
            comparison: None,
            preflight,
            estimate: None,
        }
    }
}

fn fmt_rate(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.1}")).unwrap_or_else(|| "—".to_string())
}

/// Guaranteed cleanup for one temp backend: stop it (when
/// live), unregister its spec, and remove its journal active
/// record. This runs outside the experiment future, so neither
/// cancellation nor timeout can skip it; the temp id is owned
/// here, independently of that future.
pub(crate) async fn cleanup_temp(state: &AppState, temp_id: &str) {
    {
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        let live = sup
            .status()
            .iter()
            .any(|s| s.id == temp_id && s.pid.is_some());
        if live {
            let _ = sup.stop(temp_id, false).await;
        }
        sup.unregister_spec(temp_id);
    }
    let mut journal = load_journal(state);
    if journal.remove_active(temp_id) {
        save_journal(state, &journal);
    }
}

/// Wait for a spawned temp backend to answer Healthy on the
/// existing health probe. No backend probes are registered
/// anywhere in the app (the supervisor's probe registry is
/// unused for backends), so — exactly as `mcp_test_connection`
/// does — the wait uses a local `HttpHealthProbe` against the
/// backend's loopback `/health`, polled until healthy, until
/// the supervisor reports the process gone/failed, or until
/// the bounded wait expires.
pub(crate) async fn wait_healthy(state: &AppState, temp_id: &str, port: u16) -> Result<(), String> {
    let probe = HttpHealthProbe::new(
        format!("http://127.0.0.1:{port}/health"),
        Duration::from_secs(5),
    );
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    loop {
        if let Ok(manager_supervisor::HealthStatus::Healthy) = probe.probe().await {
            return Ok(());
        }
        {
            let mut sup = state.supervisor().await.lock().await;
            sup.reap_exited();
            let status = sup.status().into_iter().find(|s| s.id == temp_id);
            match status {
                Some(s) if s.state == ProcessState::Failed => {
                    return Err(format!(
                        "temp backend {temp_id} exited before becoming healthy"
                    ));
                }
                Some(s) if s.pid.is_none() => {
                    return Err(format!(
                        "temp backend {temp_id} is not running (state {:?})",
                        s.state
                    ));
                }
                _ => {}
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "temp backend {temp_id} did not become healthy within {} s",
                HEALTH_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// The measurement fingerprint inputs for a temp backend,
/// built directly from its in-memory configuration (the
/// `fingerprint_inputs` helper in optimize.rs is shaped for
/// configured backends; §A.3's note applies).
fn temp_fingerprint_inputs(
    temp: &manager_config::BackendConfig,
    model: Option<&manager_config::ModelConfig>,
    file_size_bytes: Option<u64>,
    gpus: &[(u32, String)],
) -> MeasurementFingerprintInputs {
    let gpu_set: Vec<(u32, String)> = match crate::backend_gpu_visibility(temp) {
        crate::GpuVisibility::CpuOnly => Vec::new(),
        crate::GpuVisibility::Gpus(indices) => indices
            .iter()
            .map(|i| {
                (
                    *i,
                    gpus.iter()
                        .find(|(idx, _)| idx == i)
                        .map(|(_, name)| name.clone())
                        .unwrap_or_default(),
                )
            })
            .collect(),
        crate::GpuVisibility::Undetermined => gpus.to_vec(),
    };
    MeasurementFingerprintInputs {
        verified_sha256: model.and_then(|m| m.verified_sha256.clone()),
        file_size_bytes,
        quant: model.map(|m| m.quant.clone()).unwrap_or_default(),
        n_ctx: temp.flags.n_ctx,
        n_batch: temp.flags.n_batch,
        offload: manager_models::expert_offload(temp.raw_flags.as_deref()),
        tensor_split: temp.flags.tensor_split.clone(),
        split_mode: temp.flags.split_mode,
        raw_flags: temp.raw_flags.clone(),
        gpus: gpu_set,
    }
}

/// Run one experiment end to end (preflight → baseline if
/// needed → temp backend → benchmark → eval). Every failure
/// is an `InnerOutcome`, not an Err: the batch loop records it
/// and moves on. Cleanup is NOT done here — the loop owns it,
/// so a dropped (cancelled / timed-out) inner future cannot
/// skip it.
async fn run_experiment_inner(
    state: &AppState,
    cfg: &AppConfig,
    proposal: &ExperimentProposal,
    ctx: &mut BatchCtx,
    mode: RunMode,
) -> InnerOutcome {
    let temp_id = temp_backend_id(proposal.id);

    // -- Provisioning gate: when the plan provisions this
    // candidate through register/download items, those items
    // must have been applied before the run. Without this check
    // the run fell through to plain candidate resolution and
    // failed as though the model were simply missing, even
    // though the plan carried the remedy (Oct 19, 2026 field
    // report). Name the unapplied items so the failure is
    // actionable.
    if let Some(prov) = &proposal.provisioning {
        let file = crate::optimize::load_plan_file(state);
        if let Some(plan) = &file.plan {
            let unapplied: Vec<usize> = prov
                .item_ids
                .iter()
                .copied()
                .filter(|id| {
                    plan.items.iter().any(|i| {
                        i.id == *id
                            && i.apply_outcome != Some(manager_optimize::ApplyOutcome::Applied)
                    })
                })
                .collect();
            if !unapplied.is_empty() {
                let ids = unapplied
                    .iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                let message = format!(
                    "candidate '{}' is provisioned by plan items {ids}, which have not been applied — apply the plan's register/download steps first, then re-run the experiment",
                    prov.model_id
                );
                return InnerOutcome::failed(message.clone(), message);
            }
        }
    }

    // -- Candidate resolution: the file must already be on
    // disk; a run never downloads (§A.3 step 1) — provisioning
    // happens as plan items, applied before the batch.
    let registered: Vec<RegisteredModelFile> = cfg
        .models
        .iter()
        .map(|m| RegisteredModelFile {
            id: m.id.clone(),
            path: crate::resolve_under_data_dir(&cfg.data_dir, &m.gguf_path),
        })
        .collect();
    let catalog_ids: Vec<String> = manager_models::curated_models()
        .into_iter()
        .map(|s| s.id)
        .collect();
    let models_dir = cfg.data_dir.join("models");
    let (model_id, gguf_abs) = match resolve_candidate(
        &proposal.candidate_model_id,
        &registered,
        &catalog_ids,
        &models_dir,
        |p| std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false),
    ) {
        CandidateResolution::OnDisk { model_id, path } => (model_id, path),
        CandidateResolution::NotOnDisk { message } => {
            return InnerOutcome::failed(message.clone(), message);
        }
    };
    // The temp backend's model_file in the form the config
    // stores: the registered model's configured path when the
    // candidate resolved through one, else `models/<file>`.
    let registered_model = cfg.models.iter().find(|m| m.id == model_id).cloned();
    let model_file: PathBuf = match &registered_model {
        Some(m) => m.gguf_path.clone(),
        None => PathBuf::from("models").join(
            gguf_abs
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
    };

    // -- Gather live state for preflight and the baseline
    // decision.
    let gathered = crate::optimize::gather(state, cfg).await;

    // -- Temp backend configuration (in-memory only).
    let mut used_ports: HashSet<u16> = cfg.backends.iter().map(|b| b.port).collect();
    used_ports.extend(ctx.used_ports.iter().copied());
    let port = lowest_free_port(&used_ports, cfg.gateway.port, cfg.postgres.port);
    ctx.used_ports.insert(port);
    // The candidate's MoE layer count from the gathered layout
    // facts (GGUF layout or catalog pin, as resolved by
    // `crate::optimize::gather`) — the same source the fit
    // preflight below prices from. `None` when unknown.
    let moe_layers = gathered
        .live
        .layouts
        .get(&model_id)
        .and_then(|f| f.moe_layers);
    let temp = temp_backend_config(proposal, model_file.clone(), port, moe_layers);
    // A cloned config carrying the temp backend (and, for an
    // unregistered HF candidate, a synthetic model entry so
    // the existing resolution machinery can price it). Never
    // saved, never routed.
    let mut temp_cfg: AppConfig = cfg.clone();
    if registered_model.is_none() {
        temp_cfg.models.push(manager_config::ModelConfig {
            id: model_id.clone(),
            gguf_path: model_file.clone(),
            quant: "unknown".to_string(),
            params_b: 1.0,
            sha256: String::new(),
            verified_at: None,
            verified_sha256: None,
            source_url: String::new(),
            assigned_backends: vec![temp_id.clone()],
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
            mtp: None,
        });
    }
    temp_cfg.backends.push(temp.clone());

    // -- Fit preflight, stated in numbers (§A.3 step 1).
    let n_gpus = gathered.live.free_vram_mib_per_gpu.len();
    let mut to_stop: Vec<String> = Vec::new();
    let preflight = match crate::backend_memory_requirement(&temp_cfg, &temp) {
        None => format!(
            "fit unchecked: the candidate's memory requirement could not be computed (no layout facts for '{model_id}'); proceeding without a fit verdict"
        ),
        Some(req) => {
            let needs = manager_optimize::fit::gpu_total_mib(&req);
            if n_gpus == 0 {
                format!(
                    "fit unchecked: no GPU telemetry snapshot; the candidate needs {needs} MiB GPU RAM"
                )
            } else {
                let free = gathered.live.free_vram_mib_per_gpu.clone();
                let (run_res, acct_res) = manager_optimize::fit::others_residency(
                    &temp_cfg,
                    &gathered.live.layouts,
                    &gathered.live.backend_running,
                    &temp_id,
                    n_gpus,
                );
                let free_after: u64 = free
                    .iter()
                    .zip(run_res.iter())
                    .map(|(f, o)| f.saturating_sub(*o))
                    .sum();
                let outcome =
                    evaluate_fit(&req, &temp.flags.tensor_split, &free, &run_res, &acct_res);
                if outcome.placeable {
                    fit_verdict_text(needs, free_after, true, false, &[])
                } else {
                    // Hypothetically stop the accepted
                    // requires_stopping ids and re-evaluate.
                    let mut running2 = gathered.live.backend_running.clone();
                    for id in &proposal.requires_stopping {
                        running2.insert(id.clone(), false);
                    }
                    let (run2, acct2) = manager_optimize::fit::others_residency(
                        &temp_cfg,
                        &gathered.live.layouts,
                        &running2,
                        &temp_id,
                        n_gpus,
                    );
                    let free_after2: u64 = free
                        .iter()
                        .zip(run2.iter())
                        .map(|(f, o)| f.saturating_sub(*o))
                        .sum();
                    let outcome2 =
                        evaluate_fit(&req, &temp.flags.tensor_split, &free, &run2, &acct2);
                    if outcome2.placeable {
                        to_stop = proposal
                            .requires_stopping
                            .iter()
                            .filter(|id| {
                                gathered
                                    .live
                                    .backend_running
                                    .get(*id)
                                    .copied()
                                    .unwrap_or(false)
                            })
                            .cloned()
                            .collect();
                        fit_verdict_text(needs, free_after2, false, true, &proposal.requires_stopping)
                    } else {
                        let text =
                            fit_verdict_text(needs, free_after2, false, false, &proposal.requires_stopping);
                        return InnerOutcome::failed(
                            format!("preflight failed: {text}"),
                            text,
                        );
                    }
                }
            }
        }
    };

    // -- Baseline (only when this experiment runs the eval):
    // use the stored baseline when fresh for its backend;
    // else, when the plan's consulted backend is running, run
    // the baseline against it first and store it; else the
    // eval runs without a comparison. This happens after the
    // fit verdict (a failed preflight burns no eval time) but
    // BEFORE any requires_stopping stops: the consulted
    // backend may be one of the backends the fit needs stopped.
    let mut baseline: Option<manager_optimize::BaselineRecord> = None;
    if mode == RunMode::Full && proposal.run_eval {
        let stored = crate::optimize_eval::load_baseline(state);
        let fresh = stored.as_ref().and_then(|b| {
            let is_fresh = b.suite_version == manager_optimize::EVAL_SUITE_VERSION
                && gathered.fingerprints.get(&b.backend_id)
                    == Some(&b.config_fingerprint);
            if is_fresh {
                Some(b.clone())
            } else {
                None
            }
        });
        baseline = match fresh {
            Some(b) => Some(b),
            None => {
                let consulted = ctx.consulted_backend.clone();
                let can_run = !consulted.is_empty()
                    && gathered.running.get(&consulted).copied() == Some(true);
                if can_run {
                    if let (Some(b), Some(fp)) = (
                        cfg.backends.iter().find(|b| b.id == consulted),
                        gathered.fingerprints.get(&consulted).cloned(),
                    ) {
                        let bmodel = crate::model_for_backend(cfg, b)
                            .map(|m| m.id.clone())
                            .unwrap_or_else(|| consulted.clone());
                        let run = crate::optimize_eval::run_eval_suite(
                            &consulted,
                            &format!("http://127.0.0.1:{}", b.port),
                            &bmodel,
                        )
                        .await;
                        let record =
                            manager_optimize::BaselineRecord::from_run(&run, fp);
                        let _ = crate::optimize_eval::save_baseline(state, record.clone());
                        Some(record)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };
    }

    // -- Stop exactly the accepted requires_stopping ids the
    // fit verdict needs stopped.
    for id in &to_stop {
        let outcome = state.supervisor().await.lock().await.stop(id, false).await;
        if let Err(e) = outcome {
            return InnerOutcome::failed(
                format!("preflight fit needed backend '{id}' stopped, but stopping it failed: {e:?}"),
                preflight,
            );
        }
        if ctx.batch_running.contains(id) && !ctx.stopped_to_restart.contains(id) {
            ctx.stopped_to_restart.push(id.clone());
        }
    }

    // -- Start preflight (code-review B-2): the same gate every
    // app-mediated start passes, evaluated against the cloned
    // config carrying the temp backend. A proposal whose offload
    // position the guardrails block fails here with the
    // preflight's reason instead of spawning at that position —
    // before any journal record or spec registration, so a
    // refused experiment leaves nothing to clean up.
    if let Err(e) = crate::backend_start_preflight_for(state, &temp_cfg, &temp).await {
        return InnerOutcome::failed(
            format!("start preflight refused the experiment's temp backend: {e}"),
            preflight,
        );
    }

    // -- Journal the in-flight experiment BEFORE the spawn, so
    // a crash from here on leaves a sweep record.
    {
        let mut journal = load_journal(state);
        journal.add_active(ActiveExperiment {
            temp_id: temp_id.clone(),
            pid: None,
            port,
            experiment_id: proposal.id,
            started_at: chrono::Utc::now().to_rfc3339(),
        });
        save_journal(state, &journal);
    }

    // -- Register the spec directly and spawn. No probe is
    // registered (backends never register probes in this app);
    // the health wait uses a local HttpHealthProbe.
    {
        let store = state.telemetry().await;
        let gpus = crate::lock_telemetry(&store).inventory();
        let spec = crate::backend_spec(cfg, &temp, &gpus);
        let mut sup = state.supervisor().await.lock().await;
        sup.register_spec(spec);
        if let Err(e) = sup.spawn_backend(&temp_id).await {
            return InnerOutcome::failed(
                format!("temp backend {temp_id} failed to start: {e:?}"),
                preflight,
            );
        }
        let pid = sup
            .status()
            .into_iter()
            .find(|s| s.id == temp_id)
            .and_then(|s| s.pid);
        drop(sup);
        if let Some(pid) = pid {
            let mut journal = load_journal(state);
            journal.set_active_pid(&temp_id, pid);
            save_journal(state, &journal);
        }
    }

    // -- Wait healthy on the existing probe.
    if let Err(e) = wait_healthy(state, &temp_id, port).await {
        return InnerOutcome::failed(e, preflight);
    }

    // -- Estimate pass (tracker #56): the abbreviated profile
    // against the temp backend, then out — no measurement-store
    // record, no eval. First-run effects on a freshly launched
    // backend are handled by ONE discarded warm-up on the short
    // prompt only, at the estimate's own budget: the model is
    // already loaded (`wait_healthy` gated on that), and the
    // long prompt gets no warm-up because its 4,000-token
    // prefill IS the sample — discarding one would double the
    // estimate's dominant cost to remove an effect (cold KV /
    // allocator state) that is second-order next to the
    // estimate's stated directional precision.
    if mode == RunMode::Estimate {
        let base_url = format!("http://127.0.0.1:{port}");
        let mut results: Vec<BenchResult> = Vec::new();
        for (index, prompt) in estimate_prompts().iter().enumerate() {
            match crate::optimize::benchmark_one_prompt(
                &base_url,
                &model_id,
                prompt,
                index == 0,
                ESTIMATE_RUNS,
                EXP_RUN_TIMEOUT,
            )
            .await
            {
                Ok(result) => results.push(result),
                Err(e) => {
                    return InnerOutcome::failed(
                        format!("estimate benchmark failed: {e}"),
                        preflight,
                    );
                }
            }
        }
        let mut iter = results.into_iter();
        let short = iter.next().unwrap_or_else(|| BenchResult::from_runs(Vec::new()));
        let long = iter.next().unwrap_or_else(|| BenchResult::from_runs(Vec::new()));
        let detail = "Estimate only — abbreviated benchmark (one run per prompt, reduced generation budgets). Confirm with a full experiment run before acting on it.".to_string();
        return InnerOutcome {
            status: ExperimentStatus::Completed,
            detail: detail.clone(),
            measured: None,
            eval: None,
            comparison: None,
            preflight: preflight.clone(),
            estimate: Some(ExperimentEstimate {
                short,
                long,
                batch_id: String::new(),
                temp_backend_id: temp_id.clone(),
                preflight,
                detail,
                estimated_at: chrono::Utc::now().to_rfc3339(),
            }),
        };
    }

    // -- Standardized benchmark (§A.1), mirroring the measure
    // command's run sequence.
    let base_url = format!("http://127.0.0.1:{port}");
    let mut results: Vec<BenchResult> = Vec::new();
    for prompt in benchmark_prompts() {
        let messages = vec![manager_supervisor::ChatMessage {
            role: "user".to_string(),
            content: prompt.text.to_string(),
        }];
        if let Err(e) = manager_supervisor::benchmark_completion(
            &base_url,
            &model_id,
            &messages,
            BENCHMARK_TEMPERATURE,
            prompt.max_tokens,
            EXP_RUN_TIMEOUT,
        )
        .await
        {
            return InnerOutcome::failed(format!("benchmark warm-up failed: {e}"), preflight);
        }
        let mut runs: Vec<BenchRun> = Vec::new();
        for _ in 0..MEASURED_RUNS {
            match manager_supervisor::benchmark_completion(
                &base_url,
                &model_id,
                &messages,
                BENCHMARK_TEMPERATURE,
                prompt.max_tokens,
                EXP_RUN_TIMEOUT,
            )
            .await
            {
                Ok(sample) => runs.push(BenchRun {
                    prompt_per_second: sample.prompt_per_second,
                    predicted_per_second: sample.predicted_per_second,
                    ttft_ms: sample.ttft_ms,
                    latency_ms: sample.latency_ms,
                }),
                Err(e) => {
                    return InnerOutcome::failed(
                        format!("benchmark run failed: {e}"),
                        preflight,
                    );
                }
            }
        }
        results.push(BenchResult::from_runs(runs));
    }
    let mut iter = results.into_iter();
    let short = iter.next().unwrap_or_else(|| BenchResult::from_runs(Vec::new()));
    let long = iter.next().unwrap_or_else(|| BenchResult::from_runs(Vec::new()));
    let measured = ExperimentMeasured {
        short: short.clone(),
        long: long.clone(),
    };

    // Record the benchmark in the measurement store under the
    // temp configuration's fingerprint (§A.3 step 4): it
    // survives cleanup, keyed to a configuration, with the
    // temp backend id as its provenance.
    {
        let gpu_pairs: Vec<(u32, String)> = gathered
            .measure
            .hardware
            .gpus
            .iter()
            .map(|g| (g.index, g.name.clone()))
            .collect();
        let file_size = std::fs::metadata(&gguf_abs)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len());
        let model_for_fp = registered_model.as_ref().or_else(|| {
            temp_cfg.models.iter().find(|m| m.id == model_id)
        });
        let fp_inputs = temp_fingerprint_inputs(&temp, model_for_fp, file_size, &gpu_pairs);
        let fp = manager_optimize::measurement_fingerprint(&fp_inputs);
        // The v2 provenance (predicted-throughput design §4.2):
        // an experiment entry, its inputs snapshot and identity,
        // the temp backend's driver environment, and the temp
        // server's build when it reports one. The proposal
        // derives the temp configuration from the candidate and
        // flags alone, so no origin backend is known —
        // `origin_backend_id` stays absent rather than guessed.
        let (fp_sha256, fp_size_bytes) = crate::optimize::entry_identity(&fp_inputs);
        let llama_build = manager_supervisor::server_build_info(&base_url).await;
        let entry = MeasurementEntry {
            fingerprint: fp,
            backend_id: temp_id.clone(),
            model_id: model_id.clone(),
            measured_at: chrono::Utc::now().to_rfc3339(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            short,
            long,
            source: Some(manager_optimize::MeasurementSource::Experiment),
            origin_backend_id: None,
            model_file: gguf_abs
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
            model_sha256: fp_sha256,
            model_size_bytes: fp_size_bytes,
            inputs: Some(manager_optimize::MeasurementInputs::from_fingerprint_inputs(
                &fp_inputs,
            )),
            driver_version: crate::optimize::driver_versions_for(
                &fp_inputs,
                &gathered.gpu_driver_versions,
            ),
            llama_build,
            residency: None,
            peak_temp_c: None,
        };
        let mut store = MeasurementStore::load(&crate::optimize::measurements_path(state));
        store.upsert(entry);
        if let Err(e) = store.save(&crate::optimize::measurements_path(state)) {
            return InnerOutcome {
                status: ExperimentStatus::Failed,
                detail: format!("benchmark completed but recording it failed: {e}"),
                measured: Some(measured),
                eval: None,
                comparison: None,
                preflight,
                estimate: None,
            };
        }
    }

    // -- Screening eval (§A.4) when the proposal asks for it.
    let (eval, comparison) = if proposal.run_eval {
        let record = crate::optimize_eval::run_eval_suite(&temp_id, &base_url, &model_id).await;
        let comparison = baseline
            .as_ref()
            .map(|b| compare(&record.scores, &b.scores));
        (Some(record), comparison)
    } else {
        (None, None)
    };

    let detail = format!(
        "Measured {model_id} on {temp_id}: generation {} tok/s (short), {} tok/s (long); prompt {} tok/s; TTFT {} ms.{}{}",
        fmt_rate(measured.short.predicted_per_second),
        fmt_rate(measured.long.predicted_per_second),
        fmt_rate(measured.long.prompt_per_second.or(measured.short.prompt_per_second)),
        fmt_rate(measured.short.ttft_ms.or(measured.long.ttft_ms)),
        match &eval {
            Some(e) => format!(
                " Screen: {}/{} overall.",
                e.scores.overall.passed, e.scores.overall.total
            ),
            None => String::new(),
        },
        match &comparison {
            Some(c) => format!(
                " Delta vs baseline: {:+} passed overall; screen at/above baseline: {}.",
                c.overall.delta_passed, c.screen_at_or_above_baseline
            ),
            None => String::new(),
        },
    );
    InnerOutcome {
        status: ExperimentStatus::Completed,
        detail,
        measured: Some(measured),
        eval,
        comparison,
        preflight,
        estimate: None,
    }
}

/// Record one finished result in both durable places: the
/// journal history and the plan's proposal (when the plan is
/// still the one that proposed it).
fn record_result(state: &AppState, result: &ExperimentResult) {
    // Every experiment outcome funnels through here (tracker
    // #52): completed, failed, timed out, cancelled, and
    // deadline-skipped alike — one log line per result, with the
    // measured figures and eval verdict in the detail text.
    crate::optimize::opt_log1(
        state,
        &format!(
            "experiment {} (candidate {}): {:?} — {}",
            result.experiment_id, result.candidate_model_id, result.status, result.detail
        ),
    );
    let mut journal = load_journal(state);
    journal.record_result(result.clone());
    save_journal(state, &journal);
    let mut file = crate::optimize::load_plan_file(state);
    if let Some(plan) = file.plan.as_mut() {
        if let Some(exp) = plan
            .experiments
            .iter_mut()
            .find(|e| e.id == result.experiment_id)
        {
            exp.result = Some(result.clone());
            let _ = crate::optimize::save_plan_file(state, &file);
        }
    }
}

/// Record one completed estimate pass (tracker #56): the
/// optimize log (distinctly from measurement / experiment-run
/// lines) and the plan proposal's `estimate` field. Deliberately
/// NOT the journal history and NOT the measurement store: an
/// estimate is screening evidence recorded on the proposal,
/// never a run outcome and never a measured fact.
fn record_estimate(
    state: &AppState,
    experiment_id: usize,
    candidate_model_id: &str,
    estimate: ExperimentEstimate,
) {
    crate::optimize::opt_log1(
        state,
        &format!(
            "experiment {experiment_id} estimate (candidate {candidate_model_id}): generation {} tok/s (short), {} tok/s (long); prompt {} tok/s; TTFT {} ms (estimate)",
            fmt_rate(estimate.short.predicted_per_second),
            fmt_rate(estimate.long.predicted_per_second),
            fmt_rate(estimate.long.prompt_per_second.or(estimate.short.prompt_per_second)),
            fmt_rate(estimate.short.ttft_ms.or(estimate.long.ttft_ms)),
        ),
    );
    let mut file = crate::optimize::load_plan_file(state);
    if let Some(plan) = file.plan.as_mut() {
        if let Some(exp) = plan.experiments.iter_mut().find(|e| e.id == experiment_id) {
            exp.estimate = Some(estimate);
            let _ = crate::optimize::save_plan_file(state, &file);
        }
    }
}

/// Restart the requires_stopping backends the batch stopped
/// (those running at batch start, still stopped now), via the
/// ordinary spec registration + supervisor spawn. Returns the
/// note stated in each of the batch's result details.
pub(crate) async fn restart_stopped(state: &AppState, ids: &[String]) -> String {
    if ids.is_empty() {
        return String::new();
    }
    crate::ensure_backend_specs(state).await;
    let mut restarted: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for id in ids {
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        let running = sup
            .status()
            .iter()
            .any(|s| &s.id == id && s.pid.is_some());
        if running {
            restarted.push(id.clone());
            continue;
        }
        drop(sup);
        // Start preflight (code-review B-2): the restart goes
        // through the same gate as any app-mediated start, so a
        // backend whose saved position the guardrails now block
        // is not relaunched unpreflighted; the refusal is stated
        // in the note like any other restart failure.
        if let Err(e) = crate::backend_start_preflight(state, id).await {
            failed.push(format!("{id} (start preflight refused the restart: {e})"));
            continue;
        }
        let mut sup = state.supervisor().await.lock().await;
        match sup.spawn_backend(id).await {
            Ok(()) => restarted.push(id.clone()),
            Err(e) => failed.push(format!("{id} ({e:?})")),
        }
    }
    let mut note = format!(
        "Backends stopped for this batch and restarted afterwards: {}.",
        if restarted.is_empty() {
            "none".to_string()
        } else {
            restarted.join(", ")
        }
    );
    if !failed.is_empty() {
        note.push_str(&format!(
            " Restart failed for: {}.",
            failed.join(", ")
        ));
    }
    note
}

/// Run the plan's accepted experiments as one batch (§A.3):
/// one tracked task under the shared GPU-job slot, one
/// experiment at a time, budgets enforced server-side.
/// Undecided and denied proposals never run. `mode` selects
/// the full run (`optimize-experiments`) or the estimate pass
/// (tracker #56, `optimize-estimate-experiments`): same
/// lifecycle, preflight, stopping, cleanup, and budgets; an
/// estimate pass runs only the abbreviated benchmark and
/// records its figures on the proposal as an estimate.
async fn run_experiment_batch(state: &std::sync::Arc<crate::AppState>, _sink: &std::sync::Arc<dyn crate::EventSink>, mode: RunMode) -> crate::ApiResult {
    {
        let slot = state.optimize_job_abort.lock().await;
        if slot.is_some() {
            return Err(ApiError::InvalidInput(
                "a measurement or experiment job is already running".to_string(),
            ));
        }
    }
    let file = crate::optimize::load_plan_file_current(state).await;
    let plan = file
        .plan
        .clone()
        .ok_or_else(|| ApiError::NotFound("no plan".to_string()))?;
    let batch: Vec<ExperimentProposal> = plan
        .experiments
        .iter()
        .filter(|e| e.decision == Decision::Accepted)
        .cloned()
        .collect();
    if batch.is_empty() {
        return Err(ApiError::InvalidInput(
            "no accepted experiments in the current plan; accept at least one proposal to run it".to_string(),
        ));
    }
    if batch.len() > manager_optimize::MAX_EXPERIMENTS {
        return Err(ApiError::InvalidInput(format!(
            "at most {} experiments per batch",
            manager_optimize::MAX_EXPERIMENTS
        )));
    }
    let cfg = state.config.read().await.clone();
    // Mutual exclusion (tracker #70): an experiment batch runs
    // against a real backend where it contends the same way a
    // benchmark does — its baseline sub-run executes the
    // screening suite against the plan's consulted backend, and
    // the batch may stop that backend for fit — so the batch
    // claims the consulted backend as a benchmark operation for
    // its whole run. A consultation already running there
    // refuses the batch, naming the running operation; a batch
    // holding the claim refuses a consultation there. A plan
    // with no consulted backend claims nothing (its runs target
    // temp backends only). The guard releases the record on
    // every exit below — completion, failure, the batch
    // deadline (timeout), and operator cancellation alike.
    let _benchmark_op_guard = if plan.consulted_backend.is_empty() {
        None
    } else {
        Some(crate::optimize::OptimizeOpGuard::claim(
            state,
            &plan.consulted_backend,
            crate::OptimizeOpFamily::Benchmark,
            if mode == RunMode::Estimate {
                "experiment-estimate"
            } else {
                "experiment"
            },
        )?)
    };
    let gathered0 = crate::optimize::gather(state, &cfg).await;
    let task_id = state
        .record_task(
            if mode == RunMode::Estimate {
                "optimize-estimate-experiments"
            } else {
                "optimize-experiments"
            },
            json!({
                "experiments": batch.iter().map(|e| e.id).collect::<Vec<_>>(),
                "candidates": batch.iter().map(|e| e.candidate_model_id.clone()).collect::<Vec<_>>(),
            }),
        )
        .await;
    let batch_id = task_id.clone();
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    {
        let mut slot = state.optimize_job_abort.lock().await;
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
    let mut ctx = BatchCtx {
        consulted_backend: plan.consulted_backend.clone(),
        batch_running: gathered0
            .running
            .iter()
            .filter(|(_, r)| **r)
            .map(|(id, _)| id.clone())
            .collect(),
        stopped_to_restart: Vec::new(),
        used_ports: HashSet::new(),
    };
    crate::optimize::opt_log1(
        state,
        &format!(
            "experiment {}batch {batch_id}: {} experiments: {}",
            if mode == RunMode::Estimate { "estimate " } else { "" },
            batch.len(),
            batch
                .iter()
                .map(|e| format!(
                    "#{} {} (serves: {})",
                    e.id, e.candidate_model_id, e.serves
                ))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    );
    let batch_start = Instant::now();
    let mut cancelled = false;
    let mut completed = 0usize;
    let mut failed = 0usize;
    let mut idx = 0usize;
    while idx < batch.len() {
        let proposal = &batch[idx];
        // Batch deadline: everything not yet run is recorded
        // as a timeout that states why it never ran.
        if batch_start.elapsed() >= Duration::from_secs(BATCH_TIMEOUT_SECS) {
            for rest in &batch[idx..] {
                let now = chrono::Utc::now().to_rfc3339();
                record_result(
                    state,
                    &ExperimentResult {
                        experiment_id: rest.id,
                        candidate_model_id: rest.candidate_model_id.clone(),
                        batch_id: batch_id.clone(),
                        status: ExperimentStatus::Timeout,
                        detail: if mode == RunMode::Estimate {
                            "estimate not run: the 60-minute batch deadline was exceeded"
                        } else {
                            "not run: the 60-minute batch deadline was exceeded"
                        }
                        .to_string(),
                        measured: None,
                        eval: None,
                        comparison: None,
                        preflight: String::new(),
                        started_at: now.clone(),
                        finished_at: now,
                    },
                );
                failed += 1;
            }
            break;
        }
        let started_at = chrono::Utc::now().to_rfc3339();
        let temp_id = temp_backend_id(proposal.id);
        crate::optimize::opt_log1(
            state,
            &format!(
                "experiment {} (candidate {}): launching as temp backend {temp_id}",
                proposal.id, proposal.candidate_model_id
            ),
        );
        let remaining_batch = Duration::from_secs(BATCH_TIMEOUT_SECS)
            .saturating_sub(batch_start.elapsed());
        let budget = Duration::from_secs(EXPERIMENT_TIMEOUT_SECS).min(remaining_batch);
        let inner = run_experiment_inner(state, &cfg, proposal, &mut ctx, mode);
        // Cancellation and timeout drop the inner future; the
        // cleanup below is outside it and always runs.
        let outcome = tokio::select! {
            _ = &mut rx => {
                cancelled = true;
                InnerOutcome {
                    status: ExperimentStatus::Cancelled,
                    detail: "experiment cancelled by the operator".to_string(),
                    measured: None,
                    eval: None,
                    comparison: None,
                    preflight: String::new(),
                    estimate: None,
                }
            }
            res = tokio::time::timeout(budget, inner) => match res {
                Ok(o) => o,
                Err(_) => InnerOutcome {
                    status: ExperimentStatus::Timeout,
                    detail: format!(
                        "experiment exceeded its {}-minute budget",
                        budget.as_secs() / 60
                    ),
                    measured: None,
                    eval: None,
                    comparison: None,
                    preflight: String::new(),
                    estimate: None,
                },
            },
        };
        cleanup_temp(state, &temp_id).await;
        match outcome.status {
            ExperimentStatus::Completed => completed += 1,
            _ => failed += 1,
        }
        let mut estimate_recorded = false;
        if mode == RunMode::Estimate && outcome.status == ExperimentStatus::Completed {
            if let Some(mut estimate) = outcome.estimate {
                // A completed estimate pass records an estimate,
                // not a result (tracker #56): no journal history,
                // no measurement store, nothing that reads as a run.
                estimate.batch_id = batch_id.clone();
                record_estimate(state, proposal.id, &proposal.candidate_model_id, estimate);
                estimate_recorded = true;
            }
        }
        if !estimate_recorded {
            // Estimate-mode failures still record a result so
            // the failure is visible on the proposal; the detail
            // is prefixed so it can never read as a full run's.
            let detail = if mode == RunMode::Estimate
                && outcome.status == ExperimentStatus::Failed
            {
                format!("Estimate: {}", outcome.detail)
            } else {
                outcome.detail
            };
            record_result(
                state,
                &ExperimentResult {
                    experiment_id: proposal.id,
                    candidate_model_id: proposal.candidate_model_id.clone(),
                    batch_id: batch_id.clone(),
                    status: outcome.status,
                    detail,
                    measured: outcome.measured,
                    eval: outcome.eval,
                    comparison: outcome.comparison,
                    preflight: outcome.preflight,
                    started_at,
                    finished_at: chrono::Utc::now().to_rfc3339(),
                },
            );
        }
        idx += 1;
        if cancelled {
            break;
        }
    }
    // Restart what the batch stopped (any outcome, including
    // cancellation and deadline), and state it in every result
    // this batch recorded — journal history and plan alike.
    let note = restart_stopped(state, &ctx.stopped_to_restart.clone()).await;
    if !note.is_empty() {
        let mut journal = load_journal(state);
        journal.append_batch_detail(&batch_id, &note);
        save_journal(state, &journal);
        let mut file = crate::optimize::load_plan_file(state);
        if let Some(plan) = file.plan.as_mut() {
            for exp in plan.experiments.iter_mut() {
                if let Some(result) = exp.result.as_mut() {
                    if result.batch_id == batch_id {
                        if !result.detail.is_empty() {
                            result.detail.push(' ');
                        }
                        result.detail.push_str(&note);
                    }
                }
                if let Some(estimate) = exp.estimate.as_mut() {
                    if estimate.batch_id == batch_id {
                        if !estimate.detail.is_empty() {
                            estimate.detail.push(' ');
                        }
                        estimate.detail.push_str(&note);
                    }
                }
            }
            let _ = crate::optimize::save_plan_file(state, &file);
        }
    }
    {
        let mut slot = state.optimize_job_abort.lock().await;
        *slot = None;
    }
    let (status, detail) = if cancelled {
        (
            TaskStatus::Aborted,
            json!({ "batch": batch_id, "completed": completed, "failed": failed, "cancelled": true }),
        )
    } else {
        (
            TaskStatus::Done,
            json!({ "batch": batch_id, "completed": completed, "failed": failed }),
        )
    };
    crate::optimize::opt_log1(
        state,
        &format!(
            "experiment {}batch {batch_id} end: {completed} {}, {failed} failed{}",
            if mode == RunMode::Estimate { "estimate " } else { "" },
            if mode == RunMode::Estimate { "estimated" } else { "completed" },
            if cancelled { ", cancelled" } else { "" }
        ),
    );
    state.finish_task(&task_id, status, detail).await;
    if cancelled {
        return Err(ApiError::InvalidInput(
            if mode == RunMode::Estimate {
                "experiment estimate batch cancelled"
            } else {
                "experiment batch cancelled"
            }
            .to_string(),
        ));
    }
    crate::optimize::state_payload(state).await
}

/// Run the plan's accepted experiments as one full batch
/// (§A.3): benchmark + optional screening eval, recorded as
/// experiment results.
pub async fn op_optimize_run_experiments(state: &std::sync::Arc<crate::AppState>, _sink: &std::sync::Arc<dyn crate::EventSink>) -> crate::ApiResult {
    run_experiment_batch(state, _sink, RunMode::Full).await
}

/// Run the plan's accepted experiments as one estimate batch
/// (tracker #56): the same lifecycle as a full batch, but each
/// experiment runs only the abbreviated benchmark against its
/// temp backend and records the figures on the proposal as an
/// estimate, so a batch can be screened before full runs are
/// committed.
pub async fn op_optimize_estimate_experiments(state: &std::sync::Arc<crate::AppState>, _sink: &std::sync::Arc<dyn crate::EventSink>) -> crate::ApiResult {
    run_experiment_batch(state, _sink, RunMode::Estimate).await
}

// ---------------------------------------------------------------------------
// Tests (code-review B-2)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A state over a temp dir whose config registers the
    /// Coder-Next catalog model — the #37 guardrail pin (partial
    /// offload on compute capability (6,0) is EndpointsOnly) —
    /// and whose telemetry inventory is an A4000 (8,6) at index 0
    /// and a P100 (6,0) at index 1, so a [0.5, 0.5] split places
    /// the backend on both.
    async fn guardrail_test_state() -> (Arc<AppState>, AppConfig, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "llm-mgr-b2-preflight-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("models")).unwrap();
        // The candidate's file must exist for resolution; an
        // empty file suffices — the guardrail prices from the
        // catalog when the GGUF does not parse.
        std::fs::write(dir.join("models/Coder.gguf"), b"").unwrap();
        let mut cfg = AppConfig::default_config();
        cfg.data_dir = dir.clone();
        cfg.models.push(manager_config::ModelConfig {
            id: "qwen3-coder-30b".to_string(),
            gguf_path: PathBuf::from("models/Coder.gguf"),
            quant: "UD-Q4_K_M".to_string(),
            params_b: 80.0,
            sha256: "b".repeat(64),
            // Verified, so the preflight's verification gate
            // passes and the guardrail is the refusing check.
            verified_at: Some(chrono::Utc::now()),
            verified_sha256: None,
            source_url: String::new(),
            assigned_backends: vec![],
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
            mtp: None,
        });
        let state = Arc::new(AppState::new(cfg.clone(), dir.join("config.json"), dir.clone()));
        // Initialize the telemetry store, then replace its
        // inventory with the fixture. The init's own probe finds
        // no inventory in a test environment; on a machine with
        // real NVIDIA GPUs it loads the real one during this
        // call, and the fixture overwrite below still lands
        // before any preflight reads it.
        let store = state.telemetry().await;
        let gpu = |index: u32, name: &str, cc: (u32, u32)| manager_telemetry::GpuInfo {
            index,
            name: name.to_string(),
            uuid: format!("GPU-fixture-{index}"),
            compute_capability: cc,
            total_vram_mib: 16384,
            pci_bus_id: format!("00000000:{:02x}:00.0", index + 1),
            driver_version: "581.57".to_string(),
            driver_model: None,
            source: manager_telemetry::TelemetrySourceKind::NvidiaSmi,
        };
        crate::lock_telemetry(&store).set_inventory(vec![
            gpu(0, "RTX A4000", (8, 6)),
            gpu(1, "Tesla P100-PCIE-16GB", (6, 0)),
        ]);
        (state, cfg, dir)
    }

    /// The code review's traced case: a proposal at a
    /// guardrail-blocked offload position must fail pre-spawn
    /// with the preflight's reason — never reach the supervisor,
    /// never leave a journal record.
    #[tokio::test]
    async fn guardrail_blocked_proposal_fails_pre_spawn_with_the_preflight_reason() {
        let (state, cfg, dir) = guardrail_test_state().await;
        // The review's parse fixture: Coder-Next at 40 of 48 MoE
        // layers, split [0.5, 0.5] across the A4000 and the P100.
        let proposal = ExperimentProposal {
            id: 1,
            fingerprint: "b2-test".to_string(),
            candidate_model_id: "qwen3-coder-30b".to_string(),
            flags: manager_optimize::ExperimentFlags {
                n_ctx: None,
                offload_layers: Some(40),
                tensor_split: Some(vec![0.5, 0.5]),
                moe_cache_mib: None,
            },
            serves: "guardrail wiring test".to_string(),
            hypothesis: String::new(),
            predicted: Default::default(),
            requires_stopping: Vec::new(),
            run_eval: false,
            decision: Decision::Accepted,
            previously_denied: false,
            provisioning: None,
            result: None,
            estimate: None,
        };
        let mut ctx = BatchCtx {
            consulted_backend: String::new(),
            batch_running: HashSet::new(),
            stopped_to_restart: Vec::new(),
            used_ports: HashSet::new(),
        };
        let outcome = run_experiment_inner(&state, &cfg, &proposal, &mut ctx, RunMode::Full).await;
        assert_eq!(outcome.status, ExperimentStatus::Failed);
        assert!(
            outcome.detail.contains("outside the verified range"),
            "the failure must carry the start preflight's guardrail reason, got: {}",
            outcome.detail
        );
        // Pre-spawn evidence: the temp backend never reached the
        // supervisor, and the gate runs before the journal's
        // active record is written.
        let sup = state.supervisor().await.lock().await;
        assert!(
            sup.status().iter().all(|s| s.id != "opt-exp-1"),
            "a refused experiment must never reach the supervisor"
        );
        drop(sup);
        assert!(load_journal(&state).active.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The batch restart path goes through the same gate: a
    /// stopped backend whose saved position the guardrails block
    /// is not relaunched, and the note states the refusal with
    /// the preflight's reason.
    #[tokio::test]
    async fn restart_stopped_refuses_a_guardrail_blocked_backend() {
        let (state, cfg, dir) = guardrail_test_state().await;
        // Point the saved coder backend at the registered
        // Coder-Next model at the blocked position (40 of 48
        // layers, split across both GPUs).
        let mut cfg = cfg;
        let coder = cfg.backends.iter_mut().find(|b| b.id == "coder").unwrap();
        coder.enabled = true;
        coder.model_file = PathBuf::from("models/Coder.gguf");
        coder.flags.tensor_split = vec![0.5, 0.5];
        coder.raw_flags = Some("--n-cpu-moe 40".to_string());
        // restart_stopped's preflight reads the saved config.
        *state.config.write().await = cfg;
        let note = restart_stopped(&state, &["coder".to_string()]).await;
        assert!(
            note.contains("Restart failed for: coder"),
            "the refusal must be stated as a restart failure, got: {note}"
        );
        assert!(
            note.contains("outside the verified range"),
            "the note must carry the preflight's guardrail reason, got: {note}"
        );
        let sup = state.supervisor().await.lock().await;
        assert!(sup.status().iter().all(|s| s.id != "coder" || s.pid.is_none()));
        drop(sup);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
