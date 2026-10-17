//! Optimize page commands (design: docs/optimize-design.md §7).
//!
//! The page's server side: state assembly (config + telemetry +
//! supervisor + gateway stats, fed to `manager-optimize`'s pure
//! core), the consultation task with real scoped cancellation,
//! plan persistence and decisions. Apply has no command here by
//! design — the UI orchestrates the existing mutation commands
//! (§5.2), and this module's commands never mutate configuration
//! except purpose text (`optimize_save_purpose`, a scoped save
//! through `manager_config::save` like every other config write).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::State;

use manager_config::AppConfig;
use manager_optimize::{
    benchmark_prompts, build_context, build_prompts, config_fingerprint, consult_default,
    default_purpose_seed, estimate_tokens, measurement_facts, measurement_fingerprint, parse_plan,
    probe_hardware, validate_plan, BackendMeasured, BenchResult, BenchRun, CandidateFact,
    ConsultCandidate, Decision, HardwareContext, HfClient, ItemValidation, LayoutFacts,
    LiveSnapshot, MeasureInputs, MeasurementEntry, MeasurementFingerprintInputs, MeasurementStore,
    Plan, TrafficWindow, ValidationStatus, BENCHMARK_TEMPERATURE, MEASURED_RUNS,
};
use manager_supervisor::ProcessState;

use crate::{err, jerr, ApiError, ApiResult, AppState, TaskStatus};

/// Consultation generation budget (design §3).
const CONSULT_MAX_TOKENS: u32 = 4096;
const CONSULT_TEMPERATURE: f32 = 0.2;
const CONSULT_TIMEOUT: Duration = Duration::from_secs(900);
/// Per-run benchmark timeout (Phase 5a): generous, because the
/// long prompt on a slow (heavily offloaded) backend is the
/// slowest legitimate run — prompt processing alone can take
/// minutes there.
const MEASURE_RUN_TIMEOUT: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------------------
// Plan persistence (<data_dir>/optimize-plan.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct PlanFile {
    #[serde(default)]
    plan: Option<Plan>,
    /// The stored plan came from sample mode, not a consultation.
    #[serde(default)]
    sample: bool,
    /// Denial memory (design §6): fingerprints the operator has
    /// denied, accumulated across plans. Kept when a plan is
    /// discarded — the denials were explicit decisions.
    #[serde(default)]
    denied_fingerprints: Vec<String>,
}

fn plan_path(state: &AppState) -> PathBuf {
    state.data_dir.join("optimize-plan.json")
}

fn load_plan_file(state: &AppState) -> PlanFile {
    std::fs::read_to_string(plan_path(state))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_plan_file(state: &AppState, file: &PlanFile) -> Result<(), String> {
    let text = serde_json::to_string_pretty(file).map_err(|e| e.to_string())?;
    std::fs::write(plan_path(state), text).map_err(|e| e.to_string())
}

fn measurements_path(state: &AppState) -> PathBuf {
    state.data_dir.join("optimize-measurements.json")
}

// ---------------------------------------------------------------------------
// Discovery persistence (<data_dir>/optimize-candidates.json)
// ---------------------------------------------------------------------------

/// The stored discovery pre-pass (Phase 5c, §A.5): the candidate
/// facts from one `optimize_discover` run, the directive text
/// they were gathered for, and when they were retrieved. A
/// consultation only receives candidates whose stored directive
/// matches its own (see `optimize_consult`) — facts gathered
/// for a different question are not evidence for this one.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CandidatesFile {
    #[serde(default)]
    directive: String,
    #[serde(default)]
    retrieved_at: String,
    #[serde(default)]
    candidates: Vec<CandidateFact>,
}

fn candidates_path(state: &AppState) -> PathBuf {
    state.data_dir.join("optimize-candidates.json")
}

fn load_candidates_file(state: &AppState) -> CandidatesFile {
    std::fs::read_to_string(candidates_path(state))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_candidates_file(state: &AppState, file: &CandidatesFile) -> Result<(), String> {
    let text = serde_json::to_string_pretty(file).map_err(|e| e.to_string())?;
    std::fs::write(candidates_path(state), text).map_err(|e| e.to_string())
}

/// The payload form of the stored candidates: the list with the
/// directive it belongs to and its retrieval time, or `null`
/// when no discovery has run (or a run found nothing).
fn candidates_json(file: &CandidatesFile) -> Value {
    if file.candidates.is_empty() {
        Value::Null
    } else {
        json!({
            "directive": file.directive,
            "retrieved_at": file.retrieved_at,
            "items": file.candidates,
        })
    }
}

/// The measurement-fingerprint inputs for one backend (§A.1),
/// resolved from the live configuration: the model's verified
/// stamp or on-disk size, the resolved offload, and the GPU set
/// the backend is placed on (indices + names from the telemetry
/// snapshot). An undetermined placement fingerprints against
/// the whole GPU set — the launch is unrestricted then, so any
/// GPU could be doing the work.
fn fingerprint_inputs(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
    gpus: &[(u32, String)],
) -> MeasurementFingerprintInputs {
    let model = crate::model_for_backend(cfg, b);
    let file_size_bytes = model
        .map(|m| crate::resolve_under_data_dir(&cfg.data_dir, &m.gguf_path))
        .or_else(|| Some(crate::resolve_under_data_dir(&cfg.data_dir, &b.model_file)))
        .and_then(|p| std::fs::metadata(p).ok())
        .filter(|m| m.is_file())
        .map(|m| m.len());
    let gpu_set: Vec<(u32, String)> = match crate::backend_gpu_visibility(b) {
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
    // Resolved offload: the offload info's resolved layer count
    // when the backend's model is offload-capable, else the raw
    // flags' own resolution.
    let offload = match crate::backend_offload_info(cfg, b) {
        Some(info) => manager_models::offload_for_layers(info.layers, info.moe_layers),
        None => manager_models::expert_offload(b.raw_flags.as_deref()),
    };
    MeasurementFingerprintInputs {
        verified_sha256: model.and_then(|m| m.verified_sha256.clone()),
        file_size_bytes,
        quant: model.map(|m| m.quant.clone()).unwrap_or_default(),
        n_ctx: b.flags.n_ctx,
        n_batch: b.flags.n_batch,
        offload,
        tensor_split: b.flags.tensor_split.clone(),
        split_mode: b.flags.split_mode,
        raw_flags: b.raw_flags.clone(),
        gpus: gpu_set,
    }
}

// ---------------------------------------------------------------------------
// Input gathering: everything the pure core needs from the live app
// ---------------------------------------------------------------------------

struct Gathered {
    measure: MeasureInputs,
    live: LiveSnapshot,
    candidates: Vec<ConsultCandidate>,
    running: HashMap<String, bool>,
    /// Each configured backend's current measurement
    /// fingerprint (§A.1), computed from the same snapshot the
    /// context is built from.
    fingerprints: HashMap<String, String>,
}

async fn gather(state: &AppState, cfg: &AppConfig) -> Gathered {
    // Telemetry snapshot (GPU inventory, per-process VRAM, peaks).
    let store = state.telemetry().await;
    let (samples, processes, temp_peaks_c) = {
        let guard = crate::lock_telemetry(&store);
        let samples = guard.snapshot();
        let processes = guard.last_processes().to_vec();
        let peaks: Vec<(u32, f32)> = samples
            .iter()
            .filter_map(|s| {
                guard
                    .peaks(s.index)
                    .and_then(|p| p.temp_c)
                    .map(|m| (s.index, m.value))
            })
            .collect();
        (samples, processes, peaks)
    };
    // Supervisor statuses (reap first so stale pids do not
    // attribute VRAM to recycled processes).
    let statuses = {
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        sup.status()
    };
    let pid_to_backend: HashMap<u32, String> = statuses
        .iter()
        .filter_map(|s| s.pid.map(|pid| (pid, s.id.clone())))
        .collect();
    let running: HashMap<String, bool> = statuses
        .iter()
        .map(|s| (s.id.clone(), s.state == ProcessState::Running))
        .collect();

    let probe = probe_hardware();
    let mut measure = MeasureInputs {
        hardware: HardwareContext {
            gpus: samples
                .iter()
                .map(|s| manager_optimize::context::GpuContext {
                    index: s.index,
                    name: s.name.clone(),
                    vram_total_mib: s.vram_total_mib,
                    vram_used_mib: s.vram_used_mib,
                    driver_model: s
                        .driver_model
                        .map(|d| format!("{d:?}").to_lowercase()),
                })
                .collect(),
            ram_total_mib: probe.ram_total_mib,
            cpu_model: probe.cpu_model.clone(),
            cpu_cores: probe.cpu_cores,
        },
        temp_peaks_c,
        ..Default::default()
    };
    for b in &cfg.backends {
        if let Some(req) = crate::backend_memory_requirement(cfg, b) {
            measure.requirements.insert(b.id.clone(), req);
        }
    }
    for s in &statuses {
        let mut vram: Vec<(u32, Option<u64>)> = processes
            .iter()
            .filter(|p| pid_to_backend.get(&p.pid) == Some(&s.id))
            .map(|p| (p.gpu_index, p.vram_mib))
            .collect();
        vram.sort_by_key(|(gpu, _)| *gpu);
        measure.backend_states.insert(
            s.id.clone(),
            BackendMeasured {
                state: serde_json::to_value(s.state)
                    .ok()
                    .and_then(|v| v.as_str().map(|x| x.to_string()))
                    .unwrap_or_else(|| "stopped".to_string()),
                uptime_secs: s.uptime.map(|u| u.as_secs()),
                process_vram: vram,
            },
        );
    }
    for m in &cfg.models {
        let path = crate::resolve_under_data_dir(&cfg.data_dir, &m.gguf_path);
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.is_file() {
                measure.downloaded_models.insert(m.id.clone());
                measure.model_sizes.insert(m.id.clone(), meta.len());
            }
        }
    }
    // Gateway traffic window (same store list_backends reads).
    let pairs: Vec<(String, u16)> = cfg
        .backends
        .iter()
        .map(|b| (b.id.clone(), b.port))
        .collect();
    let windows = state
        .gateway_stats
        .lock()
        .await
        .window_stats(&pairs);
    let mut errors_1h: HashMap<String, u64> = HashMap::new();
    if let Some(windows) = &windows {
        let rows: Vec<(String, u64, u64, u64)> = cfg
            .backends
            .iter()
            .filter_map(|b| {
                windows.get(&b.id).map(|w| {
                    errors_1h.insert(b.id.clone(), w.errors);
                    (b.id.clone(), w.requests, w.errors, w.tokens)
                })
            })
            .collect();
        if !rows.is_empty() {
            let window_secs = windows.values().map(|w| w.window_secs).fold(0.0, f64::max);
            measure.traffic = Some(TrafficWindow {
                window_secs,
                per_backend: rows,
            });
        }
    }

    // Measured throughput (§A.1): every configured backend's
    // current fingerprint, then the store's facts against them —
    // current entries plus superseded ones labeled as such.
    let gpu_pairs: Vec<(u32, String)> = samples
        .iter()
        .map(|s| (s.index, s.name.clone()))
        .collect();
    let mut fingerprints: HashMap<String, String> = HashMap::new();
    for b in &cfg.backends {
        fingerprints.insert(
            b.id.clone(),
            measurement_fingerprint(&fingerprint_inputs(cfg, b, &gpu_pairs)),
        );
    }
    let store = MeasurementStore::load(&measurements_path(state));
    measure.throughput = measurement_facts(&store, &fingerprints, chrono::Utc::now());

    // Layout facts per model: from serving backends' resolved
    // layouts; catalog-only facts for models no backend serves.
    let mut layouts: HashMap<String, LayoutFacts> = HashMap::new();
    for b in &cfg.backends {
        if let (Some(model), Some(l)) = (
            crate::model_for_backend(cfg, b),
            crate::resolve_weight_layout(cfg, b),
        ) {
            layouts.insert(
                model.id.clone(),
                LayoutFacts {
                    total_bytes: l.total_bytes,
                    expert_bytes: l.expert_bytes,
                    expert_bytes_by_block: l.expert_bytes_by_block.clone(),
                    moe_layers: l.moe_layers,
                    kv_per_token: l.kv_per_token,
                    basis: l.basis,
                    ctx_max: l.catalog.as_ref().map(|c| c.ctx_max).unwrap_or(0),
                    params_b: model.params_b,
                },
            );
        }
    }
    for spec in manager_models::curated_models() {
        layouts.entry(spec.id.clone()).or_insert_with(|| LayoutFacts {
            total_bytes: spec.size_bytes,
            expert_bytes: spec.expert_bytes,
            expert_bytes_by_block: None,
            moe_layers: spec.moe_layers,
            kv_per_token: None,
            basis: manager_models::MemoryBasis::Catalog,
            ctx_max: spec.ctx_max,
            params_b: spec.params_b,
        });
    }
    let mut free: Vec<(u32, u64)> = samples
        .iter()
        .map(|s| (s.index, s.vram_total_mib.saturating_sub(s.vram_used_mib)))
        .collect();
    free.sort_by_key(|(i, _)| *i);
    let live = LiveSnapshot {
        layouts: layouts.clone(),
        free_vram_mib_per_gpu: free.into_iter().map(|(_, f)| f).collect(),
        backend_running: running.clone(),
    };

    let candidates: Vec<ConsultCandidate> = cfg
        .backends
        .iter()
        .map(|b| {
            let model = crate::model_for_backend(cfg, b);
            let ctx_rank = model
                .and_then(|m| layouts.get(&m.id))
                .map(|f| f.ctx_max)
                .filter(|c| *c > 0)
                .map(|c| c as u64)
                .unwrap_or(b.flags.n_ctx as u64);
            ConsultCandidate {
                id: b.id.clone(),
                running: running.get(&b.id).copied().unwrap_or(false),
                ctx_rank,
                params_b: model.map(|m| m.params_b).unwrap_or(0.0),
                errors_1h: errors_1h.get(&b.id).copied().unwrap_or(0),
            }
        })
        .collect();

    Gathered {
        measure,
        live,
        candidates,
        running,
        fingerprints,
    }
}

fn current_config_hash(cfg: &AppConfig) -> String {
    let bytes = serde_json::to_vec(cfg).unwrap_or_default();
    config_fingerprint(&bytes)
}

// ---------------------------------------------------------------------------
// State payload
// ---------------------------------------------------------------------------

fn validation_json(v: &ItemValidation) -> Value {
    json!({
        "status": match v.status {
            ValidationStatus::Valid => "valid",
            ValidationStatus::Warning => "warning",
            ValidationStatus::Invalid => "invalid",
        },
        "reason": v.reason,
        "expected": {
            "gpu_ram_mib": v.expected.gpu_ram_mib.map(|(a, b)| json!([a, b])),
            "sys_ram_mib": v.expected.sys_ram_mib.map(|(a, b)| json!([a, b])),
            "note": v.expected.note,
        },
    })
}

async fn state_payload(state: &AppState) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let gathered = gather(state, &cfg).await;
    let (default_running, best_overall) = consult_default(&gathered.candidates);
    let consulted = default_running.clone().unwrap_or_default();
    let (context, meta) = build_context(
        &cfg,
        &gathered.measure,
        env!("CARGO_PKG_VERSION"),
        &consulted,
    );
    let file = load_plan_file(state);
    let current_hash = current_config_hash(&cfg);
    let plan_json = file.plan.as_ref().map(|plan| {
        let validations = validate_plan(plan, &cfg, &gathered.live);
        let mut v = serde_json::to_value(plan).unwrap_or(Value::Null);
        v["validations"] = Value::Array(validations.iter().map(validation_json).collect());
        v["config_stale"] = json!(plan.config_hash != current_hash);
        v["sample"] = json!(file.sample);
        v
    });
    let purpose = cfg.optimize_purpose.clone();
    // The stored plan's directive (addendum A.2), offered as the
    // next consultation field's default text; "" when no plan (or
    // a plan produced without a directive) exists.
    let last_directive = file
        .plan
        .as_ref()
        .map(|p| p.directive.clone())
        .unwrap_or_default();
    Ok(json!({
        "purpose": purpose,
        "last_directive": last_directive,
        "purpose_is_seed": purpose.trim().is_empty(),
        "purpose_seed": if purpose.trim().is_empty() {
            default_purpose_seed(&cfg)
        } else {
            String::new()
        },
        "groups": cfg.gateway.groups.iter().map(|g| json!({
            "name": g.name, "purpose": g.purpose,
        })).collect::<Vec<_>>(),
        "backends": cfg.backends.iter().map(|b| {
            let model = crate::model_for_backend(&cfg, b);
            // The current-fingerprint measurement summary, when
            // one exists (§A.1): the headline figures the Measure
            // action produced, with their age. Superseded entries
            // are context history, not the backend's figures.
            let measured = gathered.measure.throughput.iter()
                .find(|t| t.backend_id == b.id && !t.superseded)
                .map(|t| json!({
                    "prompt_per_second": t.prompt_per_second,
                    "predicted_per_second": t.predicted_per_second,
                    "ttft_ms": t.ttft_ms,
                    "age_secs": t.age_secs,
                }));
            json!({
                "id": b.id,
                "purpose": b.purpose,
                "running": gathered.running.get(&b.id).copied().unwrap_or(false),
                "enabled": b.enabled,
                "model_id": model.map(|m| m.id.clone()),
                "short_name": model.map(|m| manager_optimize::model_short_name(&cfg, m)),
                "measured": measured,
            })
        }).collect::<Vec<_>>(),
        "context_summary": {
            "gpus": meta.gpu_count,
            "backends": meta.backend_count,
            "groups": meta.group_count,
            "models": meta.model_count,
            "measured_blocks": meta.measured_blocks,
            "traffic_window_label": meta.traffic_window_label,
        },
        "context": context,
        "default_backend": default_running,
        "better_backend": match (&best_overall, &default_running) {
            (Some(best), _) if gathered.running.get(best).copied() != Some(true)
                && best_overall != default_running => {
                let model = cfg.backends.iter().find(|b| &b.id == best)
                    .and_then(|b| crate::model_for_backend(&cfg, b));
                Some(json!({
                    "id": best,
                    "short_name": model.map(|m| manager_optimize::model_short_name(&cfg, m)),
                }))
            }
            _ => None,
        },
        "plan": plan_json,
        "candidates": candidates_json(&load_candidates_file(state)),
        "consulting": state.optimize_abort.lock().await.is_some(),
        "measuring": state.optimize_job_abort.lock().await.is_some(),
    }))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Full page state: purposes, context summary + document, the
/// stored plan with live validation states.
#[tauri::command]
pub async fn optimize_get_state(state: State<'_, AppState>) -> ApiResult {
    state_payload(&state).await
}

/// Persist a purpose text (design §2.3): the deployment purpose,
/// or one group's / backend's. A scoped config save through
/// `manager_config::save`, like every other config write.
#[tauri::command]
pub async fn optimize_save_purpose(
    state: State<'_, AppState>,
    scope: String,
    target: Option<String>,
    text: String,
) -> ApiResult {
    let mut new_cfg = state.config.read().await.clone();
    match scope.as_str() {
        "deployment" => new_cfg.optimize_purpose = text,
        "backend" => {
            let target = target.unwrap_or_default();
            let b = new_cfg
                .backends
                .iter_mut()
                .find(|b| b.id == target)
                .ok_or_else(|| err(ApiError::NotFound(format!("backend {target}"))))?;
            b.purpose = text;
        }
        "group" => {
            let target = target.unwrap_or_default();
            let g = new_cfg
                .gateway
                .groups
                .iter_mut()
                .find(|g| g.name == target)
                .ok_or_else(|| err(ApiError::NotFound(format!("group {target}"))))?;
            g.purpose = text;
        }
        other => {
            return Err(err(ApiError::InvalidInput(format!(
                "unknown purpose scope '{other}'"
            ))))
        }
    }
    manager_config::save(&new_cfg, &state.config_path)
        .map_err(ApiError::from)
        .map_err(jerr)?;
    *state.config.write().await = new_cfg;
    state_payload(&state).await
}

/// Run (or sample) a consultation. Live mode consults the chosen
/// running backend directly over loopback as a tracked task with
/// the scoped cancellation handle; sample mode parses a locally
/// generated plan against the real context, with no backend
/// involved.
#[tauri::command]
pub async fn optimize_consult(
    state: State<'_, AppState>,
    backend_id: Option<String>,
    sample: Option<bool>,
    directive: Option<String>,
    candidate_repos: Option<Vec<String>>,
) -> ApiResult {
    let sample = sample.unwrap_or(false);
    // The operator directive for this consultation (addendum
    // A.2): trimmed; empty (or absent) means no directive. Sample
    // consultations record it the same way live ones do — the
    // plan records what produced it.
    let directive = directive
        .as_deref()
        .map(|d| d.trim().to_string())
        .unwrap_or_default();
    {
        let slot = state.optimize_abort.lock().await;
        if slot.is_some() {
            return Err(err(ApiError::InvalidInput(
                "a consultation is already running".to_string(),
            )));
        }
    }
    let cfg = state.config.read().await.clone();
    let mut gathered = gather(&state, &cfg).await;
    // Discovery pre-pass carriage (Phase 5c, §A.5): only a
    // consultation WITH a directive receives a candidates block,
    // and only the stored candidates gathered for that same
    // directive — the consult never re-runs discovery itself;
    // the block comes from the stored, operator-visible set,
    // intersected with the repos the operator left un-pruned
    // (`candidate_repos`; absent = all stored).
    if !directive.is_empty() {
        let stored = load_candidates_file(&state);
        if stored.directive == directive {
            gathered.measure.candidates = stored
                .candidates
                .into_iter()
                .filter(|c| {
                    candidate_repos
                        .as_ref()
                        .map(|repos| repos.iter().any(|r| r == &c.repo_id))
                        .unwrap_or(true)
                })
                .collect();
        }
    }
    let (default_running, best_overall) = consult_default(&gathered.candidates);
    let target = backend_id
        .clone()
        .or_else(|| default_running.clone())
        .or(best_overall)
        .or_else(|| cfg.backends.first().map(|b| b.id.clone()))
        .ok_or_else(|| {
            err(ApiError::InvalidInput(
                "no backends are configured".to_string(),
            ))
        })?;
    let backend = cfg
        .backends
        .iter()
        .find(|b| b.id == target)
        .ok_or_else(|| err(ApiError::NotFound(format!("backend {target}"))))?
        .clone();
    if !sample && gathered.running.get(&target).copied() != Some(true) {
        return Err(err(ApiError::InvalidInput(format!(
            "backend '{target}' is not running; consultations go to a running backend"
        ))));
    }
    let (context, meta) = build_context(
        &cfg,
        &gathered.measure,
        env!("CARGO_PKG_VERSION"),
        &target,
    );
    let purpose = if cfg.optimize_purpose.trim().is_empty() {
        default_purpose_seed(&cfg)
    } else {
        cfg.optimize_purpose.clone()
    };
    let (system_prompt, user_prompt) = build_prompts(&purpose, &directive, &context, &meta);

    // Context-fit pre-check (design §3): the assembled prompt plus
    // the response budget must fit the consulted model's context.
    if !sample {
        let est = estimate_tokens(&system_prompt) + estimate_tokens(&user_prompt);
        let n_ctx = backend.flags.n_ctx as usize;
        if est + CONSULT_MAX_TOKENS as usize > n_ctx {
            return Err(err(ApiError::InvalidInput(format!(
                "the assembled context is ~{est} tokens (estimated at 4 characters per token); with the {CONSULT_MAX_TOKENS}-token response budget it needs {} tokens, over backend '{target}'s configured context of {n_ctx} — pick a backend with a larger context",
                est + CONSULT_MAX_TOKENS as usize
            ))));
        }
    }

    let task_id = state
        .record_task(
            "optimize-consult",
            json!({ "backend": target, "sample": sample }),
        )
        .await;

    let response_text: String = if sample {
        sample_response(&cfg)
    } else {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        {
            let mut slot = state.optimize_abort.lock().await;
            if slot.is_some() {
                state
                    .finish_task(
                        &task_id,
                        TaskStatus::Failed,
                        json!({ "error": "a consultation is already running" }),
                    )
                    .await;
                return Err(err(ApiError::InvalidInput(
                    "a consultation is already running".to_string(),
                )));
            }
            *slot = Some(tx);
        }
        let model_name = crate::model_for_backend(&cfg, &backend)
            .map(|m| m.id.clone())
            .unwrap_or_else(|| backend.id.clone());
        let messages = vec![
            manager_supervisor::ChatMessage {
                role: "system".to_string(),
                content: system_prompt,
            },
            manager_supervisor::ChatMessage {
                role: "user".to_string(),
                content: user_prompt,
            },
        ];
        // Loopback by construction (design §3): the URL is a
        // literal loopback host with the backend's own port —
        // never the gateway, never the LAN address.
        let base_url = format!("http://127.0.0.1:{}", backend.port);
        let completion = manager_supervisor::chat_completion(
            &base_url,
            &model_name,
            &messages,
            CONSULT_TEMPERATURE,
            CONSULT_MAX_TOKENS,
            CONSULT_TIMEOUT,
        );
        let outcome = tokio::select! {
            res = completion => Some(res),
            _ = rx => None,
        };
        {
            let mut slot = state.optimize_abort.lock().await;
            *slot = None;
        }
        match outcome {
            None => {
                state
                    .finish_task(
                        &task_id,
                        TaskStatus::Aborted,
                        json!({ "backend": target }),
                    )
                    .await;
                return Err("consultation cancelled".to_string());
            }
            Some(Err(e)) => {
                state
                    .finish_task(
                        &task_id,
                        TaskStatus::Failed,
                        json!({ "error": e.to_string() }),
                    )
                    .await;
                return Err(err(ApiError::InvalidInput(format!(
                    "consultation failed: {e}"
                ))));
            }
            Some(Ok(result)) => result.content,
        }
    };

    // Denial memory: everything denied so far — the stored set
    // plus the outgoing plan's denied items — pre-denies matches
    // in the new plan (§6).
    let mut file = load_plan_file(&state);
    let mut denied: HashSet<String> = file.denied_fingerprints.iter().cloned().collect();
    if let Some(old) = &file.plan {
        for item in &old.items {
            if item.decision == Decision::Denied {
                denied.insert(item.fingerprint.clone());
            }
        }
    }
    let config_hash = current_config_hash(&cfg);
    let plan = match parse_plan(
        &response_text,
        &cfg,
        &target,
        &config_hash,
        context,
        &denied,
        &directive,
        chrono::Utc::now(),
    ) {
        Ok(p) => p,
        Err(e) => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "error": format!("unparseable response: {e}") }),
                )
                .await;
            return Err(err(ApiError::InvalidInput(format!(
                "the backend's response could not be parsed: {e} — the raw response is not kept; try again or consult a different backend"
            ))));
        }
    };
    for item in &plan.items {
        if item.decision == Decision::Denied {
            denied.insert(item.fingerprint.clone());
        }
    }
    let item_count = plan.items.len();
    file.plan = Some(plan);
    file.sample = sample;
    file.denied_fingerprints = denied.into_iter().collect();
    save_plan_file(&state, &file)?;
    state
        .finish_task(
            &task_id,
            TaskStatus::Done,
            json!({ "backend": target, "items": item_count }),
        )
        .await;
    state_payload(&state).await
}

/// Run the discovery pre-pass for a directive (Phase 5c,
/// §A.5 option (b)): HF API search seeded from the directive,
/// per-repo model records, and fit verdicts computed against
/// the live telemetry free-VRAM snapshot from `gather()`.
/// The result replaces the stored candidate set in
/// `<data_dir>/optimize-candidates.json`; nothing is consulted
/// and no configuration changes. A later consultation with
/// the same directive receives the (operator-pruned) set as
/// its context's `candidates` block.
#[tauri::command]
pub async fn optimize_discover(
    state: State<'_, AppState>,
    directive: String,
) -> ApiResult {
    let directive = directive.trim().to_string();
    if directive.is_empty() {
        return Err(err(ApiError::InvalidInput(
            "candidate discovery needs a directive: enter one to seed the search".to_string(),
        )));
    }
    let cfg = state.config.read().await.clone();
    let gathered = gather(&state, &cfg).await;
    let free_vram = gathered.live.free_vram_mib_per_gpu.clone();
    let retrieved_at = chrono::Utc::now().to_rfc3339();
    let client = HfClient::with_default_base()
        .map_err(|e| err(ApiError::InvalidInput(format!("candidate discovery failed: {e}"))))?;
    let candidates = client
        .discover(&directive, &free_vram, &retrieved_at)
        .await
        .map_err(|e| err(ApiError::InvalidInput(format!("candidate discovery failed: {e}"))))?;
    save_candidates_file(
        &state,
        &CandidatesFile {
            directive,
            retrieved_at,
            candidates,
        },
    )?;
    state_payload(&state).await
}

/// Cancel the running consultation (design §1.2): fires the
/// scoped abort handle, which drops the in-flight request.
#[tauri::command]
pub async fn optimize_cancel_consult(state: State<'_, AppState>) -> ApiResult {
    let sender = {
        let mut slot = state.optimize_abort.lock().await;
        slot.take()
    };
    match sender {
        Some(tx) => {
            let _ = tx.send(());
            Ok(json!({ "cancelled": true }))
        }
        None => Ok(json!({ "cancelled": false })),
    }
}

/// Measure one running backend (Phase 5a, §A.1): the
/// standardized benchmark — one discarded warm-up then three
/// measured runs per prompt, short then long, sequentially —
/// against the backend's loopback port, under the shared
/// GPU-job slot with scoped cancellation. The result replaces
/// any entry under the same config fingerprint in the
/// measurement store; the backend is never started, stopped,
/// or reconfigured.
#[tauri::command]
pub async fn optimize_measure_backend(
    state: State<'_, AppState>,
    backend_id: String,
) -> ApiResult {
    {
        let slot = state.optimize_job_abort.lock().await;
        if slot.is_some() {
            return Err(err(ApiError::InvalidInput(
                "a measurement or experiment job is already running".to_string(),
            )));
        }
    }
    let cfg = state.config.read().await.clone();
    let backend = cfg
        .backends
        .iter()
        .find(|b| b.id == backend_id)
        .ok_or_else(|| err(ApiError::NotFound(format!("backend {backend_id}"))))?
        .clone();
    let gathered = gather(&state, &cfg).await;
    if gathered.running.get(&backend_id).copied() != Some(true) {
        return Err(err(ApiError::InvalidInput(format!(
            "backend '{backend_id}' is not running; measurements run against a running backend"
        ))));
    }
    let fingerprint = gathered
        .fingerprints
        .get(&backend_id)
        .cloned()
        .ok_or_else(|| err(ApiError::NotFound(format!("backend {backend_id}"))))?;
    let model_id = crate::model_for_backend(&cfg, &backend)
        .map(|m| m.id.clone())
        .unwrap_or_else(|| {
            backend
                .model_file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| backend.id.clone())
        });
    let task_id = state
        .record_task(
            "optimize-measure",
            json!({ "backend": backend_id, "fingerprint": fingerprint }),
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
            return Err(err(ApiError::InvalidInput(
                "a measurement or experiment job is already running".to_string(),
            )));
        }
        *slot = Some(tx);
    }
    // Loopback by construction, as with consultations (§3).
    let base_url = format!("http://127.0.0.1:{}", backend.port);
    let run_model = model_id.clone();
    let run_backend = backend_id.clone();
    let run_fingerprint = fingerprint.clone();
    let run = async move {
        let mut results: Vec<BenchResult> = Vec::new();
        for prompt in benchmark_prompts() {
            let messages = vec![manager_supervisor::ChatMessage {
                role: "user".to_string(),
                content: prompt.text.to_string(),
            }];
            // Warm-up, discarded (§A.1): first-run effects after
            // a model load are not the configuration's steady
            // state.
            manager_supervisor::benchmark_completion(
                &base_url,
                &run_model,
                &messages,
                BENCHMARK_TEMPERATURE,
                prompt.max_tokens,
                MEASURE_RUN_TIMEOUT,
            )
            .await
            .map_err(|e| e.to_string())?;
            let mut runs: Vec<BenchRun> = Vec::new();
            for _ in 0..MEASURED_RUNS {
                let sample = manager_supervisor::benchmark_completion(
                    &base_url,
                    &run_model,
                    &messages,
                    BENCHMARK_TEMPERATURE,
                    prompt.max_tokens,
                    MEASURE_RUN_TIMEOUT,
                )
                .await
                .map_err(|e| e.to_string())?;
                runs.push(BenchRun {
                    prompt_per_second: sample.prompt_per_second,
                    predicted_per_second: sample.predicted_per_second,
                    ttft_ms: sample.ttft_ms,
                    latency_ms: sample.latency_ms,
                });
            }
            results.push(BenchResult::from_runs(runs));
        }
        let mut iter = results.into_iter();
        Ok::<MeasurementEntry, String>(MeasurementEntry {
            fingerprint: run_fingerprint,
            backend_id: run_backend,
            model_id: run_model,
            measured_at: chrono::Utc::now().to_rfc3339(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            short: iter.next().unwrap_or_else(|| BenchResult::from_runs(Vec::new())),
            long: iter.next().unwrap_or_else(|| BenchResult::from_runs(Vec::new())),
        })
    };
    let outcome = tokio::select! {
        res = run => Some(res),
        _ = rx => None,
    };
    {
        let mut slot = state.optimize_job_abort.lock().await;
        *slot = None;
    }
    let entry = match outcome {
        None => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Aborted,
                    json!({ "backend": backend_id }),
                )
                .await;
            return Err("measurement cancelled".to_string());
        }
        Some(Err(e)) => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "error": e.clone() }),
                )
                .await;
            return Err(err(ApiError::InvalidInput(format!(
                "measurement failed: {e}"
            ))));
        }
        Some(Ok(entry)) => entry,
    };
    let mut store = MeasurementStore::load(&measurements_path(&state));
    store.upsert(entry);
    store.save(&measurements_path(&state))?;
    state
        .finish_task(
            &task_id,
            TaskStatus::Done,
            json!({ "backend": backend_id, "fingerprint": fingerprint }),
        )
        .await;
    state_payload(&state).await
}

/// Cancel the running GPU-bound Optimize job (a measurement,
/// or an experiment batch in a later sub-phase): fires the
/// shared job slot's abort handle, which drops the job's
/// in-flight benchmark future.
#[tauri::command]
pub async fn optimize_cancel_job(state: State<'_, AppState>) -> ApiResult {
    let sender = {
        let mut slot = state.optimize_job_abort.lock().await;
        slot.take()
    };
    match sender {
        Some(tx) => {
            let _ = tx.send(());
            Ok(json!({ "cancelled": true }))
        }
        None => Ok(json!({ "cancelled": false })),
    }
}

/// Set one plan item's tri-state decision (§5.2) and persist it.
#[tauri::command]
pub async fn optimize_set_item_state(
    state: State<'_, AppState>,
    item_id: usize,
    decision: String,
) -> ApiResult {
    let decision = match decision.as_str() {
        "accepted" => Decision::Accepted,
        "denied" => Decision::Denied,
        "undecided" => Decision::Undecided,
        other => {
            return Err(err(ApiError::InvalidInput(format!(
                "unknown decision '{other}'"
            ))))
        }
    };
    let mut file = load_plan_file(&state);
    let plan = file
        .plan
        .as_mut()
        .ok_or_else(|| err(ApiError::NotFound("no plan".to_string())))?;
    let item = plan
        .items
        .iter_mut()
        .find(|i| i.id == item_id)
        .ok_or_else(|| err(ApiError::NotFound(format!("plan item {item_id}"))))?;
    if item.kind.is_advice() {
        return Err(err(ApiError::InvalidInput(
            "advice items have no decision".to_string(),
        )));
    }
    item.decision = decision;
    if decision == Decision::Denied {
        let fp = item.fingerprint.clone();
        if !file.denied_fingerprints.contains(&fp) {
            file.denied_fingerprints.push(fp);
        }
    }
    save_plan_file(&state, &file)?;
    state_payload(&state).await
}

/// Record an apply outcome on one item (Phase 4 bookkeeping,
/// §5.2): the UI orchestrator reports each item as it executes.
#[tauri::command]
pub async fn optimize_set_item_outcome(
    state: State<'_, AppState>,
    item_id: usize,
    outcome: String,
    error: Option<String>,
) -> ApiResult {
    let outcome = match outcome.as_str() {
        "applied" => manager_optimize::ApplyOutcome::Applied,
        "failed" => manager_optimize::ApplyOutcome::Failed,
        "not_attempted" => manager_optimize::ApplyOutcome::NotAttempted,
        other => {
            return Err(err(ApiError::InvalidInput(format!(
                "unknown outcome '{other}'"
            ))))
        }
    };
    let mut file = load_plan_file(&state);
    let plan = file
        .plan
        .as_mut()
        .ok_or_else(|| err(ApiError::NotFound("no plan".to_string())))?;
    let item = plan
        .items
        .iter_mut()
        .find(|i| i.id == item_id)
        .ok_or_else(|| err(ApiError::NotFound(format!("plan item {item_id}"))))?;
    item.apply_outcome = Some(outcome);
    item.apply_error = if outcome == manager_optimize::ApplyOutcome::Failed {
        error
    } else {
        None
    };
    save_plan_file(&state, &file)?;
    state_payload(&state).await
}

/// Re-validate the stored plan against live state. Validation
/// states are derived on every state read, so this simply
/// returns the refreshed payload.
#[tauri::command]
pub async fn optimize_validate_plan(state: State<'_, AppState>) -> ApiResult {
    state_payload(&state).await
}

/// Discard the stored plan and its decisions (§1.2). Denial
/// memory is kept: those were explicit operator decisions.
#[tauri::command]
pub async fn optimize_discard_plan(state: State<'_, AppState>) -> ApiResult {
    let mut file = load_plan_file(&state);
    file.plan = None;
    file.sample = false;
    save_plan_file(&state, &file)?;
    state_payload(&state).await
}

// ---------------------------------------------------------------------------
// Sample mode (design Phase 2): a locally generated plan against
// the real context and real ids — no backend is consulted.
// ---------------------------------------------------------------------------

fn sample_response(cfg: &AppConfig) -> String {
    let mut items: Vec<Value> = Vec::new();
    if let Some(b) = cfg.backends.first() {
        items.push(json!({
            "kind": "backend.set_context", "target": b.id,
            "change": { "n_ctx": b.flags.n_ctx.saturating_mul(2).min(262144) },
            "rationale": "Sample item: doubling the context doubles the KV cache — the expected effect on the row is computed from the model's real geometry.",
            "expected_effect": "Larger context window", "confidence": "medium",
        }));
        items.push(json!({
            "kind": "backend.set_flags", "target": b.id,
            "change": { "n_batch": 1024 },
            "rationale": "Sample item: a larger batch raises prompt-processing throughput at some memory cost.",
            "expected_effect": "Faster prompt processing", "confidence": "low",
        }));
    }
    if let Some(g) = cfg.gateway.groups.first() {
        items.push(json!({
            "kind": "group.set_strategy", "target": g.name,
            "change": { "strategy": "least-busy" },
            "rationale": "Sample item: least-busy spreads requests across the group's members instead of shuffling.",
            "expected_effect": "Better load spread", "confidence": "medium",
        }));
    }
    if let Some(b) = cfg.backends.iter().find(|b| !b.enabled) {
        items.push(json!({
            "kind": "backend.set_enabled", "target": b.id,
            "change": { "enabled": true },
            "rationale": "Sample item: this backend is disabled; enabling it adds serving capacity to its group.",
            "expected_effect": "More capacity", "confidence": "low",
        }));
    }
    items.push(json!({
        "kind": "model.add", "target": "sample-model",
        "change": { "source_url": "https://example.com/models/sample-model-Q4_K_M.gguf", "quant": "Q4_K_M" },
        "rationale": "Sample item: model.add registers a model without downloading it — the warning on the row is standing, and the download stays a Models-view act.",
        "expected_effect": "Model registered", "confidence": "low",
    }));
    if let Some(b) = cfg.backends.last() {
        items.push(json!({
            "kind": "backend.delete", "target": b.id, "change": {},
            "rationale": "Sample item: deletes start denied. Flip to Accepted only if you mean it; applying stops the backend first, then deletes it.",
            "expected_effect": "Backend removed", "confidence": "low",
        }));
    }
    items.push(json!({
        "kind": "advice", "target": null,
        "change": { "text": "Sample advice item: advice renders as text only and never executes. Items whose payload the app cannot validate are demoted to advice with their reasoning preserved." },
        "rationale": "", "expected_effect": "", "confidence": "low",
    }));
    json!({
        "summary": "Sample plan — generated locally; no backend was consulted.",
        "items": items,
    })
    .to_string()
}
