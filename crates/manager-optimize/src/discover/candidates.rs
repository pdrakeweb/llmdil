//! Fit verdicts and candidate assembly: one [`CandidateFact`] per
//! quant file, fit computed locally against a free-VRAM snapshot.

use manager_models::{ExpertOffload, MemoryBasis};

use super::{
    quant_files, CandidateFact, HfModelFacts, FIT_DOES_NOT_FIT, FIT_FITS, FIT_FITS_WITH_SPLIT,
    FIT_UNCHECKED,
};
use crate::fit::{evaluate_fit, layout_requirement, LayoutFacts};

/// The fit verdict for one candidate size against a free-VRAM
/// snapshot (per GPU, MiB, GPU-index order) — the existing
/// evaluator over a [`LayoutFacts`] built from HF facts: the
/// quant file's exact size as the total, no expert/KV geometry
/// (HF does not report it), and an `Estimate` basis label —
/// the honest one, because a weights-only requirement with
/// unknown KV is an estimate-grade figure even though the
/// file size itself is exact. The snapshot is the telemetry
/// free VRAM, which already excludes running backends'
/// residency, so no other residency is subtracted here.
pub fn fit_verdict(
    size_bytes: u64,
    ctx_max: u32,
    params_b: f32,
    free_vram_mib_per_gpu: &[u64],
) -> String {
    if free_vram_mib_per_gpu.is_empty() {
        return FIT_UNCHECKED.to_string();
    }
    let facts = LayoutFacts {
        total_bytes: size_bytes,
        expert_bytes: None,
        expert_bytes_by_block: None,
        moe_layers: None,
        kv_per_token: None,
        kv_geometry: None,
        rs_bytes_per_seq: None,
        basis: MemoryBasis::Estimate,
        ctx_max,
        params_b,
    };
    // No offload is proposed for a candidate, and with no KV or
    // recurrent-state geometry the requirement is weights-only
    // at any context and any slot count.
    let req = layout_requirement(
        &facts,
        ExpertOffload::None,
        manager_models::CacheSelection::default(),
        0,
        1,
        0,
    );
    let zeros = vec![0u64; free_vram_mib_per_gpu.len()];
    let outcome = evaluate_fit(&req, &[], free_vram_mib_per_gpu, &zeros, &zeros);
    if !outcome.placeable {
        return FIT_DOES_NOT_FIT.to_string();
    }
    let total_mib = crate::fit::gpu_total_mib(&req);
    if free_vram_mib_per_gpu.iter().any(|&f| f >= total_mib) {
        FIT_FITS.to_string()
    } else {
        FIT_FITS_WITH_SPLIT.to_string()
    }
}

/// Assemble one repo's candidate facts from its model record:
/// one entry per quant file, each with the record's shared
/// facts and its own fit verdict.
pub fn candidates_from_model(
    facts: &HfModelFacts,
    free_vram_mib_per_gpu: &[u64],
    retrieved_at: &str,
) -> Vec<CandidateFact> {
    let params_b = facts
        .params_total
        .map(|t| t as f32 / 1_000_000_000.0)
        .unwrap_or(0.0);
    let ctx_max = facts.ctx_max.unwrap_or(0);
    quant_files(&facts.siblings)
        .into_iter()
        .map(|q| CandidateFact {
            repo_id: facts.repo_id.clone(),
            fit: fit_verdict(q.size_bytes, ctx_max, params_b, free_vram_mib_per_gpu),
            file: q.file,
            architecture: facts.architecture.clone().unwrap_or_default(),
            params_b,
            ctx_max,
            size_bytes: q.size_bytes,
            downloads: facts.downloads,
            retrieved_at: retrieved_at.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::test_fixtures::*;
    use crate::discover::*;

    // -- candidate assembly + fit ---------------------------------------------

    #[test]
    fn candidates_carry_record_facts_and_provenance() {
        let facts = parse_model_record(&record_body()).unwrap();
        let free = [16 * 1024];
        let cands = candidates_from_model(&facts, &free, "2026-10-17T12:00:00+00:00");
        // mmproj excluded; the two real quants remain.
        assert_eq!(cands.len(), 2);
        let q4 = &cands[0];
        assert_eq!(q4.repo_id, "Qwen/Qwen3-8B-GGUF");
        assert_eq!(q4.file, "Qwen3-8B-Q4_K_M.gguf");
        assert_eq!(q4.architecture, "qwen3");
        assert!((q4.params_b - 8.190735).abs() < 0.001, "{}", q4.params_b);
        assert_eq!(q4.ctx_max, 40960);
        assert_eq!(q4.size_bytes, 5027783488);
        assert_eq!(q4.downloads, 594664);
        assert_eq!(q4.retrieved_at, "2026-10-17T12:00:00+00:00");
        // ~4.7 GiB in 16 GiB free on one GPU.
        assert_eq!(q4.fit, FIT_FITS);
        // ~8.1 GiB also fits the single GPU.
        assert_eq!(cands[1].fit, FIT_FITS);
    }

    #[test]
    fn fit_verdicts_cover_split_too_large_and_unchecked() {
        let gib = MIB64 * 1024;
        // 12 GiB across two 8 GiB GPUs: split only.
        assert_eq!(
            fit_verdict(12 * gib, 0, 12.0, &[8 * 1024, 8 * 1024]),
            FIT_FITS_WITH_SPLIT
        );
        // 20 GiB against 16 GiB total free: no fit.
        assert_eq!(
            fit_verdict(20 * gib, 0, 20.0, &[8 * 1024, 8 * 1024]),
            FIT_DOES_NOT_FIT
        );
        // Fits on the second GPU alone even when the first is full.
        assert_eq!(fit_verdict(4 * gib, 0, 4.0, &[0, 16 * 1024]), FIT_FITS);
        // No telemetry: no fit claim either way.
        assert_eq!(fit_verdict(4 * gib, 0, 4.0, &[]), FIT_UNCHECKED);
    }
}
