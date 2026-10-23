//! Per-backend tuning sweeps (design Addendum B, §B.1–§B.6):
//! "Optimize this backend" — a user-directed, bounded sweep of
//! ONE existing backend's configuration (same model), returning
//! ranked, measured arms. Adoption is a separate explicit
//! action; nothing here mutates a saved configuration.
//!
//! This module holds the pure parts: the coordinate and arm
//! types, the axis value sets, sweep planning (exclusions are
//! computed, not discovered), the coordinate-descent driver,
//! the ranking rule, the thermal guard predicate, and the
//! sweep record's persistence (`optimize-tuning.json`). The
//! I/O runner (parking, temp backends, benchmarks, residency
//! verification) lives in the app glue
//! (`optimize_tuning.rs`), which drives the descent through
//! [`Descent`] one axis at a time; everything it decides that
//! can be decided purely is decided here.
//!
//! Reconciliation against the design draft (§B.9): the MoE
//! cache prerequisite the draft waited on has landed —
//! `--moe-cache-mib` is in the flag table, priced by
//! [`crate::fit::layout_requirement`], and carried on
//! `ExperimentFlags` — and the KV/flash-attention prerequisite
//! has landed (cache types + predicted-best auto-selection,
//! `manager_models::cache`), so both axes ship priced, per
//! decisions B.8.5 and B.8.9. The cache/FA values live in raw
//! flags (resolved by `manager_models::cache_selection`), not
//! in `ServerFlags` fields as the draft's §B.1 table sketched.

use std::path::Path;

use manager_config::{BackendConfig, SplitMode};
use manager_models::CacheSelection;
use serde::{Deserialize, Serialize};

use crate::eval::{EvalComparison, EvalRunRecord};
use crate::experiment::ExperimentMeasured;

/// Per-sweep arm budget, confirmation arm included (§B.5).
pub const TUNING_MAX_ARMS: usize = 16;
/// Per-arm wall-clock budget in seconds (§B.5): 15 minutes.
pub const TUNING_ARM_TIMEOUT_SECS: u64 = 15 * 60;
/// Per-sweep wall-clock budget in seconds (§B.5): 90 minutes.
pub const TUNING_SWEEP_TIMEOUT_SECS: u64 = 90 * 60;
/// Thermal abort threshold in °C on any tuned GPU (§B.4.2,
/// decision B.8.8): the P100's limit, applied to every GPU.
pub const THERMAL_ABORT_C: f32 = 80.0;
/// Consecutive thermal aborts that stop a sweep (§B.4.2).
pub const THERMAL_ABORT_STREAK: usize = 2;

/// Whether a sampled GPU temperature trips the thermal guard
/// (§B.4.2). An absent sample never trips it — the guard acts
/// on measurements, not on missing telemetry.
pub fn thermal_tripped(temp_c: Option<f32>) -> bool {
    temp_c.map(|t| t >= THERMAL_ABORT_C).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Coordinates
// ---------------------------------------------------------------------------

/// One complete configuration of the tuned backend's own
/// model: the coordinates on the five tuning axes (§B.1).
/// Every field is resolved — an arm never carries an
/// "inherit" marker; the resolvers that produced the
/// incumbent's values are the ones the arm generator varies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuningCoords {
    /// GPU indices the arm is placed on, ascending.
    pub gpu_indices: Vec<u32>,
    /// Tensor split over `gpu_indices`; empty for a
    /// single-GPU placement.
    pub tensor_split: Vec<f32>,
    /// Expert-offload position in MoE layers; `None` when the
    /// model has no MoE offload axis (dense / unknown).
    pub offload_layers: Option<u32>,
    /// `--moe-cache-mib`; `None` when unset (the disabled
    /// form). Only meaningful with an offload position
    /// above 0.
    pub moe_cache_mib: Option<u32>,
    /// MTP draft length; `None` is spec off (or the axis
    /// absent — the two are the same arm).
    pub mtp_n_max: Option<u32>,
    /// Context size in tokens.
    pub n_ctx: u32,
    /// KV cache types + flash attention (the coupled context
    /// group's parameters, §B.1).
    pub cache: CacheSelection,
    /// `--parallel` slot count.
    pub parallel_slots: u32,
}

impl TuningCoords {
    /// The arm's identity for deduplication (§B.3 filter 3):
    /// two axis combinations resolving to the same full
    /// configuration are one arm. The string form is stable
    /// across runs so a sweep record's arms stay comparable.
    pub fn fingerprint_key(&self) -> String {
        format!(
            "g{:?}|s{:?}|o{:?}|c{:?}|m{:?}|x{}|k{}|v{}|f{}|p{}",
            self.gpu_indices,
            self.tensor_split
                .iter()
                .map(|f| format!("{f:.3}"))
                .collect::<Vec<_>>(),
            self.offload_layers,
            self.moe_cache_mib,
            self.mtp_n_max,
            self.n_ctx,
            self.cache.k.as_str(),
            self.cache.v.as_str(),
            self.cache.flash_attn.as_str(),
            self.parallel_slots,
        )
    }

    /// Coordinates as deltas against `incumbent`, in the
    /// ranked table's Arm column form (§B.6): one
    /// "Label: old → new" entry per changed axis, unchanged
    /// axes omitted. An empty result is the incumbent itself.
    pub fn delta_labels(&self, incumbent: &TuningCoords) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if self.gpu_indices != incumbent.gpu_indices
            || self.tensor_split != incumbent.tensor_split
        {
            out.push(format!(
                "Placement: {} → {}",
                placement_label(incumbent),
                placement_label(self)
            ));
        }
        if self.offload_layers != incumbent.offload_layers {
            out.push(format!(
                "Offload: {} → {}",
                offload_label(incumbent.offload_layers),
                offload_label(self.offload_layers)
            ));
        }
        if self.moe_cache_mib != incumbent.moe_cache_mib {
            out.push(format!(
                "MoE cache: {} → {}",
                cache_mib_label(incumbent.moe_cache_mib),
                cache_mib_label(self.moe_cache_mib)
            ));
        }
        if self.mtp_n_max != incumbent.mtp_n_max {
            out.push(format!(
                "Spec: {} → {}",
                spec_label(incumbent.mtp_n_max),
                spec_label(self.mtp_n_max)
            ));
        }
        if self.n_ctx != incumbent.n_ctx {
            out.push(format!("Context: {} → {}", incumbent.n_ctx, self.n_ctx));
        }
        if self.cache != incumbent.cache {
            out.push(format!(
                "Cache: {}/{} FA {} → {}/{} FA {}",
                incumbent.cache.k.as_str(),
                incumbent.cache.v.as_str(),
                incumbent.cache.flash_attn.as_str(),
                self.cache.k.as_str(),
                self.cache.v.as_str(),
                self.cache.flash_attn.as_str(),
            ));
        }
        if self.parallel_slots != incumbent.parallel_slots {
            out.push(format!(
                "Slots: {} → {}",
                incumbent.parallel_slots, self.parallel_slots
            ));
        }
        out
    }
}

fn placement_label(c: &TuningCoords) -> String {
    if c.gpu_indices.is_empty() {
        return "CPU".to_string();
    }
    let gpus = c
        .gpu_indices
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("+");
    if c.tensor_split.len() > 1 {
        format!("GPU {gpus} (split)")
    } else {
        format!("GPU {gpus}")
    }
}

fn offload_label(layers: Option<u32>) -> String {
    match layers {
        None => "n/a".to_string(),
        Some(n) => n.to_string(),
    }
}

fn cache_mib_label(mib: Option<u32>) -> String {
    match mib {
        None | Some(0) => "off".to_string(),
        Some(n) => format!("{n} MiB"),
    }
}

fn spec_label(n_max: Option<u32>) -> String {
    match n_max {
        None => "off".to_string(),
        Some(n) => format!("n-max {n}"),
    }
}

// ---------------------------------------------------------------------------
// Axis value sets (§B.1)
// ---------------------------------------------------------------------------

/// One installed GPU as the planner sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuningGpu {
    pub index: u32,
    pub name: String,
    pub total_mib: u64,
    /// Compute capability, e.g. (8, 6); (0, 0) = unknown.
    pub compute_capability: (u32, u32),
    /// Whether the pinned llama.cpp can run flash attention
    /// on this architecture (§B.1: absent on Pascal sm_60).
    pub fa_supported: bool,
}

/// Everything sweep planning resolves about the target,
/// gathered by the caller from the live app: the incumbent's
/// coordinates, the model's axis capabilities, the installed
/// GPUs, and the predicted-best cache configuration the
/// context axis starts from (§B.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuningInputs {
    pub backend_id: String,
    pub model_id: String,
    pub incumbent: TuningCoords,
    /// MoE layer count when known; the offload axis exists
    /// only then (§B.1).
    pub moe_layers: Option<u32>,
    /// The catalog's MTP pin for the backend's model (§B.1):
    /// the spec axis exists only when true.
    pub mtp_capable: bool,
    /// The model's rated maximum context; 0 = unknown (the
    /// context axis then clamps only at its floor).
    pub ctx_max: u32,
    pub gpus: Vec<TuningGpu>,
    /// The predicted-best cache selection for the incumbent's
    /// placement/context (the backend form's auto-selection),
    /// when it could be priced. The context descent starts
    /// there (§B.5); absent when unpriced, in which case the
    /// cache/FA parameters stay at the incumbent's values and
    /// only context size varies.
    pub predicted_cache: Option<CacheSelection>,
}

/// The fixed axis order of the descent (§B.5).
pub const AXIS_ORDER: [&str; 5] = ["placement", "offload", "spec", "context", "slots"];

/// Placement value set (§B.1): each installed GPU alone, plus
/// the even layer split across each pair, in GPU-index order.
/// The incumbent's placement is always a member.
pub fn placement_values(inputs: &TuningInputs) -> Vec<(Vec<u32>, Vec<f32>)> {
    let mut out: Vec<(Vec<u32>, Vec<f32>)> = Vec::new();
    let mut push = |g: Vec<u32>, s: Vec<f32>| {
        if !out.iter().any(|(og, os)| *og == g && *os == s) {
            out.push((g, s));
        }
    };
    for gpu in &inputs.gpus {
        push(vec![gpu.index], Vec::new());
    }
    for i in 0..inputs.gpus.len() {
        for j in (i + 1)..inputs.gpus.len() {
            push(
                vec![inputs.gpus[i].index, inputs.gpus[j].index],
                vec![0.5, 0.5],
            );
        }
    }
    push(
        inputs.incumbent.gpu_indices.clone(),
        inputs.incumbent.tensor_split.clone(),
    );
    out
}

/// Offload value set (§B.1): {0, ⌈L/4⌉, ⌈L/2⌉, ⌈3L/4⌉, L}
/// deduplicated, plus the incumbent position. Empty when the
/// model has no MoE axis.
pub fn offload_values(inputs: &TuningInputs) -> Vec<u32> {
    let Some(l) = inputs.moe_layers else {
        return Vec::new();
    };
    let mut out: Vec<u32> = Vec::new();
    for v in [
        0,
        l.div_ceil(4),
        l.div_ceil(2),
        (3 * l).div_ceil(4),
        l,
    ] {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    if let Some(inc) = inputs.incumbent.offload_layers {
        if !out.contains(&inc) {
            out.push(inc);
        }
    }
    out.sort_unstable();
    out
}

/// Spec value set (§B.1): {off, 1, 2, 3}. Empty when the
/// model's catalog entry carries no MTP head.
pub fn spec_values(inputs: &TuningInputs) -> Vec<Option<u32>> {
    if !inputs.mtp_capable {
        return Vec::new();
    }
    let mut out: Vec<Option<u32>> = vec![None, Some(1), Some(2), Some(3)];
    if !out.contains(&inputs.incumbent.mtp_n_max) {
        out.push(inputs.incumbent.mtp_n_max);
    }
    out
}

/// Context value set (§B.1): {incumbent, incumbent ÷ 2,
/// incumbent × 2} clamped to [4096, ctx_max], deduplicated.
pub fn context_values(inputs: &TuningInputs) -> Vec<u32> {
    let inc = inputs.incumbent.n_ctx.max(4096);
    let mut out: Vec<u32> = Vec::new();
    for raw in [inc, inc / 2, inc.saturating_mul(2)] {
        let mut v = raw.max(4096);
        if inputs.ctx_max > 0 {
            v = v.min(inputs.ctx_max.max(4096));
        }
        if !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// The cache quality ladder (§B.1), highest first; flash
/// attention rides each rung as a separate coordinate the
/// descent flips (see [`context_group_values`]).
pub fn cache_ladder() -> Vec<(manager_models::CacheType, manager_models::CacheType)> {
    use manager_models::CacheType::*;
    vec![(F16, F16), (Q8_0, Q8_0), (Q8_0, Q4_0), (Q4_0, Q4_0)]
}

/// Slot value set (§B.1): {1, 2, 4} plus the incumbent value.
pub fn slot_values(inputs: &TuningInputs) -> Vec<u32> {
    let mut out: Vec<u32> = vec![1, 2, 4];
    if !out.contains(&inputs.incumbent.parallel_slots) {
        out.push(inputs.incumbent.parallel_slots);
    }
    out.sort_unstable();
    out
}

/// MoE cache candidates for one offload position (§B.1):
/// {0, ¼, ½} of the VRAM left free on the arm's primary GPU
/// after the arm's own priced requirement, rounded down to
/// 256 MiB. Empty unless experts sit on the CPU (offload
/// above 0).
pub fn moe_cache_values(offload_layers: Option<u32>, free_after_mib: u64) -> Vec<u32> {
    if offload_layers.unwrap_or(0) == 0 {
        return Vec::new();
    }
    let mut out: Vec<u32> = vec![0];
    for frac in [4u64, 2] {
        let v = ((free_after_mib / frac) / 256) * 256;
        if v > 0 && !out.contains(&(v as u32)) {
            out.push(v as u32);
        }
    }
    out
}

/// The context group's candidate cache selections for the
/// descent (§B.5): the predicted-best starting coordinate,
/// one ladder step either way from it, flash attention
/// flipped, and the incumbent's as-found values — so "the
/// prediction was wrong" is a measured outcome. When no
/// prediction could be priced, only the incumbent's values
/// are offered (an unpriced parameter is not swept, §B.1).
/// Flash-attention-on values are offered only when every
/// placed GPU supports it; the caller filters through
/// [`cache_fa_offered`].
pub fn context_cache_values(inputs: &TuningInputs) -> Vec<CacheSelection> {
    let inc = inputs.incumbent.cache;
    let mut out: Vec<CacheSelection> = vec![inc];
    let Some(predicted) = inputs.predicted_cache else {
        return out;
    };
    let ladder = cache_ladder();
    let pos = ladder
        .iter()
        .position(|(k, v)| *k == predicted.k && *v == predicted.v)
        .unwrap_or(0);
    for p in [pos.saturating_sub(1), pos, (pos + 1).min(ladder.len() - 1)] {
        let (k, v) = ladder[p];
        let sel = CacheSelection {
            k,
            v,
            flash_attn: predicted.flash_attn,
        };
        if !out.contains(&sel) {
            out.push(sel);
        }
    }
    let flipped = CacheSelection {
        k: predicted.k,
        v: predicted.v,
        flash_attn: match predicted.flash_attn {
            manager_models::FlashAttn::On => manager_models::FlashAttn::Off,
            _ => manager_models::FlashAttn::On,
        },
    };
    // A quantized V cache with flash attention explicitly off
    // is a combination llama.cpp refuses at context creation
    // (`CacheSelection::v_quantized_fa_off`): never an arm.
    if flipped.v_quantized_fa_off() {
        return out;
    }
    if !out.contains(&flipped) {
        out.push(flipped);
    }
    out
}

/// Whether a cache selection may be offered on a placement:
/// flash attention on requires every placed GPU to support
/// it (§B.1 capability gating — absent, not disabled).
pub fn cache_fa_offered(sel: &CacheSelection, placed: &[&TuningGpu]) -> bool {
    if sel.flash_attn != manager_models::FlashAttn::On {
        return true;
    }
    !placed.is_empty() && placed.iter().all(|g| g.fa_supported)
}

// ---------------------------------------------------------------------------
// Planning (§B.3): exclusions are computed, not discovered
// ---------------------------------------------------------------------------

/// One candidate coordinate excluded at planning, with its
/// reason in numbers (§B.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuningExclusion {
    pub coords: TuningCoords,
    pub reason: String,
}

/// The priced requirement of one arm: per-GPU MiB in
/// `gpu_indices` order, as the fit filter judges it.
#[derive(Debug, Clone, PartialEq)]
pub struct ArmPricing {
    pub per_gpu_mib: Vec<u64>,
}

/// The sweep preview (§B.2/§B.3): value sets per axis, the
/// arm count the descent will visit at most, the stop list,
/// the budgets, and every planning exclusion with its reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepPlan {
    pub backend_id: String,
    pub model_id: String,
    /// Axis name → its values in display form, in descent
    /// order; an absent axis is omitted.
    pub axes: Vec<(String, Vec<String>)>,
    /// Upper bound on arms the descent will measure,
    /// incumbent and confirmation arm included, before the
    /// budget cap (§B.5) is applied.
    pub planned_arms: usize,
    /// Backends the sweep will park (§B.4.1), computed by the
    /// caller from the sweep's GPU set.
    pub stop_list: Vec<String>,
    pub exclusions: Vec<TuningExclusion>,
    pub max_arms: usize,
    pub arm_timeout_secs: u64,
    pub sweep_timeout_secs: u64,
}

/// Apply the planning filters to one candidate coordinate:
/// fit against each target GPU's TOTAL VRAM (§B.3 filter 1 —
/// free VRAM is not a planning filter; the sweep parks what
/// it must), then the caller's guardrail verdict (filter 2).
/// `Ok(())` admits the arm; `Err(reason)` excludes it.
pub fn plan_filter(
    coords: &TuningCoords,
    inputs: &TuningInputs,
    pricing: Option<&ArmPricing>,
    guardrail_reason: Option<String>,
) -> Result<(), String> {
    if let Some(reason) = guardrail_reason {
        return Err(reason);
    }
    let Some(pricing) = pricing else {
        // Unpriced arms are admitted: §B.3's fit filter needs
        // a requirement, and where none can be computed the
        // start preflight remains the gate (its refusal is a
        // failed arm, §B.3). The cache/FA parameters are the
        // exception — they are only ever generated when
        // priced (§B.1), which the caller's value sets
        // guarantee.
        return Ok(());
    };
    for (i, gpu_index) in coords.gpu_indices.iter().enumerate() {
        let Some(gpu) = inputs.gpus.iter().find(|g| g.index == *gpu_index) else {
            continue;
        };
        let share = pricing.per_gpu_mib.get(i).copied().unwrap_or(0);
        if share > gpu.total_mib {
            return Err(format!(
                "GPU {} share {} MiB > {} MiB total",
                gpu.index, share, gpu.total_mib
            ));
        }
    }
    Ok(())
}

/// The descent's arm-count upper bound: the incumbent plus,
/// per axis, its non-incumbent values (each measured once
/// from the then-current winner), plus the MoE cache
/// sub-axis allowance and the confirmation arm — capped by
/// the sweep budget, which is the number the preview states.
pub fn planned_arm_count(inputs: &TuningInputs) -> usize {
    let mut n = 1usize; // the incumbent arm
    n += placement_values(inputs).len().saturating_sub(1);
    let offload = offload_values(inputs);
    n += offload.len().saturating_sub(1);
    if offload.iter().any(|l| *l > 0) {
        // Cache sub-axis: up to two non-zero sizes at the
        // winning position (§B.1 set {0, ¼, ½}).
        n += 2;
    }
    n += spec_values(inputs).len().saturating_sub(1);
    // Context group: context sizes × cache selections is the
    // group's grid at the winning placement — bounded here by
    // the larger of the two sets, as the descent visits the
    // group's coordinates jointly but the budget cap governs.
    n += context_values(inputs)
        .len()
        .max(context_cache_values(inputs).len())
        .saturating_sub(1);
    n += slot_values(inputs).len().saturating_sub(1);
    n += 1; // the confirmation arm (§B.6)
    n.min(TUNING_MAX_ARMS)
}

// ---------------------------------------------------------------------------
// Coordinate descent (§B.5)
// ---------------------------------------------------------------------------

/// The descent's state: the current winning coordinates and
/// every configuration already visited (measured, failed, or
/// excluded), so no configuration is measured twice (§B.3
/// filter 3 applied across the whole sweep).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Descent {
    pub current: TuningCoords,
    pub visited: Vec<String>,
}

impl Descent {
    pub fn new(incumbent: TuningCoords) -> Self {
        Self {
            current: incumbent,
            visited: Vec::new(),
        }
    }

    /// The candidate coordinates for one axis: the axis's
    /// values applied to the current winner, minus the
    /// current configuration itself and anything visited.
    /// The context axis varies its coupled group jointly:
    /// context sizes at the current cache selection, then
    /// cache selections at the current context.
    pub fn axis_candidates(&mut self, axis: &str, inputs: &TuningInputs) -> Vec<TuningCoords> {
        let mut out: Vec<TuningCoords> = Vec::new();
        let mut push = |c: TuningCoords, out: &mut Vec<TuningCoords>| {
            let key = c.fingerprint_key();
            if c == self.current || self.visited.contains(&key) || out.iter().any(|o: &TuningCoords| o.fingerprint_key() == key) {
                return;
            }
            self.visited.push(key);
            out.push(c);
        };
        match axis {
            "placement" => {
                for (gpus, split) in placement_values(inputs) {
                    let mut c = self.current.clone();
                    c.gpu_indices = gpus;
                    c.tensor_split = split;
                    push(c, &mut out);
                }
            }
            "offload" => {
                for layers in offload_values(inputs) {
                    let mut c = self.current.clone();
                    c.offload_layers = Some(layers);
                    if layers == 0 {
                        c.moe_cache_mib = None;
                    }
                    push(c, &mut out);
                }
            }
            "spec" => {
                for n in spec_values(inputs) {
                    let mut c = self.current.clone();
                    c.mtp_n_max = n;
                    if n.is_some() {
                        // Draft-mtp runs single-slot (§B.1):
                        // the slots axis is pinned on spec-on
                        // arms, stated on the arm.
                        c.parallel_slots = 1;
                    }
                    push(c, &mut out);
                }
            }
            "context" => {
                for n_ctx in context_values(inputs) {
                    let mut c = self.current.clone();
                    c.n_ctx = n_ctx;
                    push(c.clone(), &mut out);
                }
                for sel in context_cache_values(inputs) {
                    let mut c = self.current.clone();
                    c.cache = sel;
                    push(c, &mut out);
                }
            }
            "slots" => {
                // Free only on spec-off arms (§B.1 coupling).
                if self.current.mtp_n_max.is_none() {
                    for slots in slot_values(inputs) {
                        let mut c = self.current.clone();
                        c.parallel_slots = slots;
                        push(c, &mut out);
                    }
                }
            }
            _ => {}
        }
        out
    }

    /// MoE cache candidates at the current (winning) offload
    /// position, priced by the caller: `free_after_mib` is
    /// the primary GPU's free VRAM after the current arm's
    /// own requirement. Runs as the offload axis's sub-axis
    /// (§B.1) immediately after the position is chosen.
    pub fn cache_candidates(&mut self, free_after_mib: u64) -> Vec<TuningCoords> {
        let mut out: Vec<TuningCoords> = Vec::new();
        for mib in moe_cache_values(self.current.offload_layers, free_after_mib) {
            let mut c = self.current.clone();
            c.moe_cache_mib = if mib == 0 { None } else { Some(mib) };
            let key = c.fingerprint_key();
            if c == self.current || self.visited.contains(&key) {
                continue;
            }
            self.visited.push(key);
            out.push(c);
        }
        out
    }

    /// Keep an axis's winner (§B.5 step 2). The caller passes
    /// the best measured coordinates of the axis (or the
    /// current ones when every arm failed / was unverified —
    /// a dead axis keeps the incumbent value and the sweep
    /// proceeds).
    pub fn keep(&mut self, winner: TuningCoords) {
        self.current = winner;
    }
}

// ---------------------------------------------------------------------------
// Arms, ranking, and the sweep record (§B.4.4, §B.6)
// ---------------------------------------------------------------------------

/// An arm's terminal (or planning) status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArmStatus {
    /// Measured under the §A.1 protocol.
    Completed,
    /// Failed at runtime (start refused, server exit, health
    /// timeout, benchmark error): recorded with its reason,
    /// never retried within the sweep (§B.3).
    Failed,
    /// Aborted by the thermal guard (§B.4.2).
    AbortedThermal,
    /// Measured, but residency could not be verified:
    /// reported, excluded from the ranking, never the winner
    /// (§B.4.1).
    Unverified,
    /// Excluded at planning (§B.3); never spawned.
    Excluded,
}

/// The residency verdict an arm records (§B.4.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResidencyVerdict {
    VerifiedExclusive,
    Unverified { reason: String },
}

impl ResidencyVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResidencyVerdict::VerifiedExclusive => "verified_exclusive",
            ResidencyVerdict::Unverified { .. } => "unverified",
        }
    }
}

/// One arm of a sweep: its coordinates and everything the
/// ranked table and the record carry (§B.4.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuningArm {
    /// Position in the sweep's visit order; arm 0 is the
    /// incumbent.
    pub index: usize,
    /// The axis that produced this arm ("incumbent",
    /// "placement", "offload", "offload-cache", "spec",
    /// "context", "slots", "confirmation").
    pub axis: String,
    pub coords: TuningCoords,
    pub status: ArmStatus,
    /// Failure / abort / exclusion reason, in numbers where
    /// the planning filters produced them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured: Option<ExperimentMeasured>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residency: Option<ResidencyVerdict>,
    /// Per-GPU free VRAM (MiB) recorded at residency
    /// verification, in `coords.gpu_indices` order — an
    /// outside process's footprint is visible here rather
    /// than hidden (§B.4.1 scope honesty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub free_vram_mib: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_start_c: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_peak_c: Option<f32>,
    /// The measurement-store fingerprint the arm's figures
    /// were recorded under (its own — never the incumbent
    /// backend's, §B.4.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

impl TuningArm {
    /// The ranking figure: long-prompt median generation
    /// rate (§B.6).
    pub fn gen_long(&self) -> Option<f64> {
        self.measured
            .as_ref()
            .and_then(|m| m.long.predicted_per_second)
    }

    /// Whether the arm enters the ranking: completed under a
    /// verified-exclusive residency (§B.4.1, §B.6).
    pub fn rankable(&self) -> bool {
        self.status == ArmStatus::Completed
            && matches!(self.residency, Some(ResidencyVerdict::VerifiedExclusive))
            && self.gen_long().is_some()
    }
}

/// Rank arms per §B.6: rankable arms first, by long-prompt
/// median generation descending, ties on short-prompt
/// generation then TTFT; the rest below, in visit order —
/// the table is the whole evidence, not the flattering
/// subset. Returns arm indices in display order.
pub fn rank_arms(arms: &[TuningArm]) -> Vec<usize> {
    let mut ranked: Vec<usize> = arms
        .iter()
        .enumerate()
        .filter(|(_, a)| a.rankable())
        .map(|(i, _)| i)
        .collect();
    ranked.sort_by(|&x, &y| {
        let (a, b) = (&arms[x], &arms[y]);
        let key = |a: &TuningArm| {
            (
                a.gen_long().unwrap_or(f64::MIN),
                a.measured
                    .as_ref()
                    .and_then(|m| m.short.predicted_per_second)
                    .unwrap_or(f64::MIN),
                // Lower TTFT wins: negate for descending sort.
                -a.measured
                    .as_ref()
                    .and_then(|m| m.short.ttft_ms.or(m.long.ttft_ms))
                    .unwrap_or(f64::MAX),
            )
        };
        key(b).partial_cmp(&key(a)).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut rest: Vec<usize> = arms
        .iter()
        .enumerate()
        .filter(|(_, a)| !a.rankable())
        .map(|(i, _)| i)
        .collect();
    ranked.append(&mut rest);
    ranked
}

/// The best rankable arm's index, when any arm measured.
pub fn winner_index(arms: &[TuningArm]) -> Option<usize> {
    rank_arms(arms).into_iter().find(|&i| arms[i].rankable())
}

/// The confirmation arm's outcome (§B.6): reproducibility of
/// the winner's figure plus the §A.4 screening gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuningConfirmation {
    /// The confirmation arm's index in the sweep.
    pub arm_index: usize,
    /// Whether the confirmation's generation median is
    /// within 10% of the sweep arm's (§B.6 step 1).
    pub reproduced: bool,
    /// Confirmation-vs-sweep generation delta, percent.
    pub gen_delta_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval: Option<EvalRunRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison: Option<EvalComparison>,
    /// §A.4's replacement-claim rule: screen overall at or
    /// above baseline, and no category at zero that the
    /// baseline passes. `false` when no comparison exists —
    /// speed-only adoption is never the presented default.
    pub screen_passes: bool,
}

/// A sweep's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepState {
    Running,
    Completed,
    Cancelled,
    Failed,
}

/// One sweep's durable record (`optimize-tuning.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuningSweep {
    pub id: String,
    pub backend_id: String,
    pub model_id: String,
    pub state: SweepState,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub incumbent: TuningCoords,
    /// Backends parked for the sweep (§B.4.1), restarted
    /// afterwards whatever the outcome.
    pub stop_list: Vec<String>,
    pub arms: Vec<TuningArm>,
    /// Planning exclusions (§B.3), also listed in the results
    /// table below the ranked arms.
    pub exclusions: Vec<TuningExclusion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner_arm: Option<usize>,
    /// True when every measured arm lost to (or no arm beat)
    /// the incumbent: the incumbent stands, no adoption is
    /// offered (§B.5).
    pub incumbent_stands: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<TuningConfirmation>,
    /// Set when the arm or wall-clock budget ended the sweep
    /// early (§B.5): the truncation is stated, the arms
    /// measured so far are ranked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The winning arm was adopted into the backend's saved
    /// configuration through the structured save ops (§B.6).
    pub adopted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted_at: Option<String>,
    /// Adoption went through the explicit adopt-anyway flip
    /// (the screen did not pass, or the winner was not
    /// reproduced).
    pub adopted_anyway: bool,
}

/// Whether adoption may be presented as the default action
/// for a sweep (§B.6): a winner exists, it is not the
/// incumbent, and the confirmation arm reproduced it with
/// the screen passing. Otherwise adoption is adopt-anyway
/// only — or not offered at all when the incumbent stands.
pub fn adopt_is_default(sweep: &TuningSweep) -> bool {
    if sweep.incumbent_stands {
        return false;
    }
    let Some(winner) = sweep.winner_arm else {
        return false;
    };
    if sweep.arms.get(winner).map(|a| a.coords == sweep.incumbent).unwrap_or(true) {
        return false;
    }
    match &sweep.confirmation {
        Some(c) => c.reproduced && c.screen_passes,
        None => false,
    }
}

/// The on-disk sweep store (`<data_dir>/optimize-tuning.json`):
/// every sweep's record, in run order. A sweep's results are
/// the operator's work and survive an app restart (§B.2).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TuningStore {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub sweeps: Vec<TuningSweep>,
}

impl TuningStore {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| e.to_string())
    }

    pub fn upsert(&mut self, sweep: TuningSweep) {
        if let Some(existing) = self.sweeps.iter_mut().find(|s| s.id == sweep.id) {
            *existing = sweep;
        } else {
            self.sweeps.push(sweep);
        }
    }

    pub fn latest_for(&self, backend_id: &str) -> Option<&TuningSweep> {
        self.sweeps.iter().rev().find(|s| s.backend_id == backend_id)
    }

    pub fn running(&self) -> Option<&TuningSweep> {
        self.sweeps.iter().rev().find(|s| s.state == SweepState::Running)
    }
}

// ---------------------------------------------------------------------------
// Temp backend construction (§B.1, §B.6 preservation)
// ---------------------------------------------------------------------------

/// Build an arm's temporary backend from the incumbent
/// backend's own configuration: the incumbent cloned, with
/// the arm's coordinates written through the same structured
/// writers the app itself uses (`set_expert_offload`,
/// `set_moe_cache`, `set_mtp_draft`, `set_parallel_slots`,
/// `set_cache_selection`) and its placement/context in the
/// flags struct. Everything the axes do not own — threads,
/// batch size, the operator's other raw flags — is preserved
/// verbatim, which is what the writers guarantee (§B.6). The
/// temp backend lives outside every gateway group and is
/// never written to the saved config (the runner registers
/// it in memory only, per §A.3).
pub fn tuning_temp_backend(
    incumbent: &BackendConfig,
    coords: &TuningCoords,
    temp_id: &str,
    port: u16,
    moe_layers: Option<u32>,
) -> BackendConfig {
    let mut temp = incumbent.clone();
    temp.id = temp_id.to_string();
    temp.port = port;
    temp.flags.n_ctx = coords.n_ctx;
    temp.flags.tensor_split = coords.tensor_split.clone();
    temp.flags.split_mode = if coords.tensor_split.len() > 1 {
        SplitMode::Layer
    } else {
        SplitMode::None
    };
    let mut raw = incumbent.raw_flags.clone();
    if let Some(layers) = coords.offload_layers {
        let offload = match moe_layers {
            Some(moe) => manager_models::offload_for_layers(layers, moe),
            None => manager_models::ExpertOffload::CpuMoeLayers(layers),
        };
        raw = manager_models::set_expert_offload(raw.as_deref(), offload);
    }
    raw = manager_models::set_moe_cache(raw.as_deref(), coords.moe_cache_mib);
    raw = match coords.mtp_n_max {
        Some(n) => manager_models::set_mtp_draft(raw.as_deref(), manager_models::MtpDraft::On(Some(n))),
        None => manager_models::set_mtp_draft(raw.as_deref(), manager_models::MtpDraft::Off),
    };
    raw = manager_models::set_parallel_slots(raw.as_deref(), Some(i64::from(coords.parallel_slots)));
    raw = manager_models::set_cache_selection(raw.as_deref(), Some(coords.cache));
    temp.raw_flags = raw;
    temp
}

#[cfg(test)]
mod tests {
    use super::*;
    use manager_models::{CacheType, FlashAttn};

    fn gpu(index: u32, total: u64, cc: (u32, u32), fa: bool) -> TuningGpu {
        TuningGpu {
            index,
            name: format!("GPU{index}"),
            total_mib: total,
            compute_capability: cc,
            fa_supported: fa,
        }
    }

    fn inputs() -> TuningInputs {
        TuningInputs {
            backend_id: "planner-mtp".to_string(),
            model_id: "qwen3.8-27b-mtp".to_string(),
            incumbent: TuningCoords {
                gpu_indices: vec![0],
                tensor_split: vec![],
                offload_layers: Some(0),
                moe_cache_mib: None,
                mtp_n_max: None,
                n_ctx: 32768,
                cache: CacheSelection::default(),
                parallel_slots: 1,
            },
            moe_layers: Some(48),
            mtp_capable: true,
            ctx_max: 262144,
            gpus: vec![
                gpu(0, 16384, (8, 6), true),
                gpu(1, 16384, (6, 0), false),
            ],
            predicted_cache: Some(CacheSelection {
                k: CacheType::Q8_0,
                v: CacheType::Q4_0,
                flash_attn: FlashAttn::On,
            }),
        }
    }

    #[test]
    fn value_sets_match_b1() {
        let inp = inputs();
        assert_eq!(offload_values(&inp), vec![0, 12, 24, 36, 48]);
        assert_eq!(spec_values(&inp), vec![None, Some(1), Some(2), Some(3)]);
        assert_eq!(context_values(&inp), vec![32768, 16384, 65536]);
        assert_eq!(slot_values(&inp), vec![1, 2, 4]);
        let placements = placement_values(&inp);
        assert!(placements.contains(&(vec![0], vec![])));
        assert!(placements.contains(&(vec![1], vec![])));
        assert!(placements.contains(&(vec![0, 1], vec![0.5, 0.5])));
    }

    #[test]
    fn coder_endpoints_come_from_the_guardrail_filter_not_the_set() {
        // The offload SET is the full ladder; the (6,0)
        // EndpointsOnly exclusion is a planning filter over
        // placements (§B.3, §B.7 worked example).
        let inp = inputs();
        assert_eq!(offload_values(&inp).len(), 5);
        let pricing = ArmPricing {
            per_gpu_mib: vec![17000],
        };
        let mut coords = inp.incumbent.clone();
        coords.offload_layers = Some(24);
        let err = plan_filter(&coords, &inp, Some(&pricing), None).unwrap_err();
        assert!(err.contains("17000 MiB > 16384 MiB"), "{err}");
    }

    #[test]
    fn descent_visits_each_axis_once_and_pins_slots_on_spec() {
        let inp = inputs();
        let mut d = Descent::new(inp.incumbent.clone());
        let placements = d.axis_candidates("placement", &inp);
        assert_eq!(placements.len(), 2); // GPU 1 alone + the even split
        let spec = d.axis_candidates("spec", &inp);
        assert_eq!(spec.len(), 3);
        assert!(spec.iter().all(|c| c.parallel_slots == 1));
        // Slots are pinned while a spec winner is current.
        d.keep(spec[0].clone());
        assert!(d.axis_candidates("slots", &inp).is_empty());
    }

    #[test]
    fn cache_values_need_an_offload_position() {
        assert!(moe_cache_values(Some(0), 8192).is_empty());
        assert!(moe_cache_values(None, 8192).is_empty());
        assert_eq!(moe_cache_values(Some(24), 8192), vec![0, 2048, 4096]);
    }

    #[test]
    fn fa_on_is_not_offered_on_pascal() {
        let inp = inputs();
        let pascal = &inp.gpus[1];
        let on = CacheSelection {
            k: CacheType::F16,
            v: CacheType::F16,
            flash_attn: FlashAttn::On,
        };
        assert!(!cache_fa_offered(&on, &[pascal]));
        assert!(cache_fa_offered(&on, &[&inp.gpus[0]]));
    }

    #[test]
    fn ranking_excludes_unverified_and_failed_arms() {
        let measured = |gen: f64| {
            Some(ExperimentMeasured {
                short: crate::measure::BenchResult::from_runs(vec![]),
                long: crate::measure::BenchResult {
                    prompt_per_second: Some(100.0),
                    predicted_per_second: Some(gen),
                    ttft_ms: Some(50.0),
                    runs: vec![],
                },
            })
        };
        let arm = |index: usize, status: ArmStatus, residency: Option<ResidencyVerdict>, gen: f64| {
            TuningArm {
                index,
                axis: "spec".to_string(),
                coords: inputs().incumbent.clone(),
                status,
                detail: None,
                measured: measured(gen),
                residency,
                free_vram_mib: vec![],
                temp_start_c: None,
                temp_peak_c: None,
                fingerprint: None,
            }
        };
        let arms = vec![
            arm(0, ArmStatus::Completed, Some(ResidencyVerdict::VerifiedExclusive), 22.5),
            arm(1, ArmStatus::Completed, Some(ResidencyVerdict::VerifiedExclusive), 29.5),
            arm(2, ArmStatus::Completed, Some(ResidencyVerdict::Unverified { reason: "x".into() }), 99.0),
            arm(3, ArmStatus::Failed, None, 0.0),
        ];
        let order = rank_arms(&arms);
        assert_eq!(order[0], 1);
        assert_eq!(order[1], 0);
        assert_eq!(winner_index(&arms), Some(1));
    }

    #[test]
    fn thermal_guard_trips_at_80() {
        assert!(!thermal_tripped(Some(79.9)));
        assert!(thermal_tripped(Some(80.0)));
        assert!(!thermal_tripped(None));
    }

    #[test]
    fn temp_backend_preserves_unowned_flags() {
        let incumbent = BackendConfig {
            id: "planner-mtp".to_string(),
            enabled: true,
            model_file: std::path::PathBuf::from("models/planner.gguf"),
            port: 9000,
            flags: manager_config::ServerFlags {
                n_ctx: 32768,
                n_batch: 512,
                split_mode: SplitMode::None,
                tensor_split: vec![],
            },
            raw_flags: Some("--threads 8".to_string()),
            flags_advanced: false,
            restart_policy: manager_config::RestartPolicy {
                max_retries: 5,
                backoff_base_secs: 2,
            },
            overrides_global: false,
            purpose: String::new(),
            tool_call_probe: None,
        };
        let mut coords = inputs().incumbent.clone();
        coords.mtp_n_max = Some(1);
        let temp = tuning_temp_backend(&incumbent, &coords, "opt-exp-tuning-1", 19000, Some(48));
        let raw = temp.raw_flags.unwrap();
        assert!(raw.contains("--threads 8"), "{raw}");
        assert!(raw.contains("draft-mtp"), "{raw}");
        assert!(raw.contains("--parallel 1"), "{raw}");
        assert_eq!(temp.id, "opt-exp-tuning-1");
        assert_eq!(temp.port, 19000);
    }
}
