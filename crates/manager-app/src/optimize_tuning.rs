//! Optimize per-backend tuning runner (design Addendum B,
//! §B.2–§B.6): "Optimize this backend" — a bounded sweep of
//! one existing backend's configuration, same model, returning
//! ranked measured arms. The pure machinery (value sets,
//! planning filters, coordinate descent, ranking, the sweep
//! record) lives in `manager_optimize::tuning`; this module is
//! the I/O side: resolving the incumbent from the live app,
//! pricing arms, parking conflicting backends, running arms
//! as temporary `opt-exp-*` backends under the §A.1 protocol
//! with residency verification and the thermal guard, and
//! adoption through the existing structured save ops.
//!
//! Serialization (§B.4.3, extension E2): a sweep holds the
//! shared Optimize job slot (`optimize_job_abort`) for its
//! whole run, so Measure / Estimate / experiment batches
//! refuse while it runs and it refuses while they run; it
//! additionally refuses while a consultation occupies
//! `optimize_abort`, and `op_optimize_consult` refuses while
//! the job slot is occupied (the reciprocal check lives
//! there). On top of the slots, the sweep claims the tuned
//! backend in the per-backend mutual-exclusion record
//! (tracker #70) for its whole run.
//!
//! Parking (§B.4.1): at sweep start the runner stops the
//! tuned backend and every running backend placed on any GPU
//! the sweep will use (the sweep's placement axis spans every
//! installed GPU), and restarts those that were running after
//! the sweep, whatever its outcome — the experiment batch's
//! restart behavior, with the stop list shown in the preview
//! before anything runs. A stop failure refuses the sweep
//! rather than degrading to contended measurement.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use manager_config::AppConfig;
use manager_optimize::{
    benchmark_prompts, compare, lowest_free_port, plan_filter, planned_arm_count, rank_arms,
    residency_per_gpu, thermal_tripped, tuning_temp_backend, winner_index, ActiveExperiment,
    ArmPricing, ArmStatus, BenchResult, BenchRun, Descent, ExperimentJournal, ExperimentMeasured,
    MeasurementEntry, MeasurementFingerprintInputs, MeasurementStore, ResidencyVerdict, SweepPlan,
    SweepState, TuningArm, TuningConfirmation, TuningCoords, TuningExclusion, TuningGpu,
    TuningInputs, TuningStore, TuningSweep, AXIS_ORDER, BENCHMARK_TEMPERATURE, MEASURED_RUNS,
    TUNING_ARM_TIMEOUT_SECS, TUNING_MAX_ARMS, TUNING_SWEEP_TIMEOUT_SECS,
};

use crate::{ApiError, AppState, TaskStatus};

// ---------------------------------------------------------------------------
// Sweep record (<data_dir>/optimize-tuning.json)
// ---------------------------------------------------------------------------

fn tuning_path(state: &AppState) -> PathBuf {
    state.data_dir.join("optimize-tuning.json")
}

fn load_tuning_store(state: &AppState) -> TuningStore {
    TuningStore::load(&tuning_path(state))
}

fn save_tuning_store(state: &AppState, store: &TuningStore) {
    if let Err(e) = store.save(&tuning_path(state)) {
        eprintln!("optimize-tuning: store save failed: {e}");
    }
}

/// The tuning section of the Optimize state payload: the
/// latest sweep per backend is too much for the payload, so
/// it carries the running sweep (progress) and the most
/// recent finished sweep, whichever backends they tuned.
pub(crate) fn tuning_payload(state: &AppState) -> Value {
    let store = load_tuning_store(state);
    let running = store.running().map(|s| serde_json::to_value(s).unwrap_or(Value::Null));
    let latest = store
        .sweeps
        .iter()
        .rev()
        .find(|s| s.state != SweepState::Running)
        .map(|s| serde_json::to_value(s).unwrap_or(Value::Null));
    json!({ "running": running, "latest": latest })
}

// ---------------------------------------------------------------------------
// Incumbent resolution and pricing
// ---------------------------------------------------------------------------

/// GPU indices a backend is placed on, resolved the way the
/// fingerprint resolution resolves them: an undetermined
/// placement counts as every installed GPU.
fn placed_indices(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
    all: &[u32],
) -> Vec<u32> {
    let _ = cfg;
    match crate::backend_gpu_visibility(b) {
        crate::GpuVisibility::CpuOnly => Vec::new(),
        crate::GpuVisibility::Gpus(indices) => indices,
        crate::GpuVisibility::Undetermined => all.to_vec(),
    }
}

/// Resolve everything sweep planning needs about one backend
/// from the live app (§B.1): its incumbent coordinates through
/// the same resolvers the axes vary, the model's axis
/// capabilities, the installed GPUs, and the predicted-best
/// cache configuration.
pub(crate) async fn build_tuning_inputs(
    state: &AppState,
    cfg: &AppConfig,
    backend_id: &str,
) -> Result<TuningInputs, ApiError> {
    let backend = cfg
        .backends
        .iter()
        .find(|b| b.id == backend_id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {backend_id}")))?
        .clone();
    let model = crate::model_for_backend(cfg, &backend)
        .ok_or_else(|| ApiError::InvalidInput(format!("backend {backend_id} serves no registered model")))?
        .clone();

    let inventory = {
        let store = state.telemetry().await;
        let guard = crate::lock_telemetry(&store);
        guard.inventory()
    };
    let gpus: Vec<TuningGpu> = inventory
        .iter()
        .map(|g| TuningGpu {
            index: g.index,
            name: g.name.clone(),
            total_mib: g.total_vram_mib,
            compute_capability: g.compute_capability,
            // Flash attention is offered only on architectures
            // the pinned llama.cpp runs it on: Pascal (sm_60)
            // is the documented exclusion (§B.1); unknown
            // capability (0,0) is not offered either — absent,
            // not offered-and-disabled.
            fa_supported: g.compute_capability >= (7, 0),
        })
        .collect();
    let all_indices: Vec<u32> = gpus.iter().map(|g| g.index).collect();
    let placed = placed_indices(cfg, &backend, &all_indices);
    // The incumbent's split over its placed GPUs: the stored
    // split is full-length over all GPUs; the coordinates
    // carry the placed subset (§B.1 placement axis form).
    let tensor_split: Vec<f32> = if placed.len() > 1 {
        let subset: Vec<f32> = placed
            .iter()
            .map(|i| {
                backend
                    .flags
                    .tensor_split
                    .get(*i as usize)
                    .copied()
                    .unwrap_or(0.0)
            })
            .collect();
        let sum: f32 = subset.iter().sum();
        if sum > 0.0 {
            subset.iter().map(|f| f / sum).collect()
        } else {
            vec![1.0 / placed.len() as f32; placed.len()]
        }
    } else {
        Vec::new()
    };

    let offload_info = crate::backend_offload_info(cfg, &backend);
    let gathered = crate::optimize::gather(state, cfg).await;
    let layout = gathered.live.layouts.get(&model.id);
    let moe_layers = offload_info
        .as_ref()
        .map(|i| i.moe_layers)
        .or_else(|| layout.and_then(|l| l.moe_layers));
    let incumbent = TuningCoords {
        gpu_indices: placed,
        tensor_split,
        offload_layers: offload_info.as_ref().map(|i| i.layers),
        moe_cache_mib: manager_models::moe_cache_mib(backend.raw_flags.as_deref())
            .filter(|n| *n > 0),
        mtp_n_max: match manager_models::mtp_draft(backend.raw_flags.as_deref()) {
            manager_models::MtpDraft::On(n) => {
                Some(n.unwrap_or(manager_models::LLAMA_CPP_N_MAX_DEFAULT))
            }
            manager_models::MtpDraft::Off => None,
        },
        n_ctx: backend.flags.n_ctx,
        cache: manager_models::cache_selection(backend.raw_flags.as_deref()),
        parallel_slots: manager_models::effective_slots(backend.raw_flags.as_deref()),
    };
    // Predicted-best cache selection for the incumbent's
    // placement/context (§B.5 starting coordinate), priced
    // from the same requirement the backend prices with.
    let predicted_cache = (|| {
        let geo = layout?.kv_geometry?;
        let req = crate::backend_memory_requirement(cfg, &backend)?;
        let placed_totals: Vec<u64> = incumbent
            .gpu_indices
            .iter()
            .map(|i| {
                gpus.iter()
                    .find(|g| g.index == *i)
                    .map(|g| g.total_mib)
                    .unwrap_or(0)
            })
            .collect();
        let inputs = manager_models::CacheFitInputs {
            kv: geo,
            gpu_weights_mib: req.gpu_weights_mib,
            rs_mib: req.rs_mib,
            moe_cache_mib: manager_models::moe_cache_mib(backend.raw_flags.as_deref())
                .unwrap_or(0) as u64,
            n_ctx: backend.flags.n_ctx,
            split: incumbent.tensor_split.clone(),
            gpu_total_mib: placed_totals,
        };
        manager_models::predicted_best(&inputs).map(|r| r.selection)
    })();

    Ok(TuningInputs {
        backend_id: backend_id.to_string(),
        model_id: model.id.clone(),
        incumbent,
        moe_layers,
        mtp_capable: crate::backend_mtp_info(cfg, &backend).is_some()
            || manager_models::effective_mtp(&model.id, &model.source_url, model.mtp),
        ctx_max: layout.map(|l| l.ctx_max).unwrap_or(0),
        gpus,
        predicted_cache,
    })
}

/// Price one arm through the app's requirement model: build
/// the arm's temp configuration in a cloned config and ask
/// `backend_memory_requirement`, distributed over the arm's
/// placement by `residency_per_gpu` (the distribution the
/// start gate uses, §B.3).
fn price_coords(
    cfg: &AppConfig,
    backend: &manager_config::BackendConfig,
    inputs: &TuningInputs,
    coords: &TuningCoords,
) -> Option<ArmPricing> {
    let temp = tuning_temp_backend(
        backend,
        coords,
        "opt-exp-tuning-price",
        backend.port,
        inputs.moe_layers,
    );
    let mut temp_cfg = cfg.clone();
    temp_cfg.backends.push(temp.clone());
    let req = crate::backend_memory_requirement(&temp_cfg, &temp)?;
    let n = coords.gpu_indices.len();
    if n == 0 {
        return Some(ArmPricing { per_gpu_mib: Vec::new() });
    }
    let split: Vec<f32> = if coords.tensor_split.len() == n {
        coords.tensor_split.clone()
    } else {
        Vec::new()
    };
    let per_gpu_mib = residency_per_gpu(&req, &split, n);
    Some(ArmPricing { per_gpu_mib })
}

/// The guardrail verdict for one arm (§B.3 filter 2): the
/// model's offload restrictions evaluated for the
/// architectures the arm's placement actually lands on, plus
/// the flash-attention capability gate (§B.1). `Some(reason)`
/// excludes the arm at planning with the restriction cited.
fn guardrail_reason(
    cfg: &AppConfig,
    backend: &manager_config::BackendConfig,
    inputs: &TuningInputs,
    coords: &TuningCoords,
) -> Option<String> {
    if coords.cache.flash_attn == manager_models::FlashAttn::On {
        for idx in &coords.gpu_indices {
            if let Some(g) = inputs.gpus.iter().find(|g| g.index == *idx) {
                if !g.fa_supported {
                    return Some(format!(
                        "flash attention is not offered on GPU {} ({}): the pinned llama.cpp cannot run it on compute capability {}.{}",
                        g.index, g.name, g.compute_capability.0, g.compute_capability.1
                    ));
                }
            }
        }
    }
    let layers = coords.offload_layers?;
    let moe = inputs.moe_layers?;
    let restrictions = crate::backend_offload_info(cfg, backend)
        .map(|i| i.restrictions)
        .unwrap_or_else(|| {
            manager_models::curated_models()
                .into_iter()
                .find(|s| s.id == inputs.model_id)
                .map(|s| s.offload_restrictions)
                .unwrap_or_default()
        });
    if restrictions.is_empty() {
        return None;
    }
    let placed_ccs: Vec<(u32, u32)> = coords
        .gpu_indices
        .iter()
        .filter_map(|i| {
            inputs
                .gpus
                .iter()
                .find(|g| g.index == *i)
                .map(|g| g.compute_capability)
        })
        .collect();
    match manager_models::offload_status(&restrictions, layers, moe, &placed_ccs, false) {
        manager_models::OffloadStatus::Blocked => {
            let cc = placed_ccs
                .iter()
                .find(|cc| restrictions.iter().any(|r| &r.compute_capability == *cc))
                .copied()
                .unwrap_or((0, 0));
            Some(format!(
                "offload {layers} is blocked by the model's restriction for compute capability {}.{} (tracker #37 guardrails)",
                cc.0, cc.1
            ))
        }
        _ => None,
    }
}

/// One candidate's planning verdict (§B.3): fit, then
/// guardrails. `Err(reason)` excludes it.
fn exclusion_reason(
    cfg: &AppConfig,
    backend: &manager_config::BackendConfig,
    inputs: &TuningInputs,
    coords: &TuningCoords,
) -> Option<String> {
    let pricing = price_coords(cfg, backend, inputs, coords);
    plan_filter(coords, inputs, pricing.as_ref(), guardrail_reason(cfg, backend, inputs, coords))
        .err()
}

/// The stop list (§B.4.1): every running backend placed on
/// any installed GPU (the sweep's placement axis spans them
/// all), the tuned backend included. CPU-only backends are
/// never parked — a sweep does not contend with them for
/// VRAM, and parking governs GPU residency.
async fn compute_stop_list(state: &AppState, cfg: &AppConfig, inputs: &TuningInputs) -> Vec<String> {
    let gathered = crate::optimize::gather(state, cfg).await;
    let all: Vec<u32> = inputs.gpus.iter().map(|g| g.index).collect();
    let mut out: Vec<String> = Vec::new();
    for b in &cfg.backends {
        if gathered.running.get(&b.id).copied() != Some(true) {
            continue;
        }
        if !placed_indices(cfg, b, &all).is_empty() {
            out.push(b.id.clone());
        }
    }
    out
}

/// The preview's display form of one axis's values.
fn axis_display(inputs: &TuningInputs) -> Vec<(String, Vec<String>)> {
    let mut axes: Vec<(String, Vec<String>)> = Vec::new();
    let placements = manager_optimize::placement_values(inputs);
    if placements.len() > 1 {
        axes.push((
            "placement".to_string(),
            placements
                .iter()
                .map(|(g, _)| {
                    format!(
                        "GPU {}",
                        g.iter().map(|i| i.to_string()).collect::<Vec<_>>().join("+")
                    )
                })
                .collect(),
        ));
    }
    let offload = manager_optimize::offload_values(inputs);
    if !offload.is_empty() {
        axes.push((
            "offload".to_string(),
            offload.iter().map(|l| l.to_string()).collect(),
        ));
    }
    let spec = manager_optimize::spec_values(inputs);
    if !spec.is_empty() {
        axes.push((
            "spec".to_string(),
            spec.iter()
                .map(|n| match n {
                    None => "off".to_string(),
                    Some(n) => format!("n-max {n}"),
                })
                .collect(),
        ));
    }
    axes.push((
        "context".to_string(),
        manager_optimize::context_values(inputs)
            .iter()
            .map(|n| n.to_string())
            .collect(),
    ));
    axes.push((
        "slots".to_string(),
        manager_optimize::slot_values(inputs)
            .iter()
            .map(|n| n.to_string())
            .collect(),
    ));
    axes
}

/// Build the full sweep plan for the preview (§B.2/§B.3):
/// value sets, arm count, stop list, and every planning
/// exclusion computed from the incumbent's coordinates.
async fn build_plan(
    state: &AppState,
    cfg: &AppConfig,
    backend_id: &str,
) -> Result<(TuningInputs, SweepPlan), ApiError> {
    let inputs = build_tuning_inputs(state, cfg, backend_id).await?;
    let backend = cfg
        .backends
        .iter()
        .find(|b| b.id == backend_id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {backend_id}")))?;
    let stop_list = compute_stop_list(state, cfg, &inputs).await;
    // Exclusions: every axis candidate off the incumbent,
    // filtered — the descent's later candidates are filtered
    // again at run time against their then-current winner.
    let mut exclusions: Vec<TuningExclusion> = Vec::new();
    for axis in AXIS_ORDER {
        let mut descent = Descent::new(inputs.incumbent.clone());
        for coords in descent.axis_candidates(axis, &inputs) {
            if let Some(reason) = exclusion_reason(cfg, backend, &inputs, &coords) {
                exclusions.push(TuningExclusion { coords, reason });
            }
        }
    }
    let plan = SweepPlan {
        backend_id: backend_id.to_string(),
        model_id: inputs.model_id.clone(),
        axes: axis_display(&inputs),
        planned_arms: planned_arm_count(&inputs),
        stop_list,
        exclusions,
        max_arms: TUNING_MAX_ARMS,
        arm_timeout_secs: TUNING_ARM_TIMEOUT_SECS,
        sweep_timeout_secs: TUNING_SWEEP_TIMEOUT_SECS,
    };
    Ok((inputs, plan))
}

/// Preview a tuning sweep for one backend (§B.2): axes and
/// value sets, arm count, stop list, budgets, and planning
/// exclusions — rendered before anything runs; nothing has
/// been stopped yet.
pub async fn op_optimize_tuning_preview(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
    backend_id: String,
) -> crate::ApiResult {
    let cfg = state.config.read().await.clone();
    let (inputs, plan) = build_plan(state, &cfg, &backend_id).await?;
    Ok(json!({
        "backend_id": backend_id,
        "model_id": inputs.model_id,
        "incumbent": inputs.incumbent,
        "plan": plan,
    }))
}

// ---------------------------------------------------------------------------
// Arm execution
// ---------------------------------------------------------------------------

/// Per-GPU temperatures from a fresh telemetry snapshot, in
/// the snapshot's GPU-index order subset `indices`.
async fn gpu_temps(state: &AppState, indices: &[u32]) -> Vec<(u32, f32)> {
    let store = state.telemetry().await;
    let guard = crate::lock_telemetry(&store);
    guard
        .snapshot()
        .iter()
        .filter(|s| indices.contains(&s.index))
        .map(|s| (s.index, s.temp_c))
        .collect()
}

/// One arm's outcome before the sweep loop records it.
struct ArmOutcome {
    arm: TuningArm,
    eval: Option<manager_optimize::EvalRunRecord>,
    comparison: Option<manager_optimize::EvalComparison>,
}

/// The measurement fingerprint inputs for a tuning temp
/// backend, built from its in-memory configuration (the
/// experiment runner's construction, §A.3's note applies).
fn tuning_fingerprint_inputs(
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

/// Run one arm end to end: planning filter, start preflight,
/// temp spawn, residency verification, thermal-guarded
/// benchmark under the §A.1 protocol, measurement recording
/// under the arm's own fingerprint, and (for the
/// confirmation arm) the screening eval. Cleanup is NOT done
/// here — the sweep loop owns it, so a dropped (cancelled /
/// timed-out) arm future cannot skip it (the experiment
/// runner's structure, §A.3).
#[allow(clippy::too_many_arguments)]
async fn run_arm(
    state: &AppState,
    cfg: &AppConfig,
    backend: &manager_config::BackendConfig,
    inputs: &TuningInputs,
    coords: &TuningCoords,
    axis: &str,
    index: usize,
    run_eval: bool,
    baseline: Option<&manager_optimize::BaselineRecord>,
) -> ArmOutcome {
    let temp_id = format!("opt-exp-tuning-{index}");
    let mut arm = TuningArm {
        index,
        axis: axis.to_string(),
        coords: coords.clone(),
        status: ArmStatus::Failed,
        detail: None,
        measured: None,
        residency: None,
        free_vram_mib: Vec::new(),
        temp_start_c: None,
        temp_peak_c: None,
        fingerprint: None,
    };
    let fail = |arm: &TuningArm| ArmOutcome {
        arm: arm.clone(),
        eval: None,
        comparison: None,
    };
    // Planning filter (§B.3): an excluded arm never spawns.
    if let Some(reason) = exclusion_reason(cfg, backend, inputs, coords) {
        arm.status = ArmStatus::Excluded;
        arm.detail = Some(reason);
        return fail(&arm);
    }
    let gathered = crate::optimize::gather(state, cfg).await;
    let model_id = inputs.model_id.clone();
    let model = cfg.models.iter().find(|m| m.id == model_id);
    let gguf_abs = model
        .map(|m| crate::resolve_under_data_dir(&cfg.data_dir, &m.gguf_path))
        .unwrap_or_else(|| crate::resolve_under_data_dir(&cfg.data_dir, &backend.model_file));
    let mut used_ports: HashSet<u16> = cfg.backends.iter().map(|b| b.port).collect();
    let port = lowest_free_port(&used_ports, cfg.gateway.port, cfg.postgres.port);
    used_ports.insert(port);
    let temp = tuning_temp_backend(backend, coords, &temp_id, port, inputs.moe_layers);
    let mut temp_cfg = cfg.clone();
    temp_cfg.backends.push(temp.clone());

    // Start preflight (the same gate every app-mediated start
    // passes): a refused arm is a failed arm with the
    // preflight's reason (§B.3).
    if let Err(e) = crate::backend_start_preflight_for(state, &temp_cfg, &temp).await {
        arm.detail = Some(format!("start preflight refused the arm: {e}"));
        return fail(&arm);
    }
    // Journal the in-flight arm BEFORE the spawn (the
    // experiment journal is the shared active-record store,
    // so the startup sweep reaps a tuning leftover the same
    // way).
    {
        let mut journal =
            ExperimentJournal::load(&state.data_dir.join("optimize-experiments.json"));
        journal.add_active(ActiveExperiment {
            temp_id: temp_id.clone(),
            pid: None,
            port,
            experiment_id: 9000 + index,
            started_at: chrono::Utc::now().to_rfc3339(),
        });
        let _ = journal.save(&state.data_dir.join("optimize-experiments.json"));
    }
    // Register the spec directly and spawn (never through
    // backend_add: a temp backend joins no gateway group).
    {
        let store = state.telemetry().await;
        let gpus = crate::lock_telemetry(&store).inventory();
        let spec = crate::backend_spec(cfg, &temp, &gpus);
        let mut sup = state.supervisor().await.lock().await;
        sup.register_spec(spec);
        if let Err(e) = sup.spawn_backend(&temp_id).await {
            arm.detail = Some(format!("temp backend {temp_id} failed to start: {e:?}"));
            return fail(&arm);
        }
        let pid = sup.status().into_iter().find(|s| s.id == temp_id).and_then(|s| s.pid);
        drop(sup);
        if let Some(pid) = pid {
            let mut journal =
                ExperimentJournal::load(&state.data_dir.join("optimize-experiments.json"));
            journal.set_active_pid(&temp_id, pid);
            let _ = journal.save(&state.data_dir.join("optimize-experiments.json"));
        }
    }
    if let Err(e) = crate::optimize_experiments::wait_healthy(state, &temp_id, port).await {
        arm.detail = Some(e);
        return fail(&arm);
    }

    // Residency verification (§B.4.1): the supervisor's
    // running set on the arm's GPUs must be exactly the arm's
    // temp backend, and a fresh telemetry snapshot's free
    // VRAM is recorded on the arm.
    {
        let statuses = {
            let mut sup = state.supervisor().await.lock().await;
            sup.reap_exited();
            sup.status()
        };
        let mut others: Vec<String> = Vec::new();
        for s in &statuses {
            if s.id == temp_id || s.pid.is_none() {
                continue;
            }
            if let Some(other) = cfg.backends.iter().find(|b| b.id == s.id) {
                let placed = placed_indices(cfg, other, &inputs.gpus.iter().map(|g| g.index).collect::<Vec<_>>());
                if placed.iter().any(|i| coords.gpu_indices.contains(i)) {
                    others.push(s.id.clone());
                }
            }
        }
        let store = state.telemetry().await;
        let guard = crate::lock_telemetry(&store);
        let samples = guard.snapshot();
        let mut free: Vec<u64> = Vec::new();
        let mut temps: Vec<f32> = Vec::new();
        let mut missing = false;
        for idx in &coords.gpu_indices {
            match samples.iter().find(|s| s.index == *idx) {
                Some(s) => {
                    free.push(s.vram_total_mib.saturating_sub(s.vram_used_mib));
                    temps.push(s.temp_c);
                }
                None => missing = true,
            }
        }
        arm.free_vram_mib = free;
        arm.temp_start_c = temps.iter().copied().fold(None::<f32>, |a, t| {
            Some(a.map(|x: f32| x.max(t)).unwrap_or(t))
        });
        arm.residency = Some(if !others.is_empty() {
            ResidencyVerdict::Unverified {
                reason: format!("backends still resident on the arm's GPUs: {}", others.join(", ")),
            }
        } else if missing {
            ResidencyVerdict::Unverified {
                reason: "telemetry unavailable for a target GPU".to_string(),
            }
        } else {
            ResidencyVerdict::VerifiedExclusive
        });
    }
    // Thermal guard at arm start (§B.4.2, extension E3's
    // predicate): a card already at the threshold aborts the
    // arm before any benchmark traffic.
    if thermal_tripped(arm.temp_start_c) {
        arm.status = ArmStatus::AbortedThermal;
        arm.detail = Some(format!(
            "aborted — thermal: GPU at {:.0} °C at arm start (threshold {:.0} °C)",
            arm.temp_start_c.unwrap_or_default(),
            manager_optimize::THERMAL_ABORT_C,
        ));
        return fail(&arm);
    }

    // The §A.1 benchmark: fixed prompts, one discarded
    // warm-up per prompt, three measured runs, median stored,
    // temperature 0, the server's own timings. Temperatures
    // are sampled at each prompt boundary (§B.4.2).
    let base_url = format!("http://127.0.0.1:{port}");
    let mut results: Vec<BenchResult> = Vec::new();
    let mut peak = arm.temp_start_c;
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
            Duration::from_secs(TUNING_ARM_TIMEOUT_SECS),
        )
        .await
        {
            arm.detail = Some(format!("benchmark warm-up failed: {e}"));
            return fail(&arm);
        }
        let mut runs: Vec<BenchRun> = Vec::new();
        for _ in 0..MEASURED_RUNS {
            match manager_supervisor::benchmark_completion(
                &base_url,
                &model_id,
                &messages,
                BENCHMARK_TEMPERATURE,
                prompt.max_tokens,
                Duration::from_secs(TUNING_ARM_TIMEOUT_SECS),
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
                    arm.detail = Some(format!("benchmark run failed: {e}"));
                    return fail(&arm);
                }
            }
        }
        results.push(BenchResult::from_runs(runs));
        for (_, t) in gpu_temps(state, &coords.gpu_indices).await {
            peak = Some(peak.map(|p: f32| p.max(t)).unwrap_or(t));
        }
        if thermal_tripped(peak) {
            arm.temp_peak_c = peak;
            arm.status = ArmStatus::AbortedThermal;
            arm.detail = Some(format!(
                "aborted — thermal: GPU reached {:.0} °C during the benchmark (threshold {:.0} °C)",
                peak.unwrap_or_default(),
                manager_optimize::THERMAL_ABORT_C,
            ));
            return fail(&arm);
        }
    }
    arm.temp_peak_c = peak;
    let mut iter = results.into_iter();
    let short = iter.next().unwrap_or_else(|| BenchResult::from_runs(Vec::new()));
    let long = iter.next().unwrap_or_else(|| BenchResult::from_runs(Vec::new()));
    arm.measured = Some(ExperimentMeasured {
        short: short.clone(),
        long: long.clone(),
    });
    arm.status = if matches!(arm.residency, Some(ResidencyVerdict::VerifiedExclusive)) {
        ArmStatus::Completed
    } else {
        ArmStatus::Unverified
    };

    // Record the measurement under the ARM's fingerprint,
    // never the incumbent backend's (§B.4.4), with the
    // residency verdict and peak temperature riding the
    // entry (extension E4).
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
        let fp_inputs = tuning_fingerprint_inputs(&temp, model, file_size, &gpu_pairs);
        let fp = manager_optimize::measurement_fingerprint(&fp_inputs);
        arm.fingerprint = Some(fp.clone());
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
            origin_backend_id: Some(inputs.backend_id.clone()),
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
            residency: arm
                .residency
                .as_ref()
                .map(|r| r.as_str().to_string()),
            peak_temp_c: arm.temp_peak_c,
        };
        let mut store = MeasurementStore::load(&crate::optimize::measurements_path(state));
        store.upsert(entry);
        if let Err(e) = store.save(&crate::optimize::measurements_path(state)) {
            arm.status = ArmStatus::Failed;
            arm.detail = Some(format!("benchmark completed but recording it failed: {e}"));
            return fail(&arm);
        }
    }

    // Screening eval (§A.4) — only the confirmation arm asks.
    let (eval, comparison) = if run_eval {
        let record = crate::optimize_eval::run_eval_suite(&temp_id, &base_url, &model_id).await;
        let comparison = baseline.map(|b| compare(&record.scores, &b.scores));
        (Some(record), comparison)
    } else {
        (None, None)
    };
    ArmOutcome {
        arm,
        eval,
        comparison,
    }
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

/// Run one sweep to completion. Owns the parked backends'
/// restart and the sweep record's final state on every exit
/// path. Cancellation arrives through `rx` (the shared job
/// slot's abort handle): the in-flight arm future is dropped
/// and its cleanup runs outside it, exactly as the
/// experiment batch's does.
async fn run_sweep(
    state: Arc<AppState>,
    backend_id: String,
    mut rx: tokio::sync::oneshot::Receiver<()>,
) {
    let cfg = state.config.read().await.clone();
    let Some(backend) = cfg.backends.iter().find(|b| b.id == backend_id).cloned() else {
        return;
    };
    let (inputs, plan) = match build_plan(&state, &cfg, &backend_id).await {
        Ok(v) => v,
        Err(e) => {
            crate::optimize::opt_log1(
                &state,
                &format!("tuning sweep for '{backend_id}' could not be planned: {e}"),
            );
            return;
        }
    };
    let sweep_id = format!(
        "tuning-{backend_id}-{}",
        chrono::Utc::now().timestamp_millis()
    );
    let mut sweep = TuningSweep {
        id: sweep_id.clone(),
        backend_id: backend_id.clone(),
        model_id: inputs.model_id.clone(),
        state: SweepState::Running,
        started_at: chrono::Utc::now().to_rfc3339(),
        finished_at: None,
        incumbent: inputs.incumbent.clone(),
        stop_list: plan.stop_list.clone(),
        arms: Vec::new(),
        exclusions: plan.exclusions.clone(),
        winner_arm: None,
        incumbent_stands: false,
        confirmation: None,
        truncated: None,
        detail: None,
        adopted: false,
        adopted_at: None,
        adopted_anyway: false,
    };
    {
        let mut store = load_tuning_store(&state);
        store.upsert(sweep.clone());
        save_tuning_store(&state, &store);
    }
    crate::optimize::opt_log1(
        &state,
        &format!(
            "tuning sweep {sweep_id}: backend '{backend_id}' model '{}', {} planned arms, stop list: {}",
            inputs.model_id,
            plan.planned_arms,
            if plan.stop_list.is_empty() {
                "none".to_string()
            } else {
                plan.stop_list.join(", ")
            }
        ),
    );

    // Baseline for the confirmation arm's screen (§B.6):
    // the stored baseline when fresh for the target's
    // fingerprint, else run it against the target BEFORE
    // parking (the target may itself be parked), mirroring
    // the experiment runner's baseline order (§A.4 protocol:
    // baseline first, cached while the fingerprint holds).
    let baseline: Option<manager_optimize::BaselineRecord> = {
        let gathered = crate::optimize::gather(&state, &cfg).await;
        let stored = crate::optimize_eval::load_baseline(&state);
        let fresh = stored.as_ref().and_then(|b| {
            let is_fresh = b.suite_version == manager_optimize::EVAL_SUITE_VERSION
                && gathered.fingerprints.get(&b.backend_id) == Some(&b.config_fingerprint);
            if is_fresh {
                Some(b.clone())
            } else {
                None
            }
        });
        match fresh {
            Some(b) => Some(b),
            None => {
                let running = gathered.running.get(&backend_id).copied() == Some(true);
                if running {
                    if let Some(fp) = gathered.fingerprints.get(&backend_id).cloned() {
                        let run = crate::optimize_eval::run_eval_suite(
                            &backend_id,
                            &format!("http://127.0.0.1:{}", backend.port),
                            &inputs.model_id,
                        )
                        .await;
                        let record = manager_optimize::BaselineRecord::from_run(&run, fp);
                        let _ = crate::optimize_eval::save_baseline(&state, record.clone());
                        Some(record)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        }
    };

    // Parking (§B.4.1): stop the tuned backend and every
    // running backend on the sweep's GPUs. A stop failure
    // refuses the sweep rather than degrading to contended
    // measurement; what was already stopped is restarted.
    let mut stopped: Vec<String> = Vec::new();
    let mut park_failed: Option<String> = None;
    for id in &plan.stop_list {
        let outcome = state.supervisor().await.lock().await.stop(id, false).await;
        match outcome {
            Ok(()) => stopped.push(id.clone()),
            Err(e) => {
                park_failed = Some(format!(
                    "preflight parking needed backend '{id}' stopped, but stopping it failed: {e:?}"
                ));
                break;
            }
        }
    }

    let mut cancelled = false;
    if park_failed.is_none() {
        let deadline = Instant::now() + Duration::from_secs(TUNING_SWEEP_TIMEOUT_SECS);
        let mut descent = Descent::new(inputs.incumbent.clone());
        let mut arms: Vec<TuningArm> = Vec::new();
        let mut thermal_streak = 0usize;
        // The incumbent is arm 0 (§B.1).
        let mut queue: Vec<(String, TuningCoords, bool)> =
            vec![("incumbent".to_string(), inputs.incumbent.clone(), false)];
        let mut axis_iter = AXIS_ORDER.iter();
        let mut confirmation_done = false;
        'sweep: while !queue.is_empty() || axis_iter.len() > 0 || !confirmation_done {
            if queue.is_empty() {
                // Advance the descent: take the next axis's
                // candidates off the current winner.
                match axis_iter.next() {
                    Some(axis) => {
                        let candidates = descent.axis_candidates(axis, &inputs);
                        for c in candidates {
                            queue.push((axis.to_string(), c, false));
                        }
                        // The MoE cache sub-axis rides the
                        // offload axis's winning position.
                        if *axis == "offload" && !queue.is_empty() {
                            // Cache candidates are computed
                            // after the position arms run (they
                            // need the winner's priced free
                            // VRAM); handled below by re-queue.
                        }
                        continue;
                    }
                    None => {
                        // Descent finished: run the
                        // confirmation arm for a non-incumbent
                        // winner, then end.
                        if !confirmation_done {
                            confirmation_done = true;
                            if let Some(w) = winner_index(&arms) {
                                if arms[w].coords != inputs.incumbent {
                                    let coords = arms[w].coords.clone();
                                    queue.push(("confirmation".to_string(), coords, true));
                                    continue;
                                }
                            }
                        }
                        break 'sweep;
                    }
                }
            }
            if arms.len() >= TUNING_MAX_ARMS {
                sweep.truncated = Some(format!(
                    "arm budget reached ({TUNING_MAX_ARMS} arms); the arms measured so far are ranked"
                ));
                break 'sweep;
            }
            if Instant::now() >= deadline {
                sweep.truncated = Some(format!(
                    "sweep budget reached ({} minutes); the arms measured so far are ranked",
                    TUNING_SWEEP_TIMEOUT_SECS / 60
                ));
                break 'sweep;
            }
            let (axis, coords, is_confirmation) = queue.remove(0);
            let index = arms.len();
            let temp_id = format!("opt-exp-tuning-{index}");
            let inner = run_arm(
                &state,
                &cfg,
                &backend,
                &inputs,
                &coords,
                &axis,
                index,
                is_confirmation,
                baseline.as_ref(),
            );
            let budget = Duration::from_secs(TUNING_ARM_TIMEOUT_SECS);
            let outcome = tokio::select! {
                _ = &mut rx => {
                    cancelled = true;
                    None
                }
                res = tokio::time::timeout(budget, inner) => Some(res),
            };
            // Cleanup outside the arm future, always (§A.3).
            crate::optimize_experiments::cleanup_temp(&state, &temp_id).await;
            let outcome = match outcome {
                None => break 'sweep,
                Some(Err(_)) => ArmOutcome {
                    arm: TuningArm {
                        index,
                        axis: axis.clone(),
                        coords: coords.clone(),
                        status: ArmStatus::Failed,
                        detail: Some(format!(
                            "arm exceeded its {}-minute budget",
                            TUNING_ARM_TIMEOUT_SECS / 60
                        )),
                        measured: None,
                        residency: None,
                        free_vram_mib: Vec::new(),
                        temp_start_c: None,
                        temp_peak_c: None,
                        fingerprint: None,
                    },
                    eval: None,
                    comparison: None,
                },
                Some(Ok(o)) => o,
            };
            match outcome.arm.status {
                ArmStatus::AbortedThermal => {
                    thermal_streak += 1;
                }
                _ => {
                    thermal_streak = 0;
                }
            }
            crate::optimize::opt_log1(
                &state,
                &format!(
                    "tuning {sweep_id} arm {index} ({axis}): {:?}{}",
                    outcome.arm.status,
                    outcome
                        .arm
                        .detail
                        .as_ref()
                        .map(|d| format!(" — {d}"))
                        .unwrap_or_default()
                ),
            );
            if is_confirmation {
                let winner = winner_index(&arms);
                let (reproduced, gen_delta_pct) = match (winner, outcome.arm.gen_long()) {
                    (Some(w), Some(conf_gen)) => {
                        let win_gen = arms[w].gen_long().unwrap_or(0.0);
                        let delta = if win_gen > 0.0 {
                            Some((conf_gen - win_gen) / win_gen * 100.0)
                        } else {
                            None
                        };
                        (
                            delta.map(|d| d.abs() <= 10.0).unwrap_or(false),
                            delta,
                        )
                    }
                    _ => (false, None),
                };
                let screen_passes = outcome
                    .comparison
                    .as_ref()
                    .map(|c| {
                        c.screen_at_or_above_baseline
                            && c.zero_categories_that_baseline_passes.is_empty()
                    })
                    .unwrap_or(false);
                sweep.confirmation = Some(TuningConfirmation {
                    arm_index: index,
                    reproduced,
                    gen_delta_pct,
                    eval: outcome.eval.clone(),
                    comparison: outcome.comparison.clone(),
                    screen_passes,
                });
            }
            arms.push(outcome.arm);
            // Keep the axis winner: after an axis's queue
            // drains, the descent's current becomes the best
            // rankable arm among the current configuration and
            // this axis's arms.
            if queue.is_empty() || queue[0].0 != axis {
                let mut best: Option<&TuningArm> = arms
                    .iter()
                    .filter(|a| a.rankable() && a.coords == descent.current)
                    .next()
                    .or_else(|| arms.iter().find(|a| a.axis == "incumbent"));
                for a in arms.iter().filter(|a| a.axis == axis && a.rankable()) {
                    if best.map(|b| a.gen_long() > b.gen_long()).unwrap_or(true) {
                        best = Some(a);
                    }
                }
                if let Some(b) = best {
                    descent.keep(b.coords.clone());
                }
                // MoE cache sub-axis (§B.1): after the offload
                // winner is kept, price its free VRAM and
                // queue the cache sizes as their own axis.
                if axis == "offload" {
                    if let Some(pricing) = price_coords(&cfg, &backend, &inputs, &descent.current) {
                        let primary_total = descent
                            .current
                            .gpu_indices
                            .first()
                            .and_then(|i| inputs.gpus.iter().find(|g| g.index == *i))
                            .map(|g| g.total_mib)
                            .unwrap_or(0);
                        let share = pricing.per_gpu_mib.first().copied().unwrap_or(0);
                        let free_after = primary_total.saturating_sub(share);
                        for c in descent.cache_candidates(free_after) {
                            queue.push(("offload-cache".to_string(), c, false));
                        }
                    }
                }
            }
            if thermal_streak >= manager_optimize::THERMAL_ABORT_STREAK {
                sweep.truncated = Some(
                    "sweep stopped: two consecutive thermal aborts — the card is heat-soaked (§B.4.2)"
                        .to_string(),
                );
                break 'sweep;
            }
            // Persist progress after every arm.
            sweep.arms = arms.clone();
            {
                let mut store = load_tuning_store(&state);
                store.upsert(sweep.clone());
                save_tuning_store(&state, &store);
            }
        }
        sweep.arms = arms;
    }

    // Restart what the sweep parked, whatever its outcome.
    let note = crate::optimize_experiments::restart_stopped(&state, &stopped).await;
    if !note.is_empty() {
        crate::optimize::opt_log1(&state, &format!("tuning {sweep_id}: {note}"));
    }

    sweep.finished_at = Some(chrono::Utc::now().to_rfc3339());
    if let Some(failure) = park_failed {
        sweep.state = SweepState::Failed;
        sweep.detail = Some(failure);
    } else if cancelled {
        sweep.state = SweepState::Cancelled;
        sweep.detail = Some("sweep cancelled by the operator".to_string());
    } else {
        sweep.state = SweepState::Completed;
        sweep.winner_arm = winner_index(&sweep.arms);
        sweep.incumbent_stands = match sweep.winner_arm {
            None => true,
            Some(w) => {
                let winner = &sweep.arms[w];
                let incumbent_gen = sweep
                    .arms
                    .iter()
                    .find(|a| a.axis == "incumbent")
                    .and_then(|a| a.gen_long());
                winner.coords == sweep.incumbent
                    || match (winner.gen_long(), incumbent_gen) {
                        (Some(wg), Some(ig)) => wg <= ig,
                        _ => winner.coords == sweep.incumbent,
                    }
            }
        };
    }
    {
        let mut store = load_tuning_store(&state);
        store.upsert(sweep.clone());
        save_tuning_store(&state, &store);
    }
    crate::optimize::opt_log1(
        &state,
        &format!(
            "tuning sweep {sweep_id} end: {:?}, {} arms, winner {}, incumbent stands {}",
            sweep.state,
            sweep.arms.len(),
            sweep
                .winner_arm
                .map(|w| w.to_string())
                .unwrap_or_else(|| "-".to_string()),
            sweep.incumbent_stands
        ),
    );
}

/// Start a tuning sweep for one backend (§B.2): refuses
/// while the job slot or the consultation slot is occupied
/// (§B.4.3, extension E2), claims the backend in the
/// per-backend record, and runs the sweep as a tracked task
/// under the shared job slot — Cancel is `optimize_cancel_job`.
pub async fn op_optimize_tuning_run(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
    backend_id: String,
) -> crate::ApiResult {
    {
        let slot = state.optimize_job_abort.lock().await;
        if slot.is_some() {
            return Err(ApiError::InvalidInput(
                "a measurement or experiment job is already running".to_string(),
            ));
        }
    }
    {
        // Extension E2 (§B.4.3): a sweep refuses to start
        // while a consultation occupies its slot — a
        // consultation is generation traffic that contends
        // for the CPU and system RAM offload arms measure.
        let slot = state.optimize_abort.lock().await;
        if slot.is_some() {
            return Err(ApiError::InvalidInput(
                "a consultation is running; a tuning sweep refuses to start until it finishes (§B.4.3)".to_string(),
            ));
        }
    }
    let cfg = state.config.read().await.clone();
    if !cfg.backends.iter().any(|b| b.id == backend_id) {
        return Err(ApiError::NotFound(format!("backend {backend_id}")));
    }
    // Plan first, synchronously: a backend that cannot be
    // planned (no registered model, no GPUs) refuses here
    // rather than failing inside the task.
    let _ = build_plan(state, &cfg, &backend_id).await?;
    let guard = crate::optimize::OptimizeOpGuard::claim(
        state,
        &backend_id,
        crate::OptimizeOpFamily::Benchmark,
        "tuning",
    )?;
    let task_id = state
        .record_task(
            "optimize-tuning",
            json!({ "backend": backend_id }),
        )
        .await;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
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
    let task_state = state.clone();
    let task_backend = backend_id.clone();
    tokio::spawn(async move {
        // The guard lives for the sweep's whole run: its
        // claim is the per-backend record the refusals name.
        let _guard = guard;
        run_sweep(task_state.clone(), task_backend, rx).await;
        {
            let mut slot = task_state.optimize_job_abort.lock().await;
            *slot = None;
        }
        task_state
            .finish_task(&task_id, TaskStatus::Done, json!({ "backend": backend_id }))
            .await;
    });
    crate::optimize::state_payload(state).await
}

/// Adopt a sweep's winning arm into the real backend's saved
/// configuration (§B.6): a separate, explicit action that
/// writes through the existing structured save ops — offload
/// through `backend_set_offload`, spec through
/// `backend_set_mtp`, slots through `backend_set_parallel`,
/// the expert cache through `backend_set_moe_cache`, cache
/// types through `backend_set_cache`, placement through
/// `placement_apply`, context through `backend_update_flags`
/// — each journal-recorded as those operations record, and
/// landing in the Activation Plan like any saved change.
/// Nothing restarts here; activation governs restarts.
pub async fn op_optimize_tuning_adopt(
    state: &Arc<AppState>,
    sink: &Arc<dyn crate::EventSink>,
    sweep_id: String,
    adopt_anyway: Option<bool>,
) -> crate::ApiResult {
    let mut store = load_tuning_store(state);
    let sweep = store
        .sweeps
        .iter()
        .find(|s| s.id == sweep_id)
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("tuning sweep {sweep_id}")))?;
    if sweep.state != SweepState::Completed {
        return Err(ApiError::InvalidInput(
            "only a completed sweep can be adopted".to_string(),
        ));
    }
    if sweep.adopted {
        return Err(ApiError::InvalidInput(
            "this sweep's winner was already adopted".to_string(),
        ));
    }
    if sweep.incumbent_stands {
        return Err(ApiError::InvalidInput(
            "the incumbent stands: no adoption is offered for this sweep (§B.5)".to_string(),
        ));
    }
    let winner = sweep
        .winner_arm
        .and_then(|w| sweep.arms.get(w))
        .ok_or_else(|| ApiError::InvalidInput("the sweep recorded no winner".to_string()))?;
    let coords = winner.coords.clone();
    let default_ok = manager_optimize::adopt_is_default(&sweep);
    if !default_ok && adopt_anyway != Some(true) {
        return Err(ApiError::InvalidInput(
            "adoption is not the default for this winner (the confirmation arm did not reproduce it, or the screening gate did not pass); pass adopt_anyway to adopt it explicitly (§B.6)".to_string(),
        ));
    }
    let backend_id = sweep.backend_id.clone();
    let cfg = state.config.read().await.clone();
    let backend = cfg
        .backends
        .iter()
        .find(|b| b.id == backend_id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {backend_id}")))?
        .clone();
    let inputs = build_tuning_inputs(state, &cfg, &backend_id).await?;

    // Placement first, so the offload guardrail evaluation
    // inside backend_set_offload judges the winner's
    // placement, not the incumbent's.
    if coords.gpu_indices != inputs.incumbent.gpu_indices
        || coords.tensor_split != inputs.incumbent.tensor_split
    {
        let n_gpus = inputs.gpus.len();
        let mut split = vec![0.0f32; n_gpus];
        if coords.gpu_indices.len() == 1 {
            if let Some(i) = coords.gpu_indices.first() {
                if let Some(slot) = split.get_mut(*i as usize) {
                    *slot = 1.0;
                }
            }
        } else {
            for (pos, idx) in coords.gpu_indices.iter().enumerate() {
                if let Some(slot) = split.get_mut(*idx as usize) {
                    *slot = coords.tensor_split.get(pos).copied().unwrap_or(0.0);
                }
            }
        }
        crate::op_placement_apply(
            state,
            sink,
            inputs.model_id.clone(),
            split,
            Some(backend_id.clone()),
        )
        .await?;
    }
    if let Some(layers) = coords.offload_layers {
        if Some(layers) != inputs.incumbent.offload_layers {
            crate::op_backend_set_offload(state, sink, backend_id.clone(), layers, None).await?;
        }
    }
    if inputs.mtp_capable && coords.mtp_n_max != inputs.incumbent.mtp_n_max {
        crate::op_backend_set_mtp(state, sink, backend_id.clone(), coords.mtp_n_max).await?;
    }
    if coords.parallel_slots != inputs.incumbent.parallel_slots {
        crate::op_backend_set_parallel(state, sink, backend_id.clone(), Some(coords.parallel_slots))
            .await?;
    }
    if coords.offload_layers.unwrap_or(0) > 0
        && coords.moe_cache_mib != inputs.incumbent.moe_cache_mib
    {
        crate::op_backend_set_moe_cache(
            state,
            sink,
            backend_id.clone(),
            coords.moe_cache_mib,
            None,
        )
        .await?;
    }
    if coords.cache != inputs.incumbent.cache {
        crate::op_backend_set_cache(
            state,
            sink,
            backend_id.clone(),
            Some(coords.cache.k.as_str().to_string()),
            Some(coords.cache.v.as_str().to_string()),
            Some(coords.cache.flash_attn.as_str().to_string()),
        )
        .await?;
    }
    if coords.n_ctx != inputs.incumbent.n_ctx {
        crate::op_backend_update_flags(
            state,
            sink,
            backend_id.clone(),
            coords.n_ctx,
            backend.flags.n_batch,
        )
        .await?;
    }

    if let Some(record) = store.sweeps.iter_mut().find(|s| s.id == sweep_id) {
        record.adopted = true;
        record.adopted_at = Some(chrono::Utc::now().to_rfc3339());
        record.adopted_anyway = !default_ok;
    }
    save_tuning_store(state, &store);
    crate::optimize::opt_log1(
        state,
        &format!(
            "tuning sweep {sweep_id}: winner adopted into backend '{backend_id}'{}",
            if default_ok { "" } else { " (adopt-anyway)" }
        ),
    );
    crate::optimize::state_payload(state).await
}

/// The ranked-table payload helper the state payload uses:
/// arms in ranked display order (§B.6).
#[allow(dead_code)]
fn ranked_json(sweep: &TuningSweep) -> Value {
    let order = rank_arms(&sweep.arms);
    json!(order
        .iter()
        .map(|&i| serde_json::to_value(&sweep.arms[i]).unwrap_or(Value::Null))
        .collect::<Vec<_>>())
}
