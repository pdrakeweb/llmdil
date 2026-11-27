//! The fit evaluator (design §5): for an item that touches model,
//! placement, offload, or context, recompute the affected backend's
//! memory requirement under the proposed configuration and check it
//! against free VRAM per GPU — the telemetry snapshot's free VRAM
//! minus the other backends' accounted residency, the same
//! accounting the placement projection performs. The verdict
//! machinery is the existing [`manager_models::fit_check`], fed a
//! spec whose size is the GPU-resident requirement (weights + KV),
//! so offload-aware requirements are what get placed.

use std::collections::HashMap;

use manager_config::AppConfig;
use manager_models::{ExpertOffload, MemoryBasis, MemoryRequirement};

use crate::context::resolve_model;

/// A model's weight-layout facts, resolved by the caller (GGUF
/// parse, catalog pins, or estimate — the resolution src-tauri's
/// `resolve_weight_layout` performs). Keyed by model id.
#[derive(Debug, Clone)]
pub struct LayoutFacts {
    pub total_bytes: u64,
    pub expert_bytes: Option<u64>,
    pub expert_bytes_by_block: Option<Vec<u64>>,
    pub moe_layers: Option<u32>,
    pub kv_per_token: Option<u64>,
    /// KV geometry, when known — the input cache-type-aware KV
    /// pricing needs. `kv_per_token` above is the same geometry
    /// at the f16 default, and the fallback when no geometry (or
    /// no typed pricing) is available.
    pub kv_geometry: Option<manager_models::KvGeometry>,
    /// Recurrent-state bytes per sequence (GGUF recurrent-state
    /// geometry); None for dense models and unknown geometry.
    pub rs_bytes_per_seq: Option<u64>,
    pub basis: MemoryBasis,
    /// Rated maximum context (catalog `ctx_max`); 0 = unknown.
    pub ctx_max: u32,
    pub params_b: f32,
    /// The model's MoE block map, when the facts came from a GGUF
    /// parse with an expert tensor index (code review v0.1.60
    /// item 11): the offload unit everywhere in the plan model is
    /// MoE layers, while `--n-cpu-moe` takes a block-index bound;
    /// [`LayoutFacts::offload_for`] and
    /// [`LayoutFacts::offloaded_layers`] convert through it.
    /// `None` (catalog or estimate facts) is the identity.
    pub moe_block_map: Option<manager_models::MoeBlockMap>,
}

impl LayoutFacts {
    /// The offload a position of `layers` MoE layers writes
    /// (`--n-cpu-moe` at its block-index bound; the endpoints as
    /// no offload / all experts). `None` when the MoE layer count
    /// is unknown.
    pub fn offload_for(&self, layers: u32) -> Option<ExpertOffload> {
        let moe = self.moe_layers?;
        Some(manager_models::offload_for_moe_layers(
            layers,
            moe,
            self.moe_block_map.as_ref(),
        ))
    }

    /// MoE layers on system RAM under `offload` (a `--n-cpu-moe`
    /// bound reads back as the MoE blocks below it).
    pub fn offloaded_layers(&self, offload: ExpertOffload) -> u32 {
        offload.moe_layers_offloaded(self.moe_layers.unwrap_or(0), self.moe_block_map.as_ref())
    }
}

/// A backend's memory requirement from layout facts — the same
/// arithmetic `backend_memory_requirement` performs in src-tauri.
/// `n_seq` is the backend's effective slot count (the
/// recurrent-state term is per sequence). `cache` is the
/// backend's selected KV cache configuration: when the facts
/// carry KV geometry the KV term is priced at the selected
/// types, exactly as the app-side requirement prices it;
/// without geometry the f16 figure stands (typed pricing has
/// nothing to price from). `moe_cache_mib` is the backend's
/// decoded expert-cache size (0 when unset): it rides the
/// requirement as its own term, with the offload position and
/// MoE layer count the per-GPU distribution needs.
///
/// `--n-cpu-moe B` is priced through the facts' own
/// [`LayoutFacts::moe_block_map`] (see [`layout_requirement_mapped`]);
/// without one, as the first B MoE blocks — exact only when the
/// model's MoE blocks are blocks `0..`.
pub fn layout_requirement(
    facts: &LayoutFacts,
    offload: ExpertOffload,
    cache: manager_models::CacheSelection,
    n_ctx: u32,
    n_seq: u32,
    moe_cache_mib: u32,
) -> MemoryRequirement {
    layout_requirement_mapped(
        facts,
        facts.moe_block_map.as_ref(),
        offload,
        cache,
        n_ctx,
        n_seq,
        moe_cache_mib,
    )
}

/// [`layout_requirement`] with the model's MoE block map (from a
/// GGUF layout with an expert tensor index). llama.cpp reads
/// `--n-cpu-moe B` as a BLOCK-INDEX bound — the experts of blocks
/// with index `< B`, leading or interleaved dense blocks counting
/// toward `B` (code review v0.1.60 item 11) — so with a map the
/// bound is converted to the MoE layers it really moves
/// ([`manager_models::MoeBlockMap::layers_for_bound`]) before the
/// per-MoE-block pricing, and the expert-cache term's offload
/// position is that same count. `None` (catalog-only facts) keeps
/// the first-N-MoE-blocks approximation.
pub fn layout_requirement_mapped(
    facts: &LayoutFacts,
    block_map: Option<&manager_models::MoeBlockMap>,
    offload: ExpertOffload,
    cache: manager_models::CacheSelection,
    n_ctx: u32,
    n_seq: u32,
    moe_cache_mib: u32,
) -> MemoryRequirement {
    let moe_layers = facts.moe_layers.unwrap_or(0);
    let offloaded = offload.moe_layers_offloaded(moe_layers, block_map);
    let offload = match (offload, block_map) {
        (ExpertOffload::CpuMoeLayers(_), Some(_)) => ExpertOffload::CpuMoeLayers(offloaded),
        (offload, _) => offload,
    };
    let kv_per_token = match facts.kv_geometry {
        Some(geo) => Some(geo.bytes_per_token_typed(cache.k, cache.v)),
        None => facts.kv_per_token,
    };
    let mut req = manager_models::memory_requirement(
        facts.total_bytes,
        facts.expert_bytes,
        facts.expert_bytes_by_block.as_deref(),
        facts.basis,
        offload,
        kv_per_token,
        facts.moe_layers,
        n_ctx,
        facts.rs_bytes_per_seq,
        n_seq,
    );
    req.moe_cache_mib = moe_cache_mib as u64;
    req.moe_cache_offload_layers = offloaded;
    req.moe_cache_moe_layers = moe_layers;
    req
}

/// Total GPU-resident MiB of a requirement (weights + KV when the
/// KV geometry is known + recurrent-state cache when its geometry
/// is known + the MoE expert cache when one is set).
pub fn gpu_total_mib(req: &MemoryRequirement) -> u64 {
    req.gpu_weights_mib + req.kv_mib.unwrap_or(0) + req.rs_mib.unwrap_or(0) + req.moe_cache_mib
}

/// Distribute a requirement's GPU-resident total over the GPUs by
/// tensor split. An empty split is an unsplit launch: the whole
/// model sits on the first GPU (the placement projection's
/// convention). A split whose length differs from the GPU count is
/// stale — it earns no distribution and the caller treats the
/// backend as holding nothing accountable.
///
/// The expert-cache term distributes by its own rule
/// ([`manager_models::moe_cache_shares`]), not by the tensor
/// split: llama.cpp splits the one cache budget only among the
/// GPUs holding offloaded layers, renormalized over them, so a
/// partial-offload split placement can owe its whole cache to
/// one GPU.
pub fn residency_per_gpu(req: &MemoryRequirement, split: &[f32], n_gpus: usize) -> Vec<u64> {
    let mut out = vec![0u64; n_gpus];
    if n_gpus == 0 {
        return out;
    }
    let base = gpu_total_mib(req).saturating_sub(req.moe_cache_mib);
    if split.is_empty() {
        out[0] = base;
    } else if split.len() == n_gpus {
        let mut assigned = 0u64;
        for (i, frac) in split.iter().enumerate() {
            let share = if i + 1 == split.len() {
                base.saturating_sub(assigned)
            } else {
                (base as f64 * (*frac as f64)).round() as u64
            };
            out[i] = share;
            assigned += share;
        }
    } else {
        // A stale split distributes nothing — the base stays
        // undistributed, and the cache shares nothing either
        // (moe_cache_shares applies the same rule).
        return out;
    }
    let cache_shares = manager_models::moe_cache_shares(
        req.moe_cache_mib,
        split,
        req.moe_cache_offload_layers,
        req.moe_cache_moe_layers,
        n_gpus,
    );
    for (i, share) in cache_shares.iter().enumerate() {
        out[i] += share;
    }
    out
}

/// The outcome of one fit evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct FitOutcome {
    /// The requirement under the proposed configuration.
    pub requirement: MemoryRequirement,
    /// The model can be placed in the free VRAM that remains after
    /// running backends' residency is accounted.
    pub placeable: bool,
    /// Placement succeeds only by ignoring a stopped backend's
    /// accounted residency — it fits now, but not alongside that
    /// backend when it starts (design §5: a warning).
    pub only_by_moving_accounted: bool,
    /// The proposed split puts more on this GPU than its adjusted
    /// free VRAM, though the model is placeable in total (a
    /// different split would fit).
    pub split_overflow_gpu: Option<usize>,
}

fn fit_verdict(total_mib: u64, free: &[u64]) -> manager_models::FitVerdict {
    let spec = manager_models::ModelSpec {
        id: String::new(),
        name: String::new(),
        quant: String::new(),
        size_bytes: total_mib * manager_models::MIB,
        sha256: String::new(),
        ctx_max: 0,
        params_b: 0.0,
        enabled_by_default: false,
        expert_bytes: None,
        moe_layers: None,
        kv_bytes_per_token: None,
        default_offload_layers: None,
        url: String::new(),
        offload_restrictions: Vec::new(),
        mtp: false,
    };
    manager_models::fit_check(&spec, free)
}

/// Evaluate the fit of one backend's proposed requirement.
///
/// `free_vram_mib_per_gpu` is the telemetry snapshot's free VRAM in
/// GPU-index order. `others_running` / `others_accounted` are the
/// per-GPU residency of every *other* backend, split by whether
/// that backend is currently running (its weights are actually in
/// VRAM) or merely configured (accounted, but not held).
pub fn evaluate_fit(
    requirement: &MemoryRequirement,
    split: &[f32],
    free_vram_mib_per_gpu: &[u64],
    others_running: &[u64],
    others_accounted: &[u64],
) -> FitOutcome {
    let n = free_vram_mib_per_gpu.len();
    let total = gpu_total_mib(requirement);
    if n == 0 {
        // No telemetry: nothing can be accounted, so no fit claim
        // is made either way — the caller treats this as unchecked.
        return FitOutcome {
            requirement: requirement.clone(),
            placeable: true,
            only_by_moving_accounted: false,
            split_overflow_gpu: None,
        };
    }
    let sub = |free: &[u64], a: &[u64], b: Option<&[u64]>| -> Vec<u64> {
        (0..n)
            .map(|i| {
                let mut f = free[i].saturating_sub(a.get(i).copied().unwrap_or(0));
                if let Some(b) = b {
                    f = f.saturating_sub(b.get(i).copied().unwrap_or(0));
                }
                f
            })
            .collect()
    };
    let adj_running = sub(free_vram_mib_per_gpu, others_running, None);
    let placeable = !matches!(
        fit_verdict(total, &adj_running),
        manager_models::FitVerdict::TooLarge { .. }
    );
    let adj_all = sub(
        free_vram_mib_per_gpu,
        others_running,
        Some(others_accounted),
    );
    let fits_with_all = !matches!(
        fit_verdict(total, &adj_all),
        manager_models::FitVerdict::TooLarge { .. }
    );
    let only_by_moving_accounted = placeable && !fits_with_all;
    let proposed = residency_per_gpu(requirement, split, n);
    let split_overflow_gpu = (0..n).find(|&i| proposed[i] > adj_running[i]);
    FitOutcome {
        requirement: requirement.clone(),
        placeable,
        only_by_moving_accounted,
        split_overflow_gpu: if placeable { split_overflow_gpu } else { None },
    }
}

/// Per-GPU residency of every backend in `cfg` except
/// `exclude_id`, split into (running, accounted) buckets by the
/// live snapshot's running set. Backends whose model has no layout
/// facts contribute nothing (their requirement is unknown, and
/// unknown is not zero — it is simply unaccountable here).
pub fn others_residency(
    cfg: &AppConfig,
    layouts: &HashMap<String, LayoutFacts>,
    running: &HashMap<String, bool>,
    exclude_id: &str,
    n_gpus: usize,
) -> (Vec<u64>, Vec<u64>) {
    others_residency_mapped(cfg, layouts, &HashMap::new(), running, exclude_id, n_gpus)
}

/// [`others_residency`] with per-model MoE block maps (keyed by
/// model id, as `layouts` is): each backend's `--n-cpu-moe` bound
/// is priced by block index where its model has a map (see
/// [`layout_requirement_mapped`]).
pub fn others_residency_mapped(
    cfg: &AppConfig,
    layouts: &HashMap<String, LayoutFacts>,
    block_maps: &HashMap<String, manager_models::MoeBlockMap>,
    running: &HashMap<String, bool>,
    exclude_id: &str,
    n_gpus: usize,
) -> (Vec<u64>, Vec<u64>) {
    let mut running_res = vec![0u64; n_gpus];
    let mut accounted_res = vec![0u64; n_gpus];
    for b in &cfg.backends {
        if b.id == exclude_id {
            continue;
        }
        let Some(model) = resolve_model(cfg, b) else {
            continue;
        };
        let Some(facts) = layouts.get(&model.id) else {
            continue;
        };
        let offload = manager_models::expert_offload(b.raw_flags.as_deref());
        let req = layout_requirement_mapped(
            facts,
            block_maps.get(&model.id).or(facts.moe_block_map.as_ref()),
            offload,
            manager_models::cache_selection(b.raw_flags.as_deref()),
            b.flags.n_ctx,
            manager_models::effective_slots(b.raw_flags.as_deref()),
            manager_models::moe_cache_mib(b.raw_flags.as_deref()).unwrap_or(0),
        );
        let per = residency_per_gpu(&req, &b.flags.tensor_split, n_gpus);
        let bucket = if running.get(&b.id).copied().unwrap_or(false) {
            &mut running_res
        } else {
            &mut accounted_res
        };
        for (i, v) in per.iter().enumerate() {
            bucket[i] += v;
        }
    }
    (running_res, accounted_res)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> LayoutFacts {
        LayoutFacts {
            total_bytes: 12 * 1024 * manager_models::MIB,
            expert_bytes: None,
            expert_bytes_by_block: None,
            moe_layers: None,
            kv_per_token: None,
            kv_geometry: None,
            rs_bytes_per_seq: None,
            basis: MemoryBasis::Estimate,
            ctx_max: 0,
            params_b: 12.0,
            moe_block_map: None,
        }
    }

    #[test]
    fn residency_empty_split_is_first_gpu() {
        let req = layout_requirement(
            &facts(),
            ExpertOffload::None,
            manager_models::CacheSelection::default(),
            32768,
            4,
            0,
        );
        assert_eq!(residency_per_gpu(&req, &[], 2), vec![12 * 1024, 0]);
        assert_eq!(
            residency_per_gpu(&req, &[0.5, 0.5], 2),
            vec![6 * 1024, 6 * 1024]
        );
        // A stale split (wrong GPU count) distributes nothing.
        assert_eq!(residency_per_gpu(&req, &[1.0], 2), vec![0, 0]);
    }

    fn moe_facts() -> LayoutFacts {
        LayoutFacts {
            total_bytes: 12 * 1024 * manager_models::MIB,
            expert_bytes: Some(6 * 1024 * manager_models::MIB),
            expert_bytes_by_block: None,
            moe_layers: Some(48),
            kv_per_token: None,
            kv_geometry: None,
            rs_bytes_per_seq: None,
            basis: MemoryBasis::Estimate,
            ctx_max: 0,
            params_b: 12.0,
            moe_block_map: None,
        }
    }

    #[test]
    fn n_cpu_moe_is_priced_by_block_index_when_a_block_map_is_known() {
        // 2 leading dense blocks + 4 MoE blocks (2..=5), 1 GiB of
        // experts each.
        const GIB: u64 = 1024 * manager_models::MIB;
        let facts = LayoutFacts {
            total_bytes: 8 * GIB,
            expert_bytes: Some(4 * GIB),
            expert_bytes_by_block: Some(vec![GIB; 4]),
            moe_layers: Some(4),
            kv_per_token: None,
            kv_geometry: None,
            rs_bytes_per_seq: None,
            basis: MemoryBasis::Exact,
            ctx_max: 0,
            params_b: 8.0,
            moe_block_map: None,
        };
        let map = manager_models::MoeBlockMap::from_block_indices([2, 3, 4, 5]).unwrap();
        let price = |map: Option<&manager_models::MoeBlockMap>, bound: u32| {
            layout_requirement_mapped(
                &facts,
                map,
                ExpertOffload::CpuMoeLayers(bound),
                manager_models::CacheSelection::default(),
                0,
                1,
                1024,
            )
        };
        // `--n-cpu-moe 4` moves blocks 2 and 3: 2 MoE layers.
        let r = price(Some(&map), 4);
        assert_eq!(r.sys_weights_mib, 2 * 1024);
        assert_eq!(r.moe_cache_offload_layers, 2);
        // A bound inside the dense prefix moves nothing.
        assert_eq!(price(Some(&map), 2).sys_weights_mib, 0);
        // Without a map (catalog-only facts) the first-N
        // approximation stands, unchanged.
        let legacy = price(None, 4);
        assert_eq!(legacy.sys_weights_mib, 4 * 1024);
        assert_eq!(
            legacy,
            layout_requirement(
                &facts,
                ExpertOffload::CpuMoeLayers(4),
                manager_models::CacheSelection::default(),
                0,
                1,
                1024,
            )
        );
        // An interleaved model: MoE blocks 1, 3, 5, 7.
        let map = manager_models::MoeBlockMap::from_block_indices([1, 3, 5, 7]).unwrap();
        assert_eq!(price(Some(&map), 4).sys_weights_mib, 2 * 1024);
        assert_eq!(price(Some(&map), 7).sys_weights_mib, 3 * 1024);
    }

    #[test]
    fn layout_facts_carry_their_block_map() {
        // 2 dense + 4 MoE blocks, the map on the facts themselves.
        const GIB: u64 = 1024 * manager_models::MIB;
        let mut facts = moe_facts();
        facts.moe_layers = Some(4);
        facts.expert_bytes = Some(4 * GIB);
        facts.expert_bytes_by_block = Some(vec![GIB; 4]);
        facts.moe_block_map = manager_models::MoeBlockMap::from_block_indices([2, 3, 4, 5]);
        assert_eq!(facts.offload_for(2), Some(ExpertOffload::CpuMoeLayers(4)));
        assert_eq!(facts.offload_for(4), Some(ExpertOffload::AllExperts));
        assert_eq!(facts.offloaded_layers(ExpertOffload::CpuMoeLayers(4)), 2);
        // layout_requirement (and so others_residency) prices
        // through the facts' map without being handed one.
        let r = layout_requirement(
            &facts,
            ExpertOffload::CpuMoeLayers(4),
            manager_models::CacheSelection::default(),
            0,
            1,
            0,
        );
        assert_eq!(r.sys_weights_mib, 2 * 1024);
        // No map: identity, unchanged.
        facts.moe_block_map = None;
        assert_eq!(facts.offload_for(2), Some(ExpertOffload::CpuMoeLayers(2)));
        assert_eq!(facts.offloaded_layers(ExpertOffload::CpuMoeLayers(4)), 4);
        facts.moe_layers = None;
        assert_eq!(facts.offload_for(2), None);
    }

    #[test]
    fn expert_cache_prices_into_the_requirement_and_its_shares() {
        // Full offload: 6 GiB of experts in RAM, 6 GiB of weights
        // on the GPUs. A 4,096 MiB cache grows the total by
        // exactly its size and each GPU's share by its split
        // share of it.
        let base = layout_requirement(
            &moe_facts(),
            ExpertOffload::AllExperts,
            manager_models::CacheSelection::default(),
            32768,
            4,
            0,
        );
        let cached = layout_requirement(
            &moe_facts(),
            ExpertOffload::AllExperts,
            manager_models::CacheSelection::default(),
            32768,
            4,
            4096,
        );
        assert_eq!(gpu_total_mib(&cached) - gpu_total_mib(&base), 4096);
        assert_eq!(residency_per_gpu(&base, &[0.5, 0.5], 2), vec![3072, 3072]);
        assert_eq!(
            residency_per_gpu(&cached, &[0.5, 0.5], 2),
            vec![3072 + 2048, 3072 + 2048]
        );
        // Unsplit: the whole cache lands on the one GPU.
        assert_eq!(residency_per_gpu(&cached, &[], 1), vec![6144 + 4096]);
    }

    #[test]
    fn expert_cache_share_follows_the_offloaded_layers_gpu() {
        // Partial offload (first 24 of 48 layers) under a 50/50
        // layer split: every offloaded layer sits on GPU 0, so
        // the whole cache prices there and GPU 1's share is
        // unchanged — the renormalization the pinned llama.cpp
        // performs (see manager_models::moe_cache).
        let base = layout_requirement(
            &moe_facts(),
            ExpertOffload::CpuMoeLayers(24),
            manager_models::CacheSelection::default(),
            32768,
            4,
            0,
        );
        let cached = layout_requirement(
            &moe_facts(),
            ExpertOffload::CpuMoeLayers(24),
            manager_models::CacheSelection::default(),
            32768,
            4,
            4096,
        );
        let before = residency_per_gpu(&base, &[0.5, 0.5], 2);
        let after = residency_per_gpu(&cached, &[0.5, 0.5], 2);
        assert_eq!(after[0] - before[0], 4096);
        assert_eq!(after[1], before[1]);
    }

    #[test]
    fn fit_fails_when_free_vram_is_taken_by_a_running_backend() {
        let req = layout_requirement(
            &facts(),
            ExpertOffload::None,
            manager_models::CacheSelection::default(),
            32768,
            4,
            0,
        );
        // 16 GiB free on GPU 0, but a running backend holds 8 GiB:
        // the 12 GiB model cannot be placed.
        let out = evaluate_fit(&req, &[], &[16 * 1024, 0], &[8 * 1024, 0], &[0, 0]);
        assert!(!out.placeable);
        // Without the running backend it fits.
        let out = evaluate_fit(&req, &[], &[16 * 1024, 0], &[0, 0], &[0, 0]);
        assert!(out.placeable);
        assert!(!out.only_by_moving_accounted);
    }

    #[test]
    fn fit_warns_when_it_only_fits_by_moving_accounted_residency() {
        let req = layout_requirement(
            &facts(),
            ExpertOffload::None,
            manager_models::CacheSelection::default(),
            32768,
            4,
            0,
        );
        // 16 GiB free; a STOPPED backend accounts 8 GiB. The model
        // fits now but not alongside the stopped backend.
        let out = evaluate_fit(&req, &[], &[16 * 1024, 0], &[0, 0], &[8 * 1024, 0]);
        assert!(out.placeable);
        assert!(out.only_by_moving_accounted);
    }
}
