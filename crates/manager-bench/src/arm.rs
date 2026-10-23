//! Sweep arm types: one arm is one complete, launchable
//! configuration (design §3.1), plus sweep expansion, validity
//! pruning (design §3.2, D11), the shipped presets (design §3.5,
//! D16), and the wall-clock projection (design §2.1 cost shape).

use serde::{Deserialize, Serialize};

/// KV cache element type for one cache (K or V), as accepted by
/// llama.cpp's `--cache-type-k` / `--cache-type-v`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CacheType {
    #[serde(rename = "f16")]
    #[default]
    F16,
    #[serde(rename = "q8_0")]
    Q8_0,
    #[serde(rename = "q4_0")]
    Q4_0,
}

impl CacheType {
    /// The llama.cpp flag value for this cache type.
    pub fn flag(&self) -> &'static str {
        match self {
            CacheType::F16 => "f16",
            CacheType::Q8_0 => "q8_0",
            CacheType::Q4_0 => "q4_0",
        }
    }

    /// Whether this type is a quantized cache (anything but f16).
    pub fn quantized(&self) -> bool {
        !matches!(self, CacheType::F16)
    }
}

/// Flash attention mode, as accepted by `--flash-attn`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FlashAttn {
    #[serde(rename = "auto")]
    #[default]
    Auto,
    #[serde(rename = "on")]
    On,
    #[serde(rename = "off")]
    Off,
}

impl FlashAttn {
    /// The llama.cpp flag value for this mode.
    pub fn flag(&self) -> &'static str {
        match self {
            FlashAttn::Auto => "auto",
            FlashAttn::On => "on",
            FlashAttn::Off => "off",
        }
    }
}

/// Speculative decoding configuration of an arm. `Combined` is
/// ngram-mod + draft-simple, in that order — order is priority
/// (kit §1.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SpecKind {
    #[serde(rename = "none")]
    #[default]
    None,
    #[serde(rename = "ngram-mod")]
    NgramMod,
    #[serde(rename = "draft-simple")]
    DraftSimple,
    #[serde(rename = "combined")]
    Combined,
}

impl SpecKind {
    /// The `--spec-type` argument for this kind, or `None` when no
    /// `--spec-type` token is emitted at all.
    pub fn spec_type_arg(&self) -> Option<&'static str> {
        match self {
            SpecKind::None => None,
            SpecKind::NgramMod => Some("ngram-mod"),
            SpecKind::DraftSimple => Some("draft-simple"),
            SpecKind::Combined => Some("ngram-mod,draft-simple"),
        }
    }

    /// Whether this kind runs an n-gram drafter.
    pub fn has_ngram(&self) -> bool {
        matches!(self, SpecKind::NgramMod | SpecKind::Combined)
    }

    /// Whether this kind runs a draft-model drafter.
    pub fn has_draft(&self) -> bool {
        matches!(self, SpecKind::DraftSimple | SpecKind::Combined)
    }
}

/// Where the draft model runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DraftDevice {
    #[serde(rename = "target_gpu")]
    #[default]
    TargetGpu,
    #[serde(rename = "cpu")]
    Cpu,
}

/// Format an integer with thousands separators, for arm labels.
fn thousands(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A fully resolved arm: every axis value the runner launches from.
/// Fields that do not apply to the arm's spec kind are held at their
/// cleared/default values by [`expand`]'s normalization, so two arms
/// that launch identically also compare equal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ArmSpec {
    pub cache_k: CacheType,
    pub cache_v: CacheType,
    pub flash_attn: FlashAttn,
    /// Padding depth in tokens: the working-context size at which
    /// generation is measured (design §2.1). 0 = no padding.
    pub depth: u32,
    /// The context size the arm's backend is launched with.
    pub n_ctx: u32,
    pub spec: SpecKind,
    pub ngram_n_min: Option<u32>,
    pub draft_model_id: Option<String>,
    pub draft_n_max: Option<u32>,
    pub draft_device: DraftDevice,
    pub draft_cache_k: CacheType,
    pub draft_cache_v: CacheType,
    /// Offload expression (`-ot` / `-ncmoe` replacement tokens,
    /// verbatim), when the arm overrides the backend's offload.
    pub offload_expr: Option<String>,
}

impl Default for ArmSpec {
    fn default() -> Self {
        ArmSpec {
            cache_k: CacheType::F16,
            cache_v: CacheType::F16,
            flash_attn: FlashAttn::Auto,
            depth: 0,
            n_ctx: 0,
            spec: SpecKind::None,
            ngram_n_min: None,
            draft_model_id: None,
            draft_n_max: None,
            draft_device: DraftDevice::TargetGpu,
            draft_cache_k: CacheType::F16,
            draft_cache_v: CacheType::F16,
            offload_expr: None,
        }
    }
}

impl ArmSpec {
    /// Short human label naming the non-default axes, e.g.
    /// `q8_0/q8_0 · FA on · depth 30,720 · combined (n-min 16, draft n-max 3)`.
    pub fn label(&self) -> String {
        let mut parts: Vec<String> = vec![format!(
            "{}/{}",
            self.cache_k.flag(),
            self.cache_v.flag()
        )];
        match self.flash_attn {
            FlashAttn::Auto => {}
            FlashAttn::On => parts.push("FA on".to_string()),
            FlashAttn::Off => parts.push("FA off".to_string()),
        }
        if self.depth > 0 {
            parts.push(format!("depth {}", thousands(self.depth)));
        }
        match self.spec {
            SpecKind::None => {}
            SpecKind::NgramMod => {
                let mut s = "ngram-mod".to_string();
                if let Some(n) = self.ngram_n_min {
                    s.push_str(&format!(" (n-min {n})"));
                }
                parts.push(s);
            }
            SpecKind::DraftSimple => {
                let mut s = "draft-simple".to_string();
                if let Some(n) = self.draft_n_max {
                    s.push_str(&format!(" (draft n-max {n})"));
                }
                parts.push(s);
            }
            SpecKind::Combined => {
                let mut inner: Vec<String> = Vec::new();
                if let Some(n) = self.ngram_n_min {
                    inner.push(format!("n-min {n}"));
                }
                if let Some(n) = self.draft_n_max {
                    inner.push(format!("draft n-max {n}"));
                }
                if inner.is_empty() {
                    parts.push("combined".to_string());
                } else {
                    parts.push(format!("combined ({})", inner.join(", ")));
                }
            }
        }
        if let Some(expr) = &self.offload_expr {
            parts.push(format!("offload {expr}"));
        }
        parts.join(" · ")
    }

    /// Normalize spec-dependent fields in place: fields belonging to
    /// drafters the arm does not run are cleared to their defaults,
    /// and an n-gram arm without an explicit `ngram_n_min` takes the
    /// pin's default of 48 (kit §1.2).
    pub fn normalize(&mut self) {
        match self.spec {
            SpecKind::None => {
                self.ngram_n_min = None;
                self.clear_draft_fields();
            }
            SpecKind::NgramMod => {
                if self.ngram_n_min.is_none() {
                    self.ngram_n_min = Some(48);
                }
                self.clear_draft_fields();
            }
            SpecKind::DraftSimple => {
                self.ngram_n_min = None;
            }
            SpecKind::Combined => {
                if self.ngram_n_min.is_none() {
                    self.ngram_n_min = Some(48);
                }
            }
        }
    }

    fn clear_draft_fields(&mut self) {
        self.draft_model_id = None;
        self.draft_n_max = None;
        self.draft_device = DraftDevice::TargetGpu;
        self.draft_cache_k = CacheType::F16;
        self.draft_cache_v = CacheType::F16;
    }
}

/// One swept value inside a sweep dimension: the same fields as
/// [`ArmSpec`], each optional — `None` inherits the base arm's value.
/// Fields that are already optional in `ArmSpec` keep their single
/// `Option` here (their `None` also means inherit; [`expand`]'s
/// normalization is what clears them when the spec kind demands it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ArmVariant {
    pub cache_k: Option<CacheType>,
    pub cache_v: Option<CacheType>,
    pub flash_attn: Option<FlashAttn>,
    pub depth: Option<u32>,
    pub n_ctx: Option<u32>,
    pub spec: Option<SpecKind>,
    pub ngram_n_min: Option<u32>,
    pub draft_model_id: Option<String>,
    pub draft_n_max: Option<u32>,
    pub draft_device: Option<DraftDevice>,
    pub draft_cache_k: Option<CacheType>,
    pub draft_cache_v: Option<CacheType>,
    pub offload_expr: Option<String>,
}

impl ArmVariant {
    /// Merge this variant over `base`. No normalization here:
    /// normalization runs once, on the fully merged arm in
    /// [`expand`] — normalizing an intermediate step would clear
    /// fields (e.g. a base arm's draft model selection, held while
    /// its spec is still `none`) that a later dimension's variant
    /// legitimately inherits when it turns the spec on.
    fn apply(&self, base: &ArmSpec) -> ArmSpec {
        let mut out = base.clone();
        if let Some(v) = self.cache_k {
            out.cache_k = v;
        }
        if let Some(v) = self.cache_v {
            out.cache_v = v;
        }
        if let Some(v) = self.flash_attn {
            out.flash_attn = v;
        }
        if let Some(v) = self.depth {
            out.depth = v;
        }
        if let Some(v) = self.n_ctx {
            out.n_ctx = v;
        }
        if let Some(v) = self.spec {
            out.spec = v;
        }
        if let Some(v) = self.ngram_n_min {
            out.ngram_n_min = Some(v);
        }
        if let Some(v) = &self.draft_model_id {
            out.draft_model_id = Some(v.clone());
        }
        if let Some(v) = self.draft_n_max {
            out.draft_n_max = Some(v);
        }
        if let Some(v) = self.draft_device {
            out.draft_device = v;
        }
        if let Some(v) = self.draft_cache_k {
            out.draft_cache_k = v;
        }
        if let Some(v) = self.draft_cache_v {
            out.draft_cache_v = v;
        }
        if let Some(v) = &self.offload_expr {
            out.offload_expr = Some(v.clone());
        }
        out
    }
}

/// Expand a sweep definition into its arms: the cartesian product of
/// `dimensions` (one variant list per swept axis), each combination
/// merged over `base` and normalized. Identical arms are deduped,
/// keeping first occurrence order. No dimensions yields the single
/// (normalized) base arm; an empty dimension yields no arms.
pub fn expand(base: &ArmSpec, dimensions: &[Vec<ArmVariant>]) -> Vec<ArmSpec> {
    let mut arms: Vec<ArmSpec> = vec![base.clone()];
    for dimension in dimensions {
        let mut next: Vec<ArmSpec> = Vec::new();
        for arm in &arms {
            for variant in dimension {
                next.push(variant.apply(arm));
            }
        }
        arms = next;
        if arms.is_empty() {
            break;
        }
    }
    let mut deduped: Vec<ArmSpec> = Vec::new();
    for mut arm in arms {
        arm.normalize();
        if !deduped.contains(&arm) {
            deduped.push(arm);
        }
    }
    deduped
}

/// Facts known before launch that pruning can judge an arm against
/// (design §3.2). `None` = unknown, and an unknown fact never prunes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PruneContext {
    /// The model's GGUF context length, when known.
    pub model_ctx_len: Option<u32>,
    /// The target model's vocabulary size, when known.
    pub target_vocab: Option<u32>,
    /// The selected draft model's vocabulary size, when known.
    pub draft_vocab: Option<u32>,
}

/// The outcome of pruning one arm: ready to run, or pruned with the
/// reason shown to the operator. Pruned arms are never coerced and
/// never run (D11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArmOutcome {
    Ready(ArmSpec),
    Pruned { spec: ArmSpec, reason: String },
}

/// Apply design §3.2's pruning rules to `arms`, in input order. The
/// first rule that matches an arm supplies its reason.
pub fn prune_arms(arms: Vec<ArmSpec>, ctx: &PruneContext) -> Vec<ArmOutcome> {
    arms.into_iter()
        .map(|arm| {
            // (1) Quantized V cache with flash attention explicitly
            // off fails context creation in the pin
            // (llama-context.cpp:4063-4072): quantized V requires
            // flash attention; `auto` force-enables it, `off` fails.
            if arm.cache_v.quantized() && arm.flash_attn == FlashAttn::Off {
                return ArmOutcome::Pruned {
                    reason: format!(
                        "quantized V cache ({}) requires flash attention, but flash_attn is off: \
                         llama.cpp fails context creation for a quantized V cache with flash \
                         attention disabled (auto would force-enable it)",
                        arm.cache_v.flag()
                    ),
                    spec: arm,
                };
            }
            // (2) Context size beyond the model's GGUF context length.
            if let Some(model_ctx_len) = ctx.model_ctx_len {
                if arm.n_ctx > model_ctx_len {
                    return ArmOutcome::Pruned {
                        reason: format!(
                            "n_ctx {} exceeds the model's context length {}",
                            arm.n_ctx, model_ctx_len
                        ),
                        spec: arm,
                    };
                }
            }
            // (3) A draft-bearing spec with no draft model selected.
            if arm.spec.has_draft() && arm.draft_model_id.is_none() {
                return ArmOutcome::Pruned {
                    reason: "no draft model selected".to_string(),
                    spec: arm,
                };
            }
            // (4) Draft vocabulary incompatible with the target's
            // (the pin's vocabulary gate shape, kit §1.3): known and
            // unequal sizes prune before launch.
            if arm.spec.has_draft() {
                if let (Some(target), Some(draft)) = (ctx.target_vocab, ctx.draft_vocab) {
                    if target != draft {
                        return ArmOutcome::Pruned {
                            reason: format!(
                                "draft model vocabulary size {draft} does not match target \
                                 vocabulary size {target}"
                            ),
                            spec: arm,
                        };
                    }
                }
            }
            ArmOutcome::Ready(arm)
        })
        .collect()
}

/// A shipped sweep preset (design §3.5, D16): the dimensions to
/// expand over the operator's base arm, and the prompt the sweep is
/// pinned to when the preset chooses one (`None` = the sweep prompt
/// stays operator-chosen).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PresetSweep {
    pub dimensions: Vec<Vec<ArmVariant>>,
    pub prompt_id: Option<String>,
}

/// KV study preset: the four decision-relevant KV pairs at depth
/// 8,192 — the matched quantization ladder plus (f16 K, q8_0 V),
/// which isolates the V direction.
pub fn preset_kv() -> PresetSweep {
    let variant = |k: CacheType, v: CacheType| ArmVariant {
        cache_k: Some(k),
        cache_v: Some(v),
        depth: Some(8192),
        ..ArmVariant::default()
    };
    PresetSweep {
        dimensions: vec![vec![
            variant(CacheType::F16, CacheType::F16),
            variant(CacheType::Q8_0, CacheType::Q8_0),
            variant(CacheType::F16, CacheType::Q8_0),
            variant(CacheType::Q4_0, CacheType::Q4_0),
        ]],
        prompt_id: None,
    }
}

/// Spec study preset: the four spec types, n-min fixed at 16 and
/// draft n-max fixed at 3 (the ladders are custom sweeps once the
/// study says drafting is worth tuning).
pub fn preset_spec() -> PresetSweep {
    PresetSweep {
        dimensions: vec![vec![
            ArmVariant {
                spec: Some(SpecKind::None),
                ..ArmVariant::default()
            },
            ArmVariant {
                spec: Some(SpecKind::NgramMod),
                ngram_n_min: Some(16),
                ..ArmVariant::default()
            },
            ArmVariant {
                spec: Some(SpecKind::DraftSimple),
                draft_n_max: Some(3),
                ..ArmVariant::default()
            },
            ArmVariant {
                spec: Some(SpecKind::Combined),
                ngram_n_min: Some(16),
                draft_n_max: Some(3),
                ..ArmVariant::default()
            },
        ]],
        prompt_id: None,
    }
}

/// Combined study preset: spec {none, combined} × depth {8192,
/// 30720}. The prompt stays operator-chosen (`prompt_id: None`); the
/// 30,720 arms pad from the shipped library at run time.
pub fn preset_combined() -> PresetSweep {
    PresetSweep {
        dimensions: vec![
            vec![
                ArmVariant {
                    spec: Some(SpecKind::None),
                    ..ArmVariant::default()
                },
                ArmVariant {
                    spec: Some(SpecKind::Combined),
                    ngram_n_min: Some(16),
                    draft_n_max: Some(3),
                    ..ArmVariant::default()
                },
            ],
            vec![
                ArmVariant {
                    depth: Some(8192),
                    ..ArmVariant::default()
                },
                ArmVariant {
                    depth: Some(30720),
                    ..ArmVariant::default()
                },
            ],
        ],
        prompt_id: None,
    }
}

// Projection estimate constants (design §2.1 cost shape, halved by
// Amendment 2; [est] throughout — reasoned, unmeasured). These price
// the pre-run projection only: a projection warns against the sweep
// target, it never refuses and nothing stops a run at the target
// (D14, Pete's framing).
const FIRST_ARM_START_MIN: f64 = 0.75;
const LATER_ARM_START_MIN: f64 = 0.375;
const PADDING_DEEP_MIN: f64 = 2.0;
const PADDING_STANDARD_MIN: f64 = 0.5;
const PADDING_SMALL_MIN: f64 = 0.25;
const WARMUP_MIN: f64 = 0.15;
const MEASURED_RUN_MIN: f64 = 0.375;

/// Project a sweep's wall-clock minutes from its arms, using the
/// §2.1 halved cost shape: the first arm pays a cold backend start
/// (0.75 min), later arms a warm one (0.375); per arm a padding term
/// by depth (≥30,000 → 2.0; ≥8,000 → 0.5; else 0.25), a warmup
/// (0.15), and 3 measured runs × 0.375. Teardown is overlapped with
/// the next arm's start and priced at 0.
pub fn project_minutes(arms: &[ArmSpec]) -> f64 {
    let mut total = 0.0;
    for (i, arm) in arms.iter().enumerate() {
        total += if i == 0 {
            FIRST_ARM_START_MIN
        } else {
            LATER_ARM_START_MIN
        };
        total += if arm.depth >= 30_000 {
            PADDING_DEEP_MIN
        } else if arm.depth >= 8_000 {
            PADDING_STANDARD_MIN
        } else {
            PADDING_SMALL_MIN
        };
        total += WARMUP_MIN;
        total += MEASURED_RUN_MIN * crate::MEASURED_RUNS as f64;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ArmSpec {
        ArmSpec {
            cache_k: CacheType::Q8_0,
            cache_v: CacheType::Q8_0,
            depth: 8192,
            n_ctx: 65536,
            draft_model_id: Some("draft-1".to_string()),
            draft_cache_k: CacheType::Q8_0,
            draft_cache_v: CacheType::Q8_0,
            ..ArmSpec::default()
        }
    }

    #[test]
    fn expand_cartesian_product_merges_variants_over_base() {
        let dims = vec![
            vec![
                ArmVariant {
                    cache_k: Some(CacheType::F16),
                    cache_v: Some(CacheType::F16),
                    ..ArmVariant::default()
                },
                ArmVariant {
                    cache_k: Some(CacheType::Q4_0),
                    cache_v: Some(CacheType::Q4_0),
                    ..ArmVariant::default()
                },
            ],
            vec![
                ArmVariant {
                    depth: Some(8192),
                    ..ArmVariant::default()
                },
                ArmVariant {
                    depth: Some(30720),
                    ..ArmVariant::default()
                },
            ],
        ];
        let arms = expand(&base(), &dims);
        assert_eq!(arms.len(), 4);
        assert!(arms.iter().all(|a| a.n_ctx == 65536));
        assert!(arms
            .iter()
            .any(|a| a.cache_k == CacheType::F16 && a.depth == 30720));
        assert!(arms
            .iter()
            .any(|a| a.cache_k == CacheType::Q4_0 && a.depth == 8192));
    }

    #[test]
    fn expand_normalizes_spec_fields() {
        let dims = vec![vec![
            ArmVariant {
                spec: Some(SpecKind::None),
                ..ArmVariant::default()
            },
            ArmVariant {
                spec: Some(SpecKind::NgramMod),
                ..ArmVariant::default()
            },
            ArmVariant {
                spec: Some(SpecKind::DraftSimple),
                draft_n_max: Some(3),
                ..ArmVariant::default()
            },
        ]];
        let arms = expand(&base(), &dims);
        assert_eq!(arms.len(), 3);
        let none = &arms[0];
        assert_eq!(none.ngram_n_min, None);
        assert_eq!(none.draft_model_id, None);
        assert_eq!(none.draft_n_max, None);
        let ngram = &arms[1];
        // Default n-min 48 applied; every draft field cleared.
        assert_eq!(ngram.ngram_n_min, Some(48));
        assert_eq!(ngram.draft_model_id, None);
        assert_eq!(ngram.draft_n_max, None);
        assert_eq!(ngram.draft_cache_k, CacheType::F16);
        let draft = &arms[2];
        assert_eq!(draft.ngram_n_min, None);
        assert_eq!(draft.draft_model_id.as_deref(), Some("draft-1"));
        assert_eq!(draft.draft_n_max, Some(3));
    }

    #[test]
    fn expand_dedupes_identical_arms() {
        let variant = ArmVariant {
            depth: Some(4096),
            ..ArmVariant::default()
        };
        let dims = vec![vec![variant.clone(), variant]];
        let arms = expand(&base(), &dims);
        assert_eq!(arms.len(), 1);
        // No dimensions at all yields the single normalized base arm.
        assert_eq!(expand(&base(), &[]).len(), 1);
    }

    #[test]
    fn prune_quantized_v_with_flash_off() {
        let mut arm = base();
        arm.cache_v = CacheType::Q8_0;
        arm.flash_attn = FlashAttn::Off;
        let out = prune_arms(vec![arm], &PruneContext::default());
        match &out[0] {
            ArmOutcome::Pruned { reason, .. } => {
                assert!(reason.contains("quantized V"), "{reason}");
                assert!(reason.contains("flash attention"), "{reason}");
            }
            ArmOutcome::Ready(_) => panic!("expected pruning"),
        }
        // FA auto force-enables in the pin, so it is not pruned.
        let mut arm = base();
        arm.cache_v = CacheType::Q4_0;
        arm.flash_attn = FlashAttn::Auto;
        assert!(matches!(
            prune_arms(vec![arm], &PruneContext::default())[0],
            ArmOutcome::Ready(_)
        ));
        // f16 V with FA off is a legal configuration.
        let mut arm = base();
        arm.cache_v = CacheType::F16;
        arm.flash_attn = FlashAttn::Off;
        assert!(matches!(
            prune_arms(vec![arm], &PruneContext::default())[0],
            ArmOutcome::Ready(_)
        ));
    }

    #[test]
    fn prune_n_ctx_beyond_model_context() {
        let ctx = PruneContext {
            model_ctx_len: Some(32768),
            ..PruneContext::default()
        };
        let out = prune_arms(vec![base()], &ctx);
        match &out[0] {
            ArmOutcome::Pruned { reason, .. } => {
                assert!(reason.contains("65536"), "{reason}");
                assert!(reason.contains("32768"), "{reason}");
            }
            ArmOutcome::Ready(_) => panic!("expected pruning"),
        }
        // Unknown model length never prunes; equal length is allowed.
        assert!(matches!(
            prune_arms(vec![base()], &PruneContext::default())[0],
            ArmOutcome::Ready(_)
        ));
        let ctx = PruneContext {
            model_ctx_len: Some(65536),
            ..PruneContext::default()
        };
        assert!(matches!(
            prune_arms(vec![base()], &ctx)[0],
            ArmOutcome::Ready(_)
        ));
    }

    #[test]
    fn prune_draft_spec_without_draft_model() {
        let mut arm = base();
        arm.spec = SpecKind::DraftSimple;
        arm.draft_model_id = None;
        let out = prune_arms(vec![arm.clone()], &PruneContext::default());
        match &out[0] {
            ArmOutcome::Pruned { reason, .. } => {
                assert_eq!(reason, "no draft model selected")
            }
            ArmOutcome::Ready(_) => panic!("expected pruning"),
        }
        // A non-draft spec needs no draft model.
        let mut ngram = base();
        ngram.spec = SpecKind::NgramMod;
        ngram.draft_model_id = None;
        assert!(matches!(
            prune_arms(vec![ngram], &PruneContext::default())[0],
            ArmOutcome::Ready(_)
        ));
        // With a draft model selected the arm is ready.
        arm.draft_model_id = Some("draft-1".to_string());
        assert!(matches!(
            prune_arms(vec![arm], &PruneContext::default())[0],
            ArmOutcome::Ready(_)
        ));
    }

    #[test]
    fn prune_vocab_mismatch_only_when_both_known() {
        let mut arm = base();
        arm.spec = SpecKind::Combined;
        let ctx = PruneContext {
            target_vocab: Some(151936),
            draft_vocab: Some(151936),
            ..PruneContext::default()
        };
        assert!(matches!(
            prune_arms(vec![arm.clone()], &ctx)[0],
            ArmOutcome::Ready(_)
        ));
        let ctx = PruneContext {
            target_vocab: Some(151936),
            draft_vocab: Some(32000),
            ..PruneContext::default()
        };
        match &prune_arms(vec![arm.clone()], &ctx)[0] {
            ArmOutcome::Pruned { reason, .. } => {
                assert!(reason.contains("151936"), "{reason}");
                assert!(reason.contains("32000"), "{reason}");
            }
            ArmOutcome::Ready(_) => panic!("expected pruning"),
        }
        // One side unknown: no pruning on this rule.
        let ctx = PruneContext {
            target_vocab: Some(151936),
            draft_vocab: None,
            ..PruneContext::default()
        };
        assert!(matches!(
            prune_arms(vec![arm], &ctx)[0],
            ArmOutcome::Ready(_)
        ));
    }

    #[test]
    fn presets_expand_to_exactly_four_arms() {
        for preset in [preset_kv(), preset_spec(), preset_combined()] {
            let arms = expand(&base(), &preset.dimensions);
            assert_eq!(arms.len(), 4, "preset {preset:?}");
        }
        let kv = preset_kv();
        let arms = expand(&base(), &kv.dimensions);
        assert!(arms.iter().all(|a| a.depth == 8192));
        assert!(arms.iter().any(|a| a.cache_k == CacheType::F16
            && a.cache_v == CacheType::Q8_0));
        assert_eq!(preset_combined().prompt_id, None);
    }

    #[test]
    fn label_names_non_default_axes() {
        let arm = ArmSpec {
            cache_k: CacheType::Q8_0,
            cache_v: CacheType::Q8_0,
            flash_attn: FlashAttn::On,
            depth: 30720,
            spec: SpecKind::Combined,
            ngram_n_min: Some(16),
            draft_n_max: Some(3),
            ..ArmSpec::default()
        };
        assert_eq!(
            arm.label(),
            "q8_0/q8_0 · FA on · depth 30,720 · combined (n-min 16, draft n-max 3)"
        );
        assert_eq!(ArmSpec::default().label(), "f16/f16");
    }

    #[test]
    fn projection_uses_halved_cost_shape() {
        assert_eq!(project_minutes(&[]), 0.0);
        let standard = ArmSpec {
            depth: 8192,
            ..ArmSpec::default()
        };
        // First arm: 0.75 start + 0.5 padding + 0.15 warmup + 3*0.375.
        assert!((project_minutes(std::slice::from_ref(&standard)) - 2.525).abs() < 1e-9);
        // Later arms pay the warm start (0.375).
        let two = project_minutes(&[standard.clone(), standard]);
        assert!((two - (2.525 + 2.15)).abs() < 1e-9);
        let deep = ArmSpec {
            depth: 30720,
            ..ArmSpec::default()
        };
        assert!((project_minutes(&[deep]) - (0.75 + 2.0 + 0.15 + 1.125)).abs() < 1e-9);
    }

    #[test]
    fn enum_serialization_matches_contract() {
        assert_eq!(
            serde_json::to_string(&CacheType::Q8_0).expect("ser"),
            "\"q8_0\""
        );
        assert_eq!(
            serde_json::to_string(&SpecKind::Combined).expect("ser"),
            "\"combined\""
        );
        assert_eq!(
            serde_json::to_string(&DraftDevice::TargetGpu).expect("ser"),
            "\"target_gpu\""
        );
        assert_eq!(SpecKind::Combined.spec_type_arg(), Some("ngram-mod,draft-simple"));
        assert_eq!(SpecKind::None.spec_type_arg(), None);
        assert!(CacheType::Q4_0.quantized());
        assert!(!CacheType::F16.quantized());
    }
}
