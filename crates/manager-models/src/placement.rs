//! Tensor-split placement projection and application (spec §14:
//! `placement_propose` / `placement_apply`).
//!
//! Dependency direction: this crate depends on `manager-config` for
//! [`BackendConfig`](manager_config::BackendConfig). `manager-config` never
//! depends on `manager-models` — placement math lives here next to
//! [`fit_check`](super::fit_check).

use manager_config::{BackendConfig, SplitMode};
use serde::{Deserialize, Serialize};

use super::{fit_check, FitVerdict, ModelError, ModelSpec, MIB};

/// Projected placement of a model onto discovered GPUs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlacementProjection {
    pub model_id: String,
    /// The proposed per-GPU fractions (sums to 1.0).
    pub split: Vec<f32>,
    /// Projected allocation per GPU in MiB, in split order.
    pub per_gpu_mib: Vec<u64>,
    /// Free VRAM per GPU in MiB from the same snapshot the fit verdict
    /// was computed against, in split order. Carried in the payload so
    /// the UI does not reconstruct "free" from a different (newer or
    /// older) telemetry copy than the one `per_gpu_fits` was judged on.
    pub free_mib: Vec<u64>,
    /// The in-scope backend's CURRENT GPU-resident weight allocation
    /// per GPU in MiB, in split order — what its running process
    /// already holds, which a restart frees before loading the new
    /// split. All zeros when no backend was in scope or the backend
    /// is not running. Weights only: KV cache and compute buffers are
    /// never credited, they are re-created either way.
    #[serde(default)]
    pub current_mib: Vec<u64>,
    /// Additional weight allocation each GPU must find in its free
    /// VRAM: `per_gpu_mib - current_mib`, floored at 0, in split
    /// order. This — not the full projected allocation — is what
    /// `per_gpu_fits` judges against free VRAM.
    #[serde(default)]
    pub additional_mib: Vec<u64>,
    /// Per-GPU fit: additional allocation <= free VRAM.
    pub per_gpu_fits: Vec<bool>,
    /// True when every GPU's allocation fits its free VRAM.
    pub fits: bool,
    /// The catalog [`FitVerdict`] for the same inputs (recommendation view).
    pub verdict: FitVerdict,
    /// Weights resident in system RAM under the serving backend's
    /// expert offload (0 when no backend was in scope or no offload
    /// applies). The per-GPU figures cover GPU-resident weights
    /// only in that case.
    #[serde(default)]
    pub sys_mib: u64,
    /// The weight total the per-GPU projection was computed from:
    /// the model's full size, or its GPU-resident share when expert
    /// offload applies.
    #[serde(default)]
    pub gpu_weights_mib: u64,
}

/// Validate a tensor split: non-empty, `len == n_gpus`, every fraction in
/// [0, 1], fractions summing to 1.0 within 1e-3.
pub fn validate_split(split: &[f32], n_gpus: usize) -> Result<(), ModelError> {
    if split.is_empty() {
        return Err(ModelError::InvalidSplit(
            "tensor split must not be empty".to_string(),
        ));
    }
    if split.len() != n_gpus {
        return Err(ModelError::InvalidSplit(format!(
            "tensor split has {} entries but there are {n_gpus} GPUs",
            split.len()
        )));
    }
    for (i, f) in split.iter().enumerate() {
        if !f.is_finite() || *f < 0.0 || *f > 1.0 {
            return Err(ModelError::InvalidSplit(format!(
                "tensor split entry {i} = {f} is outside [0, 1]"
            )));
        }
    }
    let sum: f32 = split.iter().sum();
    if (sum - 1.0).abs() > 1e-3 {
        return Err(ModelError::InvalidSplit(format!(
            "tensor split sums to {sum}, expected 1.0"
        )));
    }
    Ok(())
}

/// Project `split` for `spec` onto per-GPU free VRAM.
///
/// Unlike [`fit_check`], this evaluates an explicit user-supplied split
/// rather than recommending one: `per_gpu_mib` is the projected allocation
/// (model size × fraction, rounded up) and `fits` reports whether every
/// allocation fits its GPU's free VRAM.
pub fn propose_placement(
    spec: &ModelSpec,
    split: &[f32],
    free_vram_mib_per_gpu: &[u64],
) -> Result<PlacementProjection, ModelError> {
    propose_placement_resident(spec, split, free_vram_mib_per_gpu, None, 0, None)
}

/// Project a split using the model's GPU-resident weight share
/// instead of its full file size, for a serving backend whose
/// expert offload keeps part of the weights in system RAM
/// ([`super::memory`]). `gpu_resident_bytes` is the weight total
/// that lands on GPUs (the split fractions divide it); `sys_bytes`
/// is the system-RAM share, carried into the projection so the UI
/// can show where the rest of the model lives. With
/// `gpu_resident_bytes: None` this is exactly [`propose_placement`].
///
/// `current_split` is the serving backend's current split (same
/// length as `split`) when its process is running: the weights it
/// already holds on each GPU are credited, because applying a new
/// split restarts the backend and frees them first. Only the
/// additional allocation above the current one must fit in free
/// VRAM — moving a model from 80% to 100% on the same GPU needs
/// only the remaining 20% to fit. The credit is per GPU: weights
/// held on a different GPU credit nothing.
pub fn propose_placement_resident(
    spec: &ModelSpec,
    split: &[f32],
    free_vram_mib_per_gpu: &[u64],
    gpu_resident_bytes: Option<u64>,
    sys_bytes: u64,
    current_split: Option<&[f32]>,
) -> Result<PlacementProjection, ModelError> {
    validate_split(split, free_vram_mib_per_gpu.len())?;
    let needed_mib = gpu_resident_bytes.unwrap_or(spec.size_bytes).div_ceil(MIB);
    // f32 fractions carry representation error (e.g. 0.6f32 is
    // 0.60000002384…), so a naive ceil() over-allocates by 1 MiB whenever the
    // true product is an integer. Snap values within 1e-6 of an integer to it;
    // genuine fractional products still round up.
    let project = |fractions: &[f32]| -> Vec<u64> {
        fractions
            .iter()
            .map(|f| {
                let raw = *f as f64 * needed_mib as f64;
                if (raw - raw.round()).abs() < 1e-6 {
                    raw.round() as u64
                } else {
                    raw.ceil() as u64
                }
            })
            .collect()
    };
    let per_gpu_mib: Vec<u64> = project(split);
    // The current allocation, on the same projection basis. A
    // current split whose length does not match the GPU set is
    // stale (GPUs added or removed since it was configured) and
    // earns no credit rather than an invented one. A current split
    // that is not itself a valid split (entry outside [0, 1], or
    // fractions not summing to 1) is stale by the same rule: an
    // out-of-range entry would otherwise project more credit than
    // the model has bytes (e.g. [1.5, -0.5] crediting 150% of the
    // model on GPU 0).
    let current_mib: Vec<u64> = match current_split {
        Some(cur) if cur.len() == split.len() && validate_split(cur, cur.len()).is_ok() => {
            project(cur)
        }
        _ => vec![0; split.len()],
    };
    let additional_mib: Vec<u64> = per_gpu_mib
        .iter()
        .zip(current_mib.iter())
        .map(|(alloc, cur)| alloc.saturating_sub(*cur))
        .collect();
    let per_gpu_fits: Vec<bool> = additional_mib
        .iter()
        .zip(free_vram_mib_per_gpu.iter())
        .map(|(add, free)| add <= free)
        .collect();
    let fits = per_gpu_fits.iter().all(|b| *b);
    // The recommendation verdict judges the same weight total the
    // projection used: with offload in scope, a model whose file is
    // larger than any GPU can still fit by its resident share. It
    // judges against free VRAM plus the credited current allocation,
    // the same basis as per_gpu_fits.
    let adjusted_free: Vec<u64> = free_vram_mib_per_gpu
        .iter()
        .zip(current_mib.iter())
        .map(|(free, cur)| free.saturating_add(*cur))
        .collect();
    let verdict = match gpu_resident_bytes {
        Some(resident) => {
            let mut resized = spec.clone();
            resized.size_bytes = resident;
            fit_check(&resized, &adjusted_free)
        }
        None => fit_check(spec, &adjusted_free),
    };
    Ok(PlacementProjection {
        model_id: spec.id.clone(),
        split: split.to_vec(),
        per_gpu_mib,
        free_mib: free_vram_mib_per_gpu.to_vec(),
        current_mib,
        additional_mib,
        per_gpu_fits,
        fits,
        verdict,
        sys_mib: sys_bytes.div_ceil(MIB),
        gpu_weights_mib: needed_mib,
    })
}

/// Apply a tensor split to a backend's config (validates first; the caller
/// re-validates the whole `AppConfig` via
/// [`apply_config_diff`](manager_config::apply_config_diff) semantics).
///
/// `split_mode` is only touched when it is currently [`SplitMode::None`]:
/// a multi-GPU split then implies [`SplitMode::Layer`], a single-GPU split
/// keeps `None`. An explicitly chosen `Layer`/`Row` mode is never clobbered.
pub fn apply_placement(backend: &mut BackendConfig, split: &[f32]) -> Result<(), ModelError> {
    if split.is_empty() {
        return Err(ModelError::InvalidSplit(
            "tensor split must not be empty".to_string(),
        ));
    }
    for (i, f) in split.iter().enumerate() {
        if !f.is_finite() || *f < 0.0 || *f > 1.0 {
            return Err(ModelError::InvalidSplit(format!(
                "tensor split entry {i} = {f} is outside [0, 1]"
            )));
        }
    }
    let sum: f32 = split.iter().sum();
    if (sum - 1.0).abs() > 1e-3 {
        return Err(ModelError::InvalidSplit(format!(
            "tensor split sums to {sum}, expected 1.0"
        )));
    }
    backend.flags.tensor_split = split.to_vec();
    if backend.flags.split_mode == SplitMode::None && split.len() > 1 {
        backend.flags.split_mode = SplitMode::Layer;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ModelSpec {
        ModelSpec {
            id: "m".to_string(),
            name: "M".to_string(),
            quant: "Q4_K_M".to_string(),
            size_bytes: 10 * MIB,
            sha256: String::new(),
            url: String::new(),
            params_b: 1.0,
            ctx_max: 0,
            enabled_by_default: true,
            expert_bytes: None,
            moe_layers: None,
            default_offload_layers: None,
            offload_restrictions: Vec::new(),
        }
    }

    #[test]
    fn propose_projects_allocation_per_gpu() {
        let p = propose_placement(&spec(), &[0.6, 0.4], &[8, 8]).unwrap();
        assert_eq!(p.per_gpu_mib, vec![6, 4]);
        assert_eq!(p.free_mib, vec![8, 8]);
        assert_eq!(p.per_gpu_fits, vec![true, true]);
        assert!(p.fits);
    }

    #[test]
    fn propose_flags_gpu_that_does_not_fit() {
        let p = propose_placement(&spec(), &[0.6, 0.4], &[8, 2]).unwrap();
        assert_eq!(p.per_gpu_fits, vec![true, false]);
        assert!(!p.fits);
    }

    #[test]
    fn propose_rejects_bad_splits() {
        assert!(propose_placement(&spec(), &[], &[8, 8]).is_err());
        assert!(propose_placement(&spec(), &[0.6], &[8, 8]).is_err());
        assert!(propose_placement(&spec(), &[0.5, 0.4], &[8, 8]).is_err());
        assert!(propose_placement(&spec(), &[0.6, 1.4], &[8, 8]).is_err());
        assert!(propose_placement(&spec(), &[f32::NAN, 1.0], &[8, 8]).is_err());
    }

    #[test]
    fn propose_resident_projects_gpu_share_and_carries_sys() {
        // 10 MiB file, 8 MiB of it experts in system RAM: the GPUs
        // split only the 2 MiB resident share.
        let p =
            propose_placement_resident(&spec(), &[0.5, 0.5], &[8, 8], Some(2 * MIB), 8 * MIB, None)
                .unwrap();
        assert_eq!(p.per_gpu_mib, vec![1, 1]);
        assert_eq!(p.sys_mib, 8);
        assert_eq!(p.gpu_weights_mib, 2);
        assert!(p.fits);
        // The verdict judges the resident share too: 2 MiB fits a
        // single 8 MiB GPU even though the 10 MiB file would not.
        assert!(matches!(p.verdict, FitVerdict::Fits { .. }));
    }

    #[test]
    fn propose_credits_current_allocation_on_the_same_gpu() {
        // 10 MiB model currently split 80/20; proposing 100% on
        // GPU 0 needs only the remaining 2 MiB to fit GPU 0's free
        // VRAM — the 8 MiB already held there is freed by the
        // restart and re-used.
        let p =
            propose_placement_resident(&spec(), &[1.0, 0.0], &[2, 8], None, 0, Some(&[0.8, 0.2]))
                .unwrap();
        assert_eq!(p.per_gpu_mib, vec![10, 0]);
        assert_eq!(p.current_mib, vec![8, 2]);
        assert_eq!(p.additional_mib, vec![2, 0]);
        assert_eq!(p.per_gpu_fits, vec![true, true]);
        assert!(p.fits);
    }

    #[test]
    fn propose_credit_does_not_cover_a_shortfall() {
        // Same move, but GPU 0 has only 1 MiB free: the additional
        // 2 MiB does not fit.
        let p =
            propose_placement_resident(&spec(), &[1.0, 0.0], &[1, 8], None, 0, Some(&[0.8, 0.2]))
                .unwrap();
        assert_eq!(p.additional_mib, vec![2, 0]);
        assert_eq!(p.per_gpu_fits, vec![false, true]);
        assert!(!p.fits);
    }

    #[test]
    fn propose_credit_stays_on_its_own_gpu() {
        // Moving the model to GPU 1 earns no credit from the 8 MiB
        // held on GPU 0: only the 2 MiB already on GPU 1 counts, so
        // GPU 1 needs 8 MiB more than its 2 MiB free.
        let p =
            propose_placement_resident(&spec(), &[0.0, 1.0], &[8, 2], None, 0, Some(&[0.8, 0.2]))
                .unwrap();
        assert_eq!(p.additional_mib, vec![0, 8]);
        assert_eq!(p.per_gpu_fits, vec![true, false]);
        assert!(!p.fits);
    }

    #[test]
    fn propose_reduction_needs_nothing_where_it_shrinks() {
        // 80/20 -> 50/50: GPU 0's share shrinks, so it needs no free
        // VRAM at all; GPU 1 needs the 3 MiB growth only.
        let p =
            propose_placement_resident(&spec(), &[0.5, 0.5], &[0, 3], None, 0, Some(&[0.8, 0.2]))
                .unwrap();
        assert_eq!(p.additional_mib, vec![0, 3]);
        assert!(p.fits);
    }

    #[test]
    fn propose_stale_current_split_earns_no_credit() {
        // A current split recorded against a different GPU count
        // is ignored, not guessed at.
        let p = propose_placement_resident(
            &spec(),
            &[1.0, 0.0],
            &[2, 8],
            None,
            0,
            Some(&[0.8, 0.1, 0.1]),
        )
        .unwrap();
        assert_eq!(p.current_mib, vec![0, 0]);
        assert_eq!(p.additional_mib, vec![10, 0]);
        assert!(!p.fits);
    }

    #[test]
    fn propose_out_of_range_current_split_earns_no_credit() {
        // [1.5, -0.5] is not a split of anything: projected
        // naively it would credit 15 MiB of a 10 MiB model on
        // GPU 0 and report a fit against zero free VRAM.
        let p =
            propose_placement_resident(&spec(), &[1.0, 0.0], &[0, 8], None, 0, Some(&[1.5, -0.5]))
                .unwrap();
        assert_eq!(p.current_mib, vec![0, 0]);
        assert_eq!(p.additional_mib, vec![10, 0]);
        assert!(!p.fits);
    }

    #[test]
    fn apply_placement_sets_split_and_implies_layer_mode() {
        let mut b = manager_config::AppConfig::default_config()
            .backends
            .remove(0);
        apply_placement(&mut b, &[0.5, 0.5]).unwrap();
        assert_eq!(b.flags.tensor_split, vec![0.5, 0.5]);
        assert_eq!(b.flags.split_mode, SplitMode::Layer);
    }

    #[test]
    fn apply_placement_preserves_explicit_row_mode() {
        let mut b = manager_config::AppConfig::default_config()
            .backends
            .remove(0);
        b.flags.split_mode = SplitMode::Row;
        apply_placement(&mut b, &[0.7, 0.3]).unwrap();
        assert_eq!(b.flags.split_mode, SplitMode::Row);
    }

    #[test]
    fn apply_placement_rejects_invalid() {
        let mut b = manager_config::AppConfig::default_config()
            .backends
            .remove(0);
        let before = b.flags.tensor_split.clone();
        assert!(apply_placement(&mut b, &[0.5, 0.4]).is_err());
        assert_eq!(b.flags.tensor_split, before);
    }
}
