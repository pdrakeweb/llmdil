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
    build_context, build_prompts, config_fingerprint, consult_default, default_purpose_seed,
    estimate_tokens, parse_plan, probe_hardware, validate_plan, BackendMeasured, ConsultCandidate,
    Decision, HardwareContext, ItemValidation, LayoutFacts, LiveSnapshot, MeasureInputs, Plan,
    TrafficWindow, ValidationStatus,
};
use manager_supervisor::ProcessState;

use crate::{err, jerr, ApiError, ApiResult, AppState, TaskStatus};

/// Consultation generation budget (design §3).
const CONSULT_MAX_TOKENS: u32 = 4096;
const CONSULT_TEMPERATURE: f32 = 0.2;
const CONSULT_TIMEOUT: Duration = Duration::from_secs(900);

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

// ---------------------------------------------------------------------------
// Input gathering: everything the pure core needs from the live app
// ---------------------------------------------------------------------------

struct Gathered {
    measure: MeasureInputs,
    live: LiveSnapshot,
    candidates: Vec<ConsultCandidate>,
    running: HashMap<String, bool>,
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
    Ok(json!({
        "purpose": purpose,
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
            json!({
                "id": b.id,
                "purpose": b.purpose,
                "running": gathered.running.get(&b.id).copied().unwrap_or(false),
                "enabled": b.enabled,
                "model_id": model.map(|m| m.id.clone()),
                "short_name": model.map(|m| manager_optimize::model_short_name(&cfg, m)),
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
        "consulting": state.optimize_abort.lock().await.is_some(),
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
) -> ApiResult {
    let sample = sample.unwrap_or(false);
    {
        let slot = state.optimize_abort.lock().await;
        if slot.is_some() {
            return Err(err(ApiError::InvalidInput(
                "a consultation is already running".to_string(),
            )));
        }
    }
    let cfg = state.config.read().await.clone();
    let gathered = gather(&state, &cfg).await;
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
    let (system_prompt, user_prompt) = build_prompts(&purpose, &context, &meta);

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
