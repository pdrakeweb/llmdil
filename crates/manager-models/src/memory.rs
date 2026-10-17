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
/// offload spellings are removed first (`--n-cpu-moe` in both
/// forms, and any `-ot` / `--override-tensor` assignment whose
/// pattern mentions `exps` — sibling assignments in the same
/// `-ot` list keep their flag), then the canonical tokens for
/// `offload` are appended. Returns `None` when nothing remains.
pub fn set_expert_offload(raw_flags: Option<&str>, offload: ExpertOffload) -> Option<String> {
    let tokens: Vec<&str> = raw_flags.unwrap_or("").split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if t == "--n-cpu-moe" {
            // Skip the value token too when it parses as a count;
            // a malformed value is left for the main walk.
            let has_value = tokens
                .get(i + 1)
                .map(|v| v.parse::<u32>().is_ok())
                .unwrap_or(false);
            i += if has_value { 2 } else { 1 };
            continue;
        }
        if t.starts_with("--n-cpu-moe=") {
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
/// layout or the catalog pin); `moe_layers` — the number of blocks
/// that carry routed experts — is only needed to resolve a partial
/// [`ExpertOffload::CpuMoeLayers`] share. It must be the MoE layer
/// count, not the total block count: `--n-cpu-moe N` counts MoE
/// layers, and hybrid models carry experts in only some blocks
/// (Nemotron: 24 of 53).
/// `kv_bytes_per_token` comes from the GGUF KV geometry.
///
/// Degradation rule: when offload is configured but the expert
/// share cannot be resolved (unknown expert bytes, or a partial
/// layer offload without a MoE layer count), the split is
/// unresolvable and the requirement falls back to
/// all-weights-on-GPU with basis [`MemoryBasis::Estimate`] — never
/// a silently invented split.
pub fn memory_requirement(
    total_bytes: u64,
    expert_bytes: Option<u64>,
    basis: MemoryBasis,
    offload: ExpertOffload,
    kv_bytes_per_token: Option<u64>,
    moe_layers: Option<u32>,
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
        ExpertOffload::CpuMoeLayers(n) => match (expert_bytes, moe_layers) {
            (Some(e), Some(layers)) if layers > 0 => {
                let share = n.min(layers) as u64;
                (e.min(total_bytes) * share) / layers as u64
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
            MemoryBasis::Exact,
            ExpertOffload::CpuMoeLayers(16),
            None,
            Some(24),
            32_768,
        );
        assert_eq!(r.sys_weights_mib, 14_515);
        assert_eq!(r.gpu_weights_mib, 9_574);
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
