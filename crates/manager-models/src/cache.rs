//! KV cache types and flash attention in a backend's raw
//! llama-server flags: the `--cache-type-k` / `--cache-type-v`
//! element types the KV cache is stored in, and the
//! `--flash-attn` mode.
//!
//! The decode/encode pair mirrors the MTP pair in [`super::mtp`]
//! and the expert-offload pair in [`super::memory`]: state is
//! decoded from the saved raw flags — there is no separate stored
//! setting — and the writer rewrites only the tokens the setting
//! owns, leaving every other flag untouched and in order.
//! Malformed or hand-edited spellings degrade to a safe decoded
//! state, never a panic: an unrecognized cache type reads as the
//! llama.cpp default (f16, the value an absent token runs), and a
//! valueless flag the writer does not own is preserved rather
//! than deleted.
//!
//! The value sets are the pinned llama.cpp build's (b11658,
//! `common/arg.cpp`): `--cache-type-k/v` accept f32, f16, bf16,
//! q8_0, q4_0, q4_1, iq4_nl, q5_0, q5_1; this app offers the three
//! types v1 covers (f16, q8_0, q4_0) — [`CacheType`] is the clean
//! extension point for the rest. `--flash-attn` is tri-state on
//! the pinned build (`-fa, --flash-attn [on|off|auto]`, default
//! auto; `include/llama.h`'s `llama_flash_attn_type`), so the
//! control offers all three states.
//!
//! Two pinned-build rules shape what a valid selection is:
//! - A quantized V cache requires flash attention to be enabled
//!   (`src/llama-context.cpp`): with `--flash-attn auto` the
//!   server promotes it to enabled, with `off` context creation
//!   fails outright. [`CacheSelection::v_quantized_fa_off`] names
//!   the invalid combination so the save path can refuse it
//!   instead of persisting a backend that cannot start.
//! - Flash attention itself is supported on every architecture
//!   the pinned CUDA build targets, Pascal (sm_60) included: the
//!   CUDA backend's `ggml_cuda_flash_attn_ext_supported` falls
//!   back to a generic tile kernel when no tensor-core (Volta+)
//!   MMA kernel applies (`ggml/src/ggml-cuda/fattn.cu`), and
//!   q8_0/q4_0 are in its supported KV type set. There is
//!   therefore no compute-capability gate on the control — an
//!   earlier expectation that Pascal would need one does not
//!   hold against the pinned source.

use serde::{Deserialize, Serialize};

use super::gguf::KvGeometry;
use super::MIB;

/// One KV cache element type the app offers.
///
/// The byte geometry is ggml's block geometry for the type
/// (pinned llama.cpp `ggml/src/ggml-common.h`, mirrored by
/// [`super::gguf`]'s tensor type table): a block of
/// `block_elems` elements occupies `block_bytes` bytes, so an
/// element costs `block_bytes / block_elems` bytes — f16: 2/1,
/// q8_0: 34/32 (an fp16 scale plus 32 int8 quants), q4_0: 18/32
/// (an fp16 scale plus 32 nibbles). Pricing divides per cache
/// row (one head's vector for one token), rounding a partial
/// block up, so a head dimension that is not block-aligned is
/// overpriced by less than one block per row, never underpriced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheType {
    F16,
    Q8_0,
    Q4_0,
}

impl CacheType {
    /// Every type the control offers, in quality order.
    pub const ALL: [CacheType; 3] = [CacheType::F16, CacheType::Q8_0, CacheType::Q4_0];

    /// The llama.cpp spelling (`--cache-type-k q8_0`).
    pub fn as_str(&self) -> &'static str {
        match self {
            CacheType::F16 => "f16",
            CacheType::Q8_0 => "q8_0",
            CacheType::Q4_0 => "q4_0",
        }
    }

    /// Parse a llama.cpp spelling. Any other value — including
    /// types the pinned build accepts but v1 does not offer
    /// (q5_0, f32, ...) — reads as `None`: the decoder treats it
    /// as unset rather than inventing a position the control
    /// cannot render.
    pub fn parse(s: &str) -> Option<CacheType> {
        match s {
            "f16" => Some(CacheType::F16),
            "q8_0" => Some(CacheType::Q8_0),
            "q4_0" => Some(CacheType::Q4_0),
            _ => None,
        }
    }

    /// ggml block geometry: (elements per block, bytes per block).
    pub fn block_geometry(&self) -> (u64, u64) {
        match self {
            CacheType::F16 => (1, 2),
            CacheType::Q8_0 => (32, 34),
            CacheType::Q4_0 => (32, 18),
        }
    }

    /// Whether llama.cpp treats the type as quantized — the
    /// property the quantized-V rule keys on (f16 is not).
    pub fn is_quantized(&self) -> bool {
        !matches!(self, CacheType::F16)
    }

    /// Bytes for one cache row of `elems` elements at this type.
    pub fn row_bytes(&self, elems: u64) -> u64 {
        let (block_elems, block_bytes) = self.block_geometry();
        elems
            .div_ceil(block_elems)
            .saturating_mul(block_bytes)
    }
}

/// Flash attention mode in a backend's raw flags — the pinned
/// build's tri-state (`--flash-attn auto|on|off`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlashAttn {
    Auto,
    On,
    Off,
}

impl FlashAttn {
    /// The llama.cpp spelling (`--flash-attn auto`).
    pub fn as_str(&self) -> &'static str {
        match self {
            FlashAttn::Auto => "auto",
            FlashAttn::On => "on",
            FlashAttn::Off => "off",
        }
    }

    /// Parse a llama.cpp spelling. The pinned build also accepts
    /// truthy/falsey spellings (1/true/yes, 0/false/no) through
    /// `is_truthy`/`is_falsey`; the decoder accepts those too, so
    /// a hand-written `--flash-attn 1` reads as the state it runs.
    pub fn parse(s: &str) -> Option<FlashAttn> {
        match s {
            "auto" => Some(FlashAttn::Auto),
            "on" | "1" | "true" | "yes" => Some(FlashAttn::On),
            "off" | "0" | "false" | "no" => Some(FlashAttn::Off),
            _ => None,
        }
    }
}

/// A backend's cache configuration: the K and V cache types and
/// the flash-attention mode, as DECODED from its raw flags. An
/// absent token decodes to the llama.cpp default for that
/// setting (f16 / f16 / auto) — the state the server runs — so
/// the decoded selection is always the effective one. Whether
/// the tokens are present at all (the state a pre-feature backend
/// is in, whose spawned command line must not change) is
/// [`CacheSelection::is_default`]-independent: the writer decides
/// presence, see [`set_cache_selection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheSelection {
    pub k: CacheType,
    pub v: CacheType,
    pub flash_attn: FlashAttn,
}

impl Default for CacheSelection {
    /// llama.cpp's defaults: f16 K and V caches, flash attention
    /// auto (the pinned build's `common_params` default).
    fn default() -> Self {
        CacheSelection {
            k: CacheType::F16,
            v: CacheType::F16,
            flash_attn: FlashAttn::Auto,
        }
    }
}

impl CacheSelection {
    /// The combination llama.cpp refuses at context creation: a
    /// quantized V cache with flash attention explicitly off
    /// (auto would be promoted to enabled; off is a hard error —
    /// see the module docs). The save path refuses this pairing
    /// rather than persisting a backend that cannot start.
    pub fn v_quantized_fa_off(&self) -> bool {
        self.v.is_quantized() && self.flash_attn == FlashAttn::Off
    }
}

/// The value token of a `--flag value` pair, for flags whose
/// value is a word (never begins with `-`): the next token when
/// it exists and is not itself a flag. Mirrors [`super::mtp`]'s
/// helper of the same name.
fn pair_value<'a>(tokens: &[&'a str], i: usize) -> Option<&'a str> {
    tokens.get(i + 1).copied().filter(|v| !v.starts_with('-'))
}

/// The last value written for one of the cache flags, across the
/// pair and `=` spellings (llama.cpp's sequential-parse rule: the
/// last occurrence wins). `None` when the flag never appears with
/// a value.
fn last_flag_value<'a>(tokens: &[&'a str], flag: &str, short: &str) -> Option<&'a str> {
    let mut value: Option<&str> = None;
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if t == flag || t == short {
            if let Some(v) = pair_value(tokens, i) {
                value = Some(v);
                i += 1;
            }
        } else if let Some(v) = t
            .strip_prefix(&format!("{flag}="))
            .or_else(|| t.strip_prefix(&format!("{short}=")))
        {
            value = Some(v);
        }
        i += 1;
    }
    value
}

/// Detect a backend's cache selection in raw llama-server flags.
///
/// Each setting resolves independently: the last parseable value
/// wins; an absent or unparseable token (a type v1 does not
/// offer, a malformed spelling) reads as the llama.cpp default
/// for that setting, so decoding never invents a setting the
/// flags do not effectively state.
pub fn cache_selection(raw_flags: Option<&str>) -> CacheSelection {
    let Some(raw) = raw_flags else {
        return CacheSelection::default();
    };
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let mut sel = CacheSelection::default();
    if let Some(t) = last_flag_value(&tokens, "--cache-type-k", "-ctk")
        .and_then(CacheType::parse)
    {
        sel.k = t;
    }
    if let Some(t) = last_flag_value(&tokens, "--cache-type-v", "-ctv")
        .and_then(CacheType::parse)
    {
        sel.v = t;
    }
    if let Some(f) = last_flag_value(&tokens, "--flash-attn", "-fa")
        .and_then(FlashAttn::parse)
    {
        sel.flash_attn = f;
    }
    sel
}

/// Whether any of the cache tokens is present in raw flags —
/// the distinction between a pre-feature backend (no tokens; its
/// spawned command line must stay exactly as it is until the
/// operator touches these controls) and one the controls have
/// written. Presence is about tokens, not values: a hand-written
/// `--cache-type-k f16` is present.
pub fn cache_tokens_present(raw_flags: Option<&str>) -> bool {
    let Some(raw) = raw_flags else {
        return false;
    };
    raw.split_whitespace().any(|t| {
        let name = t.split('=').next().unwrap_or(t);
        matches!(
            name,
            "--cache-type-k" | "-ctk" | "--cache-type-v" | "-ctv" | "--flash-attn" | "-fa"
        )
    })
}

/// Rewrite the cache tokens in raw llama-server flags, leaving
/// every other flag untouched and in order. All existing
/// `--cache-type-k` / `-ctk`, `--cache-type-v` / `-ctv`, and
/// `--flash-attn` / `-fa` occurrences (pair and `=` spellings)
/// are removed, then the canonical tokens are appended.
///
/// `None` removes the tokens without appending, returning the
/// backend to having no cache tokens at all (llama.cpp's
/// defaults, and the pre-feature state). A `Some` selection
/// always writes all three tokens explicitly — the state the app
/// wrote is the state the flags state, with no implicit member.
/// Returns `None` when nothing remains.
pub fn set_cache_selection(
    raw_flags: Option<&str>,
    setting: Option<CacheSelection>,
) -> Option<String> {
    let tokens: Vec<&str> = raw_flags.unwrap_or("").split_whitespace().collect();
    let owned = |t: &str| -> bool {
        let name = t.split('=').next().unwrap_or(t);
        matches!(
            name,
            "--cache-type-k" | "-ctk" | "--cache-type-v" | "-ctv" | "--flash-attn" | "-fa"
        )
    };
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if owned(t) {
            // Skip the value token too when the flag is in pair
            // spelling and a value follows that is not itself a
            // flag; a malformed value token is left for the main
            // walk (mirrors set_parallel_slots).
            let has_value = !t.contains('=')
                && tokens
                    .get(i + 1)
                    .map(|v| !v.starts_with('-'))
                    .unwrap_or(false);
            i += if has_value { 2 } else { 1 };
            continue;
        }
        out.push(t.to_string());
        i += 1;
    }
    if let Some(sel) = setting {
        out.push("--cache-type-k".to_string());
        out.push(sel.k.as_str().to_string());
        out.push("--cache-type-v".to_string());
        out.push(sel.v.as_str().to_string());
        out.push("--flash-attn".to_string());
        out.push(sel.flash_attn.as_str().to_string());
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join(" "))
    }
}

// ---------------------------------------------------------------------------
// Predicted-best selection
// ---------------------------------------------------------------------------

/// The headroom reserve in the predicted-best fit test: a rung
/// fits only when its predicted per-GPU total is at most
/// `100 - HEADROOM_RESERVE_PERCENT` percent of that GPU's total
/// VRAM. The requirement model prices weights, KV, and the
/// recurrent-state cache; llama.cpp additionally allocates
/// compute buffers and graph scratch the model does not price,
/// and Pete's Oct 20, 2026 measurements put a 64K f16 planner
/// configuration whose priced total was ~96.5% of the card in
/// the spill regime (13.2 tok/s vs 29.5 at 32K) — a 3% reserve
/// would call that configuration fitting. 5% is the smallest
/// round reserve that prices it as not fitting.
pub const HEADROOM_RESERVE_PERCENT: u64 = 5;

/// One rung of the predicted-best quality ladder: a cache
/// selection plus its predicted footprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRung {
    pub selection: CacheSelection,
    /// KV cache MiB at this rung's cache types.
    pub kv_mib: u64,
    /// Total predicted GPU-resident MiB (weights + KV +
    /// recurrent-state cache when its geometry is known).
    pub total_mib: u64,
    /// Whether the rung fits every target GPU under the headroom
    /// reserve (see [`HEADROOM_RESERVE_PERCENT`]).
    pub fits: bool,
}

/// The inputs the predicted-best chooser prices, resolved by the
/// caller from the backend's layout and placement: everything is
/// a number the existing requirement model already computes, so
/// the chooser is a pure function of them.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheFitInputs {
    /// KV geometry of the backend's model.
    pub kv: KvGeometry,
    /// GPU-resident weight MiB under the backend's current
    /// offload configuration (`MemoryRequirement::gpu_weights_mib`).
    pub gpu_weights_mib: u64,
    /// Recurrent-state cache MiB at the effective slot count,
    /// when the model's geometry is known.
    pub rs_mib: Option<u64>,
    /// MoE expert cache MiB (`--moe-cache-mib`) the backend
    /// reserves: a constant addend to every rung's total (it
    /// does not vary with the cache selection being priced).
    /// 0 for backends that set no expert cache.
    pub moe_cache_mib: u64,
    /// The context size the KV cache is priced at, in tokens.
    /// This is the per-slot context the requirement model prices
    /// (llama.cpp's unified KV cache holds the full context once,
    /// shared across slots, at the pinned build's default).
    pub n_ctx: u32,
    /// The tensor split distributing the total over the target
    /// GPUs, in target order; empty for an unsplit placement (the
    /// whole total on the first target).
    pub split: Vec<f32>,
    /// Total VRAM MiB of each target GPU, in split order.
    pub gpu_total_mib: Vec<u64>,
}

/// KV bytes for one token at a selection: the K side priced with
/// the K geometry and type, the V side with the V geometry and
/// type (see [`KvGeometry::bytes_per_token_typed`]).
pub fn kv_bytes_per_token(kv: &KvGeometry, sel: &CacheSelection) -> u64 {
    kv.bytes_per_token_typed(sel.k, sel.v)
}

/// The quality ladder, highest first (Pete, Oct 20, 2026:
/// predicted high-quality options with a balance of speed —
/// quality decides, and flash attention stays on at every rung:
/// on supported builds it is a pure speed/memory-traffic win at
/// these contexts, and a quantized V cache requires it). V
/// degrades before K: the mixed rung quantizes V to q4_0 while K
/// stays q8_0.
pub const PREDICTED_BEST_LADDER: [CacheSelection; 4] = [
    CacheSelection {
        k: CacheType::F16,
        v: CacheType::F16,
        flash_attn: FlashAttn::On,
    },
    CacheSelection {
        k: CacheType::Q8_0,
        v: CacheType::Q8_0,
        flash_attn: FlashAttn::On,
    },
    CacheSelection {
        k: CacheType::Q8_0,
        v: CacheType::Q4_0,
        flash_attn: FlashAttn::On,
    },
    CacheSelection {
        k: CacheType::Q4_0,
        v: CacheType::Q4_0,
        flash_attn: FlashAttn::On,
    },
];

/// The per-GPU shares of a predicted total, distributed over
/// the target GPUs by the split (an empty split places
/// everything on the first target; the last target takes the
/// rounding remainder). `None` when there is nothing to
/// distribute over (no targets) or the split cannot distribute
/// (its length differs from the target count — the placement
/// projection's stale-split rule). This is the distribution
/// [`predicted_fits`] judges; the cache preview payload carries
/// the same shares as each target's required figure.
pub fn predicted_shares(
    total_mib: u64,
    split: &[f32],
    gpu_total_mib: &[u64],
) -> Option<Vec<u64>> {
    if gpu_total_mib.is_empty() {
        return None;
    }
    if split.is_empty() {
        let mut v = vec![0u64; gpu_total_mib.len()];
        v[0] = total_mib;
        return Some(v);
    }
    if split.len() != gpu_total_mib.len() {
        return None;
    }
    let mut assigned = 0u64;
    Some(
        split
            .iter()
            .enumerate()
            .map(|(i, frac)| {
                let share = if i + 1 == split.len() {
                    total_mib.saturating_sub(assigned)
                } else {
                    (total_mib as f64 * (*frac as f64)).round() as u64
                };
                assigned += share;
                share
            })
            .collect(),
    )
}

/// Whether a predicted total fits the target GPUs: every GPU's
/// share of the total (see [`predicted_shares`]) must be within
/// the headroom reserve of its total. With no targets there is
/// nothing to fit against.
pub fn predicted_fits(total_mib: u64, split: &[f32], gpu_total_mib: &[u64]) -> bool {
    match predicted_shares(total_mib, split, gpu_total_mib) {
        Some(shares) => shares.iter().zip(gpu_total_mib.iter()).all(|(share, total)| {
            *share <= total * (100 - HEADROOM_RESERVE_PERCENT) / 100
        }),
        None => false,
    }
}

/// Price every ladder rung against the inputs, in ladder order.
pub fn price_ladder(inputs: &CacheFitInputs) -> Vec<CacheRung> {
    PREDICTED_BEST_LADDER
        .iter()
        .map(|selection| {
            let kv_bytes = kv_bytes_per_token(&inputs.kv, selection)
                .saturating_mul(inputs.n_ctx as u64);
            let kv_mib = kv_bytes.div_ceil(MIB);
            let total_mib = inputs
                .gpu_weights_mib
                .saturating_add(kv_mib)
                .saturating_add(inputs.rs_mib.unwrap_or(0))
                .saturating_add(inputs.moe_cache_mib);
            CacheRung {
                selection: *selection,
                kv_mib,
                total_mib,
                fits: predicted_fits(total_mib, &inputs.split, &inputs.gpu_total_mib),
            }
        })
        .collect()
}

/// The predicted-best selection: the highest-quality rung that
/// fits, or `None` when no rung fits — the caller then reports
/// the lowest rung's predicted numbers and states the
/// configuration does not fit at the chosen context; it does not
/// silently reduce the context, change the placement, or apply
/// a non-fitting combination.
pub fn predicted_best(inputs: &CacheFitInputs) -> Option<CacheRung> {
    price_ladder(inputs).into_iter().find(|r| r.fits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_defaults_and_values() {
        assert_eq!(cache_selection(None), CacheSelection::default());
        assert_eq!(cache_selection(Some("")), CacheSelection::default());
        assert_eq!(
            cache_selection(Some("--parallel 1")),
            CacheSelection::default()
        );
        assert_eq!(
            cache_selection(Some("--cache-type-k q8_0")),
            CacheSelection {
                k: CacheType::Q8_0,
                v: CacheType::F16,
                flash_attn: FlashAttn::Auto,
            }
        );
        // Pair, short, and = spellings; the last occurrence wins.
        assert_eq!(
            cache_selection(Some(
                "-ctk q4_0 --cache-type-k=q8_0 -ctv=q4_0 -fa off --flash-attn on"
            )),
            CacheSelection {
                k: CacheType::Q8_0,
                v: CacheType::Q4_0,
                flash_attn: FlashAttn::On,
            }
        );
        // Truthy/falsey flash-attn spellings the pinned build accepts.
        assert_eq!(
            cache_selection(Some("--flash-attn 1")).flash_attn,
            FlashAttn::On
        );
        assert_eq!(
            cache_selection(Some("--flash-attn false")).flash_attn,
            FlashAttn::Off
        );
    }

    #[test]
    fn decode_unoffered_or_malformed_values_read_as_defaults() {
        // q5_0 is a real llama.cpp cache type v1 does not offer:
        // it decodes to the default position, never an invented one.
        assert_eq!(cache_selection(Some("--cache-type-k q5_0")).k, CacheType::F16);
        assert_eq!(cache_selection(Some("--cache-type-k")).k, CacheType::F16);
        assert_eq!(
            cache_selection(Some("--flash-attn banana")).flash_attn,
            FlashAttn::Auto
        );
        // A following flag is never eaten as a value.
        assert_eq!(
            cache_selection(Some("--cache-type-k --parallel 1")),
            CacheSelection::default()
        );
    }

    #[test]
    fn tokens_present_distinguishes_pre_feature_backends() {
        assert!(!cache_tokens_present(None));
        assert!(!cache_tokens_present(Some("--parallel 1 -ot exps=CPU")));
        assert!(cache_tokens_present(Some("--cache-type-k f16")));
        assert!(cache_tokens_present(Some("-fa on")));
        assert!(cache_tokens_present(Some("--cache-type-v=q4_0")));
    }

    #[test]
    fn write_from_scratch_and_round_trip() {
        let sel = CacheSelection {
            k: CacheType::Q8_0,
            v: CacheType::Q4_0,
            flash_attn: FlashAttn::On,
        };
        let written = set_cache_selection(None, Some(sel));
        assert_eq!(
            written.as_deref(),
            Some("--cache-type-k q8_0 --cache-type-v q4_0 --flash-attn on")
        );
        assert_eq!(cache_selection(written.as_deref()), sel);
        // Removing from nothing stays nothing.
        assert_eq!(set_cache_selection(None, None), None);
    }

    #[test]
    fn write_preserves_unrelated_flags_and_emits_exact_tokens() {
        // Flag emission: exactly the canonical tokens for the
        // selection, every other flag untouched and in order.
        let before = "--parallel 1 --spec-type draft-mtp --spec-draft-n-max 1";
        let sel = CacheSelection {
            k: CacheType::Q8_0,
            v: CacheType::Q8_0,
            flash_attn: FlashAttn::On,
        };
        assert_eq!(
            set_cache_selection(Some(before), Some(sel)).as_deref(),
            Some("--parallel 1 --spec-type draft-mtp --spec-draft-n-max 1 --cache-type-k q8_0 --cache-type-v q8_0 --flash-attn on")
        );
        // An unchanged (pre-feature) backend emits nothing new:
        // the writer is only invoked on an explicit change, and a
        // removal rewrite of a tokenless string is byte-identical.
        assert_eq!(
            set_cache_selection(Some(before), None).as_deref(),
            Some(before)
        );
        assert_eq!(set_cache_selection(None, None), None);
    }

    #[test]
    fn write_replaces_every_spelling_and_removal_strips_all() {
        let sel = CacheSelection {
            k: CacheType::F16,
            v: CacheType::F16,
            flash_attn: FlashAttn::Auto,
        };
        assert_eq!(
            set_cache_selection(
                Some("-ctk q4_0 --cache-type-v=q8_0 -fa off --threads 8"),
                Some(sel)
            )
            .as_deref(),
            Some("--threads 8 --cache-type-k f16 --cache-type-v f16 --flash-attn auto")
        );
        assert_eq!(
            set_cache_selection(
                Some("--threads 8 --cache-type-k q8_0 --flash-attn on"),
                None
            )
            .as_deref(),
            Some("--threads 8")
        );
        assert_eq!(
            set_cache_selection(Some("--cache-type-k q8_0"), None),
            None
        );
        // Round-trip across starts and targets.
        for raw in [
            None,
            Some(""),
            Some("-c 262144"),
            Some("--cache-type-k q4_0 --parallel 2"),
            Some("-fa=off -ot exps=CPU"),
        ] {
            for target in [None, Some(sel), Some(PREDICTED_BEST_LADDER[3])] {
                let out = set_cache_selection(raw, target);
                assert_eq!(
                    cache_selection(out.as_deref()),
                    target.unwrap_or_default(),
                    "raw={raw:?} target={target:?}"
                );
                assert_eq!(
                    cache_tokens_present(out.as_deref()),
                    target.is_some(),
                    "raw={raw:?} target={target:?}"
                );
            }
        }
    }

    #[test]
    fn v_quantized_fa_off_is_the_invalid_pairing() {
        let mut sel = CacheSelection::default();
        assert!(!sel.v_quantized_fa_off());
        sel.v = CacheType::Q8_0;
        // Auto promotes to enabled on the pinned build: valid.
        assert!(!sel.v_quantized_fa_off());
        sel.flash_attn = FlashAttn::On;
        assert!(!sel.v_quantized_fa_off());
        sel.flash_attn = FlashAttn::Off;
        assert!(sel.v_quantized_fa_off());
        // A quantized K alone does not trip the rule.
        sel.v = CacheType::F16;
        sel.k = CacheType::Q4_0;
        assert!(!sel.v_quantized_fa_off());
    }

    #[test]
    fn block_geometry_is_the_ggml_quanta() {
        // ggml-common.h at the pinned build: block_q8_0 is an fp16
        // scale + 32 int8 quants = 34 B per 32 elements; block_q4_0
        // is an fp16 scale + 32 nibbles = 18 B per 32 elements.
        assert_eq!(CacheType::Q8_0.block_geometry(), (32, 34));
        assert_eq!(CacheType::Q4_0.block_geometry(), (32, 18));
        assert_eq!(CacheType::F16.block_geometry(), (1, 2));
        assert_eq!(CacheType::Q8_0.row_bytes(256), 272);
        assert_eq!(CacheType::Q4_0.row_bytes(256), 144);
        assert_eq!(CacheType::F16.row_bytes(256), 512);
        // A partial block rounds up, never down.
        assert_eq!(CacheType::Q8_0.row_bytes(40), 68);
    }

    /// Pete's Oct 20, 2026 measured case: Qwen3.8-27B Planner-MTP
    /// IQ3_S on the RTX A4000 (16 GiB), parallel 1. The geometry
    /// is the model's own (GGUF metadata shape: 64 blocks, full
    /// attention every 4th layer → 16 KV layers, 4 KV heads, head
    /// dimensions 256/256); the file is the catalog pin
    /// (12,120,016,960 bytes including the MTP head); the
    /// recurrent-state term is the family geometry the GGUF parse
    /// yields (156,893,184 B per sequence — see the planner-moe
    /// requirement test in memory.rs), at one slot.
    fn planner_mtp_inputs(n_ctx: u32) -> CacheFitInputs {
        let req = super::super::memory::memory_requirement(
            12_120_016_960,
            Some(0),
            None,
            super::super::MemoryBasis::Catalog,
            super::super::ExpertOffload::None,
            Some(65_536),
            None,
            n_ctx,
            Some(156_893_184),
            1,
        );
        CacheFitInputs {
            kv: KvGeometry {
                kv_layers: 16,
                head_count_kv: 4,
                key_length: 256,
                value_length: 256,
            },
            gpu_weights_mib: req.gpu_weights_mib,
            rs_mib: req.rs_mib,
            moe_cache_mib: 0,
            n_ctx,
            split: Vec::new(),
            gpu_total_mib: vec![16_384],
        }
    }

    #[test]
    fn chooser_reproduces_petes_measured_cases() {
        // 32K: rung 1 (f16/f16, FA on) fits — his 29.5 tok/s point.
        let inputs = planner_mtp_inputs(32_768);
        let ladder = price_ladder(&inputs);
        assert_eq!(ladder[0].kv_mib, 2_048);
        assert_eq!(ladder[0].total_mib, 11_559 + 2_048 + 150);
        assert!(ladder[0].fits);
        assert_eq!(predicted_best(&inputs).unwrap(), ladder[0]);

        // 64K: f16 does not fit — his measured spill regime
        // (13.2 tok/s) — and the q8_0 rung is chosen (his q8_0
        // recovery measurement point).
        let inputs = planner_mtp_inputs(65_536);
        let ladder = price_ladder(&inputs);
        assert_eq!(ladder[0].kv_mib, 4_096);
        assert_eq!(ladder[0].total_mib, 11_559 + 4_096 + 150);
        assert!(!ladder[0].fits, "64K f16 must not fit: {:?}", ladder[0]);
        assert_eq!(ladder[1].kv_mib, 2_176);
        assert!(ladder[1].fits);
        let best = predicted_best(&inputs).unwrap();
        assert_eq!(best.selection.k, CacheType::Q8_0);
        assert_eq!(best.selection.v, CacheType::Q8_0);
        assert_eq!(best.selection.flash_attn, FlashAttn::On);

        // 256K: no rung fits at the chosen context; the lowest
        // rung's numbers are the honest report (16,317 MiB against
        // a 16,384 MiB card — under the raw total, over the
        // headroom limit, so still no fit claim).
        let inputs = planner_mtp_inputs(262_144);
        let ladder = price_ladder(&inputs);
        assert!(ladder.iter().all(|r| !r.fits));
        assert_eq!(predicted_best(&inputs), None);
        let lowest = ladder[3];
        assert_eq!(lowest.kv_mib, 4_608);
        assert_eq!(lowest.total_mib, 11_559 + 4_608 + 150);
        assert!(lowest.total_mib < 16_384);
    }

    #[test]
    fn ladder_degrades_v_before_k() {
        assert_eq!(PREDICTED_BEST_LADDER[2].k, CacheType::Q8_0);
        assert_eq!(PREDICTED_BEST_LADDER[2].v, CacheType::Q4_0);
        // Flash attention stays on at every rung.
        assert!(PREDICTED_BEST_LADDER
            .iter()
            .all(|s| s.flash_attn == FlashAttn::On));
    }

    #[test]
    fn predicted_shares_distribute_like_predicted_fits() {
        // Unsplit: the whole total lands on the first target.
        assert_eq!(predicted_shares(1_000, &[], &[16_384]), Some(vec![1_000]));
        assert_eq!(
            predicted_shares(1_000, &[], &[16_384, 16_384]),
            Some(vec![1_000, 0])
        );
        // Split: shares follow the fractions and the last target
        // takes the rounding remainder, so shares sum to the total.
        assert_eq!(
            predicted_shares(10_001, &[0.5, 0.5], &[16_384, 16_384]),
            Some(vec![5_001, 5_000])
        );
        assert_eq!(
            predicted_shares(900, &[0.25, 0.25, 0.5], &[16_384, 16_384, 16_384]),
            Some(vec![225, 225, 450])
        );
        // Nothing to distribute over, and a stale split: no shares.
        assert_eq!(predicted_shares(1_000, &[], &[]), None);
        assert_eq!(predicted_shares(1_000, &[1.0], &[16_384, 16_384]), None);
    }

    #[test]
    fn split_placement_fits_per_gpu() {
        // The same 64K f16 total split evenly across two 16 GiB
        // GPUs fits each card's share, though it fits neither
        // card alone.
        let mut inputs = planner_mtp_inputs(65_536);
        inputs.split = vec![0.5, 0.5];
        inputs.gpu_total_mib = vec![16_384, 16_384];
        assert!(price_ladder(&inputs)[0].fits);
        // A stale split (length mismatch) distributes nothing and
        // fits nothing.
        inputs.split = vec![1.0];
        assert!(!price_ladder(&inputs)[0].fits);
        // No targets: no fit claim.
        inputs.split = Vec::new();
        inputs.gpu_total_mib = Vec::new();
        assert!(!price_ladder(&inputs)[0].fits);
    }
}
