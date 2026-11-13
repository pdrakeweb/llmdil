//! The Optimize state payload, predicted rows, and the measurement
//! lookup / delete operations.

use super::*;

// ---------------------------------------------------------------------------
// State payload
// ---------------------------------------------------------------------------

pub(crate) fn validation_json(v: &ItemValidation) -> Value {
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

/// The predicted throughput of a plan item's RESULTING
/// configuration (predicted-throughput design §9): the saved
/// backend with the item's change applied, resolved through the
/// same fingerprint path as every other surface and answered by
/// the store's fingerprint-only lookup. Only the flag-changing
/// kinds carry one — context/batch (`set_context`, `set_flags`),
/// offload (`set_offload`), model assignment (`set_model`) —
/// because they are the items whose destination configuration
/// the measurement key describes; every other kind renders no
/// figure (principle 13). The change application mirrors the
/// plan validator's own simulation and the lookup resolver's
/// rule: a value the item does not change is never rewritten, so
/// the resolved fingerprint is the one the applied configuration
/// will actually carry.
pub(super) fn plan_item_predicted(
    cfg: &AppConfig,
    item: &PlanItem,
    store: &MeasurementStore,
    gpu_pairs: &[(u32, String)],
) -> Option<Value> {
    let target = item.target.as_ref()?;
    let saved = cfg.backends.iter().find(|b| &b.id == target)?;
    let mut merged = saved.clone();
    let change = &item.change;
    let get_u32 = |key: &str| change.get(key).and_then(Value::as_u64).map(|n| n as u32);
    match item.kind {
        PlanKind::BackendSetContext | PlanKind::BackendSetFlags => {
            if let Some(n) = get_u32("n_ctx") {
                merged.flags.n_ctx = n;
            }
            if let Some(n) = get_u32("n_batch") {
                merged.flags.n_batch = n;
            }
        }
        PlanKind::BackendSetOffload => {
            let layers = get_u32("offload_layers")?;
            if let Some(info) = crate::backend_offload_info(cfg, &merged) {
                if layers != info.layers {
                    merged.raw_flags = manager_models::set_expert_offload(
                        merged.raw_flags.as_deref(),
                        info.offload_for(layers),
                    );
                }
            }
            // The change's expert-cache value rides the same
            // merged clone (a 0 removes the token — the writer's
            // disabled form), so the predicted fingerprint prices
            // the configuration the item would actually save.
            if let Some(mib) = get_u32("moe_cache_mib") {
                merged.raw_flags =
                    manager_models::set_moe_cache(merged.raw_flags.as_deref(), Some(mib));
            }
        }
        PlanKind::BackendSetModel => {
            let model_id = change.get("model_id").and_then(Value::as_str)?;
            let model = cfg.models.iter().find(|m| m.id == model_id)?;
            merged.model_file = model.gguf_path.clone();
        }
        _ => return None,
    }
    let inputs = fingerprint_inputs(cfg, &merged, gpu_pairs);
    let fingerprint = measurement_fingerprint(&inputs);
    store.lookup(&fingerprint).map(predicted_json)
}

pub(crate) async fn state_payload(state: &AppState) -> ApiResult {
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
    let file = load_plan_file_current(state).await;
    let current_hash = current_config_hash(&cfg);
    // Plan-item predictions (design §9) resolve against the
    // measurement store and the telemetry GPU set, through the
    // same inputs the card payload resolves (op_measurement_lookup
    // fetches the pair list the same way).
    let measure_store = MeasurementStore::load(&measurements_path(state));
    let gpu_pairs: Vec<(u32, String)> = {
        let telemetry = state.telemetry().await;
        let guard = crate::lock_telemetry(&telemetry);
        guard
            .snapshot()
            .iter()
            .map(|s| (s.index, s.name.clone()))
            .collect()
    };
    let plan_json = file.plan.as_ref().map(|plan| {
        let validations = validate_plan(plan, &cfg, &gathered.live);
        let mut v = serde_json::to_value(plan).unwrap_or(Value::Null);
        let mut validation_values: Vec<Value> = validations.iter().map(validation_json).collect();
        for (item, value) in plan.items.iter().zip(validation_values.iter_mut()) {
            if let Some(p) = plan_item_predicted(&cfg, item, &measure_store, &gpu_pairs) {
                value["predicted"] = p;
            }
        }
        v["validations"] = Value::Array(validation_values);
        // Experiment validations (Phase 5e), aligned with
        // `plan.experiments`: light checks (candidate
        // resolvable, flags sane, stopping ids exist), in the
        // same validation shape as the items'. The candidates
        // block this plan's consultation carried supplies the
        // extra resolvable ids (`repo_id/file` forms).
        let extra_candidates: Vec<String> = plan
            .context
            .get("candidates")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| {
                        let repo = c.get("repo_id").and_then(|x| x.as_str())?;
                        let file = c.get("file").and_then(|x| x.as_str())?;
                        Some(format!("{repo}/{file}"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        v["experiment_validations"] = Value::Array(
            plan.experiments
                .iter()
                .map(|e| {
                    // Plan-aware: a candidate the plan itself
                    // registers through its provisioning items is
                    // resolvable even though the current config
                    // does not register it yet.
                    let ev = manager_optimize::validate_experiment_for_plan(
                        e,
                        &cfg,
                        &extra_candidates,
                        &plan.items,
                        Some(&gathered.live.layouts),
                    );
                    json!({
                        "status": match ev.status {
                            ValidationStatus::Valid => "valid",
                            ValidationStatus::Warning => "warning",
                            ValidationStatus::Invalid => "invalid",
                        },
                        "reason": ev.reason,
                        "expected": {
                            "gpu_ram_mib": Value::Null,
                            "sys_ram_mib": Value::Null,
                            "note": Value::Null,
                        },
                    })
                })
                .collect(),
        );
        v["config_stale"] = json!(plan.config_hash != current_hash);
        v["sample"] = json!(file.sample);
        v
    });
    // Current-fingerprint estimate summaries (tracker #56),
    // composed with the same headline rule as the measured
    // figures but carried under their own key and basis: the
    // page renders them as "Estimate", and they never enter
    // `gathered.measure.throughput` (the consultation facts).
    let store = MeasurementStore::load(&measurements_path(state));
    let now_utc = chrono::Utc::now();
    let estimated_by_backend: HashMap<String, Value> = cfg
        .backends
        .iter()
        .filter_map(|b| {
            let fp = gathered.fingerprints.get(&b.id)?;
            let e = store.current_estimate(&b.id, fp)?;
            let estimated_at = chrono::DateTime::parse_from_rfc3339(&e.estimated_at)
                .map(|t| t.with_timezone(&chrono::Utc))
                .unwrap_or(now_utc);
            let age_secs = now_utc
                .signed_duration_since(estimated_at)
                .num_seconds()
                .max(0) as u64;
            Some((
                b.id.clone(),
                json!({
                    "prompt_per_second": e.long.prompt_per_second.or(e.short.prompt_per_second),
                    "predicted_per_second": e.short.predicted_per_second.or(e.long.predicted_per_second),
                    "ttft_ms": e.short.ttft_ms.or(e.long.ttft_ms),
                    "age_secs": age_secs,
                }),
            ))
        })
        .collect();
    // The Measurements history (predicted-throughput design
    // §10): every stored entry with its v2 provenance, each
    // annotated with the configured backends whose prediction
    // currently resolves to it — the fingerprint-only lookup
    // winner for the backend's current fingerprint. The winner
    // is identified by position in the store so duplicate
    // (fingerprint, timestamp) entries annotate exactly the
    // entry lookup surfaces.
    let winner_index: HashMap<String, usize> = cfg
        .backends
        .iter()
        .filter_map(|b| {
            let fp = gathered.fingerprints.get(&b.id)?;
            let winner = store.lookup(fp)?;
            let idx = store.entries.iter().position(|e| std::ptr::eq(e, winner))?;
            Some((b.id.clone(), idx))
        })
        .collect();
    let measurements_json: Vec<Value> = store
        .entries
        .iter()
        .enumerate()
        .map(|(idx, e)| {
            let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
            let current_for: Vec<String> = winner_index
                .iter()
                .filter(|(_, i)| **i == idx)
                .map(|(id, _)| id.clone())
                .collect();
            v["current_for"] = json!(current_for);
            v
        })
        .collect();
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
                "estimated": estimated_by_backend.get(&b.id).cloned(),
                // The backend's current configuration in the
                // history view's terms (§10): its fingerprint
                // and readable inputs, so an entry's differing
                // fields can be named against it.
                "current_fingerprint": gathered.fingerprints.get(&b.id),
                "current_inputs": gathered.input_sets.get(&b.id)
                    .map(|i| serde_json::to_value(MeasurementInputs::from_fingerprint_inputs(i))
                        .unwrap_or(Value::Null)),
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
        "measurements": measurements_json,
        "candidates": candidates_json(&load_candidates_file(state)),
        "eval_baseline": crate::optimize_eval::eval_baseline_json(state, &gathered.fingerprints),
        "consulting": state.optimize_abort.lock().await.is_some(),
        "consult_started_unix_ms": *state.optimize_consult_started_unix_ms.lock().await,
        // Streaming progress of the running consultation (Phase
        // 3): chunk (≈ token) count, partial answer size, time to
        // first token; null when none is streaming.
        "consult_progress": state.optimize_live.progress_json(),
        // The Rust apply executor is running (Phase 3).
        "applying": state.optimize_live.applying(),
        "consult_error": state.optimize_consult_error.lock().await.as_ref().map(|(m, _)| m.clone()),
        "consult_error_unix_ms": state.optimize_consult_error.lock().await.as_ref().map(|(_, t)| *t),
        "measuring": state.optimize_job_abort.lock().await.is_some(),
        // The running Estimate/Measure job's descriptor and the
        // last such job's failure (tracker #69), following the
        // consult_* pattern above: any page instance renders the
        // true job kind/backend/elapsed and surfaces failures
        // without having started (or awaited) the job itself.
        "job": job_json(&state.optimize_job.lock().await.clone()),
        "job_error": state.optimize_job_error.lock().await.as_ref().map(|(m, _)| m.clone()),
        "job_error_unix_ms": state.optimize_job_error.lock().await.as_ref().map(|(_, t)| *t),
        // Per-backend running Optimize operations (tracker #70):
        // the mutual-exclusion record the start refusals name,
        // exposed so any page instance can see which operation
        // holds a backend.
        "running_ops": running_ops_json(state),
        // Per-backend tuning sweeps (Addendum B §B.2): the
        // running sweep's progress record and the most recent
        // finished sweep, so the page renders sweep state the
        // same way it renders `consulting` and `measuring`.
        "tuning": crate::optimize_tuning::tuning_payload(state),
    }))
}

// ---------------------------------------------------------------------------
// Predicted throughput (predicted-throughput design §5–§7)
// ---------------------------------------------------------------------------

/// The JSON shape of one resolved measurement, shared by the
/// lookup's exact/near results and the `predicted` field of a
/// list_backends row (§7: same shape): the headline figures
/// composed the one way every surface composes them, plus the
/// provenance the surfaces disclose — measured date, source,
/// app version, driver and llama build (§5.5), and the inputs
/// snapshot the tooltip and history name (§4.3).
pub(crate) fn predicted_json(entry: &MeasurementEntry) -> Value {
    let figures = headline_figures(entry);
    json!({
        "prompt_per_second": figures.prompt_per_second,
        "predicted_per_second": figures.predicted_per_second,
        "ttft_ms": figures.ttft_ms,
        "measured_at": entry.measured_at,
        "source": entry.source,
        "backend_id": entry.backend_id,
        "origin_backend_id": entry.origin_backend_id,
        "model_id": entry.model_id,
        "model_file": entry.model_file,
        "app_version": entry.app_version,
        "driver_version": entry.driver_version,
        "llama_build": entry.llama_build,
        "inputs": entry.inputs,
    })
}

/// The draft a `measurement_lookup` request carries (design
/// §6.2): the form's full draft state, not a delta. The Edit
/// Flags form sends `backend_id` and merges over the saved
/// backend; the Add Backend form sends `model_id` and resolves
/// the configuration creation would build. `parallel_slots`
/// null is the field's Auto state (no token), not an absence;
/// `raw_flags` present is the advanced editor's text (§6.2,
/// tracker #60) and supersedes the structured flag fields.
#[derive(Debug, Deserialize)]
pub(crate) struct MeasurementLookupRequest {
    pub backend_id: Option<String>,
    pub model_id: Option<String>,
    pub n_ctx: Option<u32>,
    pub n_batch: Option<u32>,
    pub offload_layers: Option<u32>,
    pub mtp_position: Option<u32>,
    pub parallel_slots: Option<i64>,
    pub gpu_indices: Option<Vec<u32>>,
    pub raw_flags: Option<String>,
}

/// Resolve a lookup draft to the backend configuration it
/// describes — the one resolver for the form line, shared by
/// every surface that resolves a draft (§11). The Edit form
/// path clones the saved backend and applies each structured
/// field ONLY where the draft differs from the saved
/// configuration's decoded state: a save op would rewrite its
/// tokens canonically, so applying a rewrite for an unchanged
/// value would fingerprint a string no save produced (a
/// catalog-default offload position, an MTP length llama.cpp
/// defaults, a `-np` spelling) and miss the measurement the
/// card shows for the saved configuration. A changed value
/// goes through the same pure transformation its save op
/// uses, so the resolved fingerprint is the saved
/// configuration's fingerprint the moment the draft is saved.
pub(super) async fn resolve_lookup_backend(
    state: &std::sync::Arc<AppState>,
    cfg: &AppConfig,
    req: &MeasurementLookupRequest,
) -> Result<manager_config::BackendConfig, ApiError> {
    if let Some(backend_id) = &req.backend_id {
        let saved = cfg
            .backends
            .iter()
            .find(|b| &b.id == backend_id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {backend_id}")))?;
        let mut merged = saved.clone();
        if let Some(n) = req.n_ctx {
            merged.flags.n_ctx = n;
        }
        if let Some(n) = req.n_batch {
            merged.flags.n_batch = n;
        }
        if let Some(raw) = &req.raw_flags {
            // Advanced mode (§6.2): the text is the single
            // source of truth; the structured fields are inert.
            merged.raw_flags = if raw.trim().is_empty() {
                None
            } else {
                Some(raw.clone())
            };
        } else {
            if let Some(layers) = req.offload_layers {
                if let Some(info) = crate::backend_offload_info(cfg, &merged) {
                    if layers != info.layers {
                        let target = info.offload_for(layers);
                        merged.raw_flags =
                            manager_models::set_expert_offload(merged.raw_flags.as_deref(), target);
                    }
                }
            }
            if let Some(pos) = req.mtp_position {
                if let Some(info) = crate::backend_mtp_info(cfg, &merged) {
                    let decoded = if info.enabled {
                        info.n_max
                            .unwrap_or(manager_models::LLAMA_CPP_N_MAX_DEFAULT)
                    } else {
                        0
                    };
                    if pos != decoded {
                        merged.raw_flags = manager_models::set_mtp_draft(
                            merged.raw_flags.as_deref(),
                            if pos == 0 {
                                manager_models::MtpDraft::Off
                            } else {
                                manager_models::MtpDraft::On(Some(pos))
                            },
                        );
                    }
                }
            }
            let saved_slots = crate::backend_parallel_info(&merged).slots;
            if req.parallel_slots != saved_slots {
                merged.raw_flags = manager_models::set_parallel_slots(
                    merged.raw_flags.as_deref(),
                    req.parallel_slots,
                );
            }
        }
        return Ok(merged);
    }
    let model_id = req.model_id.as_deref().ok_or_else(|| {
        ApiError::InvalidInput("measurement_lookup needs a backend_id or a model_id".to_string())
    })?;
    let model = cfg
        .models
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| ApiError::NotFound(format!("model {model_id}")))?;
    // The configuration creation would build (op_backend_add):
    // the form's context/batch or the creation defaults, the
    // placement from the GPU selection or the telemetry
    // assignment, the model's default offload as the raw-flags
    // baseline.
    let (tensor_split, split_mode) = match &req.gpu_indices {
        Some(selected) => {
            let store = state.telemetry().await;
            let inventory = crate::lock_telemetry(&store).inventory();
            let indices: Vec<u32> = inventory.iter().map(|g| g.index).collect();
            crate::split_for_gpu_selection(&indices, selected)?
        }
        None => match crate::propose_new_backend_split(state, model_id).await {
            Some((split, mode)) => (split, mode),
            None => (Vec::new(), manager_config::SplitMode::None),
        },
    };
    let mut backend = manager_config::BackendConfig {
        id: String::new(),
        enabled: true,
        model_file: model.gguf_path.clone(),
        port: 0,
        flags: manager_config::ServerFlags {
            n_ctx: req.n_ctx.unwrap_or(32768),
            n_batch: req.n_batch.unwrap_or(512),
            tensor_split,
            split_mode,
        },
        raw_flags: crate::model_default_raw_flags_at(&cfg.data_dir, model),
        flags_advanced: false,
        restart_policy: manager_config::RestartPolicy {
            max_retries: 5,
            backoff_base_secs: 2,
        },
        overrides_global: false,
        tool_call_probe: None,
        purpose: String::new(),
    };
    if let Some(raw) = &req.raw_flags {
        backend.raw_flags = if raw.trim().is_empty() {
            None
        } else {
            Some(raw.clone())
        };
    } else {
        if let Some(layers) = req.offload_layers {
            if let Some(info) = crate::backend_offload_info(cfg, &backend) {
                let target = info.offload_for(layers);
                backend.raw_flags =
                    manager_models::set_expert_offload(backend.raw_flags.as_deref(), target);
            }
        }
        if let Some(slots) = req.parallel_slots {
            backend.raw_flags =
                manager_models::set_parallel_slots(backend.raw_flags.as_deref(), Some(slots));
        }
    }
    Ok(backend)
}

/// Resolve a form draft and answer with its exact and nearest
/// near measurement (design §6.2): the fingerprint is computed
/// through the same resolution path the Measure action uses,
/// so the form, the card, and the consultation state the same
/// numbers for the same configuration.
pub async fn op_measurement_lookup(
    state: &std::sync::Arc<crate::AppState>,
    _sink: &std::sync::Arc<dyn crate::EventSink>,
    request: Value,
) -> crate::ApiResult {
    let req: MeasurementLookupRequest = serde_json::from_value(request)
        .map_err(|e| ApiError::InvalidInput(format!("invalid measurement_lookup request: {e}")))?;
    let cfg = state.config.read().await.clone();
    let resolved = resolve_lookup_backend(state, &cfg, &req).await?;
    let gpu_pairs: Vec<(u32, String)> = {
        let store = state.telemetry().await;
        let guard = crate::lock_telemetry(&store);
        guard
            .snapshot()
            .iter()
            .map(|s| (s.index, s.name.clone()))
            .collect()
    };
    let inputs = fingerprint_inputs(&cfg, &resolved, &gpu_pairs);
    let fingerprint = measurement_fingerprint(&inputs);
    let store = MeasurementStore::load(&measurements_path(state));
    let exact = store.lookup(&fingerprint);
    let near = if exact.is_none() {
        let model_id = crate::model_for_backend(&cfg, &resolved)
            .map(|m| m.id.clone())
            .unwrap_or_else(|| {
                resolved
                    .model_file
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        let (sha, size) = entry_identity(&inputs);
        let target = MeasurementInputs::from_fingerprint_inputs(&inputs);
        store
            .near_match(&model_id, sha.as_deref(), size, &target)
            .map(|(entry, diffs)| {
                let mut v = predicted_json(entry);
                v["differs"] = json!(diffs);
                v
            })
    } else {
        None
    };
    Ok(json!({
        "fingerprint": fingerprint,
        "exact": exact.map(predicted_json),
        "near": near,
    }))
}

/// Delete one stored measurement (design §10), identified by
/// (fingerprint, measured_at). Deleting the entry a prediction
/// resolves to removes the prediction at the next payload.
pub async fn op_measurement_delete(
    state: &std::sync::Arc<crate::AppState>,
    _sink: &std::sync::Arc<dyn crate::EventSink>,
    fingerprint: String,
    measured_at: String,
) -> crate::ApiResult {
    let mut store = MeasurementStore::load(&measurements_path(state));
    if !store.delete(&fingerprint, &measured_at) {
        return Err(ApiError::NotFound(
            "measurement entry not found".to_string(),
        ));
    }
    store
        .save(&measurements_path(state))
        .map_err(ApiError::InvalidInput)?;
    Ok(json!({
        "deleted": true,
        "fingerprint": fingerprint,
        "measured_at": measured_at,
    }))
}
