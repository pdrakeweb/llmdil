//! The MoE expert cache in a backend's raw llama-server flags:
//! the `--moe-cache-mib N` GPU cache for experts that expert
//! offload keeps in system RAM.
//!
//! The decode/encode pair mirrors the expert-offload pair in
//! [`super::memory`] and the parallel pair in [`super::parallel`]:
//! state is decoded from the saved raw flags — there is no
//! separate stored setting — and the writer rewrites only the
//! token the setting owns, leaving every other flag untouched
//! and in order. Malformed or hand-edited spellings degrade to a
//! safe decoded state, never a panic: an unparseable value reads
//! as unset, and a malformed value token is preserved rather
//! than deleted.
//!
//! The pinned llama.cpp build (b11658, `common/arg.cpp`) defines
//! the flag: `--moe-cache-mib N` — "GPU cache size in MiB for
//! the MoE experts kept in the CPU. with multiple GPUs, it is
//! split among them like the layers (--tensor-split) (default:
//! 0, disabled)". The implementation (`src/llama-moe-cache.cpp`)
//! keeps the most recently used experts of host-resident MoE
//! layers in a device buffer, computed on the device on a hit
//! and on the CPU on a miss. The rules that shape this module:
//!
//! - The size is ONE budget for the whole model, not per GPU:
//!   it is split among the devices that hold host-resident
//!   (offloaded) layers, weighted by the tensor split and
//!   renormalized over those devices alone (with no tensor
//!   split — or an all-zero one — the weights are the devices'
//!   free memory instead). A device's actual allocation never
//!   exceeds its share of the budget (slots are added while
//!   they fit the share), so pricing the full budget, split
//!   the same way, never underprices a device.
//! - Only layers whose experts are entirely host-resident are
//!   cached, on the device the layer is assigned to — so with
//!   no expert offload there is nothing to cache, and the
//!   cache is inert (llama.cpp logs that no layer qualifies
//!   and disables it). The set op therefore refuses a nonzero
//!   cache while a backend offloads nothing.
//! - The cache requires a GPU backend and a MoE model, and it
//!   does not support tensor parallelism: context creation
//!   throws for split mode tensor (`--split-mode row` in this
//!   app's spelling). A row-split backend must not carry the
//!   token; the set op refuses it there too.

/// The size choices the backend form offers, in MiB. The flag
/// itself accepts any non-negative size; these are the control's
/// stops (the sweep points of the motivating experiment: a
/// power-of-two ladder from 2 GiB to 10 GiB).
pub const MOE_CACHE_PRESETS_MIB: [u32; 5] = [2048, 4096, 6144, 8192, 10240];

/// The size the form selects when the cache is switched on for
/// a backend that carries no saved size: the ladder's middle
/// stop, and the first nonzero point of the motivating sweep.
pub const MOE_CACHE_DEFAULT_MIB: u32 = 4096;

/// The reserve the Auto size leaves unallocated, in MiB: Auto
/// sizes the cache to the target GPUs' priced free VRAM minus
/// this constant, so the priced model's own unpriced runtime
/// allocations (compute buffers, graph scratch — the same
/// headroom concern as [`super::cache::HEADROOM_RESERVE_PERCENT`],
/// as a flat figure because the cache is an add-on allocation,
/// not the model itself) keep room.
pub const MOE_CACHE_AUTO_RESERVE_MIB: u64 = 1024;

/// The `--moe-cache-mib` size in raw flags, when set explicitly:
/// the last parseable occurrence wins (llama.cpp's
/// sequential-parse rule). `Some(0)` is an explicit setting —
/// llama.cpp's disabled value — and decodes as one; the writer
/// canonicalizes it away (see [`set_moe_cache`]). A value that
/// does not parse, or a trailing flag, reads as unset.
pub fn moe_cache_mib(raw_flags: Option<&str>) -> Option<u32> {
    let raw = raw_flags?;
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let mut mib: Option<u32> = None;
    for (i, t) in tokens.iter().enumerate() {
        if *t == "--moe-cache-mib" {
            if let Some(v) = tokens.get(i + 1) {
                if let Ok(n) = v.parse::<u32>() {
                    mib = Some(n);
                }
            }
        } else if let Some(v) = t.strip_prefix("--moe-cache-mib=") {
            if let Ok(n) = v.parse::<u32>() {
                mib = Some(n);
            }
        }
    }
    mib
}

/// Rewrite the `--moe-cache-mib` token in raw llama-server
/// flags, leaving every other flag untouched and in order. All
/// existing occurrences (pair and `=` spellings) are removed,
/// then the canonical `--moe-cache-mib N` is appended for
/// `Some(n)` with `n > 0`. `None` — and `Some(0)`, llama.cpp's
/// disabled value — remove the token without appending, so a
/// backend the control has switched off carries no token at
/// all (its spawned command line is exactly its pre-feature
/// one). Returns `None` when nothing remains.
pub fn set_moe_cache(raw_flags: Option<&str>, mib: Option<u32>) -> Option<String> {
    let tokens: Vec<&str> = raw_flags.unwrap_or("").split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if t == "--moe-cache-mib" {
            // Skip the value token too when it parses as a size;
            // a malformed value is left for the main walk
            // (mirrors set_parallel_slots).
            let has_value = tokens
                .get(i + 1)
                .map(|v| v.parse::<u32>().is_ok())
                .unwrap_or(false);
            i += if has_value { 2 } else { 1 };
            continue;
        }
        if t.starts_with("--moe-cache-mib=") {
            i += 1;
            continue;
        }
        out.push(t.to_string());
        i += 1;
    }
    if let Some(n) = mib {
        if n > 0 {
            out.push("--moe-cache-mib".to_string());
            out.push(n.to_string());
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join(" "))
    }
}

/// The MoE-layer range each GPU holds under a layer split, as
/// `[start, end)` MoE-layer indices: contiguous ranges whose
/// sizes follow the split proportions (cumulative floors, the
/// last GPU taking the remainder layers). This approximates
/// llama.cpp's layer assignment — the cache split needs only
/// WHICH devices hold offloaded layers, and for that the
/// proportional ranges agree with the server's own assignment
/// except possibly at a boundary layer.
fn moe_layer_ranges(split: &[f32], moe_layers: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity(split.len());
    let mut cum: f64 = 0.0;
    for (i, frac) in split.iter().enumerate() {
        let start = (cum * moe_layers as f64).floor() as u32;
        cum += *frac as f64;
        let end = if i + 1 == split.len() {
            moe_layers
        } else {
            ((cum * moe_layers as f64).floor() as u32).max(start)
        };
        out.push((start, end));
    }
    out
}

/// Distribute an expert-cache budget over `n_gpus` GPUs the way
/// the pinned llama.cpp allocates it (see the module docs):
/// only GPUs whose layer range holds at least one offloaded
/// MoE layer (the first `offload_layers` MoE layers) receive a
/// share, weighted by their tensor-split fraction renormalized
/// over the receiving GPUs; the last receiving GPU takes the
/// rounding remainder, so the shares sum to the budget. An
/// empty split is an unsplit placement: the whole budget lands
/// on the first GPU. A split whose length differs from the GPU
/// count is stale and distributes nothing (the residency
/// projection's rule). No offload, no MoE layers, or a zero
/// budget likewise distribute nothing.
pub fn moe_cache_shares(
    cache_mib: u64,
    split: &[f32],
    offload_layers: u32,
    moe_layers: u32,
    n_gpus: usize,
) -> Vec<u64> {
    let mut out = vec![0u64; n_gpus];
    if n_gpus == 0 || cache_mib == 0 || offload_layers == 0 || moe_layers == 0 {
        return out;
    }
    if split.is_empty() {
        out[0] = cache_mib;
        return out;
    }
    if split.len() != n_gpus {
        return out;
    }
    let ranges = moe_layer_ranges(split, moe_layers);
    let holders: Vec<usize> = (0..n_gpus)
        .filter(|&i| ranges[i].0 < offload_layers.min(moe_layers) && ranges[i].1 > ranges[i].0)
        .collect();
    let weight_sum: f64 = holders.iter().map(|&i| split[i] as f64).sum();
    if holders.is_empty() || weight_sum <= 0.0 {
        return out;
    }
    let mut assigned = 0u64;
    for (pos, &i) in holders.iter().enumerate() {
        let share = if pos + 1 == holders.len() {
            cache_mib.saturating_sub(assigned)
        } else {
            (cache_mib as f64 * (split[i] as f64 / weight_sum)).round() as u64
        };
        out[i] = share;
        assigned += share;
    }
    out
}

/// The Auto expert-cache budget: the largest cache whose
/// per-GPU shares fit every placed GPU's priced free VRAM,
/// minus `reserve_mib`. Each entry of `available` is one placed
/// GPU's (priced free MiB available to the cache, the fraction
/// of the budget that GPU bears — its share under
/// [`moe_cache_shares`] divided by the budget). GPUs bearing no
/// fraction do not constrain the size. `None` when no GPU bears
/// a fraction, or when the budget left after the reserve is
/// zero — Auto never invents a size the priced free VRAM cannot
/// state.
pub fn auto_moe_cache_budget(available: &[(u64, f64)], reserve_mib: u64) -> Option<u64> {
    let mut budget: Option<f64> = None;
    for (free, frac) in available {
        if *frac <= 0.0 {
            continue;
        }
        let cap = *free as f64 / *frac;
        budget = Some(budget.map_or(cap, |b: f64| b.min(cap)));
    }
    let budget = budget? as u64;
    let sized = budget.saturating_sub(reserve_mib);
    if sized == 0 {
        None
    } else {
        Some(sized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_spellings_and_last_wins() {
        assert_eq!(moe_cache_mib(None), None);
        assert_eq!(moe_cache_mib(Some("-ot exps=CPU")), None);
        assert_eq!(moe_cache_mib(Some("--moe-cache-mib 4096")), Some(4096));
        assert_eq!(moe_cache_mib(Some("--moe-cache-mib=8192")), Some(8192));
        // An explicit zero is a setting (llama.cpp's disabled).
        assert_eq!(moe_cache_mib(Some("--moe-cache-mib 0")), Some(0));
        // Last parseable wins; malformed occurrences do not clear.
        assert_eq!(
            moe_cache_mib(Some("--moe-cache-mib 2048 --moe-cache-mib 6144")),
            Some(6144)
        );
        assert_eq!(
            moe_cache_mib(Some("--moe-cache-mib 2048 --moe-cache-mib abc")),
            Some(2048)
        );
        assert_eq!(moe_cache_mib(Some("--moe-cache-mib abc")), None);
        assert_eq!(moe_cache_mib(Some("--moe-cache-mib")), None);
        // A following flag is never eaten as a value (its name
        // does not parse as a size).
        assert_eq!(moe_cache_mib(Some("--moe-cache-mib --parallel 1")), None);
    }

    #[test]
    fn write_round_trips_and_preserves_siblings_byte_for_byte() {
        // Set from scratch.
        let written = set_moe_cache(None, Some(4096));
        assert_eq!(written.as_deref(), Some("--moe-cache-mib 4096"));
        assert_eq!(moe_cache_mib(written.as_deref()), Some(4096));
        // The motivating backend's flags: full offload, spec and
        // cache-type tokens — every sibling preserved exactly,
        // the cache token appended canonically.
        let before = "-ot exps=CPU --cache-type-k q8_0 --cache-type-v q8_0";
        assert_eq!(
            set_moe_cache(Some(before), Some(8192)).as_deref(),
            Some("-ot exps=CPU --cache-type-k q8_0 --cache-type-v q8_0 --moe-cache-mib 8192")
        );
        // Partial offload + MTP siblings.
        let before = "--n-cpu-moe 24 --spec-type draft-mtp --spec-draft-n-max 1 --parallel 1";
        assert_eq!(
            set_moe_cache(Some(before), Some(2048)).as_deref(),
            Some("--n-cpu-moe 24 --spec-type draft-mtp --spec-draft-n-max 1 --parallel 1 --moe-cache-mib 2048")
        );
        // Change replaces in canonical position; the old token
        // does not survive in either spelling.
        assert_eq!(
            set_moe_cache(Some("--moe-cache-mib=2048 --threads 8"), Some(10240)).as_deref(),
            Some("--threads 8 --moe-cache-mib 10240")
        );
        // An already-canonical string round-trips exactly.
        let canonical = "-ot exps=CPU --moe-cache-mib 8192";
        assert_eq!(
            set_moe_cache(Some(canonical), Some(8192)).as_deref(),
            Some(canonical)
        );
    }

    #[test]
    fn write_removal_and_zero_clear_the_token() {
        assert_eq!(
            set_moe_cache(Some("-ot exps=CPU --moe-cache-mib 8192"), None).as_deref(),
            Some("-ot exps=CPU")
        );
        // Some(0) is llama.cpp's disabled value: it writes no
        // token, so an off backend is byte-identical to its
        // pre-feature command line.
        assert_eq!(
            set_moe_cache(Some("-ot exps=CPU --moe-cache-mib 8192"), Some(0)).as_deref(),
            Some("-ot exps=CPU")
        );
        assert_eq!(set_moe_cache(Some("--moe-cache-mib 8192"), None), None);
        assert_eq!(set_moe_cache(None, None), None);
        assert_eq!(set_moe_cache(None, Some(0)), None);
        // A valueless trailing flag is removed; a malformed
        // value token survives the rewrite (only the flag is
        // dropped, mirroring set_parallel_slots).
        assert_eq!(
            set_moe_cache(Some("-c 8192 --moe-cache-mib"), Some(2048)).as_deref(),
            Some("-c 8192 --moe-cache-mib 2048")
        );
        assert_eq!(
            set_moe_cache(Some("--moe-cache-mib abc"), None).as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn decode_of_write_is_the_written_value() {
        for raw in [
            None,
            Some(""),
            Some("-ot exps=CPU"),
            Some("--n-cpu-moe 24 --moe-cache-mib 2048"),
            Some("--moe-cache-mib=10240 --parallel 2"),
        ] {
            for target in [None, Some(2048), Some(10240)] {
                assert_eq!(
                    moe_cache_mib(set_moe_cache(raw, target).as_deref()),
                    target,
                    "raw={raw:?} target={target:?}"
                );
            }
            // Zero writes no token and decodes as unset.
            assert_eq!(moe_cache_mib(set_moe_cache(raw, Some(0)).as_deref()), None);
        }
    }

    #[test]
    fn shares_single_gpu_takes_the_whole_budget() {
        assert_eq!(moe_cache_shares(8192, &[], 48, 48, 1), vec![8192]);
        assert_eq!(moe_cache_shares(8192, &[1.0], 48, 48, 1), vec![8192]);
        // A split placement naming one GPU of two: the placed
        // GPU's range holds every layer.
        assert_eq!(
            moe_cache_shares(8192, &[1.0, 0.0], 48, 48, 2),
            vec![8192, 0]
        );
    }

    #[test]
    fn shares_split_all_experts_follow_the_tensor_split() {
        assert_eq!(
            moe_cache_shares(8192, &[0.5, 0.5], 48, 48, 2),
            vec![4096, 4096]
        );
        // Shares sum to the budget; the last holder takes the
        // rounding remainder.
        let shares = moe_cache_shares(8191, &[0.25, 0.75], 48, 48, 2);
        assert_eq!(shares.iter().sum::<u64>(), 8191);
        assert_eq!(shares, vec![2048, 6143]);
    }

    #[test]
    fn shares_partial_offload_renormalizes_over_holding_gpus() {
        // 50/50 layer split over 48 MoE layers: GPU 0 holds
        // layers 0..24, GPU 1 holds 24..48. Offloading only the
        // first 24 layers leaves GPU 1 with no host experts —
        // llama.cpp renormalizes the whole budget onto GPU 0.
        assert_eq!(
            moe_cache_shares(8192, &[0.5, 0.5], 24, 48, 2),
            vec![8192, 0]
        );
        // One layer past the boundary and both GPUs hold host
        // experts: the plain tensor-split proportions apply.
        assert_eq!(
            moe_cache_shares(8192, &[0.5, 0.5], 25, 48, 2),
            vec![4096, 4096]
        );
        // Uneven split: 25% of 48 layers = 12 on GPU 0. Offload
        // of 12 layers or fewer prices entirely on GPU 0.
        assert_eq!(
            moe_cache_shares(6000, &[0.25, 0.75], 12, 48, 2),
            vec![6000, 0]
        );
        assert_eq!(
            moe_cache_shares(6000, &[0.25, 0.75], 13, 48, 2),
            vec![1500, 4500]
        );
    }

    #[test]
    fn shares_nothing_without_offload_or_a_stale_split() {
        assert_eq!(moe_cache_shares(8192, &[0.5, 0.5], 0, 48, 2), vec![0, 0]);
        assert_eq!(moe_cache_shares(8192, &[0.5, 0.5], 48, 0, 2), vec![0, 0]);
        assert_eq!(moe_cache_shares(0, &[0.5, 0.5], 48, 48, 2), vec![0, 0]);
        // Stale split (length mismatch): no distribution.
        assert_eq!(moe_cache_shares(8192, &[1.0], 48, 48, 2), vec![0, 0]);
        assert_eq!(moe_cache_shares(8192, &[], 48, 48, 0), Vec::<u64>::new());
    }

    #[test]
    fn auto_budget_is_the_binding_gpu_minus_the_reserve() {
        // Single GPU bearing the whole budget: free minus
        // reserve.
        assert_eq!(
            auto_moe_cache_budget(&[(11_000, 1.0)], MOE_CACHE_AUTO_RESERVE_MIB),
            Some(9_976)
        );
        // Two GPUs bearing half each: the tighter one binds
        // (5,000 free at a 0.5 share caps the budget at 10,000).
        assert_eq!(
            auto_moe_cache_budget(&[(8_000, 0.5), (5_000, 0.5)], 1024),
            Some(8_976)
        );
        // A GPU bearing no fraction does not constrain.
        assert_eq!(
            auto_moe_cache_budget(&[(0, 0.0), (6_000, 1.0)], 1024),
            Some(4_976)
        );
        // Nothing bears, or the reserve eats the whole budget:
        // no size is invented.
        assert_eq!(auto_moe_cache_budget(&[(6_000, 0.0)], 1024), None);
        assert_eq!(auto_moe_cache_budget(&[(1_000, 1.0)], 1024), None);
        assert_eq!(auto_moe_cache_budget(&[], 1024), None);
    }
}
