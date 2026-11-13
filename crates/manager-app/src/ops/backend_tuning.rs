//! Backend tuning edits: MTP, MoE expert cache, parallel slots, and the
//! KV cache type selection and preview.

use crate::*;

/// Set a backend's MTP speculative decoding (tracker #53; the
/// flags form's MTP draft control): rewrite only the MTP tokens
/// in the backend's raw flags — `n_max` of 1..=4 enables
/// `--spec-type draft-mtp` with that `--spec-draft-n-max`;
/// `None` turns MTP off. Every other flag is preserved,
/// including `--parallel`: MTP drafting needs a single slot,
/// but the control surfaces that conflict rather than rewriting
/// the operator's slot setting (the no-silent-rewrite rule the
/// offload guardrails follow). The backend's model must be
/// MTP-capable (see [`backend_mtp_info`]). Saved through the
/// same clone-validate-save path as the other backend mutations
/// and journaled as a backend-scope activation change with
/// `applies: "next start"`; no restart happens here.
pub async fn op_backend_set_mtp(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    n_max: Option<u32>,
) -> ApiResult {
    use manager_models::mtp::{MTP_N_MAX_MAX, MTP_N_MAX_MIN};
    if let Some(n) = n_max {
        if !(MTP_N_MAX_MIN..=MTP_N_MAX_MAX).contains(&n) {
            return Err(ApiError::InvalidInput(format!(
                "MTP draft length {n} is outside the control's range \
                 {MTP_N_MAX_MIN}..={MTP_N_MAX_MAX}"
            )));
        }
    }
    // Capability is resolved against the current config before
    // the write lock is taken, and re-resolved under the lock
    // below (a concurrent model assignment can change which
    // model this backend serves — the same shape as
    // op_backend_set_offload).
    {
        let cfg = state.config.read().await;
        let b = cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        // Advanced flags mode (tracker #60): the control rewrites
        // raw-flag tokens the operator owns as text in the mode.
        ensure_backend_not_advanced(b)?;
        if backend_mtp_info(&cfg, b).is_none() {
            return Err(ApiError::InvalidInput(format!(
                "backend {id}'s model does not support MTP drafting"
            )));
        }
    }
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    {
        // Re-validate capability against the config actually
        // being mutated.
        let b = new_cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        // Re-checked under the write lock (tracker #60), with the
        // capability re-validation.
        ensure_backend_not_advanced(b)?;
        if backend_mtp_info(&new_cfg, b).is_none() {
            return Err(ApiError::InvalidInput(format!(
                "backend {id}'s model does not support MTP drafting"
            )));
        }
    }
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    let target = match n_max {
        Some(n) => manager_models::MtpDraft::On(Some(n)),
        None => manager_models::MtpDraft::Off,
    };
    let new_raw = manager_models::set_mtp_draft(b.raw_flags.as_deref(), target);
    let changed = new_raw != b.raw_flags;
    b.raw_flags = new_raw.clone();
    if changed {
        // A raw-flags change invalidates the recorded tool-call
        // probe (MCP design §7.2).
        b.tool_call_probe = None;
    }
    if changed {
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        let summary = match n_max {
            Some(n) => format!("Backend `{id}` MTP draft set to {n} tokens"),
            None => format!("Backend `{id}` MTP draft turned off"),
        };
        let journaled =
            record_backend_change(state, &pre_cfg, &new_cfg, "backend_set_mtp", summary, &id)
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
        "n_max": n_max,
        "raw_flags": new_raw,
        "changed": changed,
        // `persisted` is about the desired state being on disk:
        // after a successful save it is, and when nothing changed
        // it already was (same note as backend_set_offload).
        "persisted": true,
        "applies": "next start",
    }))
}

/// Resolve the Auto expert-cache size for a backend: the
/// largest cache its cache-bearing GPUs can price right now,
/// less the standing reserve
/// ([`manager_models::MOE_CACHE_AUTO_RESERVE_MIB`]). Priced free
/// space per GPU is the telemetry snapshot's measured free VRAM
/// minus every other backend's accounted residency and this
/// backend's own non-cache requirement share — the fit
/// evaluator's accounting, so Auto never promises space the
/// start gate would then refuse. Errors carry the numbers when
/// placement or telemetry cannot price a cache, or when no
/// positive size survives the reserve.
pub(crate) async fn resolve_auto_moe_cache_mib(
    state: &Arc<AppState>,
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
    offload_layers: u32,
    moe_layers: u32,
) -> Result<u64, ApiError> {
    let placed: Vec<u32> = match backend_gpu_visibility(b) {
        GpuVisibility::Gpus(indices) => indices,
        _ => {
            return Err(ApiError::InvalidInput(format!(
            "Auto cannot price an expert cache for backend {}: it is not placed on specific GPUs",
            b.id
        )))
        }
    };
    let Some(mut req) = backend_memory_requirement(cfg, b) else {
        return Err(ApiError::InvalidInput(format!(
            "Auto cannot price an expert cache for backend {}: its memory requirement does not resolve",
            b.id
        )));
    };
    // Price the backend's own NON-cache requirement: an existing
    // token is being replaced, not added to.
    req.moe_cache_mib = 0;
    let mut samples = {
        let store = state.telemetry().await;
        let snapshot = lock_telemetry(&store).snapshot();
        snapshot
    };
    samples.sort_by_key(|s| s.index);
    if samples.is_empty() {
        return Err(ApiError::InvalidInput(format!(
            "Auto cannot price an expert cache for backend {}: no GPU telemetry is available",
            b.id
        )));
    }
    let n = samples.len();
    let own = manager_optimize::residency_per_gpu(&req, &b.flags.tensor_split, n);
    let mut others = vec![0u64; n];
    for ob in cfg.backends.iter().filter(|ob| ob.id != b.id) {
        if let Some(or) = backend_memory_requirement(cfg, ob) {
            let per = manager_optimize::residency_per_gpu(&or, &ob.flags.tensor_split, n);
            for (acc, v) in others.iter_mut().zip(per.iter()) {
                *acc = acc.saturating_add(*v);
            }
        }
    }
    // The cache lands only on cache-bearing GPUs (see
    // moe_cache_shares); a probe budget reads the bearing set and
    // each GPU's fraction of it.
    const PROBE_MIB: u64 = 1_000_000;
    let probe = manager_models::moe_cache_shares(
        PROBE_MIB,
        &b.flags.tensor_split,
        offload_layers,
        moe_layers,
        n,
    );
    let mut bearing: Vec<(u64, f64)> = Vec::new();
    let mut tightest: Option<(u32, u64, u64, u64)> = None;
    for (pos, sample) in samples.iter().enumerate() {
        if probe[pos] == 0 || !placed.contains(&sample.index) {
            continue;
        }
        let free = sample.free_vram_mib();
        let available = free.saturating_sub(others[pos]).saturating_sub(own[pos]);
        bearing.push((available, probe[pos] as f64 / PROBE_MIB as f64));
        let tighter = match tightest {
            None => true,
            Some((_, a, _, _)) => available < a,
        };
        if tighter {
            tightest = Some((
                sample.index,
                available,
                free,
                others[pos].saturating_add(own[pos]),
            ));
        }
    }
    match manager_models::auto_moe_cache_budget(
        &bearing,
        manager_models::MOE_CACHE_AUTO_RESERVE_MIB,
    ) {
        Some(mib) => Ok(mib),
        None => {
            let detail = match tightest {
                Some((index, available, free, accounted)) => format!(
                    "GPU {index} has {free} MiB free, of which {accounted} MiB is accounted by other backends and this backend's own requirement, leaving {available} MiB"
                ),
                None => "no placed GPU bears offloaded layers".to_string(),
            };
            Err(ApiError::InvalidInput(format!(
                "Auto found no room for an expert cache on backend {}: {detail}; the standing reserve is {} MiB",
                b.id,
                manager_models::MOE_CACHE_AUTO_RESERVE_MIB
            )))
        }
    }
}

/// Set a backend's MoE expert cache (`--moe-cache-mib`): rewrite
/// only that token in the backend's raw flags — `mib` writes the
/// canonical token, `None` (with `auto` unset) turns the cache
/// off, and `auto: true` resolves the size from priced free VRAM
/// (see [`resolve_auto_moe_cache_mib`]) and writes the resolved
/// value. Every other flag is preserved. Eligibility is the
/// offload slider's exactly ([`backend_moe_cache_info`]): a dense
/// model has no experts to cache. A nonzero cache additionally
/// requires a saved offload position above 0 — with no experts
/// in system RAM there is nothing to cache — and refuses a row
/// (tensor) split, which the pinned llama.cpp rejects the cache
/// under; turning the cache off is always allowed, so a stranded
/// token can always be cleared. The size is priced into the
/// backend's memory requirement, so a start whose cache cannot
/// fit is refused by the start gate with the numbers. Saved
/// through the same clone-validate-save path as the other
/// backend mutations and journaled as a backend-scope activation
/// change with `applies: "next start"`; no restart happens here.
pub async fn op_backend_set_moe_cache(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    mib: Option<u32>,
    auto: Option<bool>,
) -> ApiResult {
    let want_auto = auto == Some(true);
    if want_auto && mib.is_some() {
        return Err(ApiError::InvalidInput(
            "moe-cache takes either an explicit mib or auto, not both".to_string(),
        ));
    }
    // Eligibility and the static refusals resolve against the
    // current config before the write lock, and re-resolve under
    // it below (a concurrent model or placement change can alter
    // every one of them — the set_offload/set_mtp shape).
    let (offload_layers, moe_layers) = {
        let cfg = state.config.read().await;
        let b = cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        ensure_backend_not_advanced(b)?;
        let info = backend_moe_cache_info(&cfg, b).ok_or_else(|| {
            ApiError::InvalidInput(format!(
                "backend {id}'s model does not support an expert cache (it is not an offload-capable MoE model)"
            ))
        })?;
        check_moe_cache_request(b, mib, want_auto, info.offload_layers)?;
        (info.offload_layers, info.moe_layers)
    };
    // Auto resolves between the config reads: telemetry access
    // must never hold the config lock (see op_backend_set_offload).
    let resolved = if want_auto {
        let cfg = state.config.read().await;
        let b = cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        let mib = resolve_auto_moe_cache_mib(state, &cfg, b, offload_layers, moe_layers).await?;
        Some(u32::try_from(mib).map_err(|_| {
            ApiError::InvalidInput(format!(
                "Auto resolved an expert cache of {mib} MiB for backend {id}, beyond the control's range"
            ))
        })?)
    } else {
        None
    };
    let target: Option<u32> = if want_auto { resolved } else { mib };
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    {
        // Re-validate against the config actually being mutated.
        let b = new_cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        ensure_backend_not_advanced(b)?;
        let info = backend_moe_cache_info(&new_cfg, b).ok_or_else(|| {
            ApiError::InvalidInput(format!(
                "backend {id}'s model does not support an expert cache (it is not an offload-capable MoE model)"
            ))
        })?;
        check_moe_cache_request(b, target, false, info.offload_layers)?;
    }
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    let new_raw = manager_models::set_moe_cache(b.raw_flags.as_deref(), target);
    let changed = new_raw != b.raw_flags;
    b.raw_flags = new_raw.clone();
    if changed {
        // A raw-flags change invalidates the recorded tool-call
        // probe (MCP design §7.2).
        b.tool_call_probe = None;
    }
    if changed {
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        let summary = match target {
            Some(n) if want_auto => {
                format!("Backend `{id}` expert cache set to {n} MiB (Auto)")
            }
            Some(n) => format!("Backend `{id}` expert cache set to {n} MiB"),
            None => format!("Backend `{id}` expert cache turned off"),
        };
        let journaled = record_backend_change(
            state,
            &pre_cfg,
            &new_cfg,
            "backend_set_moe_cache",
            summary,
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
        "mib": manager_models::moe_cache_mib(new_raw.as_deref()).filter(|n| *n > 0),
        "auto": want_auto,
        "resolved_mib": resolved,
        "raw_flags": new_raw,
        "changed": changed,
        "persisted": true,
        "applies": "next start",
    }))
}

/// The expert-cache request's static refusals, shared by the
/// op's pre-lock and under-lock passes: a nonzero size (explicit,
/// or any Auto request, which resolves to a nonzero size by
/// construction) requires a saved offload position above 0 and a
/// non-tensor split. Turning the cache off passes always.
pub(crate) fn check_moe_cache_request(
    b: &manager_config::BackendConfig,
    mib: Option<u32>,
    auto: bool,
    offload_layers: u32,
) -> Result<(), ApiError> {
    let nonzero = auto || mib.is_some_and(|n| n > 0);
    if !nonzero {
        return Ok(());
    }
    if offload_layers == 0 {
        return Err(ApiError::InvalidInput(format!(
            "backend {} offloads 0 MoE layers: the expert cache holds the offloaded experts in GPU memory, and there are none in system RAM to cache — raise the offload above 0 first",
            b.id
        )));
    }
    if b.flags.split_mode == manager_config::SplitMode::Row {
        return Err(ApiError::InvalidInput(format!(
            "backend {} uses a row (tensor) split: the pinned llama.cpp refuses the expert cache under tensor parallelism",
            b.id
        )));
    }
    Ok(())
}

/// Set a backend's parallel slot count (tracker #61; the flags
/// form's Parallel slots field): rewrite only the `--parallel`
/// tokens in the backend's raw flags — `slots` in the control's
/// range writes the canonical `--parallel N`; `None` removes the
/// token, returning the backend to llama.cpp's default (auto).
/// Every other flag is preserved. This is the one write path for
/// the setting: the MTP control surfaces a conflicting slot count
/// but never rewrites it, so a slot change is always an explicit
/// operator action in this field. Saved through the same
/// clone-validate-save path as the other backend mutations and
/// journaled as a backend-scope activation change with
/// `applies: "next start"`; no restart happens here.
pub async fn op_backend_set_parallel(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    slots: Option<u32>,
) -> ApiResult {
    if let Some(n) = slots {
        validate_parallel_slots(n)?;
    }
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    let new_raw = manager_models::set_parallel_slots(b.raw_flags.as_deref(), slots.map(i64::from));
    let changed = new_raw != b.raw_flags;
    b.raw_flags = new_raw.clone();
    if changed {
        // A raw-flags change invalidates the recorded tool-call
        // probe (MCP design §7.2).
        b.tool_call_probe = None;
    }
    if changed {
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        let summary = match slots {
            Some(n) => format!("Backend `{id}` parallel slots set to {n}"),
            None => {
                format!("Backend `{id}` parallel slots returned to the llama.cpp default (auto)")
            }
        };
        let journaled = record_backend_change(
            state,
            &pre_cfg,
            &new_cfg,
            "backend_set_parallel",
            summary,
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
        "slots": slots,
        "raw_flags": new_raw,
        "changed": changed,
        // `persisted` is about the desired state being on disk:
        // after a successful save it is, and when nothing changed
        // it already was (same note as backend_set_offload).
        "persisted": true,
        "applies": "next start",
    }))
}

/// Parse one cache-control value from the wire: a cache type
/// spelling the pinned build accepts among the types v1 offers.
pub(crate) fn parse_cache_type(
    value: &str,
    field: &str,
) -> Result<manager_models::CacheType, ApiError> {
    manager_models::CacheType::parse(value).ok_or_else(|| {
        ApiError::InvalidInput(format!(
            "{field} must be one of f16, q8_0, q4_0; got {value:?}"
        ))
    })
}

/// Parse the flash-attention value from the wire.
pub(crate) fn parse_flash_attn(value: &str) -> Result<manager_models::FlashAttn, ApiError> {
    manager_models::FlashAttn::parse(value).ok_or_else(|| {
        ApiError::InvalidInput(format!(
            "flash_attn must be one of auto, on, off; got {value:?}"
        ))
    })
}

/// Resolve a full cache selection from the three wire values:
/// all three present parses each; all three absent is the
/// cleared state (`None` — no cache tokens); a partial set is
/// refused, because the writer owns the selection as a whole and
/// a partial write would silently freeze the unmentioned members
/// at their current values.
pub(crate) fn resolve_cache_selection(
    cache_k: Option<String>,
    cache_v: Option<String>,
    flash_attn: Option<String>,
) -> Result<Option<manager_models::CacheSelection>, ApiError> {
    match (cache_k, cache_v, flash_attn) {
        (None, None, None) => Ok(None),
        (Some(k), Some(v), Some(f)) => {
            let sel = manager_models::CacheSelection {
                k: parse_cache_type(&k, "cache_k")?,
                v: parse_cache_type(&v, "cache_v")?,
                flash_attn: parse_flash_attn(&f)?,
            };
            if sel.v_quantized_fa_off() {
                return Err(ApiError::InvalidInput(format!(
                    "a quantized V cache ({}) requires flash attention: llama.cpp refuses \
                     it with --flash-attn off — choose flash_attn auto or on",
                    sel.v.as_str()
                )));
            }
            Ok(Some(sel))
        }
        _ => Err(ApiError::InvalidInput(
            "cache_k, cache_v, and flash_attn are set together (or all omitted to clear)"
                .to_string(),
        )),
    }
}

/// A cache fit's per-GPU targets, in placement order: the split
/// shares, the GPUs' total VRAM, the placed GPU indices, and
/// their driver models (the three-state fit's paging basis).
pub(crate) type CacheFitTargets = (Vec<f32>, Vec<u64>, Vec<u32>, Vec<Option<DriverModel>>);

/// The targets a cache fit is priced against for a backend: the
/// GPUs its placement puts it on (per [`backend_gpu_visibility`]),
/// their total VRAM, and the tensor-split shares over exactly
/// those GPUs, in placement order. `None` when no GPU target can
/// be named (CPU-only placement, or a placed GPU absent from the
/// inventory): no fit claim is made then. An unsplit placement
/// yields a single target and an empty split (the whole total on
/// it, [`manager_models::predicted_fits`]' convention).
pub(crate) fn cache_fit_targets(
    b: &manager_config::BackendConfig,
    gpus: &[GpuInfo],
) -> Option<CacheFitTargets> {
    let placed = match backend_gpu_visibility(b) {
        GpuVisibility::Gpus(indices) => indices,
        _ => return None,
    };
    if placed.is_empty() {
        return None;
    }
    let mut totals: Vec<u64> = Vec::new();
    let mut drivers: Vec<Option<DriverModel>> = Vec::new();
    for i in &placed {
        let g = gpus
            .iter()
            .find(|g| g.index == *i)
            .or_else(|| gpus.get(*i as usize))?;
        totals.push(g.total_vram_mib);
        drivers.push(g.driver_model);
    }
    // Split shares over the placed GPUs: the configured split is
    // in inventory order, and the placed set's positions in it
    // are the placed indices (the placement projection's
    // convention). Shares renormalize over the placed set; a
    // split that puts nothing on a placed GPU — or no split at
    // all — is the unsplit case for a single placed GPU and
    // distributes nothing (fits nothing) for several.
    let raw: Vec<f32> = placed
        .iter()
        .map(|i| {
            b.flags
                .tensor_split
                .get(*i as usize)
                .copied()
                .unwrap_or(0.0)
        })
        .collect();
    let sum: f32 = raw.iter().sum();
    let split: Vec<f32> = if sum > 0.0 {
        raw.iter().map(|f| f / sum).collect()
    } else if placed.len() == 1 {
        Vec::new()
    } else {
        raw
    };
    Some((split, totals, placed, drivers))
}

/// The cache preview payload shared by backend_cache_preview and
/// backend_add_preview: the predicted footprint of `selection`
/// at `n_ctx` / `n_seq` under the backend's layout and offload,
/// the full predicted-best ladder priced against the targets,
/// and the best rung. KV figures are absent (null) when the
/// model's KV geometry is unknown — never priced as zero.
pub(crate) fn cache_preview_json(
    layout: &WeightLayout,
    offload: manager_models::ExpertOffload,
    selection: manager_models::CacheSelection,
    n_ctx: u32,
    n_seq: u32,
    moe_cache_mib: u32,
    targets: Option<&CacheFitTargets>,
) -> Value {
    let kv_typed = layout
        .kv_geometry
        .map(|g| g.bytes_per_token_typed(selection.k, selection.v));
    let req = manager_models::memory::memory_requirement_with_tensors(
        layout.total_bytes,
        layout.expert_bytes,
        layout.expert_bytes_by_block.as_deref(),
        layout.basis,
        offload,
        kv_typed.or(layout.kv_per_token),
        layout.moe_layers,
        n_ctx,
        layout.rs_bytes_per_seq,
        n_seq,
        layout.expert_tensors.as_deref(),
        layout.raw_flags.as_deref(),
    );
    // The expert cache prices into the preview's totals the way
    // it prices into the requirement: the whole MiB figure joins
    // the total, and per-target shares distribute it by the
    // cache's own placement (the offload-bearing GPUs), not by
    // the plain tensor split.
    let base_total_mib = req.gpu_weights_mib + req.kv_mib.unwrap_or(0) + req.rs_mib.unwrap_or(0);
    let total_mib = base_total_mib + u64::from(moe_cache_mib);
    let (ladder, best) = match (layout.kv_geometry, targets) {
        (Some(kv), Some((split, totals, _, _))) => {
            let inputs = manager_models::CacheFitInputs {
                kv,
                gpu_weights_mib: req.gpu_weights_mib,
                rs_mib: req.rs_mib,
                n_ctx,
                split: split.clone(),
                gpu_total_mib: totals.clone(),
                moe_cache_mib: u64::from(moe_cache_mib),
            };
            let ladder = manager_models::price_ladder(&inputs);
            let best = ladder.iter().find(|r| r.fits).copied();
            (ladder, best)
        }
        _ => (Vec::new(), None),
    };
    let rung_json = |r: &manager_models::CacheRung| {
        json!({
            "k": r.selection.k.as_str(),
            "v": r.selection.v.as_str(),
            "flash_attn": r.selection.flash_attn.as_str(),
            "kv_mib": r.kv_mib,
            "total_mib": r.total_mib,
            "fits": r.fits,
        })
    };
    json!({
        "selection": {
            "k": selection.k.as_str(),
            "v": selection.v.as_str(),
            "flash_attn": selection.flash_attn.as_str(),
        },
        "v_quantized_fa_off": selection.v_quantized_fa_off(),
        "weights_mib": req.gpu_weights_mib,
        "sys_weights_mib": req.sys_weights_mib,
        "kv_mib": req.kv_mib,
        "rs_mib": req.rs_mib,
        "moe_cache_mib": moe_cache_mib,
        "total_mib": total_mib,
        "basis": serde_json::to_value(req.basis).unwrap_or(Value::Null),
        "targets": targets.map(|(split, totals, placed, drivers)| {
            // Each target carries its share of the total — the
            // same distribution predicted_fits judges — and its
            // own verdict, so the forms render one capacity row
            // per GPU instead of one line against joined totals.
            // The expert-cache share rides its own distribution
            // (the offload-bearing GPUs), added to the base
            // total's plain-split share.
            let shares = manager_models::predicted_shares(base_total_mib, split, totals);
            let cache_shares = manager_models::moe_cache_shares(
                u64::from(moe_cache_mib),
                split,
                layout.offloaded_moe_layers(offload),
                layout.moe_layers.unwrap_or(0),
                placed.len(),
            );
            placed
                .iter()
                .zip(totals.iter())
                .enumerate()
                .map(|(pos, (i, t))| {
                    let required = shares.as_ref().map(|s| s[pos] + cache_shares[pos]);
                    // The three-state classification against the
                    // GPU's TOTAL (the start gate's basis), beside
                    // the headroom `fits` the ladder judges by: a
                    // WDDM GPU over total is a paged fit, not a
                    // hard refusal.
                    let (state, paged) = match required {
                        Some(r) => {
                            let (st, pg) = manager_models::fit_state(
                                r,
                                *t,
                                drivers.get(pos).copied().flatten()
                                    == Some(DriverModel::Wddm),
                            );
                            (Some(st.as_str()), pg)
                        }
                        None => (None, 0),
                    };
                    json!({
                        "gpu": i,
                        "total_mib": t,
                        "required_mib": required,
                        "fits": required.map(|r| {
                            r <= t * (100 - manager_models::HEADROOM_RESERVE_PERCENT) / 100
                        }),
                        "driver_model": drivers.get(pos).copied().flatten().map(|d| match d {
                            DriverModel::Wddm => "wddm",
                            DriverModel::Tcc => "tcc",
                        }),
                        "fit_state": state,
                        "paged_mib": paged,
                    })
                })
                .collect::<Vec<_>>()
        }),
        "headroom_reserve_percent": manager_models::HEADROOM_RESERVE_PERCENT,
        "fits_current": targets.map(|(split, totals, placed, _drivers)| {
            // The same combined distribution the targets carry:
            // base shares by the plain split, cache shares by
            // the cache's own placement — judged under the same
            // headroom rule predicted_fits applies.
            let cache_shares = manager_models::moe_cache_shares(
                u64::from(moe_cache_mib),
                split,
                layout.offloaded_moe_layers(offload),
                layout.moe_layers.unwrap_or(0),
                placed.len(),
            );
            match manager_models::predicted_shares(base_total_mib, split, totals) {
                Some(shares) => shares
                    .iter()
                    .zip(totals.iter())
                    .enumerate()
                    .all(|(pos, (share, t))| {
                        share + cache_shares[pos]
                            <= t * (100 - manager_models::HEADROOM_RESERVE_PERCENT) / 100
                    }),
                None => false,
            }
        }),
        "ladder": ladder.iter().map(rung_json).collect::<Vec<_>>(),
        "best": best.map(|r| rung_json(&r)),
    })
}

/// Preview a backend's cache configuration (the flags form's KV
/// cache / flash-attention controls): the predicted footprint of
/// the given selection at the given context and slot count, and
/// the predicted-best ladder for the backend's placement. Pure
/// preview — nothing is written. The context/slot/selection
/// arguments default to the backend's saved values when omitted,
/// so the form can price its current draft by sending only what
/// changed. The selection defaults likewise (each member
/// independently here, unlike the set op's all-or-nothing rule:
/// a preview freezes nothing).
#[allow(clippy::too_many_arguments)] // argument list is the registry contract
pub async fn op_backend_cache_preview(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    n_ctx: Option<u32>,
    slots: Option<u32>,
    cache_k: Option<String>,
    cache_v: Option<String>,
    flash_attn: Option<String>,
    moe_cache_mib: Option<u32>,
) -> ApiResult {
    // The layout resolves under the config read; the telemetry
    // inventory is fetched after the guard drops (the telemetry
    // accessor reads the config while initializing — it must
    // never be called while this command holds a config guard).
    let (layout, backend) = {
        let cfg = state.config.read().await;
        let b = cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        let layout = resolve_weight_layout(&cfg, b).ok_or_else(|| {
            ApiError::InvalidInput(format!(
                "backend {id}'s model cannot be resolved; its footprint cannot be priced"
            ))
        })?;
        (layout, b.clone())
    };
    let gpus = {
        let store = state.telemetry().await;
        let inventory = lock_telemetry(&store).inventory();
        inventory
    };
    let offload = manager_models::expert_offload(backend.raw_flags.as_deref());
    let saved = manager_models::cache_selection(backend.raw_flags.as_deref());
    let eff_ctx = n_ctx.unwrap_or(backend.flags.n_ctx);
    let eff_slots =
        slots.unwrap_or_else(|| manager_models::effective_slots(backend.raw_flags.as_deref()));
    let targets = cache_fit_targets(&backend, &gpus);
    if let Some(s) = slots {
        validate_parallel_slots(s)?;
    }
    let selection = manager_models::CacheSelection {
        k: match &cache_k {
            Some(v) => parse_cache_type(v, "cache_k")?,
            None => saved.k,
        },
        v: match &cache_v {
            Some(v) => parse_cache_type(v, "cache_v")?,
            None => saved.v,
        },
        flash_attn: match &flash_attn {
            Some(v) => parse_flash_attn(v)?,
            None => saved.flash_attn,
        },
    };
    // The expert-cache size prices as a DRAFT when the caller
    // sends one (the flags form sends its Expert cache control's
    // current state on every preview, so toggling or resizing the
    // cache re-prices the fit block immediately — before this
    // parameter existed the preview read only the SAVED token,
    // and a cache change alone never moved the figures until an
    // unrelated field happened to re-trigger a preview; Pete's
    // Oct 23, 2026 report). Omitted prices the saved token.
    let eff_moe_cache_mib = moe_cache_mib.unwrap_or_else(|| {
        manager_models::moe_cache_mib(backend.raw_flags.as_deref()).unwrap_or(0)
    });
    let mut payload = cache_preview_json(
        &layout,
        offload,
        selection,
        eff_ctx,
        eff_slots.max(1),
        eff_moe_cache_mib,
        targets.as_ref(),
    );
    payload["backend_id"] = json!(id);
    Ok(payload)
}

/// Set a backend's KV cache types and flash-attention mode (the
/// flags form's cache controls): rewrite only the cache tokens
/// in the backend's raw flags to the canonical
/// `--cache-type-k <t> --cache-type-v <t> --flash-attn <m>` —
/// every other flag is preserved. All three values are set
/// together; all three omitted clears the tokens, returning the
/// backend to llama.cpp's defaults with no cache tokens (its
/// pre-feature state). A quantized V cache with flash attention
/// off is refused: llama.cpp fails context creation for that
/// pairing, so persisting it would save a backend that cannot
/// start. Saved through the same clone-validate-save path as the
/// other backend mutations and journaled as a backend-scope
/// activation change with `applies: "next start"`; no restart
/// happens here.
pub async fn op_backend_set_cache(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    cache_k: Option<String>,
    cache_v: Option<String>,
    flash_attn: Option<String>,
) -> ApiResult {
    let target = resolve_cache_selection(cache_k, cache_v, flash_attn)?;
    let mut cfg = state.config.write().await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    // Advanced flags mode (tracker #60): the controls rewrite
    // raw-flag tokens the operator owns as text in the mode.
    ensure_backend_not_advanced(b)?;
    let new_raw = manager_models::set_cache_selection(b.raw_flags.as_deref(), target);
    let changed = new_raw != b.raw_flags;
    b.raw_flags = new_raw.clone();
    if changed {
        // A raw-flags change invalidates the recorded tool-call
        // probe (MCP design §7.2).
        b.tool_call_probe = None;
    }
    if changed {
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        let summary = match target {
            Some(sel) => format!(
                "Backend `{id}` KV cache set to {}/{}, flash attention {}",
                sel.k.as_str(),
                sel.v.as_str(),
                sel.flash_attn.as_str()
            ),
            None => format!("Backend `{id}` KV cache controls cleared (llama.cpp defaults)"),
        };
        let journaled =
            record_backend_change(state, &pre_cfg, &new_cfg, "backend_set_cache", summary, &id)
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
        "selection": target.map(|sel| json!({
            "k": sel.k.as_str(),
            "v": sel.v.as_str(),
            "flash_attn": sel.flash_attn.as_str(),
        })),
        "raw_flags": new_raw,
        "changed": changed,
        // `persisted` is about the desired state being on disk:
        // after a successful save it is, and when nothing changed
        // it already was (same note as backend_set_offload).
        "persisted": true,
        "applies": "next start",
    }))
}

#[cfg(test)]
mod tests;
