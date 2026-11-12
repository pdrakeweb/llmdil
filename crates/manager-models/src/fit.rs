//! Fit-check against discovered per-GPU free VRAM.

use crate::*;
use serde::{Deserialize, Serialize};

/// Fit verdict against discovered per-GPU free VRAM (MiB).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FitVerdict {
    /// Fits on a single GPU (index).
    Fits { gpu: usize },
    /// Needs a tensor split; recommended per-GPU fractions (sum to 1.0).
    FitsWithSplit { split: Vec<f32> },
    /// Does not fit even split across all GPUs.
    TooLarge { needed_mib: u64, free_mib: u64 },
}

/// The three-state fit classification every fit surface renders
/// (Pete, Oct 23, 2026): a requirement within a GPU's total VRAM
/// fits; a requirement over total VRAM on a **WDDM** GPU fits only
/// by paging the excess to system RAM (the Windows display driver
/// overcommits VRAM); the same excess on a **TCC** GPU — or a GPU
/// whose driver model is unreported, judged conservatively as
/// hard-bounded — does not fit. The classification is against the
/// GPU's TOTAL memory: free-memory pressure from other backends
/// is a separate, transient refusal the start gate keeps distinct
/// ([`FitState`] never upgrades an insufficient-free case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitState {
    Fits,
    /// Over total VRAM on a WDDM GPU: starts only with the
    /// operator's confirmation; the paged amount rides the
    /// payloads separately (see [`fit_state`]).
    WddmPaged,
    DoesNotFit,
}

impl FitState {
    /// The wire spelling (`fits` / `wddm_paged` / `does_not_fit`).
    pub fn as_str(&self) -> &'static str {
        match self {
            FitState::Fits => "fits",
            FitState::WddmPaged => "wddm_paged",
            FitState::DoesNotFit => "does_not_fit",
        }
    }

    /// Severity rank for worst-of aggregation across GPUs:
    /// a hard refusal outranks a paged fit, which outranks a fit.
    pub fn rank(&self) -> u8 {
        match self {
            FitState::Fits => 0,
            FitState::WddmPaged => 1,
            FitState::DoesNotFit => 2,
        }
    }
}

/// Classify one GPU's requirement against its total VRAM, given
/// whether telemetry reports the GPU's driver as WDDM. Returns
/// the state and the paged amount (MiB that will page to system
/// RAM — nonzero only for [`FitState::WddmPaged`]). An unknown
/// driver model passes `is_wddm: false`: unreported is judged as
/// hard-bounded, never as pageable.
pub fn fit_state(required_mib: u64, total_mib: u64, is_wddm: bool) -> (FitState, u64) {
    if required_mib <= total_mib {
        (FitState::Fits, 0)
    } else if is_wddm {
        (FitState::WddmPaged, required_mib - total_mib)
    } else {
        (FitState::DoesNotFit, 0)
    }
}

/// The worst [`FitState`] across per-GPU states (empty = fits:
/// no GPU was judged, the no-telemetry posture).
pub fn worst_fit_state(states: &[FitState]) -> FitState {
    states
        .iter()
        .copied()
        .max_by_key(|s| s.rank())
        .unwrap_or(FitState::Fits)
}

/// Recommend a placement for `spec` given per-GPU free VRAM.
///
/// - Single-GPU fit goes to the GPU with the most free VRAM (lowest index
///   wins ties).
/// - Otherwise, if the model fits in the combined free VRAM, the split is
///   proportional to each GPU's free VRAM and renormalized to sum to 1.0.
/// - Otherwise [`FitVerdict::TooLarge`] carries the arithmetic for the UI.
pub fn fit_check(spec: &ModelSpec, free_vram_mib_per_gpu: &[u64]) -> FitVerdict {
    let needed_mib = spec.size_bytes.div_ceil(MIB);
    if free_vram_mib_per_gpu.is_empty() {
        return FitVerdict::TooLarge {
            needed_mib,
            free_mib: 0,
        };
    }

    let mut best_idx = 0usize;
    let mut best_free = free_vram_mib_per_gpu[0];
    for (i, &free) in free_vram_mib_per_gpu.iter().enumerate().skip(1) {
        if free > best_free {
            best_free = free;
            best_idx = i;
        }
    }
    if best_free >= needed_mib {
        return FitVerdict::Fits { gpu: best_idx };
    }

    let total_free: u64 = free_vram_mib_per_gpu
        .iter()
        .fold(0u64, |acc, &f| acc.saturating_add(f));
    if total_free < needed_mib {
        return FitVerdict::TooLarge {
            needed_mib,
            free_mib: total_free,
        };
    }

    // Proportional split, computed in f64 then renormalized in f32 so the
    // fractions sum to 1.0 (llama.cpp --tensor-split expects this).
    let total_f = total_free as f64;
    let mut split: Vec<f32> = free_vram_mib_per_gpu
        .iter()
        .map(|&f| (f as f64 / total_f) as f32)
        .collect();
    let sum: f32 = split.iter().sum();
    // Fold the rounding residual into the largest entry.
    let mut max_idx = 0usize;
    for (i, &free) in free_vram_mib_per_gpu.iter().enumerate().skip(1) {
        if free > free_vram_mib_per_gpu[max_idx] {
            max_idx = i;
        }
    }
    split[max_idx] += 1.0f32 - sum;
    FitVerdict::FitsWithSplit { split }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::*;

    // -- fit-check tests ------------------------------------------------------

    #[test]
    fn fit_check_single_gpu_picks_most_free() {
        let spec = test_spec("m", 4_700_000_000); // ~4482 MiB
                                                  // Tie: lowest index wins.
        assert!(matches!(
            fit_check(&spec, &[16384, 16384, 4096]),
            FitVerdict::Fits { gpu: 0 }
        ));
        assert!(matches!(
            fit_check(&spec, &[4096, 16384]),
            FitVerdict::Fits { gpu: 1 }
        ));
    }

    #[test]
    fn fit_check_exact_fit_counts() {
        let spec = test_spec("m", 8192 * MIB);
        assert!(matches!(
            fit_check(&spec, &[8192]),
            FitVerdict::Fits { gpu: 0 }
        ));
    }

    #[test]
    fn fit_check_split_is_proportional_and_sums_to_one() {
        let spec = test_spec("m", 16_600_000_000); // ~15832 MiB needed
        match fit_check(&spec, &[9100, 8400, 100]) {
            FitVerdict::FitsWithSplit { split } => {
                assert_eq!(split.len(), 3);
                let total = 9100.0 + 8400.0 + 100.0;
                assert!((split[0] as f64 - 9100.0 / total).abs() < 1e-6);
                assert!((split[1] as f64 - 8400.0 / total).abs() < 1e-6);
                assert!((split[2] as f64 - 100.0 / total).abs() < 1e-6);
                let sum: f32 = split.iter().sum();
                assert!(
                    (sum - 1.0).abs() < 1e-6,
                    "split fractions must sum to 1.0, got {sum}"
                );
            }
            other => panic!("expected FitsWithSplit, got {other:?}"),
        }
    }

    #[test]
    fn fit_check_too_large_reports_arithmetic() {
        let spec = test_spec("m", 100_000_000_000);
        match fit_check(&spec, &[8192, 4096]) {
            FitVerdict::TooLarge {
                needed_mib,
                free_mib,
            } => {
                assert_eq!(needed_mib, 100_000_000_000u64.div_ceil(MIB));
                assert_eq!(free_mib, 12288);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn fit_check_no_gpus_is_too_large() {
        let spec = test_spec("m", 1000);
        assert!(matches!(
            fit_check(&spec, &[]),
            FitVerdict::TooLarge {
                needed_mib: 1,
                free_mib: 0
            }
        ));
    }

    #[test]
    fn fit_state_three_states_and_worst_aggregation() {
        // Within total: fits on either driver model, nothing paged.
        assert_eq!(fit_state(16_376, 16_376, true), (FitState::Fits, 0));
        assert_eq!(fit_state(13_128, 16_376, false), (FitState::Fits, 0));
        // Over total on WDDM: paged by exactly the excess.
        assert_eq!(fit_state(17_224, 16_376, true), (FitState::WddmPaged, 848));
        // Over total on TCC — and on an unreported driver model,
        // which the caller passes as not-WDDM: does not fit.
        assert_eq!(fit_state(17_224, 16_376, false), (FitState::DoesNotFit, 0));
        assert_eq!(
            worst_fit_state(&[FitState::Fits, FitState::WddmPaged]),
            FitState::WddmPaged
        );
        assert_eq!(
            worst_fit_state(&[FitState::WddmPaged, FitState::DoesNotFit]),
            FitState::DoesNotFit
        );
        assert_eq!(worst_fit_state(&[]), FitState::Fits);
    }
}
