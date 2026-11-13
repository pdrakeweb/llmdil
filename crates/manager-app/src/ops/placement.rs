//! Placement propose / apply operations.

use crate::*;

/// Project a tensor split onto discovered VRAM (spec §14
/// `placement_propose`). The model resolves from the curated catalog
/// first, else from the config (size = the on-disk file when downloaded,
/// else a params_b estimate — see the resolution code). Free VRAM comes
/// from the latest telemetry snapshot — the same snapshot the fits
/// verdict and the payload's per-GPU `free_mib` are computed from;
/// stale GPUs count their last-known values. The response is the
/// projection plus provenance for the UI's labels: `snapshot_age_ms`
/// (age of the telemetry snapshot the projection was computed from),
/// `gpu_indices` (the GPU index each split fraction maps to), and
/// `weights_only` — true only when no backend is in scope (the
/// projection then covers model weights only, with no KV-cache or
/// compute allowance, so "fits" is not a load guarantee); with a
/// backend in scope the projection prices its full requirement
/// and `weights_only` is false.
///
/// When a backend is in scope and its process is running, the fit
/// check credits the weights that process already holds on each GPU
/// (its configured split applied to the GPU-resident weight total;
/// an empty split means the whole model on the first GPU): applying
/// the new split restarts the backend, freeing its current weights
/// first, so only the additional allocation per GPU must fit in free
/// VRAM. A stopped backend holds nothing and earns no credit. The
/// architecture records no launched-split separate from the
/// configured one, so the configured split is the credit basis —
/// the same basis the Placement form's default selection uses.
pub async fn op_placement_propose(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    model_id: String,
    split: Vec<f32>,
    backend_id: Option<String>,
) -> ApiResult {
    let spec = match manager_models::curated_models()
        .into_iter()
        .find(|s| s.id == model_id)
    {
        Some(spec) => spec,
        None => {
            // Not a catalog model: resolve the configured model. Size
            // basis, in order: the on-disk file's size when the model
            // is downloaded; else an estimate from params_b using the
            // catalog's derivation (~4.92 bits per weight plus ~5%
            // overhead — an estimate, like the catalog's own sizes).
            // The config read finishes before telemetry is touched:
            // the telemetry accessor reads the config while
            // initializing, so the two must not nest.
            let cfg = state.config.read().await;
            let m = cfg
                .models
                .iter()
                .find(|m| m.id == model_id)
                .ok_or_else(|| {
                    ApiError::NotFound(format!(
                        "model {model_id} is neither a catalog entry nor a configured model"
                    ))
                })?;
            let path = resolve_under_data_dir(&cfg.data_dir, &m.gguf_path);
            let size_bytes = match std::fs::metadata(&path) {
                Ok(md) if md.is_file() => md.len(),
                _ if m.params_b > 0.0 => (m.params_b as f64 * 1e9 * 4.92 / 8.0 * 1.05) as u64,
                _ => {
                    return Err(ApiError::InvalidInput(format!(
                        "model size unknown for {model_id}: the file is not downloaded and no params_b is configured to estimate from"
                    )))
                }
            };
            manager_models::ModelSpec {
                id: m.id.clone(),
                name: m.id.clone(),
                quant: m.quant.clone(),
                sha256: m.sha256.clone(),
                url: m.source_url.clone(),
                size_bytes,
                params_b: m.params_b,
                ctx_max: 0,
                enabled_by_default: m.enabled,
                expert_bytes: None,
                moe_layers: m.moe_layers,
                kv_bytes_per_token: None,
                default_offload_layers: m.default_offload_layers,
                offload_restrictions: Vec::new(),
                mtp: false,
            }
        }
    };
    // Backend-scoped residency: when the caller names the serving
    // backend (the backend card's Placement form does), the
    // projection prices that backend's FULL memory requirement —
    // weights, KV cache, state cache, and the expert cache
    // distributed by its own rule — so the panel's Required
    // figures are the start gate's numbers (before Oct 23, 2026
    // it projected weights only and could report "Fits" for a
    // configuration the gate refused). The system-RAM share rides
    // along in the payload. The config read finishes before
    // telemetry is touched (the telemetry accessor reads the
    // config while initializing).
    let (full_req, resident_bytes, sys_bytes, credit_cfg_split) = match &backend_id {
        Some(bid) => {
            let cfg = state.config.read().await;
            let found = cfg.backends.iter().find(|b| &b.id == bid).and_then(|b| {
                let serves = model_for_backend(&cfg, b)
                    .map(|m| m.id == model_id)
                    .unwrap_or(false);
                if serves {
                    backend_memory_requirement(&cfg, b)
                        .map(|req| (req, b.flags.tensor_split.clone()))
                } else {
                    None
                }
            });
            match found {
                Some((req, split_cfg)) => {
                    let resident = if req.sys_weights_mib > 0 {
                        Some(req.gpu_weights_mib * manager_models::MIB)
                    } else {
                        None
                    };
                    let sys = req.sys_weights_mib * manager_models::MIB;
                    (Some(req), resident, sys, Some(split_cfg))
                }
                None => (None, None, 0, None),
            }
        }
        None => (None, None, 0, None),
    };
    // The current-allocation credit applies only while the backend's
    // process actually holds the weights: running or starting. A
    // stopped backend's weights are already absent from VRAM, so the
    // telemetry free figures need no adjustment for it.
    let credit_active = match (&backend_id, &credit_cfg_split) {
        (Some(bid), Some(_)) => {
            let sup = state.supervisor().await.lock().await;
            sup.status().iter().any(|s| {
                &s.id == bid && matches!(s.state, ProcessState::Running | ProcessState::Starting)
            })
        }
        _ => false,
    };
    let store = state.telemetry().await;
    let guard = lock_telemetry(&store);
    let mut samples = guard.snapshot();
    samples.sort_by_key(|s| s.index);
    if samples.is_empty() {
        return Err(ApiError::Telemetry(
            "no telemetry yet; cannot project placement".to_string(),
        ));
    }
    // Age of the snapshot: now minus the last successful poll that fed
    // the store (falling back to the newest sample's own timestamp).
    let snapshot_age_ms = guard
        .last_successful_poll_at()
        .map(|t| t.timestamp_millis())
        .or_else(|| samples.iter().map(|s| s.ts.timestamp_millis()).max())
        .map(|then_ms| now_ms().saturating_sub(then_ms.max(0) as u64));
    let gpu_indices: Vec<u32> = samples.iter().map(|s| s.index).collect();
    // A down GPU (absent from recent polls, retained by the store
    // with last-known readings) has no usable capacity: project it as
    // zero free VRAM so no split fraction is planned onto it, and
    // name it in the payload so the UI can say why it does not fit.
    let down_indices: Vec<u32> = samples.iter().filter(|s| s.down).map(|s| s.index).collect();
    let free_mib: Vec<u64> = samples
        .iter()
        .map(|s| {
            if s.down {
                0
            } else {
                s.vram_total_mib.saturating_sub(s.vram_used_mib)
            }
        })
        .collect();
    drop(guard);
    // The credit basis, aligned to the snapshot's GPU order: the
    // backend's configured split when it covers the current GPU set;
    // an empty configured split means an unsplit launch, which loads
    // the whole model on the first GPU. A configured split recorded
    // against a different GPU count is stale and earns no credit.
    let current_split: Option<Vec<f32>> = match (&credit_cfg_split, credit_active) {
        (Some(cfg_split), true)
            if cfg_split.len() == free_mib.len() && cfg_split.iter().any(|f| *f > 0.0) =>
        {
            Some(cfg_split.clone())
        }
        (Some(cfg_split), true) if cfg_split.is_empty() && !free_mib.is_empty() => {
            let mut v = vec![0.0; free_mib.len()];
            v[0] = 1.0;
            Some(v)
        }
        _ => None,
    };
    // With a backend in scope the projection is the full
    // requirement under the PROPOSED split (the expert-cache
    // shares follow the proposed split's offloaded-layer holders,
    // exactly as residency_per_gpu distributes them for the
    // start gate); the credit is the same requirement under the
    // configured split, when the backend is running. Without a
    // backend the legacy weights-only projection stands.
    let (projection, weights_only) = match &full_req {
        Some(req) => {
            let n = samples.len();
            let required = manager_optimize::residency_per_gpu(req, &split, n);
            let current: Vec<u64> = match (&current_split, credit_active) {
                (Some(cur), true) => manager_optimize::residency_per_gpu(req, cur, n),
                _ => vec![0; n],
            };
            let totals: Vec<u64> = samples.iter().map(|s| s.vram_total_mib).collect();
            let is_wddm: Vec<bool> = samples
                .iter()
                .map(|s| s.driver_model == Some(DriverModel::Wddm))
                .collect();
            (
                manager_models::propose_placement_full(
                    &spec,
                    &split,
                    &free_mib,
                    &totals,
                    &is_wddm,
                    &required,
                    &current,
                    req.gpu_weights_mib,
                    req.kv_mib.unwrap_or(0),
                    req.rs_mib,
                    req.moe_cache_mib,
                    sys_bytes,
                )
                .map_err(ApiError::from)?,
                false,
            )
        }
        None => (
            manager_models::propose_placement_resident(
                &spec,
                &split,
                &free_mib,
                resident_bytes,
                sys_bytes,
                current_split.as_deref(),
            )
            .map_err(ApiError::from)?,
            true,
        ),
    };
    let mut payload = serde_json::to_value(&projection)?;
    payload["snapshot_age_ms"] = json!(snapshot_age_ms);
    payload["gpu_indices"] = json!(gpu_indices);
    payload["down_indices"] = json!(down_indices);
    payload["weights_only"] = json!(weights_only);
    Ok(payload)
}

/// Apply a tensor split to the backends serving a model (spec §14
/// `placement_apply`). Validates the split against the telemetry
/// snapshot's GPU count (the same parity rule `placement_propose`
/// enforces), writes it into the config of every backend the model is
/// assigned to, persists via `manager_config::save`, and completes the
/// task it records. When the model has no recorded assignment, the
/// target backends are derived from the backends whose model file
/// resolves to the model's gguf_path, and the apply records them into
/// `assigned_backends` — so a model configured by hand (or installed
/// by the wizard, which never records assignments) is still placeable.
/// With `backend_id` given, the apply is scoped to that one backend
/// (which must serve the model): the GPUs placement form works per
/// backend, so two backends serving the same model can hold different
/// splits. The split takes effect on each backend's next start. All mutation
/// happens on a clone installed in memory only after a successful
/// save, so a late failure leaves memory and disk in agreement.
pub async fn op_placement_apply(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    model_id: String,
    split: Vec<f32>,
    backend_id: Option<String>,
) -> ApiResult {
    // Target resolution runs before telemetry validation: the
    // advanced-flags refusal below must be answerable from the
    // config alone (and targets are needed either way). The
    // telemetry accessor is only touched after the config read
    // guard is dropped — it reads the config while initializing,
    // so calling it under any config lock could deadlock (the
    // same constraint model_assign documents).
    let cfg = state.config.read().await;
    let pre_cfg = cfg.clone();
    let model = cfg
        .models
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| ApiError::NotFound(format!("model {model_id}")))?;
    let backend_ids = if let Some(bid) = backend_id {
        // Backend-scoped apply (the GPUs placement form): the split
        // goes to the one selected backend, which must serve the
        // model — recorded as assigned, or its model file resolving
        // to the model's gguf_path.
        let backend = cfg
            .backends
            .iter()
            .find(|b| b.id == bid)
            .ok_or_else(|| ApiError::NotFound(format!("backend {bid}")))?;
        let model_path = resolve_under_data_dir(&cfg.data_dir, &model.gguf_path);
        let serves = model.assigned_backends.iter().any(|b| b == &bid)
            || resolve_under_data_dir(&cfg.data_dir, &backend.model_file) == model_path;
        if !serves {
            return Err(ApiError::InvalidInput(format!(
                "backend {bid} does not serve model {model_id}"
            )));
        }
        vec![bid]
    } else {
        let mut ids = model.assigned_backends.clone();
        if ids.is_empty() {
            // No recorded assignment: derive the targets from the
            // backends whose model file resolves (under the data dir)
            // to the model's gguf_path. The apply below records them
            // into assigned_backends, so later applies take the direct
            // path.
            let model_path = resolve_under_data_dir(&cfg.data_dir, &model.gguf_path);
            ids = cfg
                .backends
                .iter()
                .filter(|b| resolve_under_data_dir(&cfg.data_dir, &b.model_file) == model_path)
                .map(|b| b.id.clone())
                .collect();
        }
        ids
    };
    // Advanced flags mode (tracker #60): placement writes the
    // structured tensor split, so an apply touching an advanced
    // backend is refused whole — before telemetry validation and
    // before any task is recorded — rather than leaving the other
    // targets half-placed.
    let advanced_targets: Vec<&str> = backend_ids
        .iter()
        .filter(|bid| {
            cfg.backends
                .iter()
                .any(|b| &b.id == *bid && b.flags_advanced)
        })
        .map(|s| s.as_str())
        .collect();
    drop(cfg);
    if !advanced_targets.is_empty() {
        return Err(ApiError::InvalidInput(format!(
            "placement cannot be applied: {} in advanced flags mode; its tensor \
             split is not written by the structured placement form until \
             advanced mode is turned off (backend_set_flags_mode)",
            advanced_targets
                .iter()
                .map(|id| format!("backend {id} is"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if backend_ids.is_empty() {
        return Err(ApiError::NotFound(format!(
            "model {model_id} is not referenced by any backend; point a backend's model file at it first"
        )));
    }
    // Validate the split length against the telemetry snapshot.
    let gpu_count = lock_telemetry(&state.telemetry().await).snapshot().len();
    if gpu_count == 0 {
        return Err(ApiError::Telemetry(
            "no telemetry yet; cannot validate placement".to_string(),
        ));
    }
    if split.len() != gpu_count {
        // Same wording as manager_models::validate_split.
        return Err(ApiError::InvalidInput(format!(
            "tensor split has {} entries but there are {gpu_count} GPUs",
            split.len()
        )));
    }
    let task_id = state
        .record_task(
            "placement_apply",
            json!({ "model_id": model_id, "backends": backend_ids, "split": split }),
        )
        .await;
    let mut cfg = state.config.write().await;
    let outcome: Result<(), ApiError> = (async {
        let mut new_cfg = cfg.clone();
        // Existence first: a missing backend must not leave earlier
        // backends mutated while the command reports failure.
        for bid in &backend_ids {
            if !new_cfg.backends.iter().any(|b| &b.id == bid) {
                return Err(ApiError::NotFound(format!("backend {bid}")));
            }
        }
        for bid in &backend_ids {
            let backend = new_cfg
                .backends
                .iter_mut()
                .find(|b| &b.id == bid)
                .expect("existence checked above");
            manager_models::apply_placement(backend, &split).map_err(ApiError::from)?;
        }
        // Record the (possibly derived) assignment on the model.
        if let Some(m) = new_cfg.models.iter_mut().find(|m| m.id == model_id) {
            for bid in &backend_ids {
                if !m.assigned_backends.contains(bid) {
                    m.assigned_backends.push(bid.clone());
                }
            }
        }
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        // Journal before the save (ordering rule): each target
        // backend activates the split at its next (re)start — a
        // backend-scope plan entry per target (the gateway YAML
        // does not carry splits, so no gateway entry is created
        // for this change).
        let mut journaled = activation::JournalRecords::none();
        for bid in &backend_ids {
            let record = record_backend_change(
                state,
                &pre_cfg,
                &new_cfg,
                "backend_placement",
                format!("Backend `{bid}` tensor split changed"),
                bid,
            )
            .await;
            chain_journaled(state, &mut journaled, record)?;
        }
        // Persist — without the save, the applied split silently
        // vanished on app restart while the command reported success.
        save_journaled(state, &new_cfg, &journaled)?;
        *cfg = new_cfg;
        Ok(())
    })
    .await;
    drop(cfg);
    match outcome {
        Ok(()) => {
            for bid in &backend_ids {
                state.sync_backend_service_definition(bid).await;
            }
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Done,
                    json!({ "model_id": model_id, "backends": backend_ids }),
                )
                .await;
            Ok(json!({ "task_id": task_id, "backends": backend_ids, "persisted": true }))
        }
        Err(e) => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "model_id": model_id, "error": e.to_string() }),
                )
                .await;
            Err(e)
        }
    }
}
