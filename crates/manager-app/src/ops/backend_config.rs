//! Backend config edits: add / preview / apply / delete, flags and raw
//! flags, and the catalog / model default raw flags.

use crate::*;

/// Backend configuration by id.
pub async fn op_backend_get_config(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    let cfg = state.config.read().await;
    let b = cfg
        .backends
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    Ok(serde_json::to_value(b)?)
}

/// The fields a [`BackendPatch`] has.
const BACKEND_PATCH_FIELDS: &[&str] = &[
    "enabled",
    "model_file",
    "port",
    "n_ctx",
    "n_batch",
    "tensor_split",
    "split_mode",
    "raw_flags",
    "max_retries",
    "backoff_base_secs",
];

/// Parse a backend patch as the closed object it is documented to be: an
/// unknown field or a wrong type is the caller's error, never a silent
/// no-op. `raw_flags: null` clears the flags; any other `null` leaves its
/// field alone.
fn parse_backend_patch(patch: Value) -> Result<BackendPatch, ApiError> {
    let fields = patch
        .as_object()
        .ok_or_else(|| ApiError::InvalidInput("`patch` must be a JSON object".to_string()))?;
    for (field, value) in fields {
        if !BACKEND_PATCH_FIELDS.contains(&field.as_str()) {
            return Err(ApiError::InvalidInput(format!(
                "unknown patch field `{field}`"
            )));
        }
        // One field at a time, so the message names it.
        let one: serde_json::Map<String, Value> =
            std::iter::once((field.clone(), value.clone())).collect();
        serde_json::from_value::<BackendPatch>(Value::Object(one))
            .map_err(|e| ApiError::InvalidInput(format!("patch field `{field}`: {e}")))?;
    }
    let clear_raw_flags = fields.get("raw_flags").is_some_and(Value::is_null);
    let mut parsed: BackendPatch =
        serde_json::from_value(patch).map_err(|e| ApiError::InvalidInput(format!("patch: {e}")))?;
    if clear_raw_flags {
        parsed.raw_flags = Some(None);
    }
    Ok(parsed)
}

/// The limits the single-field operations apply to the patched fields.
fn check_backend_patch(
    backend: &manager_config::BackendConfig,
    patch: &BackendPatch,
) -> Result<(), ApiError> {
    if patch.n_ctx.is_some() || patch.n_batch.is_some() {
        validate_ctx_batch(
            patch.n_ctx.unwrap_or(backend.flags.n_ctx),
            patch.n_batch.unwrap_or(backend.flags.n_batch),
        )?;
    }
    if patch.port == Some(0) {
        return Err(ApiError::InvalidInput(
            "the port must be a whole number from 1 to 65535".to_string(),
        ));
    }
    Ok(())
}

/// Preview a config diff before applying (spec §14 `backend_preview_diff`).
pub async fn op_backend_preview_diff(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    patch: Value,
) -> ApiResult {
    let patch = parse_backend_patch(patch)?;
    let cfg = state.config.read().await;
    let backend = cfg
        .backends
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    check_backend_patch(backend, &patch)?;
    let diff: ConfigDiff = manager_config::diff_config(&id, backend, &patch);
    Ok(serde_json::to_value(&diff)?)
}

/// Apply a config patch with restart semantics (spec §14
/// `backend_apply_config`). The patch is applied to a clone of the
/// config (`apply_config_diff` validates and rolls the clone back on
/// failure), the clone is persisted via `manager_config::save`, and
/// only a successful save installs it in memory — `persisted` reports
/// that truthfully. `apply_mode` is `RestartNow | OnNextRestart`:
/// `RestartNow` starts the normal graceful restart
/// ([`backend_lifecycle::restart_backend`]) after the save and returns
/// its task as `restart_task_id`; a backend that is not running is
/// refused before anything is saved.
pub async fn op_backend_apply_config(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: String,
    patch: Value,
    apply_mode: String,
) -> ApiResult {
    let patch = parse_backend_patch(patch)?;
    let apply_mode = match apply_mode.as_str() {
        "RestartNow" => "RestartNow",
        "OnNextRestart" => "OnNextRestart",
        other => {
            return Err(ApiError::InvalidInput(format!(
                "unknown apply_mode {other:?}; expected RestartNow | OnNextRestart"
            )))
        }
    };
    if apply_mode == "RestartNow" {
        let live = lifecycle::backend_liveness(state).await;
        let known = state
            .config
            .read()
            .await
            .backends
            .iter()
            .any(|b| b.id == id);
        if known && !live.get(&id).is_some_and(|l| l.state.is_live()) {
            return Err(ApiError::InvalidInput(format!(
                "backend {id} is not running, so there is nothing to restart; \
                 use OnNextRestart (nothing was saved)"
            )));
        }
    }
    // Advanced flags mode (tracker #60): a patch touching the
    // flags form's fields (context/batch, split, raw flags) is a
    // structured flags write and is refused for an advanced
    // backend; the patch's other fields (enabled, port, model
    // file, restart policy) are not flags state and still apply.
    let touches_flags = patch.n_ctx.is_some()
        || patch.n_batch.is_some()
        || patch.tensor_split.is_some()
        || patch.split_mode.is_some()
        || patch.raw_flags.is_some();
    let diff: ConfigDiff = {
        let (mut cfg, live) = config_write_for_journal(state).await;
        let pre_cfg = cfg.clone();
        let mut new_cfg = cfg.clone();
        let backend = new_cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        if touches_flags {
            ensure_backend_not_advanced(backend)?;
        }
        check_backend_patch(backend, &patch)?;
        let diff =
            manager_config::apply_config_diff(&mut new_cfg, &id, &patch).map_err(ApiError::from)?;
        let summary = backend_change_summary(&id, &diff);
        let mut journaled = record_gateway_change(
            state,
            &live,
            &pre_cfg,
            &new_cfg,
            "backend_apply_config",
            summary.clone(),
            false,
        )
        .await?;
        // The same save is also a backend activation: whatever the
        // gateway does with it, the backend process itself only
        // picks its settings up at its next (re)start. `enabled` is
        // not one of them (no launch argument carries it).
        if diff.changes.iter().any(|c| c.field != "enabled") {
            let backend_record = record_backend_change(
                state,
                &live,
                &pre_cfg,
                &new_cfg,
                "backend_apply_config",
                summary,
                &id,
            )
            .await;
            chain_journaled(state, &mut journaled, backend_record)?;
        }
        save_journaled(state, &new_cfg, &journaled)?;
        *cfg = new_cfg;
        diff
    };
    drop_net_zero_gateway_entries(state).await;
    state.sync_backend_service_definition(&id).await;
    let restart_task = if apply_mode == "RestartNow" {
        Some(spawn_backend_restart(state, sink, &id).await)
    } else {
        None
    };
    Ok(json!({
        "backend_id": id,
        "diff": diff,
        "apply_mode": apply_mode,
        "restart_task_id": restart_task,
        "persisted": true,
    }))
}

/// Restart a backend in the background through the shared restart; the
/// returned task settles with its outcome.
async fn spawn_backend_restart(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: &str,
) -> String {
    let task_id = state
        .record_task(
            "backend_restart",
            json!({ "backend_id": id, "graceful": true }),
        )
        .await;
    let event_origin = event_bus::current_origin();
    let opts = backend_lifecycle::BackendOpOpts {
        graceful: true,
        record_task: false,
        ..backend_lifecycle::BackendOpOpts::new(match event_origin {
            event_bus::EventOrigin::Api => lifecycle::Origin::Api,
            _ => lifecycle::Origin::Ui,
        })
    };
    let (state, sink, id, task) = (
        Arc::clone(state),
        Arc::clone(sink),
        id.to_string(),
        task_id.clone(),
    );
    let restart = event_bus::with_origin(event_origin, async move {
        let (status, detail) =
            match backend_lifecycle::restart_backend(&state, &sink, &id, opts).await {
                Ok(mut v) => {
                    v["task_id"] = json!(task);
                    (TaskStatus::Done, v)
                }
                Err(e) => (
                    TaskStatus::Failed,
                    json!({ "backend_id": id, "error": e.to_string(), "error_kind": e.kind() }),
                ),
            };
        state.finish_task(&task, status, detail).await;
    });
    tokio::spawn(tracing::Instrument::in_current_span(restart));
    task_id
}

/// The plan summary of a backend settings save, naming what changed.
fn backend_change_summary(id: &str, diff: &ConfigDiff) -> String {
    match diff.changes.as_slice() {
        [c] if c.field == "enabled" => {
            let what = if c.new == json!(true) {
                "enabled"
            } else {
                "disabled"
            };
            format!("Backend `{id}` {what}")
        }
        [] => format!("Backend `{id}` settings changed"),
        changes => format!(
            "Backend `{id}` settings changed: {}",
            changes
                .iter()
                .map(|c| field_change_text(&c.field, &c.old, &c.new))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The tensor split a newly created backend for `model_id` is
/// assigned from live telemetry: existing enabled backends are
/// accounted for as they are, and the new backend is assigned a
/// split by the same algorithm the wizard's tensor-split step
/// uses. Computed before any config write lock is taken (the
/// telemetry accessor reads the config while initializing, so the
/// two must not nest). No telemetry yet (or a single GPU) yields
/// None — the split stays unset and the wizard step assigns it
/// later.
pub(crate) async fn propose_new_backend_split(
    state: &Arc<AppState>,
    model_id: &str,
) -> Option<(Vec<f32>, manager_config::SplitMode)> {
    let snap = state.config.read().await.clone();
    let vrams: Vec<u64> = {
        let store = state.telemetry().await;
        let guard = lock_telemetry(&store);
        let mut samples = guard.snapshot();
        samples.sort_by_key(|s| s.index);
        samples.iter().map(|s| s.vram_total_mib).collect()
    };
    if vrams.len() < 2 {
        return None;
    }
    let size_of = |model_file: &std::path::Path| -> u64 {
        let target = resolve_under_data_dir(&snap.data_dir, model_file);
        let model = snap
            .models
            .iter()
            .find(|m| resolve_under_data_dir(&snap.data_dir, &m.gguf_path) == target);
        match model {
            Some(m) => {
                let path = resolve_under_data_dir(&snap.data_dir, &m.gguf_path);
                match std::fs::metadata(&path) {
                    Ok(md) if md.is_file() => md.len(),
                    _ => manager_wizard::steps::estimate_model_bytes(m.params_b as f64),
                }
            }
            None => 0,
        }
    };
    let mut inputs: Vec<manager_wizard::steps::SplitInput> = snap
        .backends
        .iter()
        .filter(|b| b.enabled)
        .map(|b| manager_wizard::steps::SplitInput {
            // Account for what the backend actually puts on
            // GPUs: with expert offload, only the resident
            // share occupies VRAM.
            model_bytes: backend_memory_requirement(&snap, b)
                .map(|r| r.gpu_weights_mib * manager_models::MIB)
                .unwrap_or_else(|| size_of(&b.model_file)),
            existing: if b.flags.tensor_split.is_empty() {
                None
            } else {
                Some(b.flags.tensor_split.clone())
            },
            assign: false,
        })
        .collect();
    let new_model = snap.models.iter().find(|m| m.id == model_id);
    inputs.push(manager_wizard::steps::SplitInput {
        model_bytes: match new_model {
            Some(m) => size_of(&m.gguf_path),
            None => 0,
        },
        existing: None,
        assign: true,
    });
    manager_wizard::steps::assign_splits(&inputs, &vrams)
        .into_iter()
        .last()
        .flatten()
}

/// The tensor split for an explicit GPU selection (backend_add's
/// `gpu_indices`, the add form's Placement chips): an even split
/// across the selected GPUs, positions in telemetry index order —
/// the composition the Placement form applies when no configured
/// split covers the selection. Validation mirrors placement_apply
/// (telemetry must exist, the selection must name inventory GPUs,
/// and at least one GPU must be selected). The mode is Layer, as
/// every assigned split carries (config validation requires a mode
/// whenever a split is set).
pub(crate) fn split_for_gpu_selection(
    gpu_indices: &[u32],
    selected: &[u32],
) -> Result<(Vec<f32>, manager_config::SplitMode), ApiError> {
    if gpu_indices.is_empty() {
        return Err(ApiError::Telemetry(
            "no telemetry yet; cannot validate placement".to_string(),
        ));
    }
    if selected.is_empty() {
        return Err(ApiError::InvalidInput(
            "select at least one GPU for the split".to_string(),
        ));
    }
    for s in selected {
        if !gpu_indices.contains(s) {
            return Err(ApiError::InvalidInput(format!(
                "GPU {s} is not in the telemetry inventory"
            )));
        }
    }
    let share = 1.0 / selected.len() as f32;
    let split: Vec<f32> = gpu_indices
        .iter()
        .map(|i| if selected.contains(i) { share } else { 0.0 })
        .collect();
    Ok((split, manager_config::SplitMode::Layer))
}

/// The context/batch validation shared by backend_update_flags
/// and backend_add (the add form offers the same fields under the
/// same rules): context within the accepted range, batch between
/// 1 and the context length.
pub(crate) fn validate_ctx_batch(n_ctx: u32, n_batch: u32) -> Result<(), ApiError> {
    if !(1024..=1_048_576).contains(&n_ctx) {
        return Err(ApiError::InvalidInput(
            "context length must be between 1024 and 1048576".to_string(),
        ));
    }
    if n_batch == 0 || n_batch > n_ctx {
        return Err(ApiError::InvalidInput(
            "batch size must be between 1 and the context length".to_string(),
        ));
    }
    Ok(())
}

/// The parallel-slots validation shared by backend_set_parallel
/// and backend_add (the add form offers the same field under the
/// same rules): the Parallel slots control's range — at least one
/// slot, at most the app's ceiling (see
/// [`manager_models::parallel`]).
pub(crate) fn validate_parallel_slots(slots: u32) -> Result<(), ApiError> {
    use manager_models::parallel::{PARALLEL_SLOTS_MAX, PARALLEL_SLOTS_MIN};
    if !(PARALLEL_SLOTS_MIN..=PARALLEL_SLOTS_MAX).contains(&slots) {
        return Err(ApiError::InvalidInput(format!(
            "parallel slots must be between {PARALLEL_SLOTS_MIN} and {PARALLEL_SLOTS_MAX}"
        )));
    }
    Ok(())
}

/// The resolved offload for a backend being created: the raw flags
/// it starts with, and whether an override must be journaled for
/// the position once the creation save lands.
pub(crate) struct NewBackendOffload {
    pub(crate) raw_flags: Option<String>,
    pub(crate) record_override: bool,
    pub(crate) placed_ccs: Vec<(u32, u32)>,
}

/// Validate a chosen expert-offload position for a backend being
/// created and compute its starting raw flags (tracker #49). The
/// position goes through the same capability and guardrail
/// evaluation backend_set_offload applies to an existing backend:
/// the model must be offload-capable, the position must fit its
/// MoE layer count, and a guardrail-blocked position is refused
/// unless the operator confirmed the override in the add form. The
/// refusal wording is the creation path's own (there is no Edit
/// Flags form for a backend that does not exist yet).
pub(crate) fn resolve_new_backend_offload(
    cfg: &AppConfig,
    backend: &manager_config::BackendConfig,
    gpus: &[GpuInfo],
    journal: &activation::ActivationJournal,
    layers: u32,
    allow_override: bool,
) -> Result<NewBackendOffload, ApiError> {
    let info = backend_offload_info(cfg, backend).ok_or_else(|| {
        ApiError::InvalidInput(format!(
            "backend {}'s model does not support expert offload",
            backend.id
        ))
    })?;
    if layers > info.moe_layers {
        return Err(ApiError::InvalidInput(format!(
            "offload of {layers} layers exceeds the model's {} MoE layers",
            info.moe_layers
        )));
    }
    let (placed_ccs, cpu_only) = placed_offload_placement(backend, gpus);
    let eval = evaluate_offload_position(
        &info.restrictions,
        &backend.id,
        layers,
        info.moe_layers,
        placed_ccs,
        cpu_only,
        journal,
    );
    let blocked = eval.status == manager_models::OffloadStatus::Blocked;
    if blocked && !allow_override {
        // Blocked implies a placed CC matches a restriction (the
        // status function blocks only on a placed match); the
        // restricting CC names the architecture in the message.
        let cc = eval.restricting_cc.unwrap_or((0, 0));
        return Err(ApiError::InvalidInput(format!(
            "offload of {layers} of {} layers is outside the verified range for this GPU (compute capability {}.{}) — choose {} of {} layers, or confirm the position with an explicit override",
            info.moe_layers, cc.0, cc.1, eval.nearest_verified_layers, info.moe_layers
        )));
    }
    let target = info.offload_for(layers);
    let raw_flags = manager_models::set_expert_offload(backend.raw_flags.as_deref(), target);
    Ok(NewBackendOffload {
        raw_flags,
        record_override: blocked,
        placed_ccs: eval.placed_ccs,
    })
}

/// Move a model's DEFAULT offload off a guardrail-blocked position:
/// rewrites `b`'s raw flags to the nearest allowed position for its
/// placement. Only for flags nobody chose — an operator's position
/// is refused, never clamped (docs/offload-guardrails.md).
pub(crate) fn snap_default_offload(
    cfg: &AppConfig,
    b: &mut manager_config::BackendConfig,
    gpus: &[GpuInfo],
) {
    let Some(info) = backend_offload_info(cfg, b) else {
        return;
    };
    let (placed_ccs, cpu_only) = placed_offload_placement(b, gpus);
    let eval = evaluate_offload_position(
        &info.restrictions,
        &b.id,
        info.layers,
        info.moe_layers,
        placed_ccs,
        cpu_only,
        &activation::ActivationJournal::default(),
    );
    if eval.status == manager_models::OffloadStatus::Blocked {
        let target = info.offload_for(eval.nearest_verified_layers);
        b.raw_flags = manager_models::set_expert_offload(b.raw_flags.as_deref(), target);
    }
}

/// Put a draft expert-offload position on a transient backend (the
/// previews' `offload_layers`): the same bounds the save applies. A
/// guardrail-blocked position is priced, not refused — a preview
/// saves nothing, and its `offload` row carries the verdict.
pub(crate) fn apply_offload_draft(
    cfg: &AppConfig,
    b: &mut manager_config::BackendConfig,
    layers: u32,
) -> Result<(), ApiError> {
    let info = backend_offload_info(cfg, b).ok_or_else(|| {
        ApiError::InvalidInput("the model does not support expert offload".to_string())
    })?;
    if layers > info.moe_layers {
        return Err(ApiError::InvalidInput(format!(
            "offload of {layers} layers exceeds the model's {} MoE layers",
            info.moe_layers
        )));
    }
    b.raw_flags =
        manager_models::set_expert_offload(b.raw_flags.as_deref(), info.offload_for(layers));
    Ok(())
}

/// The `--split-mode` spelling for a structured split mode.
pub(crate) fn split_mode_str(mode: manager_config::SplitMode) -> &'static str {
    match mode {
        manager_config::SplitMode::None => "none",
        manager_config::SplitMode::Layer => "layer",
        manager_config::SplitMode::Row => "row",
    }
}

/// Create a new backend and route it through the gateway (spec §14
/// `backend_add`). The backend is created with the chosen registered
/// model and the standard restart policy; it joins an existing
/// gateway group or a new one created for it. The gateway picks the
/// route up when it next (re)starts — its config is generated from
/// this state.
///
/// The launch parameters the add form offers (tracker #49) ride the
/// optional arguments; each `None` keeps the pre-#49 creation
/// default exactly: 32K context, 512 batch, a tensor split assigned
/// from live telemetry when available, and the model's catalog
/// default expert offload. A chosen offload position is validated
/// against the new backend's placement guardrails exactly as
/// `backend_set_offload` validates it for an existing backend —
/// a guardrail-blocked position is refused unless
/// `offload_override` is set, in which case the override is
/// journaled once the creation save lands. `gpu_indices` replaces
/// the telemetry assignment with an even split across the selected
/// GPUs (the Placement form's composition for an explicit
/// selection). A chosen parallel slot count (tracker #61) is
/// written into the created backend's raw flags as part of the
/// same single creation save; `None` writes no `--parallel` token,
/// leaving the new backend at llama.cpp's default exactly as
/// before.
#[allow(clippy::too_many_arguments)] // argument list is the registry contract
/// The `opt-exp-*` backend namespace is reserved for Optimize
/// experiment temp backends (code review B-3): experiment
/// cleanup stops and unregisters any backend carrying such an
/// id, and the startup sweep deletes it from the config, so a
/// user backend created with one would be destroyed by
/// machinery that assumes it is temporary. User-facing creation
/// paths refuse the prefix with this error, which names the
/// reservation.
pub(crate) fn reserved_temp_backend_id_error(id: &str) -> ApiError {
    ApiError::InvalidInput(format!(
        "backend id {id:?} is reserved: ids beginning with {:?} are reserved \
         for Optimize experiment temp backends",
        manager_optimize::TEMP_BACKEND_ID_PREFIX
    ))
}

#[allow(clippy::too_many_arguments)] // argument list is the registry contract
pub async fn op_backend_add(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    model_id: String,
    port: u16,
    group: String,
    create_group: bool,
    enabled: bool,
    n_ctx: Option<u32>,
    n_batch: Option<u32>,
    offload_layers: Option<u32>,
    offload_override: Option<bool>,
    gpu_indices: Option<Vec<u32>>,
    parallel: Option<u32>,
    cache_k: Option<String>,
    cache_v: Option<String>,
    flash_attn: Option<String>,
) -> ApiResult {
    let id = id.trim().to_string();
    let group = group.trim().to_string();
    let valid_slug = |s: &str| {
        !s.is_empty()
            && s.len() <= 48
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !s.starts_with('-')
            && !s.ends_with('-')
    };
    if !valid_slug(&id) {
        return Err(ApiError::InvalidInput(
            "backend name must be lowercase letters, digits, and hyphens".to_string(),
        ));
    }
    if manager_optimize::is_temp_backend_id(&id) {
        return Err(reserved_temp_backend_id_error(&id));
    }
    if create_group {
        manager_config::validate_group_name(&group).map_err(ApiError::InvalidInput)?;
    }
    if port == 0 {
        return Err(ApiError::InvalidInput("port must be non-zero".to_string()));
    }
    // Context/batch: the add form's values when chosen, the seeded
    // defaults otherwise; validated by the same rules
    // backend_update_flags applies (the batch bound reads the
    // EFFECTIVE context, so an oversized batch against a small
    // chosen context is refused here, not at first start).
    let eff_ctx = n_ctx.unwrap_or(32768);
    let eff_batch = n_batch.unwrap_or(512);
    validate_ctx_batch(eff_ctx, eff_batch)?;
    // Parallel slots: validated by the same rules
    // backend_set_parallel applies, when the add form chose a
    // count.
    if let Some(slots) = parallel {
        validate_parallel_slots(slots)?;
    }
    // KV cache / flash attention: validated by the same rules
    // backend_set_cache applies, when the add form chose a
    // selection (the predicted-best starting point it renders,
    // or the operator's change to it). All three values together
    // or none; the invalid quantized-V + FA-off pairing is
    // refused by the shared resolver.
    let cache_sel = resolve_cache_selection(cache_k, cache_v, flash_attn)?;
    // GPU inventory, fetched before the config write lock when the
    // request needs it (an explicit GPU selection to validate, or
    // an offload position to evaluate against the placed GPUs'
    // guardrails). The telemetry accessor reads the config while
    // initializing, so it must never be called under the lock.
    let gpu_inventory: Vec<GpuInfo> = if gpu_indices.is_some() || offload_layers.is_some() {
        let store = state.telemetry().await;
        let inventory = lock_telemetry(&store).inventory();
        inventory
    } else {
        Vec::new()
    };
    // Placement for the new backend, likewise resolved before the
    // write lock: the operator's GPU selection when one was made,
    // else the telemetry assignment.
    let (tensor_split, split_mode) = match &gpu_indices {
        Some(selected) => {
            let indices: Vec<u32> = gpu_inventory.iter().map(|g| g.index).collect();
            split_for_gpu_selection(&indices, selected)?
        }
        None => match propose_new_backend_split(state, &model_id).await {
            Some((split, mode)) => (split, mode),
            None => (Vec::new(), manager_config::SplitMode::None),
        },
    };
    let (mut cfg, live) = config_write_for_journal(state).await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    if new_cfg.backends.iter().any(|b| b.id == id) {
        return Err(ApiError::InvalidInput(format!(
            "a backend with id '{id}' already exists"
        )));
    }
    let model = new_cfg
        .models
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| ApiError::NotFound(format!("model {model_id}")))?
        .clone();
    if create_group {
        if new_cfg.gateway.groups.iter().any(|g| g.name == group) {
            return Err(ApiError::InvalidInput(format!(
                "group '{group}' already exists; join it instead of creating it"
            )));
        }
        new_cfg.gateway.groups.push(manager_config::ModelGroup {
            name: group.clone(),
            members: vec![id.clone()],
            strategy: manager_config::RoutingStrategy::SimpleShuffle,
            fallbacks: Vec::new(),
            purpose: String::new(),
        });
    } else {
        let g = new_cfg
            .gateway
            .groups
            .iter_mut()
            .find(|g| g.name == group)
            .ok_or_else(|| ApiError::NotFound(format!("group {group}")))?;
        if !g.members.iter().any(|m| m == &id) {
            g.members.push(id.clone());
        }
    }
    let mut backend = manager_config::BackendConfig {
        id: id.clone(),
        enabled,
        model_file: model.gguf_path.clone(),
        port,
        flags: manager_config::ServerFlags {
            n_ctx: eff_ctx,
            n_batch: eff_batch,
            tensor_split,
            split_mode,
        },
        // A new backend starts with its model's default expert
        // offload (the green limit), when one is pinned — by the
        // catalog, or by the operator's declaration on the model
        // record for a model the catalog does not carry — e.g. a
        // backend created for Nemotron starts at --n-cpu-moe 16
        // rather than with no offload at all. A position chosen
        // in the add form replaces it below.
        raw_flags: model_default_raw_flags_at(&cfg.data_dir, &model),
        // A created backend starts in structured flags mode; the
        // operator opts into advanced mode per backend afterward.
        flags_advanced: false,
        restart_policy: manager_config::RestartPolicy {
            max_retries: 5,
            backoff_base_secs: 2,
        },
        overrides_global: false,
        tool_call_probe: None,
        purpose: String::new(),
    };
    // A chosen offload position is resolved against the constructed
    // backend (its final placement drives the guardrail evaluation)
    // before anything is journaled or saved, so a refused position
    // leaves no trace. The override, when the operator confirmed a
    // blocked position, is journaled only after the creation save
    // lands — mirroring backend_set_offload, no override ever
    // covers a change that did not persist.
    let mut offload_override_to_record: Option<(u32, Vec<(u32, u32)>)> = None;
    if let Some(layers) = offload_layers {
        let journal = activation::load_journal(&state.data_dir);
        let resolved = resolve_new_backend_offload(
            &new_cfg,
            &backend,
            &gpu_inventory,
            &journal,
            layers,
            offload_override == Some(true),
        )?;
        backend.raw_flags = resolved.raw_flags;
        if resolved.record_override {
            offload_override_to_record = Some((layers, resolved.placed_ccs));
        }
    } else {
        snap_default_offload(&new_cfg, &mut backend, &gpu_inventory);
    }
    // A chosen parallel slot count is written into the constructed
    // backend's raw flags — after the offload resolution above,
    // which rewrites the same string — so the creation save below
    // carries it with everything else. An absent choice writes no
    // token: creation stays byte-identical to the pre-#61 shape.
    if let Some(slots) = parallel {
        backend.raw_flags = manager_models::set_parallel_slots(
            backend.raw_flags.as_deref(),
            Some(i64::from(slots)),
        );
    }
    // A chosen cache selection is written into the constructed
    // backend's raw flags last (it rewrites only its own tokens),
    // so the creation save carries it with everything else. An
    // absent choice writes no cache tokens: creation stays
    // byte-identical to the pre-feature shape, at llama.cpp's
    // defaults.
    if let Some(sel) = cache_sel {
        backend.raw_flags =
            manager_models::set_cache_selection(backend.raw_flags.as_deref(), Some(sel));
    }
    new_cfg.backends.push(backend);
    if let Some(m) = new_cfg.models.iter_mut().find(|m| m.id == model_id) {
        if !m.assigned_backends.contains(&id) {
            m.assigned_backends.push(id.clone());
        }
    }
    manager_config::validate(&new_cfg).map_err(ApiError::from)?;
    let journaled = record_gateway_change(
        state,
        &live,
        &pre_cfg,
        &new_cfg,
        "backend_add",
        format!("Backend `{id}` added to group `{group}`"),
        false,
    )
    .await?;
    save_journaled(state, &new_cfg, &journaled)?;
    *cfg = new_cfg;
    drop(cfg);
    if let Some((layers, placed_ccs)) = offload_override_to_record {
        let _ = activation::record_offload_override(&state.data_dir, &id, layers, placed_ccs);
    }
    ensure_backend_specs(state).await;
    state.sync_backend_service_definition(&id).await;
    Ok(json!({ "backend_id": id, "group": group, "persisted": true }))
}

/// Preview what creating a backend for `model_id` would carry
/// (tracker #49): the expert-offload slider state for the model —
/// the same `offload` object list_backends rows carry, computed
/// over a transient backend holding the creation defaults (the
/// catalog default offload position, the placement creation would
/// use) — plus that placement itself (`tensor_split` /
/// `split_mode`). The add form renders its offload control and
/// Placement chips from this, so the model-dependent behavior
/// (slider range, guardrail states) is the edit form's, computed by
/// the same code. `gpu_indices` previews an explicit GPU selection
/// instead of the telemetry assignment; `offload_layers` previews an
/// explicit MoE position instead of the model's default. Nothing is
/// written.
pub async fn op_backend_add_preview(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    model_id: String,
    gpu_indices: Option<Vec<u32>>,
    n_ctx: Option<u32>,
    parallel: Option<u32>,
    offload_layers: Option<u32>,
) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let model = cfg
        .models
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| ApiError::NotFound(format!("model {model_id}")))?
        .clone();
    // The GPU inventory serves both call sites below (selection
    // validation, guardrail evaluation). cfg is a clone, so no
    // config lock is held while telemetry is touched.
    let gpu_inventory: Vec<GpuInfo> = {
        let store = state.telemetry().await;
        let inventory = lock_telemetry(&store).inventory();
        inventory
    };
    let (tensor_split, split_mode) = match &gpu_indices {
        Some(selected) => {
            let indices: Vec<u32> = gpu_inventory.iter().map(|g| g.index).collect();
            split_for_gpu_selection(&indices, selected)?
        }
        None => match propose_new_backend_split(state, &model_id).await {
            Some((split, mode)) => (split, mode),
            None => (Vec::new(), manager_config::SplitMode::None),
        },
    };
    // The transient backend creation would build, at its defaults.
    let mut backend = manager_config::BackendConfig {
        id: String::new(),
        enabled: true,
        model_file: model.gguf_path.clone(),
        port: 0,
        flags: manager_config::ServerFlags {
            n_ctx: 32768,
            n_batch: 512,
            tensor_split: tensor_split.clone(),
            split_mode,
        },
        raw_flags: model_default_raw_flags_at(&cfg.data_dir, &model),
        flags_advanced: false,
        restart_policy: manager_config::RestartPolicy {
            max_retries: 5,
            backoff_base_secs: 2,
        },
        overrides_global: false,
        tool_call_probe: None,
        purpose: String::new(),
    };
    match offload_layers {
        Some(layers) => apply_offload_draft(&cfg, &mut backend, layers)?,
        None => snap_default_offload(&cfg, &mut backend, &gpu_inventory),
    }
    let offload = backend_offload_info(&cfg, &backend).map(|info| {
        let journal = activation::load_journal(&state.data_dir);
        let eval = evaluate_backend_offload(&cfg, &backend, &gpu_inventory, &journal);
        offload_row_json(&info, eval.as_ref())
    });
    // The cache preview for the backend creation would build:
    // the predicted footprint and predicted-best ladder at the
    // form's context/slot draft (the creation defaults when the
    // form sends none), so the add form renders its KV cache
    // controls from the same payload the Edit Flags form renders
    // from, with the predicted-best rung as the starting point.
    // Null when the model's layout cannot be resolved.
    let free = gpu_free_now(state).await;
    let cache = resolve_weight_layout(&cfg, &backend).map(|layout| {
        let targets = cache_fit_targets(&backend, &gpu_inventory);
        let mut preview = cache_preview_json(
            &layout,
            manager_models::expert_offload(backend.raw_flags.as_deref()),
            manager_models::cache_selection(backend.raw_flags.as_deref()),
            n_ctx.unwrap_or(backend.flags.n_ctx),
            parallel
                .map(|p| p.max(1))
                .unwrap_or_else(|| manager_models::effective_slots(backend.raw_flags.as_deref())),
            manager_models::moe_cache_mib(backend.raw_flags.as_deref()).unwrap_or(0),
            targets.as_ref(),
        );
        annotate_targets_free_fit(&mut preview, &free, &HashMap::new());
        preview
    });
    let mut memory = add_preview_memory_json(&cfg, &backend, n_ctx, parallel, &gpu_inventory);
    // The free-memory verdict beside the figures: a new backend
    // holds nothing yet, so nothing is credited.
    if let Some(per_gpu) = memory["per_gpu"].as_array_mut() {
        let now = annotate_free_fit(per_gpu, &free, &HashMap::new());
        memory["fits_now"] = json!(now);
    }
    Ok(json!({
        "model_id": model_id,
        "offload": offload,
        "cache": cache,
        "tensor_split": tensor_split,
        "split_mode": split_mode_str(split_mode),
        "memory": memory,
    }))
}

/// The add form's requirement summary (add-backend audit Q8): the
/// transient backend creation would build, sized at the form's
/// context/slot draft, priced by `backend_memory_requirement` —
/// the figures list_backends rows carry as `gpu_ram_mib` /
/// `sys_ram_mib` / `kv_ram_mib` / `rs_ram_mib` / `moe_cache_mib` /
/// `memory_basis`, so the add form's GPU RAM / System RAM tiles
/// mean what the backend card's tiles mean — plus `per_gpu`: the
/// split share and the residency the start gate would judge
/// (`residency_per_gpu` over the telemetry inventory) for every
/// GPU the previewed placement uses. An unsplit placement loads on
/// the first GPU (share 1). `per_gpu` is empty without telemetry.
/// Null when the model's layout cannot be resolved — never a
/// zero-filled requirement.
pub(crate) fn add_preview_memory_json(
    cfg: &AppConfig,
    backend: &manager_config::BackendConfig,
    n_ctx: Option<u32>,
    parallel: Option<u32>,
    gpu_inventory: &[GpuInfo],
) -> Value {
    let mut sized = backend.clone();
    if let Some(c) = n_ctx {
        sized.flags.n_ctx = c;
    }
    if let Some(p) = parallel {
        sized.raw_flags = manager_models::set_parallel_slots(
            sized.raw_flags.as_deref(),
            Some(i64::from(p.max(1))),
        );
    }
    let Some(req) = backend_memory_requirement(cfg, &sized) else {
        return Value::Null;
    };
    let split = &sized.flags.tensor_split;
    let n = gpu_inventory.len();
    let residency = manager_optimize::residency_per_gpu(&req, split, n);
    let per_gpu: Vec<Value> = gpu_inventory
        .iter()
        .enumerate()
        .filter_map(|(pos, g)| {
            let share = if split.is_empty() {
                if pos == 0 {
                    1.0
                } else {
                    0.0
                }
            } else {
                split.get(pos).copied().unwrap_or(0.0)
            };
            if share <= 0.0 {
                return None;
            }
            Some(json!({
                "gpu": g.index,
                "share": share,
                "required_mib": residency[pos],
                "total_mib": g.total_vram_mib,
                "driver_model": g.driver_model.map(|d| match d {
                    DriverModel::Wddm => "wddm",
                    DriverModel::Tcc => "tcc",
                }),
            }))
        })
        .collect();
    json!({
        "gpu_ram_mib": req.gpu_weights_mib,
        "sys_ram_mib": req.sys_weights_mib,
        "kv_ram_mib": req.kv_mib,
        "rs_ram_mib": req.rs_mib,
        "moe_cache_mib": req.moe_cache_mib,
        "basis": serde_json::to_value(req.basis).unwrap_or(Value::Null),
        "per_gpu": per_gpu,
    })
}

/// Update a backend's llama-server sizing flags: context length
/// (`-c`) and batch size (`-b`). This is the supported way to change
/// them after a backend is created — before this command the only
/// paths were hand-editing config.json or removing and re-adding the
/// backend. Takes effect on the backend's next start; a running
/// backend keeps its current values until it is restarted.
pub async fn op_backend_update_flags(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    n_ctx: u32,
    n_batch: u32,
) -> ApiResult {
    validate_ctx_batch(n_ctx, n_batch)?;
    let (mut cfg, live) = config_write_for_journal(state).await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    // Advanced flags mode (tracker #60): context/batch are the
    // flags form's fields; the form is inert in the mode.
    ensure_backend_not_advanced(b)?;
    let changes: Vec<String> = [
        ("context", b.flags.n_ctx, n_ctx),
        ("batch", b.flags.n_batch, n_batch),
    ]
    .iter()
    .filter(|(_, old, new)| old != new)
    .map(|(name, old, new)| field_change_text(name, &json!(old), &json!(new)))
    .collect();
    b.flags.n_ctx = n_ctx;
    b.flags.n_batch = n_batch;
    manager_config::validate(&new_cfg).map_err(ApiError::from)?;
    let journaled = if !changes.is_empty() {
        record_backend_change(
            state,
            &live,
            &pre_cfg,
            &new_cfg,
            "backend_flags",
            format!("Backend `{id}` settings changed: {}", changes.join(", ")),
            &id,
        )
        .await?
    } else {
        activation::JournalRecords::none()
    };
    save_journaled(state, &new_cfg, &journaled)?;
    *cfg = new_cfg;
    drop(cfg);
    ensure_backend_specs(state).await;
    state.sync_backend_service_definition(&id).await;
    Ok(json!({ "backend_id": id, "persisted": true, "applies": "next start" }))
}

/// The raw flags a backend serving `model_id` starts with: the
/// model's catalog default expert offload (its green limit — see
/// [`manager_models::catalog_default_offload`]) in canonical form,
/// when the catalog pins one. Used where a backend gains a model
/// without operator flags of its own: backend creation, model
/// assignment, and the catalog definition refresh.
pub(crate) fn catalog_default_raw_flags(model_id: &str) -> Option<String> {
    catalog_default_raw_flags_mapped(model_id, None)
}

/// [`catalog_default_raw_flags`] with the model's MoE block map:
/// the catalog pin counts MoE LAYERS, so a partial pin is written
/// at the `--n-cpu-moe` block-index bound that moves exactly that
/// many (code review v0.1.60 item 11). Without a map (the file is
/// not downloaded or not parsed) the pin is written as is — exact
/// for models whose MoE blocks start at block 0, an Estimate
/// otherwise.
pub(crate) fn catalog_default_raw_flags_mapped(
    model_id: &str,
    map: Option<&manager_models::MoeBlockMap>,
) -> Option<String> {
    manager_models::curated_models()
        .into_iter()
        .find(|s| s.id == model_id)
        .and_then(|s| {
            let offload = match manager_models::catalog_default_offload(&s) {
                manager_models::ExpertOffload::CpuMoeLayers(n) => {
                    manager_models::offload_for_moe_layers(n, s.moe_layers.unwrap_or(n), map)
                }
                other => other,
            };
            manager_models::offload_flag_string(offload)
        })
}

/// The MoE block map of `model`'s GGUF, when the file is present
/// and its header parses with an expert tensor index.
pub(crate) fn model_moe_block_map(
    data_dir: &std::path::Path,
    model: &manager_config::ModelConfig,
) -> Option<manager_models::MoeBlockMap> {
    let path = resolve_under_data_dir(data_dir, &model.gguf_path);
    let layout = gguf_layout_cached(&path)?;
    manager_models::MoeBlockMap::from_tensors(&layout.expert_tensors)
}

/// [`model_default_raw_flags`] resolved against the model's GGUF
/// (when present under `data_dir`): MoE-layer defaults are written
/// at their block-index bound (see
/// [`catalog_default_raw_flags_mapped`]).
pub(crate) fn model_default_raw_flags_at(
    data_dir: &std::path::Path,
    model: &manager_config::ModelConfig,
) -> Option<String> {
    model_default_raw_flags_mapped(model, model_moe_block_map(data_dir, model).as_ref())
}

/// The raw flags a backend serving `model` starts with, resolved
/// against the model record as well as the catalog: the catalog
/// default when the catalog pins one for the model's id (see
/// [`catalog_default_raw_flags`]); otherwise the operator's
/// declared default on the model record
/// ([`ModelConfig::moe_layers`] /
/// [`ModelConfig::default_offload_layers`]) in canonical form —
/// the path that gives a user-added MoE model the same creation
/// default a catalog model gets. `None` when neither source pins
/// a default that writes tokens.
/// (No block map: see [`model_default_raw_flags_at`].)
#[cfg(test)]
pub(crate) fn model_default_raw_flags(model: &manager_config::ModelConfig) -> Option<String> {
    model_default_raw_flags_mapped(model, None)
}

pub(crate) fn model_default_raw_flags_mapped(
    model: &manager_config::ModelConfig,
    map: Option<&manager_models::MoeBlockMap>,
) -> Option<String> {
    if let Some(flags) = catalog_default_raw_flags_mapped(&model.id, map) {
        return Some(flags);
    }
    let layers = model.moe_layers?;
    let default = model.default_offload_layers?;
    if layers == 0 {
        return None;
    }
    manager_models::offload_flag_string(manager_models::offload_for_moe_layers(
        default.min(layers),
        layers,
        map,
    ))
}

/// Turn a backend's advanced flags mode on or off (tracker #60).
///
/// The mode is editor state, and flipping it is deliberately
/// minimal: it writes ONLY the `flags_advanced` field — never the
/// backend's flags, by a single token. Entering advanced mode
/// cannot canonicalize the string the structured setters would
/// rewrite (they rewrite on save; this op performs no flags save),
/// so the string the operator sees on entry is exactly the string
/// that was live before. Leaving writes nothing either: the raw
/// string stays as saved, and the structured form re-decodes it
/// through the existing decoders (ServerFlags, expert offload,
/// MTP) when it renders.
///
/// The flip is NOT journaled as an activation change: by itself it
/// alters nothing a restart would activate — the journaled event
/// is the raw-flags save that follows it. For the same reason the
/// tool-call probe is untouched (the serving configuration did
/// not change) and no service definition is re-synced (the launch
/// spec does not carry the mode).
pub async fn op_backend_set_flags_mode(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    advanced: bool,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    let changed = b.flags_advanced != advanced;
    b.flags_advanced = advanced;
    let raw_flags = b.raw_flags.clone();
    if changed {
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
        *cfg = new_cfg;
    }
    Ok(json!({
        "backend_id": id,
        "flags_advanced": advanced,
        "raw_flags": raw_flags,
        "changed": changed,
        "persisted": true,
    }))
}

/// Save a backend's raw flags edited as text (tracker #60, the
/// advanced flags editor's save). Only available while the
/// backend is in advanced mode: in structured mode the raw string
/// is written by the token-owning setters and the config patch,
/// and this op refuses rather than becoming a second writer.
///
/// Validation is warn, never block: the string is checked with
/// [`manager_models::raw_flag_warnings`] and the warnings ride
/// the response, but any string saves (except one llama-server is
/// certain to refuse at startup, [`manager_models::raw_flag_errors`])
/// — an unrecognized flag may
/// be exactly what the operator needs, and llama-server's own
/// start is the real check. The input is normalized the way the
/// launch tokenizer sees it: leading/trailing whitespace is not
/// content, and an all-whitespace string is no flags at all
/// (`None`) — the same state the structured setters produce when
/// their tokens are removed.
///
/// Saved through the same clone-validate-save path as the other
/// flags setters and journaled as a backend-scope activation
/// change (kind `backend_set_raw_flags`); a change clears the
/// recorded tool-call probe (MCP design §7.2), as every raw-flags
/// write does. Applies on the backend's next start; nothing is
/// restarted here.
pub async fn op_backend_set_raw_flags(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    raw_flags: Option<String>,
) -> ApiResult {
    let new_raw: Option<String> = raw_flags
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // The one exception to warn-never-block: a string llama-server
    // is certain to refuse at startup (an `-ot` pattern with a
    // comma inside `{m,n}`, a class or a group — llama-server
    // splits -ot on every comma) is refused here, with a message
    // that suggests an alternation instead.
    if let Some(err) = manager_models::raw_flag_errors(new_raw.as_deref())
        .into_iter()
        .next()
    {
        return Err(ApiError::InvalidInput(err.message));
    }
    let warnings = manager_models::raw_flag_warnings(new_raw.as_deref());
    let (mut cfg, live) = config_write_for_journal(state).await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    if !b.flags_advanced {
        return Err(ApiError::InvalidInput(format!(
            "backend {id} is not in advanced flags mode: in structured mode \
             its raw flags are written by the structured flags controls; \
             turn advanced mode on (backend_set_flags_mode) to edit them \
             as text"
        )));
    }
    let changed = b.raw_flags != new_raw;
    b.raw_flags = new_raw.clone();
    if changed {
        // A raw-flags change invalidates the recorded tool-call
        // probe (MCP design §7.2).
        b.tool_call_probe = None;
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        let journaled = record_backend_change(
            state,
            &live,
            &pre_cfg,
            &new_cfg,
            "backend_set_raw_flags",
            format!("Backend `{id}` raw flags changed"),
            &id,
        )
        .await?;
        save_journaled(state, &new_cfg, &journaled)?;
        *cfg = new_cfg;
    }
    drop(cfg);
    if changed {
        ensure_backend_specs(state).await;
    }
    state.sync_backend_service_definition(&id).await;
    Ok(json!({
        "backend_id": id,
        "raw_flags": new_raw,
        "changed": changed,
        "warnings": warnings,
        // `persisted` is about the desired state being on disk:
        // after a successful save it is, and when nothing changed
        // it already was (same note as backend_set_offload).
        "persisted": true,
        "applies": "next start",
    }))
}

/// Remove a backend (spec §14 `backend_delete`). Refused while the
/// backend is running — stop it first. The id is scrubbed from every
/// group's member list (the gateway config generator rejects unknown
/// members), from every group's fallback chain (config validation
/// rejects unknown fallbacks, so a delete that left one behind could
/// never be saved), and from models' assignment lists. A group left
/// with no members stays in the config: it generates no gateway
/// deployments and can gain members again later.
pub async fn op_backend_delete(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    {
        let sup = state.supervisor().await;
        let mut sup = sup.lock().await;
        sup.reap_exited();
        if let Some(st) = sup.status().iter().find(|s| s.id == id) {
            if st.pid.is_some() {
                return Err(ApiError::InvalidInput(format!(
                    "backend {id} is running; stop it before removing it"
                )));
            }
        }
    }
    if !state
        .config
        .read()
        .await
        .backends
        .iter()
        .any(|b| b.id == id)
    {
        return Err(ApiError::NotFound(format!("backend {id}")));
    }
    // A running Windows service of this backend would otherwise keep
    // serving, invisible once the id leaves the config (SCM-18): stop
    // it through the shared lifecycle path first (markers, events,
    // intent). The registration itself stays the operator's to remove.
    let service_name = manager_services::backend_service_name(&id);
    let service_stopped =
        match lifecycle::scm_status(state, &service_name, Duration::from_secs(1)).await {
            Some(s) if lifecycle::registered(&s) && lifecycle::scm_live(&s) => {
                backend_lifecycle::stop_backend(
                    state,
                    sink,
                    &id,
                    backend_lifecycle::BackendOpOpts::new(lifecycle::Origin::Ui),
                )
                .await?;
                true
            }
            _ => false,
        };
    let (mut cfg, live) = config_write_for_journal(state).await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    let pos = new_cfg
        .backends
        .iter()
        .position(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    new_cfg.backends.remove(pos);
    for g in new_cfg.gateway.groups.iter_mut() {
        g.members.retain(|m| m != &id);
        g.fallbacks.retain(|f| f != &id);
    }
    for m in new_cfg.models.iter_mut() {
        m.assigned_backends.retain(|b| b != &id);
    }
    manager_config::validate(&new_cfg).map_err(ApiError::from)?;
    let journaled = record_gateway_change(
        state,
        &live,
        &pre_cfg,
        &new_cfg,
        "backend_delete",
        format!("Backend `{id}` removed"),
        false,
    )
    .await?;
    save_journaled(state, &new_cfg, &journaled)?;
    *cfg = new_cfg;
    drop(cfg);
    ensure_backend_specs(state).await;
    if service_stopped {
        lifecycle::mark(
            &state.data_dir,
            &lifecycle::backend_key(&id),
            "deleted from the configuration",
            Some("its running service was stopped first; the registration remains"),
            Some(lifecycle::Origin::Ui),
        );
    }
    // A deleted backend's service registration is the operator's to
    // remove (Settings → Supervision) — a config delete must not fire
    // a UAC prompt from a list action. Record the leftover so it is
    // discoverable rather than silent.
    if state.service_route_active(&service_name).await {
        append_wizard_log(
            &state.data_dir,
            &[format!(
                "[{}] backend {id} was deleted from the configuration but its Windows service is still registered; unregister it in Settings → Supervision",
                chrono::Utc::now().to_rfc3339()
            )],
        );
    }
    Ok(json!({ "backend_id": id, "deleted": true, "persisted": true }))
}

/// Exact argv the supervisor would spawn (must equal the wizard's
/// planned_commands output per spec).
pub async fn op_backend_get_launch_command(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    ensure_backend_specs(state).await;
    let argv = state
        .supervisor()
        .await
        .lock()
        .await
        .launch_argv(&id)
        .map_err(ApiError::from)?;
    Ok(json!({ "id": id, "argv": argv }))
}

#[cfg(test)]
mod tests;
