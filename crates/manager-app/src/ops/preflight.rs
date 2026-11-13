//! Backend start preflight and the VRAM start gate.

use crate::*;

/// Gates a start/restart: the backend must be configured and enabled,
/// its model must not be mid-download or mid-verification, must not
/// carry a recorded SHA-256 verification failure, and — when it has a
/// real pinned hash — must have passed verification at least once
/// (`verified_at` set). The llama-server binary and model file must
/// exist. Errors name the missing piece and the fix, so a first Start
/// after a skipped wizard step explains itself.
pub(crate) async fn backend_start_preflight(
    state: &AppState,
    id: &str,
    allow_wddm_overcommit: bool,
) -> Result<(), ApiError> {
    let cfg = state.config.read().await.clone();
    let b = cfg
        .backends
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    backend_start_preflight_for(state, &cfg, b, allow_wddm_overcommit).await
}

/// The preflight itself, against an explicit config + backend
/// rather than a lookup in the saved configuration. The Optimize
/// experiment runner gates its temporary backends through this:
/// a temp backend never enters the saved config, but its spawn
/// must pass the same gate as any app-mediated start
/// (code-review B-2) — most importantly the offload guardrail,
/// evaluated against the cloned config that carries the temp
/// backend.
pub(crate) async fn backend_start_preflight_for(
    state: &AppState,
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
    allow_wddm_overcommit: bool,
) -> Result<(), ApiError> {
    if !b.enabled {
        return Err(ApiError::InvalidInput(format!(
            "backend {} is disabled in the configuration; enable it before starting",
            b.id
        )));
    }
    reap_downloads(state).await;
    let resolved_model = resolve_under_data_dir(&cfg.data_dir, &b.model_file);
    if let Some(m) = cfg
        .models
        .iter()
        .find(|m| resolve_under_data_dir(&cfg.data_dir, &m.gguf_path) == resolved_model)
    {
        if state.downloads.handles.lock().await.contains_key(&m.id) {
            return Err(ApiError::InvalidInput(format!(
                "model {} is still downloading; a backend started now would load a truncated file",
                m.id
            )));
        }
        if state
            .downloads
            .verifying
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&m.id)
        {
            return Err(ApiError::InvalidInput(format!(
                "SHA-256 verification is still running for model {}.",
                m.id
            )));
        }
        // A recorded verification failure gates start until the model
        // is re-downloaded or passes a manual verify: the file that
        // came down did not match its pinned hash.
        if let Some(outcome) = state.downloads.outcomes.lock().await.get(&m.id) {
            if outcome.verification_failure {
                return Err(ApiError::InvalidInput(outcome.failed.clone().unwrap_or_else(
                    || {
                        format!(
                            "model {} has not passed SHA-256 verification — open Models and run Verify.",
                            m.id
                        )
                    },
                )));
            }
        }
        // Durable gate: a model with a real pinned hash that has never
        // passed verification (verified_at unset) must not start even
        // across app restarts, where no in-memory outcome survives.
        // Wizard-downloaded models land here too: the wizard's
        // DownloadModels step verifies the hash in-step but never
        // stamps verified_at, so their first start asks for a
        // Models > Verify pass — the remediation in the message.
        if is_real_pin(&m.sha256) && m.verified_at.is_none() && resolved_model.exists() {
            return Err(ApiError::InvalidInput(format!(
                "model {} has not passed SHA-256 verification — open Models and run Verify.",
                m.id
            )));
        }
    }
    // Offload guardrail (tracker #37): a saved position a placed
    // GPU's restriction blocks refuses the start unless a journaled
    // override covers exactly that position on exactly that
    // placement. Callers pass a cloned config and hold no config
    // guard, so the telemetry accessor (which reads the config
    // while initializing) is safe to call.
    {
        let store = state.telemetry().await;
        let gpus = lock_telemetry(&store).inventory();
        drop(store);
        let journal = activation::load_journal(&state.data_dir);
        if let Some(eval) = evaluate_backend_offload(cfg, b, &gpus, &journal) {
            if let Some(reason) = offload_block_reason(
                &eval.restrictions,
                eval.layers,
                eval.moe_layers,
                &eval.placed_ccs,
                eval.cpu_only,
                eval.overridden,
            ) {
                return Err(ApiError::InvalidInput(reason));
            }
        }
    }
    // Only the program path is consumed here, so the spec is built
    // without GPU inventory (its env/args are discarded).
    let spec = backend_spec(cfg, b, &[]);
    if !spec.program.exists() {
        return Err(ApiError::InvalidInput(format!(
            "llama-server is not installed (missing {}) — run the setup wizard to install llama.cpp",
            spec.program.display()
        )));
    }
    let model_path = if b.model_file.is_absolute() {
        b.model_file.clone()
    } else {
        cfg.data_dir.join(&b.model_file)
    };
    if !model_path.exists() {
        return Err(ApiError::InvalidInput(format!(
            "model file is missing ({}) — download it in the Models view",
            model_path.display()
        )));
    }
    vram_start_gate(state, cfg, b, allow_wddm_overcommit).await
}

/// One placed GPU's measured memory state, as the start gate sees
/// it: a single telemetry snapshot taken at start time.
pub(crate) struct VramGateReading {
    pub(crate) index: u32,
    pub(crate) total_mib: u64,
    pub(crate) free_mib: u64,
    /// The GPU was absent from the latest successful telemetry
    /// poll after having been reported before: the store retains
    /// it flagged down with last-known readings. A down GPU is
    /// not a full GPU — it is a device that stopped responding.
    pub(crate) down: bool,
    /// The GPU's Windows driver model from the same telemetry
    /// snapshot, when reported: WDDM GPUs can page an over-total
    /// requirement to system RAM; TCC GPUs cannot (Pete, Oct 23,
    /// 2026). `None` (unreported) is judged as hard-bounded.
    pub(crate) driver_model: Option<DriverModel>,
}

/// Why the start gate refuses one GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VramGateClass {
    /// The requirement exceeds the card's total memory: no state
    /// of the machine lets this configuration start on this GPU
    /// at this context size.
    ExceedsTotal,
    /// The requirement fits the card but not its measured free
    /// memory.
    InsufficientFree,
    /// The device itself appears offline or not responding: it
    /// is flagged down in telemetry, missing from a snapshot
    /// that reads other GPUs, or reporting zero total memory.
    /// Distinct from a memory shortage — the remedy is the GPU,
    /// not the configuration.
    GpuUnavailable,
}

/// One GPU on which a backend cannot start.
pub(crate) struct VramGateFailure {
    pub(crate) index: u32,
    pub(crate) class: VramGateClass,
    pub(crate) required_mib: u64,
    /// The free MiB the gate compared against: measured free plus
    /// the starting backend's own credited residency when it is
    /// already running (a restart frees its current weights before
    /// the new spawn allocates, so its own hold is not a shortage).
    /// Zero for a GPU with no usable reading.
    pub(crate) free_mib: u64,
    pub(crate) total_mib: u64,
    /// The failing GPU's driver model, when telemetry reported
    /// one: an [`VramGateClass::ExceedsTotal`] failure on a WDDM
    /// GPU is confirmable (see [`failures_are_wddm_overcommit`]).
    pub(crate) driver_model: Option<DriverModel>,
    /// MiB that will page to system RAM: the over-total excess
    /// on a WDDM GPU; 0 otherwise.
    pub(crate) paged_mib: u64,
}

/// Whether every failure is an over-total excess on a WDDM GPU —
/// the one failure shape an operator confirmation can allow:
/// WDDM overcommits VRAM by paging the excess to system RAM,
/// while TCC allocations are hard-bounded by physical VRAM
/// (Pete, Oct 23, 2026). Any other failure (a TCC or unreported
/// GPU over total, insufficient free memory under the total, an
/// unavailable GPU) makes the set non-confirmable as a whole.
pub(crate) fn failures_are_wddm_overcommit(failures: &[VramGateFailure]) -> bool {
    !failures.is_empty()
        && failures.iter().all(|f| {
            f.class == VramGateClass::ExceedsTotal && f.driver_model == Some(DriverModel::Wddm)
        })
}

/// The requirement's itemized breakdown in the gate's refusal
/// text — weights, KV cache, state cache, expert cache — so the
/// numbers a refusal cites are the numbers the fit surfaces
/// itemize (the expert cache included, when one is set). Terms
/// with no figure are omitted, never zero-filled.
pub(crate) fn requirement_breakdown(req: &manager_models::MemoryRequirement) -> String {
    let mut terms: Vec<String> = vec![format!("weights {} MiB", req.gpu_weights_mib)];
    if let Some(kv) = req.kv_mib {
        terms.push(format!("KV cache {kv} MiB"));
    }
    if let Some(rs) = req.rs_mib {
        terms.push(format!("state cache {rs} MiB"));
    }
    if req.moe_cache_mib > 0 {
        terms.push(format!("expert cache {} MiB", req.moe_cache_mib));
    }
    terms.join(" + ")
}

/// The pure start-gate math. Every placed GPU is judged first
/// for availability: a GPU flagged down, reporting zero total
/// memory, or missing from a non-empty reading set is
/// unavailable — no memory figure from it can be trusted,
/// whatever the requirement. (An empty reading set means
/// telemetry has nothing at all; the caller does not invoke the
/// gate then, and this function judges nothing either.) A
/// measurable GPU then fails on its requirement share exceeding
/// its effective free memory (measured free plus any credited
/// residency for that GPU); a zero requirement never fails the
/// memory math.
pub(crate) fn vram_gate_failures(
    placed: &[u32],
    required: &[(u32, u64)],
    readings: &[VramGateReading],
    credit: &[(u32, u64)],
) -> Vec<VramGateFailure> {
    let mut out = Vec::new();
    for &index in placed {
        let req = required
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, v)| *v)
            .unwrap_or(0);
        let reading = readings.iter().find(|r| r.index == index);
        let unavailable = match reading {
            Some(r) => r.down || r.total_mib == 0,
            // Missing from a snapshot that reads other GPUs:
            // unavailable. Missing because telemetry has nothing
            // at all: not judged (the skip posture).
            None => !readings.is_empty(),
        };
        if unavailable {
            out.push(VramGateFailure {
                index,
                class: VramGateClass::GpuUnavailable,
                required_mib: req,
                free_mib: 0,
                total_mib: reading.map(|r| r.total_mib).unwrap_or(0),
                driver_model: reading.and_then(|r| r.driver_model),
                paged_mib: 0,
            });
            continue;
        }
        let Some(r) = reading else {
            continue;
        };
        if req == 0 {
            continue;
        }
        let credited = credit
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, v)| *v)
            .unwrap_or(0);
        let free = r.free_mib.saturating_add(credited);
        if req > free {
            let class = if req > r.total_mib {
                VramGateClass::ExceedsTotal
            } else {
                VramGateClass::InsufficientFree
            };
            out.push(VramGateFailure {
                index,
                class,
                required_mib: req,
                free_mib: free,
                total_mib: r.total_mib,
                driver_model: r.driver_model,
                paged_mib: if class == VramGateClass::ExceedsTotal
                    && r.driver_model == Some(DriverModel::Wddm)
                {
                    req - r.total_mib
                } else {
                    0
                },
            });
        }
    }
    out
}

/// Measured free-VRAM gate (planner-moe load-failure diagnosis,
/// Oct 20, 2026): refuse a start whose per-GPU memory requirement
/// — the same requirement the backend cards display
/// ([`backend_memory_requirement`]), distributed over the
/// placement by the configured tensor split
/// ([`manager_optimize::residency_per_gpu`], the distribution the
/// fit evaluator uses) — exceeds the free VRAM measured on a
/// placed GPU at start time. Before this gate the preflight read
/// no memory figures at all, so an oversized backend started and
/// burned ~10 minutes in llama.cpp's fit-retry loop before
/// failing. The check is a single snapshot; nothing is polled or
/// cached across starts.
///
/// Skips when nothing can be measured or judged: CPU-only or
/// undetermined placement, an unresolvable memory requirement,
/// or a wholly empty telemetry snapshot (the fit evaluator's
/// "no telemetry, no fit claim" posture). A placed GPU missing
/// from a snapshot that does read other GPUs is NOT a skip: the
/// device was enumerated before and is gone, or never answered —
/// the gate refuses it as unavailable (see [`VramGateClass`]).
pub(crate) async fn vram_start_gate(
    state: &AppState,
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
    allow_wddm_overcommit: bool,
) -> Result<(), ApiError> {
    let placed: Vec<u32> = match backend_gpu_visibility(b) {
        GpuVisibility::Gpus(indices) => indices,
        _ => return Ok(()),
    };
    let Some(req) = backend_memory_requirement(cfg, b) else {
        return Ok(());
    };
    // Telemetry snapshot first, supervisor status second; neither
    // lock is held while the other is taken (the order the other
    // preflight readers follow).
    let mut samples = {
        let store = state.telemetry().await;
        let snapshot = lock_telemetry(&store).snapshot();
        snapshot
    };
    samples.sort_by_key(|s| s.index);
    if samples.is_empty() {
        return Ok(());
    }
    // Per-GPU requirement shares, aligned to the snapshot order
    // (split fraction position ↔ snapshot position is the
    // placement projection's convention).
    let per_gpu = manager_optimize::residency_per_gpu(&req, &b.flags.tensor_split, samples.len());
    let share_of = |index: u32| -> u64 {
        samples
            .iter()
            .position(|s| s.index == index)
            .map(|pos| per_gpu[pos])
            .unwrap_or(0)
    };
    let required: Vec<(u32, u64)> = placed.iter().map(|&i| (i, share_of(i))).collect();
    // Availability is judged even when every share is zero (a
    // stale split distributes nothing, but the spawn still
    // selects the placed devices): the readings carry each
    // sample's down flag through for the gate math to classify.
    let readings: Vec<VramGateReading> = samples
        .iter()
        .map(|s| VramGateReading {
            index: s.index,
            total_mib: s.vram_total_mib,
            free_mib: s.free_vram_mib(),
            down: s.down,
            driver_model: s.driver_model,
        })
        .collect();
    let statuses: Vec<(String, ProcessState)> = {
        let sup = state.supervisor().await.lock().await;
        sup.status().into_iter().map(|s| (s.id, s.state)).collect()
    };
    let running: std::collections::HashSet<&str> = statuses
        .iter()
        .filter(|(_, st)| matches!(st, ProcessState::Running | ProcessState::Starting))
        .map(|(id, _)| id.as_str())
        .collect();
    // A restart's own current residency is credited back: the
    // running process frees its weights before the replacement
    // allocates. The credit basis is the configured split applied
    // to the current requirement — the placement projection's
    // credit basis, for the same reason (no launched-split record
    // exists separate from the configured one).
    let credit: Vec<(u32, u64)> = if running.contains(b.id.as_str()) {
        required.clone()
    } else {
        Vec::new()
    };
    let failures = vram_gate_failures(&placed, &required, &readings, &credit);
    if failures.is_empty() {
        return Ok(());
    }
    // WDDM overcommit (Pete, Oct 23, 2026): when every failure is
    // an over-total excess on a WDDM GPU, the start is allowed
    // once the operator has confirmed it (the card's two-click
    // Start) — the excess pages to system RAM. Without the
    // confirmation the gate refuses, naming the paged amount, so
    // an unconfirmed caller (REST, a stale UI) never starts a
    // paging backend silently. TCC and unreported GPUs never
    // reach this branch: their failures are not all-WDDM.
    if failures_are_wddm_overcommit(&failures) {
        if allow_wddm_overcommit {
            return Ok(());
        }
        let breakdown = requirement_breakdown(&req);
        let parts: Vec<String> = failures
            .iter()
            .map(|f| {
                format!(
                    "GPU {} needs {} MiB ({}), which exceeds the GPU's total memory ({} MiB; {} MiB free) at this context size — {} MiB will page to system RAM on this WDDM GPU",
                    f.index, f.required_mib, breakdown, f.total_mib, f.free_mib, f.paged_mib
                )
            })
            .collect();
        return Err(ApiError::InvalidInput(format!(
            "backend {} cannot start without confirmation: {} — confirm the start to page the excess to system RAM, or reduce the context size or the model footprint before starting",
            b.id,
            parts.join("; ")
        )));
    }
    // Name the running app-managed backends placed on a failing
    // GPU: the known consumers of the memory that is not free.
    let consumers_on = |index: u32| -> Vec<String> {
        cfg.backends
            .iter()
            .filter(|o| o.id != b.id && running.contains(o.id.as_str()))
            .filter(|o| {
                matches!(backend_gpu_visibility(o), GpuVisibility::Gpus(ref idxs) if idxs.contains(&index))
            })
            .map(|o| o.id.clone())
            .collect()
    };
    let mut parts: Vec<String> = Vec::new();
    for f in &failures {
        match f.class {
            VramGateClass::GpuUnavailable => {
                parts.push(format!(
                    "GPU {} appears offline or not responding — telemetry has no usable reading for it",
                    f.index
                ));
            }
            VramGateClass::ExceedsTotal => {
                parts.push(format!(
                    "GPU {} needs {} MiB ({}), which exceeds the GPU's total memory ({} MiB; {} MiB free) at this context size",
                    f.index,
                    f.required_mib,
                    requirement_breakdown(&req),
                    f.total_mib,
                    f.free_mib
                ));
            }
            VramGateClass::InsufficientFree => {
                let mut part = format!(
                    "GPU {} needs {} MiB but only {} MiB is free (of {} MiB) — insufficient free memory on the GPU",
                    f.index, f.required_mib, f.free_mib, f.total_mib
                );
                let consumers = consumers_on(f.index);
                if consumers.len() == 1 {
                    part += &format!("; backend '{}' is running on this GPU", consumers[0]);
                } else if !consumers.is_empty() {
                    let named: Vec<String> = consumers.iter().map(|c| format!("'{c}'")).collect();
                    part += &format!("; backends {} are running on this GPU", named.join(", "));
                }
                parts.push(part);
            }
        }
    }
    let any_unavailable = failures
        .iter()
        .any(|f| f.class == VramGateClass::GpuUnavailable);
    let remedy = if any_unavailable {
        "check the GPU (power, seating, driver) and try again; if the GPU is healthy, reduce the context size or the model footprint, or free the GPU, before starting"
    } else {
        "reduce the context size or the model footprint, or free the GPU, before starting"
    };
    Err(ApiError::InvalidInput(format!(
        "backend {} cannot start: {} — {}",
        b.id,
        parts.join("; "),
        remedy
    )))
}

#[cfg(test)]
mod tests;
