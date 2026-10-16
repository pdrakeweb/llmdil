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
    /// Per-GPU fit: projected allocation <= free VRAM.
    pub per_gpu_fits: Vec<bool>,
    /// True when every GPU's allocation fits its free VRAM.
    pub fits: bool,
    /// The catalog [`FitVerdict`] for the same inputs (recommendation view).
    pub verdict: FitVerdict,
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
    validate_split(split, free_vram_mib_per_gpu.len())?;
    let needed_mib = spec.size_bytes.div_ceil(MIB);
    // f32 fractions carry representation error (e.g. 0.6f32 is
    // 0.60000002384…), so a naive ceil() over-allocates by 1 MiB whenever the
    // true product is an integer. Snap values within 1e-6 of an integer to it;
    // genuine fractional products still round up.
    let per_gpu_mib: Vec<u64> = split
        .iter()
        .map(|f| {
            let raw = *f as f64 * needed_mib as f64;
            if (raw - raw.round()).abs() < 1e-6 {
                raw.round() as u64
            } else {
                raw.ceil() as u64
            }
        })
        .collect();
    let per_gpu_fits: Vec<bool> = per_gpu_mib
        .iter()
        .zip(free_vram_mib_per_gpu.iter())
        .map(|(alloc, free)| alloc <= free)
        .collect();
    let fits = per_gpu_fits.iter().all(|b| *b);
    Ok(PlacementProjection {
        model_id: spec.id.clone(),
        split: split.to_vec(),
        per_gpu_mib,
        free_mib: free_vram_mib_per_gpu.to_vec(),
        per_gpu_fits,
        fits,
        verdict: fit_check(spec, free_vram_mib_per_gpu),
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
