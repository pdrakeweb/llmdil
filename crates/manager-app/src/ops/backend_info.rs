//! Per-backend derived info: GGUF weight layout, memory requirement, and
//! the cache / offload / MTP / parallel / MoE-cache info rows.

use crate::*;

/// The configured model a backend serves, resolved by comparing
/// data-dir-resolved file paths (the same rule placement apply and
/// the UI's model derivation use).
pub(crate) fn model_for_backend<'a>(
    cfg: &'a AppConfig,
    b: &manager_config::BackendConfig,
) -> Option<&'a manager_config::ModelConfig> {
    let target = resolve_under_data_dir(&cfg.data_dir, &b.model_file);
    cfg.models
        .iter()
        .find(|m| resolve_under_data_dir(&cfg.data_dir, &m.gguf_path) == target)
}

/// Cache of parsed GGUF layouts, keyed by (path, file length,
/// mtime): a model's header is parsed once per file version, not
/// once per poll. Successful parses are cached, and so are
/// definitive parse failures — bytes the parser rejects outright
/// (`Invalid`, `UnsupportedType`, or `Incomplete` on the complete
/// file) will not parse on the next poll either, and re-reading a
/// few MiB per poll to learn that again is waste. Read failures
/// are NOT cached: a transient I/O error (the file locked or
/// mid-replacement by a download, a briefly unavailable mount)
/// says nothing about the file's contents, so the next poll
/// retries and the backend self-heals to the Exact basis instead
/// of sitting on the catalog fallback for the process lifetime
/// (the Oct 20, 2026 planner-mtp incident: one transient parse
/// failure, memoized as `None`, dropped the backend's KV figure
/// until restart). Metadata failures are not cached either, for
/// the same reason.
#[allow(clippy::type_complexity)]
pub(crate) static GGUF_LAYOUT_CACHE: std::sync::OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<(PathBuf, u64, i64), Option<manager_models::GgufLayout>>,
    >,
> = std::sync::OnceLock::new();

/// Parse a GGUF file's header layout, reading longer prefixes until
/// the parser stops reporting `Incomplete` (headers run a few MiB;
/// the tokenizer metadata dominates). Returns None for missing or
/// unparseable files.
pub(crate) fn gguf_layout_cached(path: &PathBuf) -> Option<manager_models::GgufLayout> {
    gguf_layout_cached_with(path, &GGUF_PREFIX_RUNGS)
}

/// The prefix lengths [`gguf_layout_cached`] reads, shortest first.
pub(crate) const GGUF_PREFIX_RUNGS: [usize; 3] =
    [8 * 1024 * 1024, 64 * 1024 * 1024, 256 * 1024 * 1024];

/// [`gguf_layout_cached`] over an explicit prefix ladder (tests use
/// a short one).
pub(crate) fn gguf_layout_cached_with(
    path: &PathBuf,
    rungs: &[usize],
) -> Option<manager_models::GgufLayout> {
    let meta = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return None,
        Err(e) => {
            tracing::debug!(
                "gguf layout: metadata read failed for {} (error class: io): {e}",
                path.display()
            );
            return None;
        }
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let key = (path.clone(), meta.len(), mtime);
    let cache =
        GGUF_LAYOUT_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(hit) = guard.get(&key) {
            return hit.clone();
        }
    }
    let mut layout = None;
    // Every exit from this loop that does not return early is
    // definitive and cached: a layout, a rejection of the complete
    // bytes, or (P-4) a header still `Incomplete` at the largest
    // rung — the next poll would re-read and re-allocate every rung
    // only to learn the same thing. The (len, mtime) key drops the
    // entry as soon as the file changes.
    for &prefix in rungs {
        let want = (meta.len() as usize).min(prefix);
        let mut buf = vec![0u8; want];
        if let Err(e) =
            std::fs::File::open(path).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        {
            // Transient by definition: nothing about the file's
            // contents was learned. Log and leave the cache alone
            // so the next poll retries.
            tracing::debug!(
                "gguf layout: read failed for {} (error class: io): {e}",
                path.display()
            );
            return None;
        }
        match manager_models::parse_gguf_layout(&buf) {
            Ok(l) => {
                layout = Some(l);
                break;
            }
            Err(manager_models::GgufError::Incomplete) if want < meta.len() as usize => continue,
            Err(e) => {
                let class = match e {
                    manager_models::GgufError::Incomplete => "incomplete header",
                    manager_models::GgufError::Invalid(_) => "invalid header",
                    manager_models::GgufError::UnsupportedType(_) => "unsupported tensor type",
                };
                tracing::debug!(
                    "gguf layout: parse failed for {} (error class: {class}): {e:?}",
                    path.display()
                );
                break;
            }
        }
    }
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, layout.clone());
    }
    layout
}

/// A backend's model weight layout, resolved once for the memory
/// requirement and the offload slider: total weight bytes, the
/// routed-expert share, the MoE layer count, KV bytes per token,
/// and the derivation basis. The layout comes from the downloaded
/// GGUF when available (basis Exact), else the catalog's pins
/// (Catalog — expert bytes, MoE layer count, and KV bytes per
/// token are all pinned, so a partial offload and the KV figure
/// resolve before download), else a
/// parameter-count estimate (Estimate). None when the backend's
/// model cannot be resolved.
pub(crate) struct WeightLayout {
    pub(crate) total_bytes: u64,
    pub(crate) expert_bytes: Option<u64>,
    /// Per-MoE-block expert bytes from the GGUF parse, when the
    /// layout came from one — partial offload is priced from the
    /// true first-N block sums, not a uniform share.
    pub(crate) expert_bytes_by_block: Option<Vec<u64>>,
    /// The routed-expert tensor index (names and sizes) from the
    /// GGUF parse, when the layout came from one and the model has
    /// experts: operator `-ot` patterns and `--n-cpu-moe`'s
    /// block-index rule are priced against it (P-5).
    pub(crate) expert_tensors: Option<std::sync::Arc<[manager_models::gguf::ExpertTensor]>>,
    /// The backend raw flags this layout was resolved for — the
    /// override list `expert_tensors` is evaluated under.
    pub(crate) raw_flags: Option<String>,
    pub(crate) moe_layers: Option<u32>,
    /// The GGUF KV geometry, when the layout came from a parse —
    /// the input cache-type-aware KV pricing needs (each side
    /// priced at its own type). `kv_per_token` below is the same
    /// geometry at the f16 default.
    pub(crate) kv_geometry: Option<manager_models::KvGeometry>,
    pub(crate) kv_per_token: Option<u64>,
    /// Recurrent-state bytes per sequence from the GGUF header's
    /// SSM geometry (hybrid architectures), same provenance as
    /// `kv_per_token`.
    pub(crate) rs_bytes_per_seq: Option<u64>,
    pub(crate) basis: manager_models::MemoryBasis,
    /// The curated catalog entry for the backend's model, when it
    /// is a catalog model (the offload slider's green limit and
    /// default live there).
    pub(crate) catalog: Option<manager_models::ModelSpec>,
    /// The offload slider's green limit (in MoE layers) for the
    /// backend's model: the catalog's pinned default when the
    /// catalog carries one, else the operator's declared default
    /// on the model record ([`ModelConfig::default_offload_layers`])
    /// — the fallback that gives a user-added MoE model offload
    /// controls. `None` when neither source pins one.
    pub(crate) green_limit_layers: Option<u32>,
}

impl WeightLayout {
    /// The model's MoE block map (which block indices carry
    /// experts), when the layout came from a GGUF parse with an
    /// expert tensor index: what converts the slider's MoE-layer
    /// count to and from `--n-cpu-moe`'s block-index bound (code
    /// review v0.1.60 item 11). `None` for catalog-only and
    /// estimated layouts, where the conversion is the identity.
    pub(crate) fn moe_block_map(&self) -> Option<manager_models::MoeBlockMap> {
        self.expert_tensors
            .as_deref()
            .and_then(manager_models::MoeBlockMap::from_tensors)
    }

    /// MoE layers on system RAM under `offload`, through the block
    /// map (see [`manager_models::ExpertOffload::moe_layers_offloaded`]).
    pub(crate) fn offloaded_moe_layers(&self, offload: manager_models::ExpertOffload) -> u32 {
        offload.moe_layers_offloaded(self.moe_layers.unwrap_or(0), self.moe_block_map().as_ref())
    }
}

pub(crate) fn resolve_weight_layout(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
) -> Option<WeightLayout> {
    let model = model_for_backend(cfg, b)?;
    let path = resolve_under_data_dir(&cfg.data_dir, &model.gguf_path);
    let layout = gguf_layout_cached(&path);
    let catalog = manager_models::curated_models()
        .into_iter()
        .find(|s| s.id == model.id);
    let file_len = std::fs::metadata(&path)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len());
    // One source per figure set, and the basis labels THAT source:
    // when the GGUF parses, the total is the parse's own tensor
    // sum — the previous version paired the catalog's (or the file
    // length's) total with the parse's expert bytes and labeled
    // the mixture Exact, a label neither source earned
    // (code-review 4.12). Without a parse, the catalog's pins are
    // Catalog; anything else is an Estimate.
    let (
        total_bytes,
        expert_bytes,
        expert_bytes_by_block,
        kv_geometry,
        kv_per_token,
        rs_bytes_per_seq,
        moe_layers,
        basis,
    ) = match (&layout, &catalog) {
        (Some(l), _) => (
            l.tensor_bytes,
            Some(l.expert_bytes),
            Some(l.expert_bytes_by_block.clone()),
            l.kv,
            l.kv.map(|k| k.bytes_per_token()),
            l.rs.map(|g| g.bytes_per_seq()),
            l.moe_block_count,
            manager_models::MemoryBasis::Exact,
        ),
        (None, Some(c)) => (
            c.size_bytes,
            c.expert_bytes,
            None,
            None,
            c.kv_bytes_per_token,
            None,
            c.moe_layers,
            manager_models::MemoryBasis::Catalog,
        ),
        (None, None) => (
            file_len.unwrap_or_else(|| {
                manager_wizard::steps::estimate_model_bytes(model.params_b as f64)
            }),
            None,
            None,
            None,
            None,
            None,
            None,
            manager_models::MemoryBasis::Estimate,
        ),
    };
    // The slider's green limit resolves per figure, catalog
    // first: a catalog pin wins; where the catalog carries no
    // default (or no entry at all), the operator's declared
    // default on the model record supplies it.
    let green_limit_layers = catalog
        .as_ref()
        .and_then(|c| c.default_offload_layers)
        .or(model.default_offload_layers);
    // The tensor index rides only with a parse that found experts:
    // a dense file prices offload exactly as before.
    let expert_tensors = layout
        .as_ref()
        .map(|l| l.expert_tensors.clone())
        .filter(|t| !t.is_empty());
    Some(WeightLayout {
        total_bytes,
        expert_bytes,
        expert_bytes_by_block,
        expert_tensors,
        raw_flags: b.raw_flags.clone(),
        moe_layers,
        kv_geometry,
        kv_per_token,
        rs_bytes_per_seq,
        basis,
        catalog,
        green_limit_layers,
    })
}

/// A backend's memory requirement (GPU-resident weights, system-RAM
/// weights, KV at its configured context, recurrent-state cache at
/// its effective slot count) under its current serving
/// configuration — see [`manager_models::memory_requirement`]. None
/// when the backend's model cannot be resolved.
pub(crate) fn backend_memory_requirement(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
) -> Option<manager_models::MemoryRequirement> {
    let layout = resolve_weight_layout(cfg, b)?;
    let offload = manager_models::expert_offload(b.raw_flags.as_deref());
    let mut req = manager_models::memory::memory_requirement_with_tensors(
        layout.total_bytes,
        layout.expert_bytes,
        layout.expert_bytes_by_block.as_deref(),
        layout.basis,
        offload,
        kv_bytes_per_token_for(&layout, b.raw_flags.as_deref()),
        layout.moe_layers,
        b.flags.n_ctx,
        layout.rs_bytes_per_seq,
        manager_models::effective_slots(b.raw_flags.as_deref()),
        layout.expert_tensors.as_deref(),
        b.raw_flags.as_deref(),
    );
    // The expert cache rides the requirement: the decoded token
    // plus the layer geometry the requirement's per-GPU
    // distribution needs to price it (see
    // `manager_models::moe_cache_shares`), so every consumer of
    // the requirement — the start gate, residency, fit, the
    // placement projection — prices the cache the server will
    // allocate. A backend with no token prices exactly as before.
    req.moe_cache_mib = manager_models::moe_cache_mib(b.raw_flags.as_deref()).unwrap_or(0) as u64;
    req.moe_cache_offload_layers = layout.offloaded_moe_layers(offload);
    req.moe_cache_moe_layers = layout.moe_layers.unwrap_or(0);
    Some(req)
}

/// KV bytes per token for a backend under its SELECTED cache
/// types: the layout's KV geometry priced at the types decoded
/// from the backend's raw flags (see
/// [`manager_models::cache_selection`]), so the requirement —
/// and with it the start gate and every footprint figure the app
/// shows — prices the configuration actually selected, not the
/// f16 default. A backend with no cache tokens decodes to the
/// f16 default and prices exactly as before this feature. When
/// the layout carries no geometry, the f16 figure (if any) is
/// all that exists and is used unchanged.
pub(crate) fn kv_bytes_per_token_for(
    layout: &WeightLayout,
    raw_flags: Option<&str>,
) -> Option<u64> {
    match layout.kv_geometry {
        Some(geo) => {
            let sel = manager_models::cache_selection(raw_flags);
            Some(geo.bytes_per_token_typed(sel.k, sel.v))
        }
        None => layout.kv_per_token,
    }
}

pub(crate) struct CacheInfo {
    /// The decoded (effective) cache selection.
    pub(crate) selection: manager_models::CacheSelection,
    /// Whether any cache token is present in the saved flags —
    /// false for a pre-feature backend, whose row renders the
    /// controls at the decoded defaults without implying a saved
    /// setting.
    pub(crate) present: bool,
    /// True when the saved spelling is not the canonical form
    /// for its state (a `-ctk` or `=` spelling, an
    /// absent-member selection, ...). The same comparison as the
    /// MTP control's `custom`.
    pub(crate) custom: bool,
}

/// A backend's cache control state. Unlike the offload and MTP
/// states this is not capability gated: the cache controls render
/// for every configured backend, so every configured row carries
/// it (the Parallel slots field's posture).
pub(crate) fn backend_cache_info(b: &manager_config::BackendConfig) -> CacheInfo {
    let selection = manager_models::cache_selection(b.raw_flags.as_deref());
    let present = manager_models::cache_tokens_present(b.raw_flags.as_deref());
    let canonical = if present {
        manager_models::set_cache_selection(b.raw_flags.as_deref(), Some(selection))
    } else {
        b.raw_flags.clone()
    };
    let custom = token_normalized_flags(b.raw_flags.as_deref())
        != token_normalized_flags(canonical.as_deref());
    CacheInfo {
        selection,
        present,
        custom,
    }
}

/// The `cache` object a backend row carries: the decoded
/// selection, its presence, and the custom-spelling marker.
pub(crate) fn cache_row_json(c: &CacheInfo) -> Value {
    json!({
        "k": c.selection.k.as_str(),
        "v": c.selection.v.as_str(),
        "flash_attn": c.selection.flash_attn.as_str(),
        "present": c.present,
        "custom": c.custom,
    })
}

pub(crate) struct OffloadInfo {
    /// MoE layers in total (the slider's maximum).
    pub(crate) moe_layers: u32,
    /// Current slider position: MoE layers on system RAM.
    pub(crate) layers: u32,
    pub(crate) expert_bytes: u64,
    pub(crate) total_bytes: u64,
    /// The green limit in layers (the model's default offload —
    /// the catalog pin, or the operator's declared default for a
    /// model outside the catalog).
    pub(crate) green_limit_layers: u32,
    /// Slider band boundaries as fractions (see
    /// [`manager_models::offload_bands`]).
    pub(crate) green_end: f64,
    pub(crate) yellow_end: f64,
    /// System-RAM weight bytes at every slider position
    /// `0..=moe_layers` (see
    /// [`manager_models::sys_bytes_by_position`]).
    pub(crate) sys_bytes_by_position: Vec<u64>,
    /// True when the saved offload spelling is not the canonical
    /// form for its position (a hand-written `--cpu-moe`, an
    /// `--n-cpu-moe=46` `=` spelling, ...). Whitespace-only
    /// differences are not custom: the comparison is over
    /// whitespace-split tokens re-joined.
    pub(crate) custom: bool,
    /// The catalog's per-architecture offload restrictions for the
    /// backend's model (empty outside the curated catalog).
    pub(crate) restrictions: Vec<manager_models::OffloadRestriction>,
    /// The model's MoE block map when the GGUF was parsed: `layers`
    /// and every position written back convert through it
    /// ([`OffloadInfo::offload_for`]). `None` = identity (catalog
    /// or estimated layout).
    pub(crate) block_map: Option<manager_models::MoeBlockMap>,
}

impl OffloadInfo {
    /// The offload a slider position (`layers` MoE layers on
    /// system RAM) writes: `--n-cpu-moe` at the block-index bound
    /// that moves exactly that many MoE layers (see
    /// [`manager_models::offload_for_moe_layers`]).
    pub(crate) fn offload_for(&self, layers: u32) -> manager_models::ExpertOffload {
        manager_models::offload_for_moe_layers(layers, self.moe_layers, self.block_map.as_ref())
    }

    /// The `--n-cpu-moe` value each position `0..=moe_layers`
    /// writes (the identity without a block map).
    pub(crate) fn n_cpu_moe_by_position(&self) -> Vec<u32> {
        (0..=self.moe_layers)
            .map(|k| match &self.block_map {
                Some(map) => map.bound_for_layers(k),
                None => k,
            })
            .collect()
    }
}

pub(crate) fn backend_offload_info(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
) -> Option<OffloadInfo> {
    let layout = resolve_weight_layout(cfg, b)?;
    let expert_bytes = layout.expert_bytes?;
    let moe_layers = layout.moe_layers?;
    // The bands derive from the model's green limit (the catalog
    // default, or the operator's declared default for a model the
    // catalog does not carry), so a model with no green limit
    // from either source gets no slider rather than invented
    // band boundaries.
    let green_limit_layers = layout.green_limit_layers?;
    if expert_bytes == 0 || moe_layers == 0 {
        return None;
    }
    let (green_end, yellow_end) = manager_models::offload_bands(moe_layers, green_limit_layers);
    let offload = manager_models::expert_offload(b.raw_flags.as_deref());
    // The slider counts MoE layers; the saved `--n-cpu-moe` value
    // is a block-index bound. With the GGUF's block map the
    // position reads back as the MoE blocks the bound moves (code
    // review v0.1.60 item 11); without one it is the value itself.
    let block_map = layout.moe_block_map();
    let layers = offload.moe_layers_offloaded(moe_layers, block_map.as_ref());
    // Custom spelling: rewriting the detected position back through
    // the canonical writer must reproduce the saved tokens exactly
    // (after token normalization, so spacing alone never counts).
    // With a block map the canonical form is the one the slider
    // writes for the position, so a bound between MoE blocks (or
    // past the last one) reads as custom.
    let canonical_offload = match (offload, &block_map) {
        (manager_models::ExpertOffload::CpuMoeLayers(_), Some(map)) => {
            manager_models::offload_for_moe_layers(layers, moe_layers, Some(map))
        }
        _ => offload,
    };
    let canonical = manager_models::set_expert_offload(b.raw_flags.as_deref(), canonical_offload);
    let custom = token_normalized_flags(b.raw_flags.as_deref())
        != token_normalized_flags(canonical.as_deref());
    let restrictions = layout
        .catalog
        .as_ref()
        .map(|c| c.offload_restrictions.clone())
        .unwrap_or_default();
    let sys_bytes_by_position = manager_models::memory::sys_bytes_by_position_with_tensors(
        expert_bytes,
        layout.expert_bytes_by_block.as_deref(),
        moe_layers,
        layout.expert_tensors.as_deref(),
    );
    Some(OffloadInfo {
        moe_layers,
        layers,
        block_map,
        expert_bytes,
        total_bytes: layout.total_bytes,
        green_limit_layers,
        green_end,
        yellow_end,
        sys_bytes_by_position,
        custom,
        restrictions,
    })
}

/// The `offload` object a backend row carries — the slider state
/// plus its guardrail object — shared by list_backends and the add
/// form's preview (backend_add_preview), so the add form renders
/// the offload control from exactly the payload the Edit Flags form
/// renders from. The guardrail is emitted when a placed GPU is
/// governed by a restriction (status by position), or when the
/// placement is unknown (empty placed set, not CPU-only) while the
/// model carries restrictions and the saved position is partial
/// (status untested, against the first restriction's architecture);
/// null otherwise.
pub(crate) fn offload_row_json(o: &OffloadInfo, eval: Option<&OffloadEvaluation>) -> Value {
    let guardrail = eval.and_then(|eval| {
        let status = serde_json::to_value(eval.status).unwrap_or(Value::Null);
        if let Some(cc) = eval.restricting_cc {
            let restriction = eval
                .restrictions
                .iter()
                .find(|r| r.compute_capability == cc)?;
            Some(json!({
                "compute_capability": format!("{}.{}", cc.0, cc.1),
                "allowed": serde_json::to_value(restriction.allowed)
                    .unwrap_or(Value::Null),
                "status": status,
                "nearest_verified_layers": eval.nearest_verified_layers,
                "has_override": eval.has_override,
                "overridden": eval.overridden,
                // The placed compute-capability set the override
                // key names (guardrail-override UI): the card's
                // confirmation and the set/clear operations carry
                // exactly this set.
                "placed_ccs": format_placed_ccs(&eval.placed_ccs),
            }))
        } else if eval.placed_ccs.is_empty()
            && !eval.cpu_only
            && !eval.restrictions.is_empty()
            && eval.layers != 0
            && eval.layers != eval.moe_layers
        {
            let first = &eval.restrictions[0];
            Some(json!({
                "compute_capability": format!(
                    "{}.{}",
                    first.compute_capability.0, first.compute_capability.1
                ),
                "allowed": serde_json::to_value(first.allowed)
                    .unwrap_or(Value::Null),
                "status": status,
                "nearest_verified_layers": eval.nearest_verified_layers,
                "has_override": eval.has_override,
                "overridden": eval.overridden,
                "placed_ccs": format_placed_ccs(&eval.placed_ccs),
            }))
        } else {
            None
        }
    });
    json!({
        "moe_layers": o.moe_layers,
        "layers": o.layers,
        "expert_bytes": o.expert_bytes,
        "total_bytes": o.total_bytes,
        "green_limit_layers": o.green_limit_layers,
        "green_end": o.green_end,
        "yellow_end": o.yellow_end,
        "sys_bytes_by_position": o.sys_bytes_by_position,
        // The `--n-cpu-moe` value each slider position writes
        // (index = MoE layers on system RAM): the block-index bound
        // that moves exactly that many MoE layers (review item 11).
        // Endpoints are written as no tokens / `-ot exps=CPU`; their
        // entries are the bounds for reference only. Additive.
        "n_cpu_moe_by_position": o.n_cpu_moe_by_position(),
        "custom": o.custom,
        "guardrail": guardrail,
    })
}

pub(crate) struct MtpInfo {
    /// Whether `--spec-type draft-mtp` is in the saved raw flags.
    pub(crate) enabled: bool,
    /// The explicit `--spec-draft-n-max`, or `None` when MTP is on
    /// without one (llama.cpp's default then applies) or off.
    pub(crate) n_max: Option<u32>,
    /// The explicit `--parallel` slot count, when the raw flags
    /// set one. MTP drafting needs a single slot; the control
    /// compares this against 1 to surface the conflict — the MTP
    /// control itself never rewrites it (the flags form's
    /// Parallel slots field is the write path, tracker #61).
    pub(crate) parallel: Option<i64>,
    /// True when the saved MTP spelling is not the canonical form
    /// for its state (a bare `--spec-type draft-mtp` with no
    /// length token, an `=` spelling, ...). The same comparison
    /// as the offload slider's `custom`: the decoded state
    /// rewritten canonically must reproduce the saved tokens
    /// exactly (after token normalization, so spacing alone never
    /// counts).
    pub(crate) custom: bool,
}

/// A backend's MTP control state, when its model is MTP-capable.
/// Capability is the EFFECTIVE capability resolved live (see
/// [`manager_models::effective_mtp`]): the curated catalog's pin
/// where the catalog knows the model (by id or source URL), else
/// the operator's declaration on the model record — never
/// inferred from a model's name or file — so a backend whose
/// capability is unknown gets no control rather than a
/// speculative one.
pub(crate) fn backend_mtp_info(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
) -> Option<MtpInfo> {
    let model = model_for_backend(cfg, b)?;
    if !manager_models::effective_mtp(&model.id, &model.source_url, model.mtp) {
        return None;
    }
    let state = manager_models::mtp_draft(b.raw_flags.as_deref());
    let canonical = manager_models::set_mtp_draft(b.raw_flags.as_deref(), state);
    let custom = token_normalized_flags(b.raw_flags.as_deref())
        != token_normalized_flags(canonical.as_deref());
    Some(MtpInfo {
        enabled: matches!(state, manager_models::MtpDraft::On(_)),
        n_max: match state {
            manager_models::MtpDraft::On(n) => n,
            manager_models::MtpDraft::Off => None,
        },
        parallel: manager_models::parallel_slots(b.raw_flags.as_deref()),
        custom,
    })
}

pub(crate) struct ParallelInfo {
    /// The explicit `--parallel` slot count, or `None` when the
    /// raw flags set none — llama.cpp's default (auto) then
    /// applies (see [`manager_models::parallel`]).
    pub(crate) slots: Option<i64>,
    /// True when the saved parallel spelling is not the canonical
    /// form for its state (an `-np` or `=` spelling, repeated
    /// tokens, a non-canonical position). The same comparison as
    /// the offload/MTP `custom`: the decoded state rewritten
    /// canonically must reproduce the saved tokens exactly (after
    /// token normalization, so spacing alone never counts).
    pub(crate) custom: bool,
}

/// A backend's parallel-slots control state (tracker #61).
/// Unlike the offload and MTP states this is not capability
/// gated: the Parallel slots field renders for every configured
/// backend, so every configured row carries it.
pub(crate) fn backend_parallel_info(b: &manager_config::BackendConfig) -> ParallelInfo {
    let slots = manager_models::parallel_slots(b.raw_flags.as_deref());
    let canonical = manager_models::set_parallel_slots(b.raw_flags.as_deref(), slots);
    let custom = token_normalized_flags(b.raw_flags.as_deref())
        != token_normalized_flags(canonical.as_deref());
    ParallelInfo { slots, custom }
}

pub(crate) struct MoeCacheInfo {
    /// The decoded `--moe-cache-mib` value, or `None` when the
    /// token is absent (off). A decoded 0 — the pin's own
    /// disabled value, never written by the control — also reads
    /// as `None`.
    pub(crate) mib: Option<u32>,
    /// True when the saved spelling is not the canonical form
    /// for its state (an `=` spelling, a decoded 0 token). The
    /// same comparison as the MTP control's `custom`.
    pub(crate) custom: bool,
    /// The backend's saved offload position (MoE layers on
    /// system RAM): the UI disables the control at 0, when there
    /// are no host-resident experts for the cache to hold.
    pub(crate) offload_layers: u32,
    /// The model's MoE layer count (the offload slider's
    /// maximum).
    pub(crate) moe_layers: u32,
}

/// A backend's expert-cache control state. Eligibility is the
/// offload slider's exactly: the state exists iff
/// [`backend_offload_info`] resolves (a resolvable layout with
/// expert bytes and a MoE layer count), so the control appears
/// for precisely the backends that can offload — MoE models only,
/// with effectiveness resolved from the same live layout facts.
pub(crate) fn backend_moe_cache_info(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
) -> Option<MoeCacheInfo> {
    let offload = backend_offload_info(cfg, b)?;
    let mib = manager_models::moe_cache_mib(b.raw_flags.as_deref()).filter(|n| *n > 0);
    let canonical = manager_models::set_moe_cache(b.raw_flags.as_deref(), mib);
    let custom = token_normalized_flags(b.raw_flags.as_deref())
        != token_normalized_flags(canonical.as_deref());
    Some(MoeCacheInfo {
        mib,
        custom,
        offload_layers: offload.layers,
        moe_layers: offload.moe_layers,
    })
}

#[cfg(test)]
mod tests;
