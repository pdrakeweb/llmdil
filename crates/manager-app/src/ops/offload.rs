//! Backend expert offload: set / override operations, the offload
//! guardrail evaluation, and the post-change offload probe.

use crate::*;

/// Set a backend's expert offload (the Edit Flags form's offload
/// slider): rewrite only the offload tokens in the backend's raw
/// flags to the canonical form for `layers` MoE layers on system
/// RAM — 0 is no offload, the full MoE layer count is the
/// `-ot exps=CPU` form, anything between is `--n-cpu-moe N` —
/// leaving every other flag untouched. The model must be
/// offload-capable (see [`backend_offload_info`]). Saved through
/// the same clone-validate-save path as the other backend
/// mutations and journaled as a backend-scope activation change;
/// applies on the backend's next start.
///
/// Guardrails (tracker #37): when the requested position is
/// blocked for the model's pinned restriction on the GPUs the
/// backend currently runs on, the save is refused unless
/// `allow_override` is set — in which case a durable override is
/// journaled (see `activation::OffloadOverride`). A non-blocked
/// save clears any standing override for the backend.
pub async fn op_backend_set_offload(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    layers: u32,
    allow_override: Option<bool>,
) -> ApiResult {
    // Capability is resolved before the write lock (resolution can
    // parse a GGUF header); the layer count is validated against
    // it here and the flags are rewritten under the lock.
    let moe_layers = {
        let cfg = state.config.read().await;
        let b = cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        // Advanced flags mode (tracker #60): the slider rewrites
        // raw-flag tokens the operator owns as text in the mode.
        ensure_backend_not_advanced(b)?;
        backend_offload_info(&cfg, b)
            .map(|o| o.moe_layers)
            .ok_or_else(|| {
                ApiError::InvalidInput(format!(
                    "backend {id}'s model does not support expert offload"
                ))
            })?
    };
    if layers > moe_layers {
        return Err(ApiError::InvalidInput(format!(
            "offload of {layers} layers exceeds the model's {moe_layers} MoE layers"
        )));
    }
    // Guardrail inputs fetched between the config reads: the GPU
    // inventory (telemetry access must never hold the config lock —
    // see ensure_backend_specs) and the activation journal (the
    // standing override records live there).
    let gpus = {
        let store = state.telemetry().await;
        let inventory = lock_telemetry(&store).inventory();
        inventory
    };
    let journal = activation::load_journal(&state.data_dir);
    let (mut cfg, live) = config_write_for_journal(state).await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    // Re-resolve and re-validate UNDER the write lock (code-review
    // 4.8): the pre-lock resolution can be stale — a concurrent
    // model assignment changes which model this backend serves,
    // and with it the MoE layer count the request was validated
    // against. Computed before the mutable borrow is taken, and it
    // shadows the pre-lock value: everything below uses the
    // under-lock count.
    let (eval, block_map) = {
        let b_ref = new_cfg
            .backends
            .iter()
            .find(|x| x.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        // Re-checked under the write lock (tracker #60): the mode
        // can have been entered since the pre-lock check.
        ensure_backend_not_advanced(b_ref)?;
        let info = backend_offload_info(&new_cfg, b_ref).ok_or_else(|| {
            ApiError::InvalidInput(format!(
                "backend {id}'s model does not support expert offload"
            ))
        })?;
        let (placed_ccs, cpu_only) = placed_offload_placement(b_ref, &gpus);
        let eval = evaluate_offload_position(
            &info.restrictions,
            &id,
            layers,
            info.moe_layers,
            placed_ccs,
            cpu_only,
            &journal,
        );
        (eval, info.block_map)
    };
    let moe_layers = eval.moe_layers;
    // Guardrail enforcement (tracker #37), still before any
    // mutation: a blocked requested position refuses the save
    // unless the operator explicitly overrides it. An existing
    // override does NOT grandfather a fresh save — the override is
    // requested again at each blocked save, so the decision point
    // stays where the UI can explain it.
    let eval_blocked = eval.status == manager_models::OffloadStatus::Blocked;
    if eval_blocked && allow_override != Some(true) {
        if let Some(reason) = offload_block_reason(
            &eval.restrictions,
            layers,
            moe_layers,
            &eval.placed_ccs,
            eval.cpu_only,
            false,
        ) {
            return Err(ApiError::InvalidInput(reason));
        }
    }
    let b = new_cfg
        .backends
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    if layers > moe_layers {
        return Err(ApiError::InvalidInput(format!(
            "offload of {layers} layers exceeds the model's {moe_layers} MoE layers"
        )));
    }
    // `layers` is a MoE-layer count; the flag is written at the
    // block-index bound that moves exactly that many (item 11).
    let target = manager_models::offload_for_moe_layers(layers, moe_layers, block_map.as_ref());
    let new_raw = manager_models::set_expert_offload(b.raw_flags.as_deref(), target);
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
            manager_models::ExpertOffload::None => {
                format!("Backend `{id}` expert offload turned off")
            }
            manager_models::ExpertOffload::AllExperts => {
                format!("Backend `{id}` expert offload set to all {moe_layers} MoE layers")
            }
            manager_models::ExpertOffload::CpuMoeLayers(_) => {
                format!("Backend `{id}` expert offload set to {layers} of {moe_layers} MoE layers")
            }
            // offload_for_moe_layers never selects operator patterns.
            manager_models::ExpertOffload::TensorOverrides => {
                format!("Backend `{id}` expert offload set by tensor overrides")
            }
        };
        let journaled = record_backend_change(
            state,
            &live,
            &pre_cfg,
            &new_cfg,
            "backend_set_offload",
            summary,
            &id,
        )
        .await?;
        save_journaled(state, &new_cfg, &journaled)?;
        *cfg = new_cfg;
    }
    drop(cfg);
    // Override bookkeeping: the command has succeeded (a config
    // save error above returns before this point, so no override
    // is ever recorded for a save that did not land). It applies
    // even when `changed` is false — authorizing a standing value
    // is the point of the override. A blocked save that proceeded
    // records the override at the placed CCs evaluated under the
    // lock; any non-blocked save clears a standing override (a
    // verified save supersedes it).
    let mut overridden = false;
    if eval_blocked {
        if activation::record_offload_override(
            &state.data_dir,
            &id,
            layers,
            eval.placed_ccs.clone(),
        )
        .is_ok()
        {
            overridden = true;
        }
    } else {
        let _ = activation::clear_offload_overrides(&state.data_dir, &id);
    }
    if changed {
        ensure_backend_specs(state).await;
    }
    state.sync_backend_service_definition(&id).await;
    Ok(json!({
        "backend_id": id,
        "layers": layers,
        "raw_flags": new_raw,
        "changed": changed,
        "overridden": overridden,
        // `persisted` is about the desired state being on disk:
        // after a successful save it is, and when nothing changed
        // it already was (code-review 4.16 — reporting false for
        // the already-correct state made the UI warn about a loss
        // that never happened).
        "persisted": true,
        "applies": "next start",
    }))
}

/// Parse one placed compute-capability string ("6.0") for the
/// guardrail-override operations. The override key names the placed
/// set in exactly this spelling (the spelling `list_backends`
/// emits in the guardrail's `placed_ccs`), so a malformed entry is
/// refused rather than normalized into a different key.
pub(crate) fn parse_placed_cc(text: &str) -> Result<(u32, u32), ApiError> {
    let trimmed = text.trim();
    let mut parts = trimmed.split('.');
    let (Some(major), Some(minor), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(ApiError::InvalidInput(format!(
            "placed compute capability {text:?} is not in \"major.minor\" form"
        )));
    };
    let parse = |part: &str| {
        part.parse::<u32>().map_err(|_| {
            ApiError::InvalidInput(format!(
                "placed compute capability {text:?} is not in \"major.minor\" form"
            ))
        })
    };
    Ok((parse(major)?, parse(minor)?))
}

pub(crate) fn format_placed_ccs(ccs: &[(u32, u32)]) -> Vec<String> {
    ccs.iter().map(|cc| format!("{}.{}", cc.0, cc.1)).collect()
}

/// Evaluate a backend's SAVED offload position for the deliberate
/// guardrail-override operations: resolve the backend and its
/// offload capability from a config read, fetch the GPU inventory
/// without holding the config lock (the telemetry accessor reads
/// the config while initializing), and evaluate against the
/// activation journal's standing overrides.
pub(crate) async fn evaluate_saved_offload_for_override(
    state: &Arc<AppState>,
    id: &str,
) -> Result<OffloadEvaluation, ApiError> {
    let cfg = state.config.read().await.clone();
    let b = cfg
        .backends
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    let gpus = {
        let store = state.telemetry().await;
        let inventory = lock_telemetry(&store).inventory();
        inventory
    };
    let journal = activation::load_journal(&state.data_dir);
    evaluate_backend_offload(&cfg, b, &gpus, &journal).ok_or_else(|| {
        ApiError::InvalidInput(format!(
            "backend {id}'s model does not support expert offload"
        ))
    })
}

/// Record a deliberate guardrail override for a backend's SAVED
/// offload position (guardrail-override UI; tracker #37 follow-on).
///
/// The override is a standing activation-journal record — the
/// mechanism #37 shipped — keyed to exactly (backend, layers,
/// placed compute-capability set). This operation writes no config
/// and no flags: it authorizes the position the backend already
/// carries, which is the case the slider path cannot reach (a
/// blocked position that arrived by hand-edited flags or a model
/// reassignment, where there is no pending slider save to attach
/// the override to). Because there is no config mutation, the
/// clone-validate-save discipline applies to the journal record
/// itself: the key is derived and validated from the live config,
/// placement, and catalog restrictions before the journal's
/// locked load-modify-save records it, and the write is read back
/// for coverage before success is reported.
///
/// Exactness is enforced at this boundary, never widened: the
/// caller names the key it confirmed (`layers` + `placed_ccs`, as
/// rendered on the backend card), and any mismatch with the saved
/// position or the current placement refuses — an override granted
/// for one layers/CC key covers no other, and a placement change
/// since the confirmation requires a fresh confirmation. A
/// position that is not guardrail-blocked refuses as well: an
/// override is meaningful only for a blocked position, and
/// recording one for a verified position would leave a standing
/// authorization whose scope no reader could see.
pub async fn op_backend_set_offload_override(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    layers: u32,
    placed_ccs: Vec<String>,
) -> ApiResult {
    let mut expected_ccs: Vec<(u32, u32)> = Vec::new();
    for text in &placed_ccs {
        expected_ccs.push(parse_placed_cc(text)?);
    }
    let eval = evaluate_saved_offload_for_override(state, &id).await?;
    if layers != eval.layers {
        return Err(ApiError::InvalidInput(format!(
            "override layers {layers} do not match backend {id}'s saved offload position \
             ({} of {} layers) — an override covers exactly the saved position; \
             re-read the backend and confirm the position shown",
            eval.layers, eval.moe_layers
        )));
    }
    let expected_set: std::collections::BTreeSet<(u32, u32)> =
        expected_ccs.iter().copied().collect();
    let placed_set: std::collections::BTreeSet<(u32, u32)> =
        eval.placed_ccs.iter().copied().collect();
    if expected_set != placed_set {
        return Err(ApiError::InvalidInput(format!(
            "override placed compute-capability set [{}] does not match backend {id}'s \
             current placement [{}] — the placement changed since the confirmation; \
             re-read the backend and confirm the placement shown",
            format_placed_ccs(&expected_ccs).join(", "),
            format_placed_ccs(&eval.placed_ccs).join(", ")
        )));
    }
    if eval.status != manager_models::OffloadStatus::Blocked {
        return Err(ApiError::InvalidInput(format!(
            "backend {id}'s saved offload position ({} of {} layers) is not \
             guardrail-blocked on its placed GPUs — no override applies",
            eval.layers, eval.moe_layers
        )));
    }
    activation::record_offload_override(&state.data_dir, &id, layers, eval.placed_ccs.clone())
        .map_err(ApiError::Io)?;
    let journal = activation::load_journal(&state.data_dir);
    if !activation::offload_override_covers(&journal, &id, layers, &eval.placed_ccs) {
        return Err(ApiError::Io(format!(
            "the offload override for backend {id} was recorded but does not read back as covering its position"
        )));
    }
    Ok(json!({
        "backend_id": id,
        "layers": layers,
        "moe_layers": eval.moe_layers,
        "placed_ccs": format_placed_ccs(&eval.placed_ccs),
        "has_override": true,
        "overridden": true,
        "persisted": true,
    }))
}

/// Clear a backend's standing guardrail override (guardrail-
/// override UI; tracker #37 follow-on). After a clear, the saved
/// position evaluates exactly as it did before the override was
/// recorded: a blocked position is blocked again at launch
/// preflight. Clearing is idempotent — clearing a backend with no
/// standing override succeeds and reports `cleared: false` — and
/// touches only the activation journal: no config, no flags.
pub async fn op_backend_clear_offload_override(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    {
        let cfg = state.config.read().await;
        if !cfg.backends.iter().any(|b| b.id == id) {
            return Err(ApiError::NotFound(format!("backend {id}")));
        }
    }
    let had_override = activation::load_journal(&state.data_dir)
        .offload_overrides
        .iter()
        .any(|o| o.backend_id == id);
    if had_override {
        activation::clear_offload_overrides(&state.data_dir, &id).map_err(ApiError::Io)?;
    }
    // Report the position's standing after the clear when the
    // backend is offload-capable; a backend whose model carries no
    // offload simply has nothing left to override.
    let eval = evaluate_saved_offload_for_override(state, &id).await.ok();
    Ok(json!({
        "backend_id": id,
        "cleared": had_override,
        "layers": eval.as_ref().map(|e| e.layers),
        "moe_layers": eval.as_ref().map(|e| e.moe_layers),
        "placed_ccs": eval.as_ref().map(|e| format_placed_ccs(&e.placed_ccs)),
        "has_override": false,
        "overridden": false,
        "persisted": true,
    }))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OffloadEvaluation {
    pub(crate) layers: u32,
    pub(crate) moe_layers: u32,
    pub(crate) status: manager_models::OffloadStatus,
    /// First placed compute capability a restriction governs, when
    /// any placed GPU is restricted.
    pub(crate) restricting_cc: Option<(u32, u32)>,
    /// Nearest allowed position under the governing restriction
    /// (the position itself when nothing governs it).
    pub(crate) nearest_verified_layers: u32,
    /// A journaled override exists for exactly this position on
    /// exactly this placement.
    pub(crate) has_override: bool,
    /// The position is blocked AND a journaled override covers it.
    pub(crate) overridden: bool,
    pub(crate) placed_ccs: Vec<(u32, u32)>,
    pub(crate) cpu_only: bool,
    pub(crate) restrictions: Vec<manager_models::OffloadRestriction>,
}

/// Evaluate a backend's SAVED offload position against the GPU
/// inventory and the activation journal's standing overrides.
/// `None` when the backend's model is not offload-capable.
pub(crate) fn evaluate_backend_offload(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
    gpus: &[GpuInfo],
    journal: &activation::ActivationJournal,
) -> Option<OffloadEvaluation> {
    let info = backend_offload_info(cfg, b)?;
    let (placed_ccs, cpu_only) = placed_offload_placement(b, gpus);
    Some(evaluate_offload_position(
        &info.restrictions,
        &b.id,
        info.layers,
        info.moe_layers,
        placed_ccs,
        cpu_only,
        journal,
    ))
}

/// Evaluate one offload position from its parts (the pure core of
/// [`evaluate_backend_offload`], shared with the apply path, which
/// evaluates the REQUESTED position rather than the saved one).
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_offload_position(
    restrictions: &[manager_models::OffloadRestriction],
    backend_id: &str,
    layers: u32,
    moe_layers: u32,
    placed_ccs: Vec<(u32, u32)>,
    cpu_only: bool,
    journal: &activation::ActivationJournal,
) -> OffloadEvaluation {
    let status =
        manager_models::offload_status(restrictions, layers, moe_layers, &placed_ccs, cpu_only);
    let restricting_cc = placed_ccs
        .iter()
        .copied()
        .find(|cc| restrictions.iter().any(|r| r.compute_capability == *cc));
    // The nearest allowed position is computed against the
    // restricting architecture; with no placed architecture it is
    // computed against the first restriction's, so an unknown
    // placement still offers the snap target for its own CC.
    let nearest_cc = restricting_cc.or_else(|| {
        if placed_ccs.is_empty() {
            restrictions.first().map(|r| r.compute_capability)
        } else {
            None
        }
    });
    let nearest_verified_layers = match nearest_cc {
        Some(cc) => manager_models::nearest_allowed(restrictions, cc, layers, moe_layers),
        None => layers,
    };
    let has_override =
        activation::offload_override_covers(journal, backend_id, layers, &placed_ccs);
    OffloadEvaluation {
        layers,
        moe_layers,
        status,
        restricting_cc,
        nearest_verified_layers,
        has_override,
        overridden: status == manager_models::OffloadStatus::Blocked && has_override,
        placed_ccs,
        cpu_only,
        restrictions: restrictions.to_vec(),
    }
}

/// The refusal reason for an offload position, when one applies:
/// `Some` iff the position is guardrail-blocked and no journaled
/// override covers it. Pure — the preflight, the apply path, and
/// the tests all go through this one function.
pub(crate) fn offload_block_reason(
    restrictions: &[manager_models::OffloadRestriction],
    layers: u32,
    moe_layers: u32,
    placed_ccs: &[(u32, u32)],
    cpu_only: bool,
    overridden: bool,
) -> Option<String> {
    if overridden {
        return None;
    }
    if manager_models::offload_status(restrictions, layers, moe_layers, placed_ccs, cpu_only)
        != manager_models::OffloadStatus::Blocked
    {
        return None;
    }
    // Blocked implies a placed CC matches a restriction; the first
    // such CC names the architecture in the message and drives the
    // nearest-allowed snap.
    let cc = placed_ccs
        .iter()
        .copied()
        .find(|cc| restrictions.iter().any(|r| r.compute_capability == *cc))?;
    let nearest = manager_models::nearest_allowed(restrictions, cc, layers, moe_layers);
    Some(format!(
        "offload of {layers} of {moe_layers} layers is outside the verified range for this GPU (compute capability {}.{}) — set the offload to {nearest} of {moe_layers} layers in Edit Flags, or save the position with an explicit override",
        cc.0, cc.1
    ))
}

/// A backend's GPU placement for guardrail evaluation: the placed
/// GPUs' compute capabilities, and whether the launch is CPU-only.
/// Undetermined placement yields an empty set (architecture unknown).
pub(crate) fn placed_offload_placement(
    b: &manager_config::BackendConfig,
    gpus: &[GpuInfo],
) -> (Vec<(u32, u32)>, bool) {
    match backend_gpu_visibility(b) {
        GpuVisibility::CpuOnly => (Vec::new(), true),
        GpuVisibility::Gpus(indices) => (
            indices
                .iter()
                .filter_map(|i| {
                    gpus.iter()
                        .find(|g| g.index == *i)
                        .map(|g| g.compute_capability)
                })
                .collect(),
            false,
        ),
        GpuVisibility::Undetermined => (Vec::new(), false),
    }
}

/// Raw flags normalized for comparison: whitespace-split tokens
/// re-joined with single spaces; `None` when nothing remains.
pub(crate) fn token_normalized_flags(raw: Option<&str>) -> Option<String> {
    let joined = raw
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Whether a saved offload change for `id` is still awaiting this
/// start: the activation journal carries an unresolved backend-scope
/// `backend_set_offload` entry targeting it.
pub(crate) async fn offload_change_pending(state: &AppState, cfg: &AppConfig, id: &str) -> bool {
    let journal = activation::load_journal(&state.data_dir);
    let started_at = backend_start_times(state, cfg).await;
    let configured: std::collections::HashSet<String> =
        cfg.backends.iter().map(|b| b.id.clone()).collect();
    activation::unresolved_entries_scoped(&state.data_dir, &journal, &started_at, &configured)
        .iter()
        .any(|e| {
            e.scope == "backend"
                && e.kind == "backend_set_offload"
                && e.targets.iter().any(|t| t == id)
        })
}

/// The offload probe context for a start of `id`, when the start
/// activates a pending offload change: (layers, moe_layers, port)
/// of the SAVED position. Captured before the spawn, after the
/// preflight passes — a start that the preflight refuses never
/// probes, and the position probed is the one being activated.
pub(crate) async fn offload_probe_context(state: &AppState, id: &str) -> Option<(u32, u32, u16)> {
    let cfg = state.config.read().await.clone();
    if !offload_change_pending(state, &cfg, id).await {
        return None;
    }
    let b = cfg.backends.iter().find(|b| b.id == id)?;
    let info = backend_offload_info(&cfg, b)?;
    Some((info.layers, info.moe_layers, b.port))
}

/// Whether the backend's supervised process is still alive (Running
/// or Starting). Reaps first, so a process that exited since the
/// last poll reads as gone here, not as its last live state: this
/// reap-plus-status check is how the probe detects process death.
pub(crate) async fn offload_probe_backend_alive(state: &AppState, id: &str) -> bool {
    let mut sup = state.supervisor().await.lock().await;
    sup.reap_exited();
    sup.status()
        .iter()
        .find(|s| s.id == id)
        .map(|s| matches!(s.state, ProcessState::Running | ProcessState::Starting))
        .unwrap_or(false)
}

/// Mark `id` as probing and spawn its background probe task.
pub(crate) fn spawn_offload_probe(
    sink: &Arc<dyn EventSink>,
    state: &Arc<AppState>,
    id: &str,
    layers: u32,
    moe_layers: u32,
    port: u16,
) {
    {
        let mut probes = state
            .offload_probes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        probes.insert(
            id.to_string(),
            OffloadProbe {
                state: "probing".to_string(),
                layers,
                detail: None,
            },
        );
    }
    let probe_sink = Arc::clone(sink);
    let probe_state = Arc::clone(state);
    let probe_id = id.to_string();
    tokio::spawn(async move {
        run_offload_probe(probe_sink, probe_state, probe_id, layers, moe_layers, port).await;
    });
}

/// Record a failed offload probe and toast it. The detail names the
/// position and the verified alternative, or the request error —
/// never a claim that any position is certified.
pub(crate) async fn fail_offload_probe(
    sink: &dyn EventSink,
    state: &AppState,
    backend_id: &str,
    layers: u32,
    detail: String,
) {
    {
        let mut probes = state
            .offload_probes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        probes.insert(
            backend_id.to_string(),
            OffloadProbe {
                state: "failed".to_string(),
                layers,
                detail: Some(detail.clone()),
            },
        );
    }
    notify::notify(
        sink,
        state,
        notify::Category::Failures,
        "Offload probe failed",
        &detail,
    )
    .await;
}

/// Timing knobs for the offload probe's HTTP sequence. Production
/// values come from [`OffloadProbeTiming::production`]; tests shrink
/// them so the sequence runs in milliseconds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OffloadProbeTiming {
    /// How long to wait for the backend to report ready (GET
    /// /health 2xx) before concluding the probe cannot run.
    pub(crate) ready_budget: Duration,
    /// Delay between readiness polls.
    pub(crate) ready_poll: Duration,
    /// After readiness, how long transient probe-request failures
    /// (HTTP 503, transport errors) are retried before the failure
    /// is reported as terminal, measured from the first transient
    /// failure.
    pub(crate) retry_window: Duration,
    /// Delay between probe-request retries.
    pub(crate) retry_poll: Duration,
}

impl OffloadProbeTiming {
    pub(crate) fn production() -> Self {
        Self {
            ready_budget: Duration::from_secs(180),
            ready_poll: Duration::from_secs(2),
            retry_window: Duration::from_secs(60),
            retry_poll: Duration::from_secs(3),
        }
    }
}

/// What the offload probe's HTTP sequence concluded. Process death
/// is detected through the alive check, never inferred from HTTP
/// traffic, so a crashed backend is never reclassified as "not
/// ready".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OffloadProbeHttpOutcome {
    /// The backend reported ready and the probe request succeeded.
    Passed,
    /// The supervised process exited (or vanished) at some point
    /// during the sequence.
    BackendStopped,
    /// The backend never reported ready within the readiness
    /// budget, so the probe request was never sent. This is not
    /// evidence about the offload position.
    NotReady,
    /// The backend reported ready and the probe request then
    /// failed terminally: a non-transient HTTP status, or a
    /// transient failure that persisted past the retry window.
    /// Carries the failure text (e.g. "HTTP 500").
    FailedAfterReady(String),
}

/// Liveness check consulted between readiness polls and probe
/// attempts: true while the backend's process is alive.
pub(crate) type OffloadProbeAlive<'a> = dyn FnMut() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>
    + Send
    + 'a;

/// The offload probe's HTTP sequence, split from
/// [`run_offload_probe`] so tests can drive it against a stub
/// backend: wait for the app's readiness signal (GET /health 2xx —
/// the definition the supervisor's health probe and the tool-call
/// probe both use), then send the long probe prompt, retrying
/// transient failures for a bounded window.
///
/// The readiness gate must NOT be a chat request: `test_request`
/// returns Ok for any JSON-parseable response regardless of status,
/// and llama-server answers chat completions with HTTP 503 (with a
/// JSON error body) the whole time the model is loading. A
/// status-blind gate therefore passes the moment the listener is
/// up — minutes before the backend can serve — and the probe then
/// fails on that same 503. That was the v0.1.38 field failure
/// behind tracker #46.
pub(crate) async fn offload_probe_http_sequence(
    base_url: &str,
    timing: OffloadProbeTiming,
    alive: &mut OffloadProbeAlive<'_>,
) -> OffloadProbeHttpOutcome {
    // Readiness phase: poll GET /health until it reports 2xx.
    let health = HttpHealthProbe::new(
        format!("{}/health", base_url.trim_end_matches('/')),
        Duration::from_secs(2),
    );
    let ready_deadline = std::time::Instant::now() + timing.ready_budget;
    loop {
        if !alive().await {
            return OffloadProbeHttpOutcome::BackendStopped;
        }
        if matches!(
            health.probe().await,
            Ok(manager_supervisor::HealthStatus::Healthy)
        ) {
            break;
        }
        if std::time::Instant::now() >= ready_deadline {
            return OffloadProbeHttpOutcome::NotReady;
        }
        tokio::time::sleep(timing.ready_poll).await;
    }
    // Probe phase: one long prompt (about 4,000 tokens of prompt
    // processing, exercised across many 512-token ubatches) at the
    // newly activated offload position. HTTP 503 is llama-server's
    // not-ready/busy answer and a transport error can race the last
    // of the startup work, so both retry within the window; any
    // other failure — or a transient one that outlasts the window —
    // is terminal.
    let prompt = "The quick brown fox jumps over the lazy dog. ".repeat(450);
    // The window opens at the first transient failure, not before
    // the first request: a slow first attempt must not consume the
    // whole window, so a transient failure is always retried at
    // least once (and the outcome no longer depends on how long
    // that first request happened to take).
    let mut retry_deadline: Option<std::time::Instant> = None;
    loop {
        let result = test_request(base_url, &prompt).await;
        // Process death outranks a request error: a dead backend's
        // last request also fails, and the stopped detail is the
        // actionable one.
        if !alive().await {
            return OffloadProbeHttpOutcome::BackendStopped;
        }
        let (failure, transient) = match result {
            Ok(r) if r.status < 400 => return OffloadProbeHttpOutcome::Passed,
            Ok(r) => (format!("HTTP {}", r.status), r.status == 503),
            Err(e) => (e.to_string(), true),
        };
        let now = std::time::Instant::now();
        let deadline = *retry_deadline.get_or_insert(now + timing.retry_window);
        if !transient || now >= deadline {
            return OffloadProbeHttpOutcome::FailedAfterReady(failure);
        }
        tokio::time::sleep(timing.retry_poll).await;
    }
}

/// The background offload probe: wait for the backend to report
/// ready, then send one long prompt (about 4,000 tokens of prompt
/// processing, exercised across many 512-token ubatches) at the
/// newly activated offload position, confirming the process is
/// still alive throughout. A passed probe means only that the
/// probe completed.
pub(crate) async fn run_offload_probe(
    sink: Arc<dyn EventSink>,
    state: Arc<AppState>,
    backend_id: String,
    layers: u32,
    moe_layers: u32,
    port: u16,
) {
    let base_url = format!("http://127.0.0.1:{port}");
    let stopped_detail = format!(
        "the backend stopped during the offload probe at the {layers} of {moe_layers} layers offload position — this position is unverified on this GPU; full offload ({moe_layers} of {moe_layers} layers) is the verified position"
    );
    let timing = OffloadProbeTiming::production();
    let mut alive = || {
        let state = Arc::clone(&state);
        let backend_id = backend_id.clone();
        Box::pin(async move { offload_probe_backend_alive(&state, &backend_id).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
    };
    let outcome = offload_probe_http_sequence(&base_url, timing, &mut alive).await;
    match outcome {
        OffloadProbeHttpOutcome::Passed => {
            let mut probes = state
                .offload_probes
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            probes.insert(
                backend_id.clone(),
                OffloadProbe {
                    state: "passed".to_string(),
                    layers,
                    detail: None,
                },
            );
        }
        OffloadProbeHttpOutcome::BackendStopped => {
            fail_offload_probe(sink.as_ref(), &state, &backend_id, layers, stopped_detail).await;
        }
        OffloadProbeHttpOutcome::NotReady => {
            fail_offload_probe(
                sink.as_ref(),
                &state,
                &backend_id,
                layers,
                format!(
                    "the backend did not become ready within {}s, so the offload probe could not run — no conclusion about the {layers} of {moe_layers} layers offload position; this position is unverified on this GPU; full offload ({moe_layers} of {moe_layers} layers) is the verified position",
                    timing.ready_budget.as_secs()
                ),
            )
            .await;
        }
        OffloadProbeHttpOutcome::FailedAfterReady(failure) => {
            fail_offload_probe(
                sink.as_ref(),
                &state,
                &backend_id,
                layers,
                format!(
                    "the offload probe request failed at the {layers} of {moe_layers} layers offload position: {failure}"
                ),
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests;
