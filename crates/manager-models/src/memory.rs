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
    /// of the same override).
    AllExperts,
    /// Experts of the first N layers on the CPU (`--n-cpu-moe N`).
    CpuMoeLayers(u32),
}

/// Detect expert offload in raw llama-server flags. Recognizes
/// `--override-tensor` / `-ot` assignments whose pattern mentions
/// `exps` with a CPU buffer (the `-ot exps=CPU` form the app itself
/// writes, plus `=` spellings), and `--n-cpu-moe N`.
pub fn expert_offload(raw_flags: Option<&str>) -> ExpertOffload {
    let Some(raw) = raw_flags else {
        return ExpertOffload::None;
    };
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
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
        if t == "--n-cpu-moe" {
            if let Some(v) = tokens.get(i + 1) {
                if let Ok(n) = v.parse::<u32>() {
                    if n > 0 {
                        return ExpertOffload::CpuMoeLayers(n);
                    }
                }
            }
        } else if let Some(v) = t.strip_prefix("--n-cpu-moe=") {
            if let Ok(n) = v.parse::<u32>() {
                if n > 0 {
                    return ExpertOffload::CpuMoeLayers(n);
                }
            }
        }
        i += 1;
    }
    ExpertOffload::None
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
    pub basis: MemoryBasis,
}

fn mib(bytes: u64) -> u64 {
    bytes.div_ceil(MIB)
}

/// Compute a backend's memory requirement.
///
/// `expert_bytes` is the model's routed-expert total (from the GGUF
/// layout or the catalog pin); `block_count` is only needed to
/// resolve a partial [`ExpertOffload::CpuMoeLayers`] share.
/// `kv_bytes_per_token` comes from the GGUF KV geometry.
///
/// Degradation rule: when offload is configured but the expert
/// share cannot be resolved (unknown expert bytes, or a partial
/// layer offload without a block count), the split is unresolvable
/// and the requirement falls back to all-weights-on-GPU with basis
/// [`MemoryBasis::Estimate`] — never a silently invented split.
pub fn memory_requirement(
    total_bytes: u64,
    expert_bytes: Option<u64>,
    basis: MemoryBasis,
    offload: ExpertOffload,
    kv_bytes_per_token: Option<u64>,
    block_count: Option<u32>,
    n_ctx: u32,
) -> MemoryRequirement {
    let kv_mib = kv_bytes_per_token.map(|per| mib(per.saturating_mul(n_ctx as u64)));
    let sys_bytes: u64 = match offload {
        ExpertOffload::None => 0,
        ExpertOffload::AllExperts => match expert_bytes {
            Some(e) => e.min(total_bytes),
            None => {
                return MemoryRequirement {
                    gpu_weights_mib: mib(total_bytes),
                    sys_weights_mib: 0,
                    kv_mib,
                    basis: MemoryBasis::Estimate,
                }
            }
        },
        ExpertOffload::CpuMoeLayers(n) => match (expert_bytes, block_count) {
            (Some(e), Some(blocks)) if blocks > 0 => {
                let share = n.min(blocks) as u64;
                (e.min(total_bytes) * share) / blocks as u64
            }
            _ => {
                return MemoryRequirement {
                    gpu_weights_mib: mib(total_bytes),
                    sys_weights_mib: 0,
                    kv_mib,
                    basis: MemoryBasis::Estimate,
                }
            }
        },
    };
    MemoryRequirement {
        gpu_weights_mib: mib(total_bytes - sys_bytes.min(total_bytes)),
        sys_weights_mib: mib(sys_bytes),
        kv_mib,
        basis,
    }
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
    fn all_experts_offload_splits_weights() {
        // Coder-Next's real layout, rounded: 49 GB total, 46.7 GB experts.
        let r = memory_requirement(
            49 * GIB,
            Some(46 * GIB),
            MemoryBasis::Exact,
            ExpertOffload::AllExperts,
            Some(24_576),
            Some(48),
            32_768,
        );
        assert_eq!(r.sys_weights_mib, 46 * 1024);
        assert_eq!(r.gpu_weights_mib, 3 * 1024);
        // 24,576 B/token x 32,768 tokens = 768 MiB.
        assert_eq!(r.kv_mib, Some(768));
        assert_eq!(r.basis, MemoryBasis::Exact);
    }

    #[test]
    fn no_offload_counts_everything_on_gpu() {
        let r = memory_requirement(
            12 * GIB,
            Some(0),
            MemoryBasis::Exact,
            ExpertOffload::None,
            None,
            Some(48),
            32_768,
        );
        assert_eq!(r.gpu_weights_mib, 12 * 1024);
        assert_eq!(r.sys_weights_mib, 0);
        assert_eq!(r.kv_mib, None);
    }

    #[test]
    fn partial_cpu_moe_uses_layer_share() {
        let r = memory_requirement(
            49 * GIB,
            Some(48 * GIB),
            MemoryBasis::Exact,
            ExpertOffload::CpuMoeLayers(24),
            None,
            Some(48),
            32_768,
        );
        assert_eq!(r.sys_weights_mib, 24 * 1024);
        assert_eq!(r.gpu_weights_mib, 25 * 1024);
    }

    #[test]
    fn unresolvable_offload_degrades_to_estimate() {
        let r = memory_requirement(
            49 * GIB,
            None,
            MemoryBasis::Catalog,
            ExpertOffload::AllExperts,
            None,
            None,
            32_768,
        );
        assert_eq!(r.gpu_weights_mib, 49 * 1024);
        assert_eq!(r.sys_weights_mib, 0);
        assert_eq!(r.basis, MemoryBasis::Estimate);
    }
}
