//! Bench (Bench design, approved Oct 23, 2026): server-driven
//! configuration sweeps against temporary `bench-<n>` backends,
//! a prompt library with shipped defaults, planner-generated
//! prompts through the consultation transport, and a standalone
//! results store. The pure logic (sweep types, expansion and
//! validation pruning, comparability keys, spec log parsing,
//! presets, winner computation) lives in the `manager-bench`
//! crate; this module is the app glue: ops, stores, the runner,
//! the GPU interlock, and per-arm preflight.
//!
//! Standing rules from the design that this module enforces:
//! - Bench backends are supervisor-registered temporaries on the
//!   experiment-runner pattern: never in `config.json`, never in
//!   gateway groups, loopback only, guaranteed cleanup, and a
//!   startup sweep for crash leftovers (§6.4, D5).
//! - A sweep starts only when no backend sharing a GPU (or the
//!   CPU-offload resource) with the target is running; stopping
//!   a conflicting backend takes an explicit operator-confirmed
//!   list, and Bench never restarts what it stopped (D6, §6.1).
//! - The 20-minute sweep figure is a TARGET (D14 as reconciled
//!   Oct 23, 2026): the projection warns, nothing refuses and
//!   nothing stops a sweep for exceeding it. Durations are
//!   recorded and reported. The operator's stop is cancellation
//!   (D13), which is real: a scoped handle owned by the sweep,
//!   not the task bookkeeping.
//! - Results live in their own store and never feed Optimize
//!   (D10): nothing here writes the measurement store or any
//!   Optimize state.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use manager_bench as mb;
use manager_config::{AppConfig, BackendConfig};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{ApiError, AppState, TaskStatus};

mod arms;
mod generate;
mod sweep;

use arms::*;
pub use generate::*;
use sweep::*;

// ---------------------------------------------------------------------------
// Shared job state (AppState fields)
// ---------------------------------------------------------------------------

/// Live progress of the running sweep, surfaced through
/// `bench_results_list` (the results payload carries it on the
/// running sweep's record).
#[derive(Debug, Clone, Serialize)]
pub struct BenchProgress {
    pub arm_index: usize,
    pub arm_count: usize,
    pub phase: String,
    pub run: usize,
    pub started_unix_ms: i64,
}

/// The one running sweep (Bench runs one at a time, §6.4).
#[derive(Debug, Clone)]
pub struct BenchJob {
    pub sweep_id: String,
    pub progress: BenchProgress,
}

// ---------------------------------------------------------------------------
// Paths and stores
// ---------------------------------------------------------------------------

fn prompts_path(state: &AppState) -> PathBuf {
    state.data_dir.join("bench-prompts.json")
}

fn results_path(state: &AppState) -> PathBuf {
    state.data_dir.join("bench-results.json")
}

fn journal_path(state: &AppState) -> PathBuf {
    state.data_dir.join("bench-journal.json")
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Load the prompt library, materializing the shipped defaults
/// (first read creates the file; unedited shipped entries
/// refresh when the app ships newer versions, §4.1).
fn load_prompts(state: &AppState) -> mb::PromptLibrary {
    let mut lib = mb::PromptLibrary::load(&prompts_path(state)).unwrap_or_default();
    if lib.materialize_shipped() {
        if let Err(e) = lib.save(&prompts_path(state)) {
            tracing::warn!("bench: prompt library save failed: {e}");
        }
    }
    lib
}

fn load_results(state: &AppState) -> mb::BenchResults {
    mb::BenchResults::load(&results_path(state)).unwrap_or_default()
}

fn save_results(state: &AppState, results: &mb::BenchResults) {
    if let Err(e) = results.save(&results_path(state)) {
        tracing::warn!("bench: results save failed: {e}");
    }
}

fn prompts_payload(lib: &mb::PromptLibrary) -> Value {
    json!({ "prompts": lib.entries })
}

fn results_payload(state: &AppState, results: &mb::BenchResults) -> Value {
    let job = state
        .bench_job
        .try_lock()
        .ok()
        .and_then(|guard| (*guard).clone());
    let sweeps: Vec<Value> = results
        .sweeps
        .iter()
        .map(|s| {
            let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
            if let (Some(job), Some(obj)) = (&job, v.as_object_mut()) {
                if obj.get("id").and_then(|i| i.as_str()) == Some(job.sweep_id.as_str())
                    && obj.get("status").and_then(|s| s.as_str()) == Some("running")
                {
                    obj.insert(
                        "progress".to_string(),
                        serde_json::to_value(&job.progress).unwrap_or(Value::Null),
                    );
                }
            }
            v
        })
        .collect();
    json!({ "sweeps": sweeps })
}

// ---------------------------------------------------------------------------
// Journal (<data_dir>/bench-journal.json) — crash-leftover record
// for the startup sweep, on the experiment journal's pattern.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BenchJournal {
    #[serde(default)]
    active: Vec<ActiveBench>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActiveBench {
    temp_id: String,
    #[serde(default)]
    pid: Option<u32>,
    port: u16,
    sweep_id: String,
    started_at: String,
}

impl BenchJournal {
    fn load(path: &PathBuf) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &PathBuf) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| e.to_string())
    }

    fn add_active(&mut self, rec: ActiveBench) {
        self.active.retain(|a| a.temp_id != rec.temp_id);
        self.active.push(rec);
    }

    fn set_active_pid(&mut self, temp_id: &str, pid: u32) {
        if let Some(a) = self.active.iter_mut().find(|a| a.temp_id == temp_id) {
            a.pid = Some(pid);
        }
    }

    fn remove_active(&mut self, temp_id: &str) -> bool {
        let before = self.active.len();
        self.active.retain(|a| a.temp_id != temp_id);
        self.active.len() != before
    }
}

fn load_journal(state: &AppState) -> BenchJournal {
    BenchJournal::load(&journal_path(state))
}

fn save_journal(state: &AppState, journal: &BenchJournal) {
    if let Err(e) = journal.save(&journal_path(state)) {
        tracing::warn!("bench: journal save failed: {e}");
    }
}

/// Is this supervisor/config id one of Bench's temporaries?
fn is_bench_temp_id(id: &str) -> bool {
    id.starts_with("bench-")
}

// ---------------------------------------------------------------------------
// Startup sweep: remove leftovers of sweeps interrupted by an
// app crash (design §6.4, experiment §A.3 step 5 pattern).
// ---------------------------------------------------------------------------

pub async fn startup_sweep(state: &AppState) {
    // Defensive config scrub: bench temporaries are never
    // written to the config, but a leftover entry must never be
    // started or routed as a real backend.
    {
        // Hold the write lock across clone, save and install, like
        // every other config mutator: a read-clone-release-save let a
        // concurrent startup save (catalog seeding) land in between
        // and be overwritten by this stale clone (finding PS-2,
        // docs/code-review-v0.1.60.md).
        let mut live = state.config.write().await;
        let cfg = live.clone();
        if cfg.backends.iter().any(|b| is_bench_temp_id(&b.id)) {
            let mut new_cfg = cfg.clone();
            let removed: Vec<String> = new_cfg
                .backends
                .iter()
                .filter(|b| is_bench_temp_id(&b.id))
                .map(|b| b.id.clone())
                .collect();
            new_cfg.backends.retain(|b| !is_bench_temp_id(&b.id));
            for g in new_cfg.gateway.groups.iter_mut() {
                g.members.retain(|m| !removed.contains(m));
            }
            for m in new_cfg.models.iter_mut() {
                m.assigned_backends.retain(|b| !removed.contains(b));
            }
            if manager_config::save(&new_cfg, &state.config_path).is_ok() {
                *live = new_cfg;
                tracing::info!(
                    "bench: startup sweep removed leftover temp backends from config: {}",
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
                    Ok(()) => tracing::info!(
                        "bench: startup sweep terminated leftover {} (pid {pid})",
                        record.temp_id
                    ),
                    Err(e) => tracing::warn!(
                        "bench: startup sweep could not terminate {} (pid {pid}): {e}",
                        record.temp_id
                    ),
                }
            }
        }
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
    // A sweep record left "running" by the crash is partial by
    // definition; its measured arms are kept (D13's shape).
    let mut results = load_results(state);
    let mut changed = false;
    for sweep in results.sweeps.iter_mut() {
        if sweep.status == "running" {
            sweep.status = "partial".to_string();
            sweep.partial = true;
            sweep.ended_ms = Some(now_ms());
            for arm in sweep.arms.iter_mut() {
                if arm.status == mb::ArmStatus::Running || arm.status == mb::ArmStatus::Pending {
                    arm.status = mb::ArmStatus::Cancelled;
                    arm.detail = Some(
                        "sweep interrupted by an app restart; this arm never completed".to_string(),
                    );
                }
            }
            changed = true;
        }
    }
    if changed {
        save_results(state, &results);
    }
}

// ---------------------------------------------------------------------------
// Prompt library ops (§7)
// ---------------------------------------------------------------------------

pub async fn op_bench_prompts_list(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
) -> crate::ApiResult {
    let lib = load_prompts(state);
    Ok(prompts_payload(&lib))
}

pub async fn op_bench_prompt_save(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
    entry: Value,
) -> crate::ApiResult {
    let mut entry: mb::PromptEntry = serde_json::from_value(entry)
        .map_err(|e| ApiError::InvalidInput(format!("invalid prompt entry: {e}")))?;
    if entry.name.trim().is_empty() {
        return Err(ApiError::InvalidInput("a prompt needs a name".to_string()));
    }
    if entry.body.trim().is_empty() {
        return Err(ApiError::InvalidInput("a prompt needs a body".to_string()));
    }
    let mut lib = load_prompts(state);
    if entry.id.trim().is_empty() {
        entry.id = unique_prompt_id(&lib, &entry.name);
    }
    // A shipped entry whose text changed is the operator's from
    // here on (§4.1), whatever the client claimed: the shipped
    // original stays restorable from the compiled-in defaults.
    if let Some(existing) = lib.entries.iter().find(|e| e.id == entry.id) {
        if existing.origin == mb::PromptOrigin::Shipped
            && existing.body != entry.body
            && entry.origin == mb::PromptOrigin::Shipped
        {
            entry.origin = mb::PromptOrigin::User;
        }
    }
    entry.sha256 = mb::prompt_sha256(&entry.body);
    entry.updated_ms = now_ms();
    lib.upsert(entry);
    lib.save(&prompts_path(state))
        .map_err(|e| ApiError::InvalidInput(format!("prompt save failed: {e}")))?;
    Ok(prompts_payload(&lib))
}

pub async fn op_bench_prompt_delete(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
    id: String,
) -> crate::ApiResult {
    let mut lib = load_prompts(state);
    if !lib.remove(&id) {
        return Err(ApiError::NotFound(format!("no prompt '{id}'")));
    }
    // Deleting a shipped entry restores its shipped text on the
    // next materialization (§4.1) — that IS the restore path.
    lib.materialize_shipped();
    lib.save(&prompts_path(state))
        .map_err(|e| ApiError::InvalidInput(format!("prompt save failed: {e}")))?;
    Ok(prompts_payload(&lib))
}

fn unique_prompt_id(lib: &mb::PromptLibrary, name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let base = if slug.is_empty() {
        "prompt".to_string()
    } else {
        slug
    };
    if lib.entries.iter().all(|e| e.id != base) {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if lib.entries.iter().all(|e| e.id != candidate) {
            return candidate;
        }
    }
    unreachable!()
}

// ---------------------------------------------------------------------------
// Sweep ops (§7)
// ---------------------------------------------------------------------------

pub async fn op_bench_run_start(
    state: &Arc<AppState>,
    sink: &Arc<dyn crate::EventSink>,
    request: Value,
) -> crate::ApiResult {
    let req: SweepRequest = serde_json::from_value(request)
        .map_err(|e| ApiError::InvalidInput(format!("invalid sweep request: {e}")))?;
    // One sweep at a time (§6.4), and never alongside a
    // measurement/experiment job (they contend for the same
    // GPUs the interlock reasons about).
    if state.bench_job.lock().await.is_some() {
        return Err(ApiError::InvalidInput(
            "a bench sweep is already running".to_string(),
        ));
    }
    if state.optimize.job_abort.lock().await.is_some() {
        return Err(ApiError::InvalidInput(
            "a measurement or experiment job is already running".to_string(),
        ));
    }
    let cfg = state.config.read().await.clone();
    let target = cfg
        .backends
        .iter()
        .find(|b| b.id == req.backend_id)
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("no backend '{}'", req.backend_id)))?;
    let lib = load_prompts(state);
    let prompt = lib
        .entries
        .iter()
        .find(|p| p.id == req.prompt_id)
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("no prompt '{}'", req.prompt_id)))?;

    // Dimensions: a shipped preset (D16) or the operator's own.
    let preset = match req.preset.as_deref() {
        Some("kv") => Some(mb::preset_kv()),
        Some("spec") => Some(mb::preset_spec()),
        Some("combined") => Some(mb::preset_combined()),
        Some(other) => {
            return Err(ApiError::InvalidInput(format!(
                "unknown preset '{other}' (kv | spec | combined)"
            )))
        }
        None => None,
    };
    let dimensions: Vec<Vec<mb::ArmVariant>> = match (&preset, &req.dimensions) {
        (Some(p), _) => p.dimensions.clone(),
        (None, Some(d)) if !d.is_empty() => d.clone(),
        _ => {
            return Err(ApiError::InvalidInput(
                "a sweep needs a preset or custom dimensions".to_string(),
            ))
        }
    };
    let mut base = base_arm_spec(&target);
    if let Some(d) = &req.draft_model_id {
        base.draft_model_id = Some(d.clone());
    }
    let arms = mb::expand(&base, &dimensions);

    // GGUF facts for pruning (§3.2): trained context + vocab
    // shapes for the target and (when a draft arm exists) the
    // draft model.
    let model_entry = crate::model_for_backend(&cfg, &target).cloned();
    let model_abs = model_entry
        .as_ref()
        .map(|m| crate::resolve_under_data_dir(&cfg.data_dir, &m.gguf_path))
        .unwrap_or_else(|| crate::resolve_under_data_dir(&cfg.data_dir, &target.model_file));
    let (model_ctx_len, target_vocab) = gguf_header_facts(&model_abs);
    let draft_model_id = arms
        .iter()
        .find_map(|a| a.draft_model_id.clone())
        .or_else(|| base.draft_model_id.clone());
    let (draft_path, draft_vocab) = match &draft_model_id {
        Some(id) => {
            let entry = cfg.models.iter().find(|m| &m.id == id).ok_or_else(|| {
                ApiError::InvalidInput(format!("draft model '{id}' is not a registered model"))
            })?;
            let abs = crate::resolve_under_data_dir(&cfg.data_dir, &entry.gguf_path);
            let (_, vocab) = gguf_header_facts(&abs);
            (Some(abs.to_string_lossy().into_owned()), vocab)
        }
        None => (None, None),
    };
    // Offload guardrail facts for pruning (§3.2 rule 5): when
    // the target's model carries catalog offload restrictions,
    // the positions the guardrail blocks on the target's
    // placement are computed with the same evaluation the start
    // preflight uses, so blocked positions prune here with the
    // reason instead of failing one temp-backend start per arm.
    // A journaled override the target may carry does NOT extend
    // to bench temporaries — the preflight evaluates the temp's
    // own id, which no override names — so positions are judged
    // unoverridden, exactly as the preflight will judge them.
    // Nothing is pruned when no restriction governs the
    // placement (the allowed set then covers every position).
    let gpu_inventory = {
        let store = state.telemetry().await;
        let gpus = crate::lock_telemetry(&store).inventory();
        gpus
    };
    let offload_guardrail = crate::backend_offload_info(&cfg, &target).and_then(|info| {
        if info.restrictions.is_empty() {
            return None;
        }
        let (placed_ccs, cpu_only) = crate::placed_offload_placement(&target, &gpu_inventory);
        let allowed_layers: Vec<u32> = (0..=info.moe_layers)
            .filter(|&layers| {
                manager_models::offload_status(
                    &info.restrictions,
                    layers,
                    info.moe_layers,
                    &placed_ccs,
                    cpu_only,
                ) != manager_models::OffloadStatus::Blocked
            })
            .collect();
        if allowed_layers.len() as u32 == info.moe_layers + 1 {
            return None;
        }
        let compute_capability = placed_ccs
            .iter()
            .copied()
            .find(|cc| {
                info.restrictions
                    .iter()
                    .any(|r| r.compute_capability == *cc)
            })
            .or_else(|| info.restrictions.first().map(|r| r.compute_capability))?;
        Some(mb::OffloadGuardrail {
            moe_layers: info.moe_layers,
            allowed_layers,
            compute_capability,
            inherited_layers: Some(info.layers),
            // Arm `--n-cpu-moe` values are block-index bounds;
            // positions are MoE layers (review item 11). Sent only
            // when it differs from the identity.
            n_cpu_moe_by_position: Some(info.n_cpu_moe_by_position())
                .filter(|b| b.iter().enumerate().any(|(k, &v)| v != k as u32))
                .unwrap_or_default(),
        })
    });
    let prune_ctx = mb::PruneContext {
        model_ctx_len,
        target_vocab,
        draft_vocab,
        offload_guardrail,
    };
    let mut outcomes = mb::prune_arms(arms, &prune_ctx);
    // Glue rule (§2.1): an arm whose depth plus the measured
    // output cap cannot fit its context cannot run as defined.
    for outcome in outcomes.iter_mut() {
        if let mb::ArmOutcome::Ready(spec) = outcome {
            if spec.depth > 0 && spec.depth + mb::MEASURED_MAX_TOKENS > spec.n_ctx {
                *outcome = mb::ArmOutcome::Pruned {
                    spec: spec.clone(),
                    reason: format!(
                        "depth {} + {} output tokens exceeds n_ctx {}",
                        spec.depth,
                        mb::MEASURED_MAX_TOKENS,
                        spec.n_ctx
                    ),
                };
            }
        }
    }

    // Interlock (§6.1): conflicts must be stopped before the
    // sweep starts — either already stopped, or named in the
    // operator's confirmed stop list. Bench never stops a
    // backend on its own say-so, and never restarts one.
    // The running set is the shared liveness view (supervisor and
    // SCM alike): a service-managed backend holding the GPU counts
    // exactly like a supervised one.
    let running: HashSet<String> = crate::lifecycle::backend_liveness(state)
        .await
        .into_values()
        .filter(|l| l.state.is_live())
        .map(|l| l.id)
        .collect();
    let conflicts = interlock_conflicts(&cfg, &running, &target);
    let uncovered: Vec<&String> = conflicts
        .iter()
        .filter(|id| !req.stop_backend_ids.contains(id))
        .collect();
    if !uncovered.is_empty() {
        let names = uncovered
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ApiError::InvalidInput(format!(
            "running backends conflict with this sweep and must be stopped first: {names} [conflicts: {names}]"
        )));
    }
    let mut stopped_names: Vec<String> = Vec::new();
    for id in &conflicts {
        let opts = crate::backend_lifecycle::BackendOpOpts::new(crate::lifecycle::Origin::Bench);
        match crate::backend_lifecycle::stop_backend(state, sink, id, opts).await {
            Ok(_) => stopped_names.push(id.clone()),
            Err(e) => {
                return Err(ApiError::InvalidInput(format!(
                    "stopping backend '{id}' for the sweep failed: {e}"
                )))
            }
        }
    }

    // Draft device argument, resolved against the launch's
    // visibility restriction: the target's placed GPUs are the
    // only visible devices, so the target GPU is CUDA0 inside
    // the bench backend; a CPU placement runs the draft on CPU.
    let draft_device_arg = match crate::backend_gpu_visibility(&target) {
        crate::GpuVisibility::CpuOnly => Some("none".to_string()),
        _ => Some("CUDA0".to_string()),
    };

    // The sweep record, with every arm's snapshot (§3.1).
    let sweep_id = format!("bench-{}", now_ms());
    let ready_specs: Vec<mb::ArmSpec> = outcomes
        .iter()
        .filter_map(|o| match o {
            mb::ArmOutcome::Ready(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let projected = mb::project_minutes(&ready_specs);
    let baseline_arm_index = if req.baseline_arm_index < outcomes.len() {
        req.baseline_arm_index
    } else {
        0
    };
    let machine_gpus: Vec<String> = gpu_inventory
        .iter()
        .map(|g| format!("{} (GPU {})", g.name, g.index))
        .collect();
    let model_identity = mb::ModelIdentity {
        path: model_abs.to_string_lossy().into_owned(),
        size_bytes: std::fs::metadata(&model_abs).map(|m| m.len()).unwrap_or(0),
        sha256: model_entry.as_ref().and_then(|m| m.verified_sha256.clone()),
    };
    let mut record = mb::SweepRecord {
        id: sweep_id.clone(),
        status: "running".to_string(),
        started_ms: now_ms(),
        ended_ms: None,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        llama_build: String::new(),
        machine_gpus,
        backend_id: target.id.clone(),
        backend_name: target.id.clone(),
        model: model_identity,
        prompt_id: prompt.id.clone(),
        prompt_name: prompt.name.clone(),
        prompt_sha256: mb::prompt_sha256(&prompt.body),
        prompt_tokens: None,
        temperature: req.temperature,
        correctness_control: req.correctness_control,
        interlock: mb::InterlockRecord {
            running_before: conflicts.clone(),
            stopped_confirmed: stopped_names.clone(),
        },
        baseline_arm_index,
        projected_minutes: projected,
        partial: false,
        arms: outcomes
            .iter()
            .enumerate()
            .map(|(index, o)| {
                let spec = match o {
                    mb::ArmOutcome::Ready(s) => s.clone(),
                    mb::ArmOutcome::Pruned { spec, .. } => spec.clone(),
                };
                mb::ArmRecord {
                    index,
                    label: spec.label(),
                    spec: spec.clone(),
                    raw_flags: String::new(),
                    comparability_key: String::new(),
                    status: mb::ArmStatus::Pending,
                    detail: None,
                    runs: Vec::new(),
                    summary: None,
                    peak_vram_mib: None,
                    started_ms: None,
                    ended_ms: None,
                }
            })
            .collect(),
    };
    save_sweep(state, &record);

    let task_id = state
        .record_task(
            "bench-run",
            json!({
                "sweep_id": sweep_id,
                "backend": target.id,
                "prompt": prompt.id,
                "arms": outcomes.len(),
            }),
        )
        .await;
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    {
        let mut slot = state.bench_abort.lock().await;
        *slot = Some(tx);
    }
    state.tasks.controls.set_canceller(
        &task_id,
        crate::tasks::TaskCanceller::BenchRun {
            sweep_id: sweep_id.clone(),
        },
    );
    {
        let mut slot = state.bench_job.lock().await;
        *slot = Some(BenchJob {
            sweep_id: sweep_id.clone(),
            progress: BenchProgress {
                arm_index: 0,
                arm_count: outcomes.len(),
                phase: "launching".to_string(),
                run: 0,
                started_unix_ms: now_ms(),
            },
        });
    }
    let ctx = SweepCtx {
        cfg: cfg.clone(),
        target: target.clone(),
        model_id: model_entry
            .as_ref()
            .map(|m| m.id.clone())
            .unwrap_or_else(|| target.id.clone()),
        temperature: req.temperature,
        draft_path: draft_path.clone(),
        draft_device_arg: draft_device_arg.clone(),
    };
    let mut cancelled = execute_sweep(
        state,
        &ctx,
        &mut record,
        outcomes.clone(),
        &prompt.body,
        req.correctness_control,
        &mut rx,
    )
    .await;

    // Optional confirmation pass at temperature 0.6 (§2.1): a
    // separate record — the temperature differs, so the
    // comparability key differs — over the same arms.
    if req.confirm_at_06 && req.temperature == 0.0 && !cancelled {
        let mut confirm_record = record.clone();
        confirm_record.id = format!("{sweep_id}-confirm");
        confirm_record.temperature = mb::CONFIRM_TEMPERATURE;
        confirm_record.status = "running".to_string();
        confirm_record.started_ms = now_ms();
        confirm_record.ended_ms = None;
        confirm_record.partial = false;
        for arm in confirm_record.arms.iter_mut() {
            arm.status = mb::ArmStatus::Pending;
            arm.detail = None;
            arm.runs = Vec::new();
            arm.summary = None;
            arm.peak_vram_mib = None;
            arm.started_ms = None;
            arm.ended_ms = None;
            arm.raw_flags = String::new();
        }
        save_sweep(state, &confirm_record);
        if let Ok(mut slot) = state.bench_job.try_lock() {
            if let Some(job) = slot.as_mut() {
                job.sweep_id = confirm_record.id.clone();
            }
        }
        let confirm_ctx = SweepCtx {
            cfg: cfg.clone(),
            target: target.clone(),
            model_id: ctx.model_id.clone(),
            temperature: mb::CONFIRM_TEMPERATURE,
            draft_path: draft_path.clone(),
            draft_device_arg: draft_device_arg.clone(),
        };
        let confirm_cancelled = execute_sweep(
            state,
            &confirm_ctx,
            &mut confirm_record,
            outcomes,
            &prompt.body,
            false,
            &mut rx,
        )
        .await;
        cancelled = cancelled || confirm_cancelled;
    }

    {
        let mut slot = state.bench_abort.lock().await;
        *slot = None;
    }
    {
        let mut slot = state.bench_job.lock().await;
        *slot = None;
    }
    if cancelled {
        state
            .finish_task(
                &task_id,
                TaskStatus::Aborted,
                json!({ "sweep_id": sweep_id, "cancelled": true }),
            )
            .await;
        return Err(ApiError::InvalidInput("bench sweep cancelled".to_string()));
    }
    state
        .finish_task(&task_id, TaskStatus::Done, json!({ "sweep_id": sweep_id }))
        .await;
    let results = load_results(state);
    Ok(results_payload(state, &results))
}

pub async fn op_bench_run_cancel(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
    sweep_id: String,
) -> crate::ApiResult {
    let matches = {
        let slot = state.bench_job.lock().await;
        slot.as_ref().map(|j| j.sweep_id.clone())
    };
    let current = match matches {
        Some(id) => id,
        None => return Ok(json!({ "cancelled": false })),
    };
    // The confirmation record runs under "<id>-confirm" in the
    // same job; cancelling either id cancels the job.
    if sweep_id != current && sweep_id != current.trim_end_matches("-confirm") {
        return Ok(json!({ "cancelled": false }));
    }
    let sender = {
        let mut slot = state.bench_abort.lock().await;
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

pub async fn op_bench_results_list(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
) -> crate::ApiResult {
    let results = load_results(state);
    Ok(results_payload(state, &results))
}

pub async fn op_bench_result_delete(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
    sweep_id: String,
) -> crate::ApiResult {
    if let Some(job) = state.bench_job.lock().await.as_ref() {
        if job.sweep_id == sweep_id {
            return Err(ApiError::InvalidInput(
                "that sweep is running; cancel it before deleting".to_string(),
            ));
        }
    }
    let mut results = load_results(state);
    let before = results.sweeps.len();
    results.sweeps.retain(|s| s.id != sweep_id);
    if results.sweeps.len() == before {
        return Err(ApiError::NotFound(format!("no sweep '{sweep_id}'")));
    }
    save_results(state, &results);
    Ok(results_payload(state, &results))
}

#[cfg(test)]
mod tests;
