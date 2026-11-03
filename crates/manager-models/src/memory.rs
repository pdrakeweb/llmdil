//! Per-backend memory requirements: how much GPU RAM and system
//! RAM a configured backend needs, given its model's weight layout
//! and its own serving flags (tensor split, expert offload, context
//! size).
//!
//! The weight split comes from the GGUF tensor directory when the
//! file is available ([`MemoryBasis::Exact`]), from the curated
//! catalog's pinned expert share before download
//! ([`MemoryBasis::Catalog`]), or is not known at all
//! ([`MemoryBasis::Estimate`] — the requirement then degrades to
//! the pre-offload behavior: everything counted on the GPU).

use serde::{Deserialize, Serialize};

use super::MIB;

/// How a [`MemoryRequirement`] was derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryBasis {
    /// Parsed from the downloaded GGUF's tensor directory.
    Exact,
    /// From the curated catalog's pinned sizes.
    Catalog,
    /// Estimated (parameter-count derivation, or an offload split
    /// that could not be resolved). Figures are approximate.
    Estimate,
}

/// Expert offload configured in a backend's raw llama-server flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpertOffload {
    /// No offload: all weights are GPU-resident (subject to the
    /// tensor split).
    None,
    /// All routed experts on the CPU (`-ot exps=CPU` and spellings
    /// of the same override, or `--cpu-moe` / `-cmoe`).
    AllExperts,
    /// Experts of the first N layers on the CPU (`--n-cpu-moe N`,
    /// `-ncmoe N`).
    CpuMoeLayers(u32),
}

impl ExpertOffload {
    /// The offload slider position this setting represents: how
    /// many of the model's `moe_layers` MoE layers sit in system
    /// RAM. A partial count past the model's layer count clamps
    /// (llama.cpp treats `--n-cpu-moe` past the end as all).
    pub fn layers(&self, moe_layers: u32) -> u32 {
        match self {
            ExpertOffload::None => 0,
            ExpertOffload::AllExperts => moe_layers,
            ExpertOffload::CpuMoeLayers(n) => (*n).min(moe_layers),
        }
    }
}

/// The offload setting a slider position selects: 0 layers is no
/// offload, the full MoE layer count is all experts (written in
/// the `-ot exps=CPU` form), anything between is
/// `--n-cpu-moe N`.
pub fn offload_for_layers(layers: u32, moe_layers: u32) -> ExpertOffload {
    if layers == 0 || moe_layers == 0 {
        ExpertOffload::None
    } else if layers >= moe_layers {
        ExpertOffload::AllExperts
    } else {
        ExpertOffload::CpuMoeLayers(layers)
    }
}

/// The canonical raw-flags text for an offload setting — the form
/// the app itself writes. `None` when the setting writes no tokens
/// (no offload).
pub fn offload_flag_string(offload: ExpertOffload) -> Option<String> {
    match offload {
        ExpertOffload::None => None,
        ExpertOffload::AllExperts => Some("-ot exps=CPU".to_string()),
        ExpertOffload::CpuMoeLayers(0) => None,
        ExpertOffload::CpuMoeLayers(n) => Some(format!("--n-cpu-moe {n}")),
    }
}

/// The offload slider's color-band boundaries as fractions of the
/// offloaded range: `(green_end, yellow_end)`. Green runs from no
/// offload to the model's green limit — the catalog's default
/// offload, the most offload that is still fast for the model.
/// Yellow covers the first half of the range past the limit and
/// red the rest, so the bands derive from the same catalog number
/// the install default uses and the two can never disagree.
pub fn offload_bands(moe_layers: u32, green_limit_layers: u32) -> (f64, f64) {
    if moe_layers == 0 {
        return (0.0, 0.0);
    }
    let green = (green_limit_layers.min(moe_layers) as f64 / moe_layers as f64).clamp(0.0, 1.0);
    let yellow = green + (1.0 - green) / 2.0;
    (green, yellow)
}

/// Whether one comma-separated `--override-tensor` assignment
/// targets routed experts (its pattern mentions `exps`, as in
/// `exps=CPU` or `.*_exps.*=CPU`) — the assignments expert
/// offload owns. Mirrors [`expert_offload`]'s detection.
fn override_part_is_expert(part: &str) -> bool {
    part.split('=').next().unwrap_or("").contains("exps")
}

/// Rewrite the expert-offload tokens in raw llama-server flags,
/// leaving every other flag untouched and in order. All existing
/// offload spellings are removed first (`--n-cpu-moe` / `-ncmoe`
/// in both forms, `--cpu-moe` / `-cmoe`, and any `-ot` /
/// `--override-tensor` assignment whose pattern mentions `exps` —
/// sibling assignments in the same `-ot` list keep their flag),
/// then the canonical tokens for `offload` are appended. Returns
/// `None` when nothing remains.
pub fn set_expert_offload(raw_flags: Option<&str>, offload: ExpertOffload) -> Option<String> {
    let tokens: Vec<&str> = raw_flags.unwrap_or("").split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if t == "--n-cpu-moe" || t == "-ncmoe" {
            // Skip the value token too when it parses as a count;
            // a malformed value is left for the main walk.
            let has_value = tokens
                .get(i + 1)
                .map(|v| v.parse::<u32>().is_ok())
                .unwrap_or(false);
            i += if has_value { 2 } else { 1 };
            continue;
        }
        if t.starts_with("--n-cpu-moe=") || t.starts_with("-ncmoe=") {
            i += 1;
            continue;
        }
        if t == "--cpu-moe" || t == "-cmoe" {
            // Valueless all-experts spelling: drop the token alone.
            i += 1;
            continue;
        }
        let (flag, inline): (&str, Option<&str>) = if t == "-ot" || t == "--override-tensor" {
            (t, None)
        } else if let Some(v) = t.strip_prefix("--override-tensor=") {
            ("--override-tensor", Some(v))
        } else if let Some(v) = t.strip_prefix("-ot=") {
            ("-ot", Some(v))
        } else {
            out.push(t.to_string());
            i += 1;
            continue;
        };
        let value: Option<&str> = match inline {
            Some(v) => {
                i += 1;
                Some(v)
            }
            None => {
                let v = tokens.get(i + 1).copied();
                i += if v.is_some() { 2 } else { 1 };
                v
            }
        };
        let Some(value) = value else {
            // A trailing override flag with no value is malformed
            // but not ours to delete.
            out.push(flag.to_string());
            continue;
        };
        let kept: Vec<&str> = value
            .split(',')
            .filter(|part| !override_part_is_expert(part))
            .collect();
        if !kept.is_empty() {
            out.push(flag.to_string());
            out.push(kept.join(","));
        }
    }
    match offload {
        ExpertOffload::AllExperts => {
            out.push("-ot".to_string());
            out.push("exps=CPU".to_string());
        }
        ExpertOffload::CpuMoeLayers(n) if n > 0 => {
            out.push("--n-cpu-moe".to_string());
            out.push(n.to_string());
        }
        _ => {}
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join(" "))
    }
}

/// Detect expert offload in raw llama-server flags. Recognizes
/// `--override-tensor` / `-ot` assignments whose pattern mentions
/// `exps` with a CPU buffer (the `-ot exps=CPU` form the app itself
/// writes, plus `=` spellings), `--cpu-moe` / `-cmoe` (all experts
/// on the CPU), and `--n-cpu-moe N` / `-ncmoe N` (first N layers).
///
/// When both an all-experts spelling and a layer count are present
/// the all-experts setting wins regardless of order: llama.cpp
/// applies both, and `-ot exps=CPU` / `--cpu-moe` already put every
/// expert tensor on the CPU, leaving nothing for the layer count
/// to move. A repeated layer count resolves to the last one, the
/// same sequential-parse rule llama.cpp follows.
pub fn expert_offload(raw_flags: Option<&str>) -> ExpertOffload {
    let Some(raw) = raw_flags else {
        return ExpertOffload::None;
    };
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let mut layers: Option<u32> = None;
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if t == "--cpu-moe" || t == "-cmoe" {
            return ExpertOffload::AllExperts;
        }
        let assignment = if t == "-ot" || t == "--override-tensor" {
            i += 1;
            tokens.get(i).copied()
        } else {
            t.strip_prefix("--override-tensor=")
                .or_else(|| t.strip_prefix("-ot="))
        };
        if let Some(list) = assignment {
            for part in list.split(',') {
                if let Some((pattern, buffer)) = part.split_once('=') {
                    if pattern.contains("exps") && buffer.eq_ignore_ascii_case("CPU") {
                        return ExpertOffload::AllExperts;
                    }
                }
            }
        }
        if t == "--n-cpu-moe" || t == "-ncmoe" {
            if let Some(v) = tokens.get(i + 1) {
                if let Ok(n) = v.parse::<u32>() {
                    if n > 0 {
                        layers = Some(n);
                    }
                }
            }
        } else if let Some(v) = t
            .strip_prefix("--n-cpu-moe=")
            .or_else(|| t.strip_prefix("-ncmoe="))
        {
            if let Ok(n) = v.parse::<u32>() {
                if n > 0 {
                    layers = Some(n);
                }
            }
        }
        i += 1;
    }
    match layers {
        Some(n) => ExpertOffload::CpuMoeLayers(n),
        None => ExpertOffload::None,
    }
}

/// One backend's memory requirement under its current configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRequirement {
    /// Model weights resident on the GPU (all GPUs combined when a
    /// tensor split divides them; per-GPU shares are the placement
    /// projection's job).
    pub gpu_weights_mib: u64,
    /// Model weights resident in system RAM (offloaded experts).
    pub sys_weights_mib: u64,
    /// KV cache at the backend's configured context size, when the
    /// model's KV geometry is known. Weights only otherwise.
    pub kv_mib: Option<u64>,
    /// Recurrent-state cache (llama.cpp's "rs cache") at the
    /// backend's effective slot count, when the model's
    /// recurrent-state geometry is known (hybrid SSM / delta-net
    /// architectures). The state is per sequence and independent
    /// of context size; dense models carry none. Absent when the
    /// geometry is unknown — never counted as zero.
    pub rs_mib: Option<u64>,
    /// MoE expert cache (`--moe-cache-mib`): the TOTAL VRAM
    /// budget the backend reserves for caching host-resident
    /// experts on the GPU, across all placed GPUs (llama.cpp
    /// splits the one budget among the devices holding offloaded
    /// layers — see [`super::moe_cache::moe_cache_shares`] for
    /// the per-GPU distribution consumers price). 0 when the
    /// backend sets no cache. Set by the requirement builders
    /// from the decoded flags, not by [`memory_requirement`]
    /// (which prices weights/KV/state and never sees the flag).
    #[serde(default)]
    pub moe_cache_mib: u64,
    /// The backend's resolved offload position (MoE layers in
    /// system RAM) at requirement time — the distribution input
    /// [`super::moe_cache::moe_cache_shares`] needs alongside
    /// [`Self::moe_cache_moe_layers`]. 0 when unknown or no
    /// offload.
    #[serde(default)]
    pub moe_cache_offload_layers: u32,
    /// The model's MoE layer count at requirement time (the
    /// distribution's layer-range basis). 0 when unknown.
    #[serde(default)]
    pub moe_cache_moe_layers: u32,
    pub basis: MemoryBasis,
}

fn mib(bytes: u64) -> u64 {
    bytes.div_ceil(MIB)
}

/// Compute a backend's memory requirement.
///
/// `expert_bytes` is the model's routed-expert total (from the GGUF
/// layout or the catalog pin); `moe_layers` — the number of blocks
/// that carry routed experts — is only needed to resolve a partial
/// [`ExpertOffload::CpuMoeLayers`] share. It must be the MoE layer
/// count, not the total block count: `--n-cpu-moe N` counts MoE
/// layers, and hybrid models carry experts in only some blocks
/// (Nemotron: 24 of 53).
/// `kv_bytes_per_token` comes from the GGUF KV geometry.
///
/// `rs_bytes_per_seq` is the model's recurrent-state bytes for
/// one sequence (from the GGUF recurrent-state geometry) and
/// `n_seq` the backend's effective slot count: the state cache
/// llama.cpp allocates is the per-sequence figure times the slot
/// count, independent of `n_ctx`. (llama.cpp can add rollback
/// snapshot planes for speculative state restore; those are not
/// modeled — the term prices the live state, one row per slot.)
///
/// `expert_bytes_by_block` carries the per-MoE-block expert sums
/// from the GGUF layout (in block order), when available. A partial
/// layer offload moves the *first* N MoE blocks, whose byte total
/// is generally not the uniform `N / layers` share — Unsloth
/// Dynamic quants deliberately quantize different layers at
/// different rates. With per-block sums the partial share is the
/// true sum of the first N blocks and an [`MemoryBasis::Exact`]
/// input stays exact; without them the share falls back to the
/// uniform proportion and an `Exact` input is downgraded to
/// [`MemoryBasis::Estimate`] rather than asserting precision the
/// numbers do not have (a [`MemoryBasis::Catalog`] input stays
/// `Catalog` — the catalog pins only aggregates by design).
///
/// Degradation rule: when offload is configured but the expert
/// share cannot be resolved (unknown expert bytes, or a partial
/// layer offload without a MoE layer count), the split is
/// unresolvable and the requirement falls back to
/// all-weights-on-GPU with basis [`MemoryBasis::Estimate`] — never
/// a silently invented split.
// The parameter list is the layout itself (totals, expert shares,
// basis, offload, KV geometry, context) — callers pass a resolved
// layout's fields positionally, as the other multi-argument entry
// points in this crate do.
#[allow(clippy::too_many_arguments)]
pub fn memory_requirement(
    total_bytes: u64,
    expert_bytes: Option<u64>,
    expert_bytes_by_block: Option<&[u64]>,
    basis: MemoryBasis,
    offload: ExpertOffload,
    kv_bytes_per_token: Option<u64>,
    moe_layers: Option<u32>,
    n_ctx: u32,
    rs_bytes_per_seq: Option<u64>,
    n_seq: u32,
) -> MemoryRequirement {
    let kv_mib = kv_bytes_per_token.map(|per| mib(per.saturating_mul(n_ctx as u64)));
    let rs_mib = rs_bytes_per_seq
        .map(|per| mib(per.saturating_mul(n_seq.max(1) as u64)));
    let mut basis = basis;
    let sys_bytes: u64 = match offload {
        ExpertOffload::None => 0,
        ExpertOffload::AllExperts => match expert_bytes {
            Some(e) => e.min(total_bytes),
            None => {
                return MemoryRequirement {
                    gpu_weights_mib: mib(total_bytes),
                    sys_weights_mib: 0,
                    kv_mib,
                    rs_mib,
                    moe_cache_mib: 0,
                    moe_cache_offload_layers: 0,
                    moe_cache_moe_layers: 0,
                    basis: MemoryBasis::Estimate,
                }
            }
        },
        ExpertOffload::CpuMoeLayers(n) => match (expert_bytes, moe_layers) {
            (Some(e), Some(layers)) if layers > 0 => {
                let share = n.min(layers);
                match expert_bytes_by_block {
                    // The first `share` MoE blocks' true byte sum.
                    Some(blocks) if blocks.len() == layers as usize => {
                        blocks[..share as usize].iter().sum::<u64>()
                    }
                    _ => {
                        // Uniform-share approximation: exact only
                        // at the endpoints (nothing or everything
                        // offloaded); a genuinely partial share
                        // cannot carry an Exact basis.
                        if share > 0 && share < layers && basis == MemoryBasis::Exact {
                            basis = MemoryBasis::Estimate;
                        }
                        (e.min(total_bytes) * share as u64) / layers as u64
                    }
                }
            }
            _ => {
                return MemoryRequirement {
                    gpu_weights_mib: mib(total_bytes),
                    sys_weights_mib: 0,
                    kv_mib,
                    rs_mib,
                    moe_cache_mib: 0,
                    moe_cache_offload_layers: 0,
                    moe_cache_moe_layers: 0,
                    basis: MemoryBasis::Estimate,
                }
            }
        },
    };
    MemoryRequirement {
        gpu_weights_mib: mib(total_bytes - sys_bytes.min(total_bytes)),
        sys_weights_mib: mib(sys_bytes),
        kv_mib,
        rs_mib,
        moe_cache_mib: 0,
        moe_cache_offload_layers: 0,
        moe_cache_moe_layers: 0,
        basis,
    }
}

/// System-RAM weight bytes for every offload position
/// `0..=moe_layers`: entry `n` is the byte total
/// [`memory_requirement`] assigns to system RAM when the first
/// `n` MoE layers' experts are offloaded
/// ([`ExpertOffload::CpuMoeLayers`], resolved through
/// [`offload_for_layers`] at the endpoints). Position 0 is 0 and
/// position `moe_layers` is `expert_bytes`.
///
/// With per-block sums whose length matches `moe_layers`, entry
/// `n` is the true prefix sum of the first `n` blocks — the same
/// arithmetic `memory_requirement`'s `CpuMoeLayers` branch uses.
/// Without them (or with a mismatched list) it is the uniform
/// share `expert_bytes * n / moe_layers`, again mirroring that
/// branch. Callers that render a per-position table (the offload
/// slider's RAM figures, guardrail evaluation) read it from here
/// so the numbers can never drift from the requirement itself.
pub fn sys_bytes_by_position(
    expert_bytes: u64,
    expert_bytes_by_block: Option<&[u64]>,
    moe_layers: u32,
) -> Vec<u64> {
    if moe_layers == 0 {
        return vec![0];
    }
    let blocks = match expert_bytes_by_block {
        Some(blocks) if blocks.len() == moe_layers as usize => Some(blocks),
        _ => None,
    };
    (0..=moe_layers)
        .map(|n| match blocks {
            Some(blocks) => blocks[..n as usize].iter().sum(),
            None => expert_bytes * n as u64 / moe_layers as u64,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * MIB;

    #[test]
    fn detects_offload_spellings() {
        assert_eq!(
            expert_offload(Some("-ot exps=CPU")),
            ExpertOffload::AllExperts
        );
        assert_eq!(
            expert_offload(Some("--override-tensor exps=CPU")),
            ExpertOffload::AllExperts
        );
        assert_eq!(
            expert_offload(Some("--override-tensor=.*_exps.*=CPU")),
            ExpertOffload::AllExperts
        );
        assert_eq!(
            expert_offload(Some("--threads 8 -ot ffn=CUDA0")),
            ExpertOffload::None
        );
        assert_eq!(
            expert_offload(Some("--n-cpu-moe 24")),
            ExpertOffload::CpuMoeLayers(24)
        );
        assert_eq!(expert_offload(Some("--n-cpu-moe=0")), ExpertOffload::None);
        assert_eq!(expert_offload(Some("")), ExpertOffload::None);
        assert_eq!(expert_offload(None), ExpertOffload::None);
    }

    #[test]
    fn detects_cpu_moe_spellings() {
        // llama.cpp's other spellings: --cpu-moe / -cmoe put ALL
        // experts on the CPU; -ncmoe is --n-cpu-moe's short form.
        assert_eq!(expert_offload(Some("--cpu-moe")), ExpertOffload::AllExperts);
        assert_eq!(expert_offload(Some("-cmoe")), ExpertOffload::AllExperts);
        assert_eq!(
            expert_offload(Some("--threads 8 --cpu-moe")),
            ExpertOffload::AllExperts
        );
        assert_eq!(
            expert_offload(Some("-ncmoe 12")),
            ExpertOffload::CpuMoeLayers(12)
        );
        assert_eq!(
            expert_offload(Some("-ncmoe=12")),
            ExpertOffload::CpuMoeLayers(12)
        );
        // Both spellings present: all-experts wins in either order,
        // because the process applies both and -ot/--cpu-moe leave
        // nothing for the layer count to move.
        assert_eq!(
            expert_offload(Some("--n-cpu-moe 3 -ot exps=CPU")),
            ExpertOffload::AllExperts
        );
        assert_eq!(
            expert_offload(Some("-ot exps=CPU --n-cpu-moe 3")),
            ExpertOffload::AllExperts
        );
        assert_eq!(
            expert_offload(Some("--n-cpu-moe 3 --cpu-moe")),
            ExpertOffload::AllExperts
        );
        // A repeated layer count resolves to the last one.
        assert_eq!(
            expert_offload(Some("--n-cpu-moe 3 --n-cpu-moe 9")),
            ExpertOffload::CpuMoeLayers(9)
        );
    }

    #[test]
    fn rewrite_strips_cpu_moe_spellings() {
        // The surviving --cpu-moe would win in llama.cpp over the
        // slider's --n-cpu-moe, so every spelling must be removed
        // by the rewrite, in both directions.
        assert_eq!(
            set_expert_offload(Some("--cpu-moe"), ExpertOffload::CpuMoeLayers(8)).as_deref(),
            Some("--n-cpu-moe 8")
        );
        assert_eq!(
            set_expert_offload(Some("-cmoe --threads 4"), ExpertOffload::None).as_deref(),
            Some("--threads 4")
        );
        assert_eq!(
            set_expert_offload(Some("-ncmoe 5"), ExpertOffload::AllExperts).as_deref(),
            Some("-ot exps=CPU")
        );
        assert_eq!(
            set_expert_offload(Some("-ncmoe=5"), ExpertOffload::CpuMoeLayers(2)).as_deref(),
            Some("--n-cpu-moe 2")
        );
        // Round-trip: the detector reads back the target for
        // starts written in the new spellings too.
        for start in [Some("--cpu-moe"), Some("-ncmoe 7"), Some("-cmoe")] {
            for target in [
                ExpertOffload::None,
                ExpertOffload::CpuMoeLayers(13),
                ExpertOffload::AllExperts,
            ] {
                let out = set_expert_offload(start, target);
                assert_eq!(
                    expert_offload(out.as_deref()),
                    target,
                    "start {start:?} -> {target:?}"
                );
            }
        }
    }

    #[test]
    fn all_experts_offload_splits_weights() {
        // Coder-Next's real layout, rounded: 49 GB total, 46.7 GB experts.
        let r = memory_requirement(
            49 * GIB,
            Some(46 * GIB),
            None,
            MemoryBasis::Exact,
            ExpertOffload::AllExperts,
            Some(24_576),
            Some(48),
            32_768,
            None,
            4,
        );
        assert_eq!(r.sys_weights_mib, 46 * 1024);
        assert_eq!(r.gpu_weights_mib, 3 * 1024);
        // 24,576 B/token x 32,768 tokens = 768 MiB.
        assert_eq!(r.kv_mib, Some(768));
        assert_eq!(r.basis, MemoryBasis::Exact);
        // No recurrent-state geometry was passed: no state term.
        assert_eq!(r.rs_mib, None);
    }

    #[test]
    fn recurrent_state_term_completes_the_planner_moe_requirement() {
        // The planner-moe incident shape (Oct 20, 2026), in MiB:
        // the Whittle-MoE-27B Q8_0 file (27,350 MiB of tensors),
        // every routed expert on the CPU, 13,814 MiB of non-expert
        // weights left on the GPU, KV 2,048 MiB at n_ctx 32768
        // (65,536 B/token), and the recurrent-state geometry the
        // GGUF parse yields for the family (156,893,184 B per
        // sequence) at the auto slot count of 4. Before the rs
        // term existed this requirement summed to 15,862 MiB and
        // the start gate passed a configuration whose true total
        // — 16,460 MiB by llama.cpp's own allocation figures
        // (13,813.54 weights + 598.50 state + 2,048 KV) — exceeds
        // a 16,384 MiB card even empty.
        let mib_bytes = |m: u64| m * MIB;
        let r = memory_requirement(
            mib_bytes(27_350),
            Some(mib_bytes(13_536)),
            None,
            MemoryBasis::Exact,
            ExpertOffload::AllExperts,
            Some(65_536),
            Some(64),
            32_768,
            Some(156_893_184),
            4,
        );
        assert_eq!(r.gpu_weights_mib, 13_814);
        assert_eq!(r.sys_weights_mib, 13_536);
        assert_eq!(r.kv_mib, Some(2_048));
        // 156,893,184 B x 4 sequences = 598.5 MiB, ceiled to 599.
        assert_eq!(r.rs_mib, Some(599));
        let total = r.gpu_weights_mib + r.kv_mib.unwrap() + r.rs_mib.unwrap();
        assert_eq!(total, 16_461);
        assert!(total >= 16_460, "the incident's true requirement");
        assert!(total > 16_384, "over an empty 16 GiB card's total");
        // The state term scales with slots, not context: one
        // sequence carries a quarter of it.
        let r1 = memory_requirement(
            mib_bytes(27_350),
            Some(mib_bytes(13_536)),
            None,
            MemoryBasis::Exact,
            ExpertOffload::AllExperts,
            Some(65_536),
            Some(64),
            32_768,
            Some(156_893_184),
            1,
        );
        assert_eq!(r1.rs_mib, Some(150));
    }

    #[test]
    fn no_offload_counts_everything_on_gpu() {
        let r = memory_requirement(
            12 * GIB,
            Some(0),
            None,
            MemoryBasis::Exact,
            ExpertOffload::None,
            None,
            Some(48),
            32_768,
            None,
            4,
        );
        assert_eq!(r.gpu_weights_mib, 12 * 1024);
        assert_eq!(r.sys_weights_mib, 0);
        assert_eq!(r.kv_mib, None);
    }

    #[test]
    fn partial_cpu_moe_uses_layer_share() {
        // Without per-block sums the uniform share is an
        // approximation, so an Exact input is downgraded rather
        // than asserted.
        let r = memory_requirement(
            49 * GIB,
            Some(48 * GIB),
            None,
            MemoryBasis::Exact,
            ExpertOffload::CpuMoeLayers(24),
            None,
            Some(48),
            32_768,
            None,
            4,
        );
        assert_eq!(r.sys_weights_mib, 24 * 1024);
        assert_eq!(r.gpu_weights_mib, 25 * 1024);
        assert_eq!(r.basis, MemoryBasis::Estimate);
        // A Catalog input keeps its basis under the same fallback:
        // the catalog pins aggregates by design.
        let r = memory_requirement(
            49 * GIB,
            Some(48 * GIB),
            None,
            MemoryBasis::Catalog,
            ExpertOffload::CpuMoeLayers(24),
            None,
            Some(48),
            32_768,
            None,
            4,
        );
        assert_eq!(r.basis, MemoryBasis::Catalog);
    }

    #[test]
    fn partial_cpu_moe_prices_first_blocks_from_per_block_sums() {
        // Non-uniform per-block sizes (the Unsloth Dynamic case):
        // the first two of four MoE blocks hold 30 of the 40 GiB of
        // experts, so --n-cpu-moe 2 moves 30 GiB, not the uniform
        // 20 GiB — and the basis stays Exact.
        let blocks: [u64; 4] = [20 * GIB, 10 * GIB, 6 * GIB, 4 * GIB];
        let r = memory_requirement(
            49 * GIB,
            Some(40 * GIB),
            Some(&blocks),
            MemoryBasis::Exact,
            ExpertOffload::CpuMoeLayers(2),
            None,
            Some(4),
            32_768,
            None,
            4,
        );
        assert_eq!(r.sys_weights_mib, 30 * 1024);
        assert_eq!(r.gpu_weights_mib, 19 * 1024);
        assert_eq!(r.basis, MemoryBasis::Exact);
        // A block list that does not match the MoE layer count is
        // not the layout's own data: fall back to the uniform share
        // (and its downgrade).
        let r = memory_requirement(
            49 * GIB,
            Some(40 * GIB),
            Some(&blocks[..3]),
            MemoryBasis::Exact,
            ExpertOffload::CpuMoeLayers(2),
            None,
            Some(4),
            32_768,
            None,
            4,
        );
        assert_eq!(r.sys_weights_mib, 20 * 1024);
        assert_eq!(r.basis, MemoryBasis::Estimate);
    }

    #[test]
    fn partial_cpu_moe_counts_moe_layers_not_total_blocks() {
        // Nemotron 3.5 Lightning's real layout: 25,258,358,528 tensor
        // bytes, 22,829,334,528 of them routed experts spread over
        // 24 MoE blocks (of 53 total). --n-cpu-moe 16 offloads 16 of
        // the 24 MoE layers: ~14.2 GiB to system RAM, ~9.3 GiB of
        // weights left on the GPU — not the 16/53 share a total
        // block count would give.
        let r = memory_requirement(
            25_258_358_528,
            Some(22_829_334_528),
            None,
            MemoryBasis::Exact,
            ExpertOffload::CpuMoeLayers(16),
            None,
            Some(24),
            32_768,
            None,
            4,
        );
        assert_eq!(r.sys_weights_mib, 14_515);
        assert_eq!(r.gpu_weights_mib, 9_574);
        // No per-block sums were passed, so the uniform share is an
        // approximation of the real first-16 sum: not Exact.
        assert_eq!(r.basis, MemoryBasis::Estimate);
    }

    #[test]
    fn layers_positions_round_trip() {
        // Slider position <-> setting, at the boundaries of a
        // 24-layer model and inside it.
        for (layers, want) in [
            (0, ExpertOffload::None),
            (1, ExpertOffload::CpuMoeLayers(1)),
            (16, ExpertOffload::CpuMoeLayers(16)),
            (24, ExpertOffload::AllExperts),
            (99, ExpertOffload::AllExperts),
        ] {
            let off = offload_for_layers(layers, 24);
            assert_eq!(off, want, "layers {layers}");
            assert_eq!(off.layers(24), layers.min(24), "layers {layers}");
        }
        assert_eq!(ExpertOffload::CpuMoeLayers(99).layers(24), 24);
        assert_eq!(ExpertOffload::AllExperts.layers(48), 48);
        assert_eq!(ExpertOffload::None.layers(48), 0);
        assert_eq!(offload_for_layers(5, 0), ExpertOffload::None);
    }

    #[test]
    fn offload_flag_strings_are_canonical() {
        assert_eq!(offload_flag_string(ExpertOffload::None), None);
        assert_eq!(
            offload_flag_string(ExpertOffload::AllExperts).as_deref(),
            Some("-ot exps=CPU")
        );
        assert_eq!(
            offload_flag_string(ExpertOffload::CpuMoeLayers(16)).as_deref(),
            Some("--n-cpu-moe 16")
        );
        assert_eq!(offload_flag_string(ExpertOffload::CpuMoeLayers(0)), None);
    }

    #[test]
    fn bands_derive_from_the_green_limit() {
        // Nemotron: green to 16 of 24, yellow halfway through the
        // rest, red beyond.
        let (g, y) = offload_bands(24, 16);
        assert!((g - 16.0 / 24.0).abs() < 1e-9);
        assert!((y - (16.0 / 24.0 + (1.0 - 16.0 / 24.0) / 2.0)).abs() < 1e-9);
        // Coder-Next: the green limit is the whole range — full
        // offload is its design point, so nothing renders yellow
        // or red.
        assert_eq!(offload_bands(48, 48), (1.0, 1.0));
        // A limit past the layer count clamps; no layers, no bands.
        assert_eq!(offload_bands(24, 99), (1.0, 1.0));
        assert_eq!(offload_bands(0, 0), (0.0, 0.0));
    }

    #[test]
    fn rewrite_sets_partial_from_none() {
        assert_eq!(
            set_expert_offload(None, ExpertOffload::CpuMoeLayers(16)).as_deref(),
            Some("--n-cpu-moe 16")
        );
        assert_eq!(
            set_expert_offload(Some(""), ExpertOffload::CpuMoeLayers(16)).as_deref(),
            Some("--n-cpu-moe 16")
        );
    }

    #[test]
    fn rewrite_sets_max_in_ot_form() {
        assert_eq!(
            set_expert_offload(Some("--threads 8"), ExpertOffload::AllExperts).as_deref(),
            Some("--threads 8 -ot exps=CPU")
        );
    }

    #[test]
    fn rewrite_partial_to_max_and_back() {
        // Partial -> max replaces --n-cpu-moe with the -ot form.
        assert_eq!(
            set_expert_offload(Some("--n-cpu-moe 16"), ExpertOffload::AllExperts).as_deref(),
            Some("-ot exps=CPU")
        );
        // Max -> partial replaces the -ot form with --n-cpu-moe.
        assert_eq!(
            set_expert_offload(Some("-ot exps=CPU"), ExpertOffload::CpuMoeLayers(24)).as_deref(),
            Some("--n-cpu-moe 24")
        );
        // The `=` and long spellings normalize to the canonical form.
        assert_eq!(
            set_expert_offload(Some("--n-cpu-moe=16"), ExpertOffload::CpuMoeLayers(8)).as_deref(),
            Some("--n-cpu-moe 8")
        );
        assert_eq!(
            set_expert_offload(
                Some("--override-tensor exps=CPU"),
                ExpertOffload::CpuMoeLayers(8)
            )
            .as_deref(),
            Some("--n-cpu-moe 8")
        );
        assert_eq!(
            set_expert_offload(Some("-ot=exps=CPU"), ExpertOffload::AllExperts).as_deref(),
            Some("-ot exps=CPU")
        );
    }

    #[test]
    fn rewrite_to_zero_removes_offload_only() {
        assert_eq!(
            set_expert_offload(Some("--n-cpu-moe 16"), ExpertOffload::None),
            None
        );
        assert_eq!(
            set_expert_offload(Some("-ot exps=CPU"), ExpertOffload::None),
            None
        );
        assert_eq!(
            set_expert_offload(
                Some("--threads 8 --n-cpu-moe 24 --flash-attn on"),
                ExpertOffload::None
            )
            .as_deref(),
            Some("--threads 8 --flash-attn on")
        );
        // No offload present and none requested: unchanged.
        assert_eq!(
            set_expert_offload(Some("--threads 8"), ExpertOffload::None).as_deref(),
            Some("--threads 8")
        );
        assert_eq!(set_expert_offload(None, ExpertOffload::None), None);
    }

    #[test]
    fn rewrite_preserves_unrelated_flags_and_order() {
        assert_eq!(
            set_expert_offload(
                Some("--threads 8 --n-cpu-moe 4 --flash-attn on"),
                ExpertOffload::CpuMoeLayers(20)
            )
            .as_deref(),
            Some("--threads 8 --flash-attn on --n-cpu-moe 20")
        );
        // A non-expert override sharing the -ot flag survives; only
        // the exps assignment is offload.
        assert_eq!(
            set_expert_offload(Some("-ot ffn=CUDA0,exps=CPU"), ExpertOffload::None).as_deref(),
            Some("-ot ffn=CUDA0")
        );
        assert_eq!(
            set_expert_offload(
                Some("-ot exps=CPU -ot ffn=CUDA0"),
                ExpertOffload::CpuMoeLayers(3)
            )
            .as_deref(),
            Some("-ot ffn=CUDA0 --n-cpu-moe 3")
        );
        // An override that does not mention exps is not offload.
        assert_eq!(
            set_expert_offload(Some("-ot ffn=CPU"), ExpertOffload::AllExperts).as_deref(),
            Some("-ot ffn=CPU -ot exps=CPU")
        );
    }

    #[test]
    fn rewrite_result_detects_as_the_target() {
        // Whatever the rewrite writes, the detector reads back as
        // the requested setting.
        for start in [
            None,
            Some("--threads 8"),
            Some("-ot exps=CPU"),
            Some("--n-cpu-moe 7 --threads 2"),
        ] {
            for target in [
                ExpertOffload::None,
                ExpertOffload::CpuMoeLayers(13),
                ExpertOffload::AllExperts,
            ] {
                let out = set_expert_offload(start, target);
                assert_eq!(
                    expert_offload(out.as_deref()),
                    target,
                    "start {start:?} -> {target:?}"
                );
            }
        }
    }

    #[test]
    fn sys_bytes_by_position_agrees_with_memory_requirement_per_block() {
        // The same non-uniform layout as the per-block pricing
        // test: the position table must reproduce
        // memory_requirement's system-RAM figure at every
        // position, endpoints included — position 0 resolves
        // through offload_for_layers to ExpertOffload::None and
        // position 4 to AllExperts, not CpuMoeLayers.
        let blocks: [u64; 4] = [20 * GIB, 10 * GIB, 6 * GIB, 4 * GIB];
        let by_position = sys_bytes_by_position(40 * GIB, Some(&blocks), 4);
        assert_eq!(by_position, vec![0, 20 * GIB, 30 * GIB, 36 * GIB, 40 * GIB]);
        for n in 0..=4u32 {
            let r = memory_requirement(
                49 * GIB,
                Some(40 * GIB),
                Some(&blocks),
                MemoryBasis::Exact,
                offload_for_layers(n, 4),
                None,
                Some(4),
                32_768,
                None,
                4,
            );
            assert_eq!(
                mib(by_position[n as usize]),
                r.sys_weights_mib,
                "position {n}"
            );
        }
    }

    #[test]
    fn sys_bytes_by_position_agrees_with_memory_requirement_uniform() {
        // No per-block sums: the uniform share, at every position.
        let by_position = sys_bytes_by_position(40 * GIB, None, 4);
        assert_eq!(by_position, vec![0, 10 * GIB, 20 * GIB, 30 * GIB, 40 * GIB]);
        for n in 0..=4u32 {
            let r = memory_requirement(
                49 * GIB,
                Some(40 * GIB),
                None,
                MemoryBasis::Exact,
                offload_for_layers(n, 4),
                None,
                Some(4),
                32_768,
                None,
                4,
            );
            assert_eq!(
                mib(by_position[n as usize]),
                r.sys_weights_mib,
                "position {n}"
            );
        }
        // A block list that does not match the MoE layer count
        // falls back to the uniform share, like memory_requirement.
        let blocks: [u64; 4] = [20 * GIB, 10 * GIB, 6 * GIB, 4 * GIB];
        assert_eq!(
            sys_bytes_by_position(40 * GIB, Some(&blocks[..3]), 4),
            by_position
        );
        // No MoE layers: a single zero entry, no division by zero.
        assert_eq!(sys_bytes_by_position(40 * GIB, None, 0), vec![0]);
    }

    #[test]
    fn unresolvable_offload_degrades_to_estimate() {
        let r = memory_requirement(
            49 * GIB,
            None,
            None,
            MemoryBasis::Catalog,
            ExpertOffload::AllExperts,
            None,
            None,
            32_768,
            None,
            4,
        );
        assert_eq!(r.gpu_weights_mib, 49 * 1024);
        assert_eq!(r.sys_weights_mib, 0);
        assert_eq!(r.basis, MemoryBasis::Estimate);
    }
}
