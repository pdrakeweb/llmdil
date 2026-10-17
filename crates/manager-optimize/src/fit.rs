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
    pub basis: MemoryBasis,
    /// Rated maximum context (catalog `ctx_max`); 0 = unknown.
    pub ctx_max: u32,
    pub params_b: f32,
}

/// A backend's memory requirement from layout facts — the same
/// arithmetic `backend_memory_requirement` performs in src-tauri.
pub fn layout_requirement(
    facts: &LayoutFacts,
    offload: ExpertOffload,
    n_ctx: u32,
) -> MemoryRequirement {
    manager_models::memory_requirement(
        facts.total_bytes,
        facts.expert_bytes,
        facts.expert_bytes_by_block.as_deref(),
        facts.basis,
        offload,
        facts.kv_per_token,
        facts.moe_layers,
        n_ctx,
    )
}

/// Total GPU-resident MiB of a requirement (weights + KV when the
/// KV geometry is known).
pub fn gpu_total_mib(req: &MemoryRequirement) -> u64 {
    req.gpu_weights_mib + req.kv_mib.unwrap_or(0)
}

/// Distribute a requirement's GPU-resident total over the GPUs by
/// tensor split. An empty split is an unsplit launch: the whole
/// model sits on the first GPU (the placement projection's
/// convention). A split whose length differs from the GPU count is
/// stale — it earns no distribution and the caller treats the
/// backend as holding nothing accountable.
pub fn residency_per_gpu(req: &MemoryRequirement, split: &[f32], n_gpus: usize) -> Vec<u64> {
    let mut out = vec![0u64; n_gpus];
    if n_gpus == 0 {
        return out;
    }
    let total = gpu_total_mib(req);
    if split.is_empty() {
        out[0] = total;
        return out;
    }
    if split.len() != n_gpus {
        return out;
    }
    let mut assigned = 0u64;
    for (i, frac) in split.iter().enumerate() {
        let share = if i + 1 == split.len() {
            total.saturating_sub(assigned)
        } else {
            (total as f64 * (*frac as f64)).round() as u64
        };
        out[i] = share;
        assigned += share;
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
        default_offload_layers: None,
        url: String::new(),
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
    let adj_all = sub(free_vram_mib_per_gpu, others_running, Some(others_accounted));
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
        let req = layout_requirement(facts, offload, b.flags.n_ctx);
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
            basis: MemoryBasis::Estimate,
            ctx_max: 0,
            params_b: 12.0,
        }
    }

    #[test]
    fn residency_empty_split_is_first_gpu() {
        let req = layout_requirement(&facts(), ExpertOffload::None, 32768);
        assert_eq!(residency_per_gpu(&req, &[], 2), vec![12 * 1024, 0]);
        assert_eq!(
            residency_per_gpu(&req, &[0.5, 0.5], 2),
            vec![6 * 1024, 6 * 1024]
        );
        // A stale split (wrong GPU count) distributes nothing.
        assert_eq!(residency_per_gpu(&req, &[1.0], 2), vec![0, 0]);
    }

    #[test]
    fn fit_fails_when_free_vram_is_taken_by_a_running_backend() {
        let req = layout_requirement(&facts(), ExpertOffload::None, 32768);
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
        let req = layout_requirement(&facts(), ExpertOffload::None, 32768);
        // 16 GiB free; a STOPPED backend accounts 8 GiB. The model
        // fits now but not alongside the stopped backend.
        let out = evaluate_fit(&req, &[], &[16 * 1024, 0], &[0, 0], &[8 * 1024, 0]);
        assert!(out.placeable);
        assert!(out.only_by_moving_accounted);
    }
}
