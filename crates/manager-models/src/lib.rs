//! Model library: downloads, checksums, fit-check (spec §4.6).
//!
//! Curated GGUF catalog with SHA-256 verification before a model is marked
//! "installed". Downloads run on a background task and support progress
//! callbacks, pause/resume (HTTP range requests), and cancel (partial file
//! removed). Fit-check compares model size against discovered per-GPU free
//! VRAM and recommends a placement: single GPU, tensor split proportional to
//! free VRAM (fractions sum to 1.0), or too-large with the arithmetic shown.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

pub mod cache;
pub mod flags;
pub mod gguf;
pub mod guardrails;
pub mod memory;
pub mod moe_cache;
pub mod mtp;
pub mod parallel;
pub mod placement;

pub use cache::{
    cache_selection, cache_tokens_present, kv_bytes_per_token, predicted_best, predicted_fits,
    predicted_shares, price_ladder, set_cache_selection, CacheFitInputs, CacheRung, CacheSelection,
    CacheType,
    FlashAttn, HEADROOM_RESERVE_PERCENT, PREDICTED_BEST_LADDER,
};
pub use flags::{raw_flag_warnings, FlagWarning};
pub use gguf::{parse_gguf_layout, GgufError, GgufLayout, KvGeometry, RsGeometry};
pub use guardrails::{
    nearest_allowed, offload_status, restriction_for, AllowedOffload, OffloadRestriction,
    OffloadStatus,
};
pub use memory::{
    expert_offload, memory_requirement, offload_bands, offload_flag_string, offload_for_layers,
    set_expert_offload, sys_bytes_by_position, ExpertOffload, MemoryBasis, MemoryRequirement,
};
pub use moe_cache::{
    auto_moe_cache_budget, moe_cache_mib, moe_cache_shares, set_moe_cache, MOE_CACHE_AUTO_RESERVE_MIB,
    MOE_CACHE_DEFAULT_MIB, MOE_CACHE_PRESETS_MIB,
};
pub use mtp::{mtp_draft, set_mtp_draft, MtpDraft, LLAMA_CPP_N_MAX_DEFAULT};
pub use parallel::{effective_slots, parallel_slots, set_parallel_slots};
pub use placement::{
    apply_placement, propose_placement, propose_placement_full, propose_placement_resident,
    validate_split,
    PlacementProjection,
};

/// Bytes in one MiB.
pub const MIB: u64 = 1024 * 1024;

/// Marker for an unpinned SHA-256 value. The curated catalog ships real
/// pinned hashes (below); this marker remains for hand-written specs and
/// tests. Verification fails closed on it: a model whose sha256 is still
/// the placeholder can never be marked "installed".
pub const PLACEHOLDER_SHA256: &str = "PLACEHOLDER-pin-real-sha256-at-install-time";

/// One downloadable model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSpec {
    pub id: String,
    pub name: String,
    /// e.g. "Q4_K_M".
    pub quant: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub url: String,
    /// Parameter count in billions. Seeded into the model config,
    /// whose validation requires a positive value.
    pub params_b: f32,
    /// The model's rated maximum context length in tokens, from the
    /// publisher's specification. The backend flags editor builds its
    /// context presets from this: values beyond it are offered but
    /// annotated as beyond the rating. 0 means no rating is known
    /// (models defined outside the curated catalog).
    #[serde(default)]
    pub ctx_max: u32,
    /// Whether seeding enables this model. The default fleet is
    /// enabled; opt-in candidates (alternative models for a role)
    /// seed disabled and unassigned — the operator enables one in
    /// the Models view, downloads it, and assigns it to a backend.
    pub enabled_by_default: bool,
    /// Total bytes of the routed-expert tensors in the GGUF, when
    /// known (derived from the file's tensor directory; see
    /// [`gguf`]). The memory-requirement calculation uses it to
    /// split a backend's requirement between GPU and system RAM
    /// when expert offload is configured, before the file is
    /// downloaded. `None` when the split is not pinned; models with
    /// no routed experts pin `Some(0)`.
    #[serde(default)]
    pub expert_bytes: Option<u64>,
    /// Number of blocks that carry routed-expert tensors, when the
    /// model is a mixture-of-experts model and the count is pinned
    /// (derived from the GGUF tensor directory, like
    /// `expert_bytes`). `--n-cpu-moe` counts these layers, not
    /// total blocks. `None` for dense models and when unpinned.
    #[serde(default)]
    pub moe_layers: Option<u32>,
    /// KV-cache bytes per token at llama.cpp's default F16 cache
    /// types, when known (derived from the file's GGUF KV geometry
    /// — the same inputs [`gguf::parse_gguf_layout`] uses: trunk
    /// block count with `full_attention_interval` folding and any
    /// nextn prediction blocks excluded, `head_count_kv`, and the
    /// key/value lengths). The memory-requirement calculation
    /// prices a backend's KV cache from it before the file is
    /// downloaded, and whenever the downloaded file's header
    /// cannot be parsed. `None` when the geometry is not pinned.
    #[serde(default)]
    pub kv_bytes_per_token: Option<u64>,
    /// The catalog's default expert offload for a backend serving
    /// this model, in MoE layers: the most offload that is still
    /// "green" (fast) for the model. It is both the Edit Flags
    /// offload slider's green limit and the offload a newly
    /// created or newly assigned backend starts with (applied only
    /// where the backend carries no raw flags of its own — an
    /// operator-changed offload is never clobbered). `None` when
    /// the model has no expert offload to default.
    #[serde(default)]
    pub default_offload_layers: Option<u32>,
    /// Per-architecture expert-offload guardrails for this model
    /// (see [`guardrails`]): positions a restriction does not
    /// allow are blocked on GPUs with its compute capability,
    /// regardless of the slider's color bands. Empty when every
    /// position is allowed on every architecture. Resolved from
    /// the live catalog by model id, like the slider's green
    /// limit, so catalog pins reach existing configurations
    /// without a catalog-revision bump.
    #[serde(default)]
    pub offload_restrictions: Vec<OffloadRestriction>,
    /// Whether this model's GGUF carries an MTP (multi-token
    /// prediction) head that llama.cpp's `--spec-type draft-mtp`
    /// self-speculative decoding can draft with. Only the planner
    /// model's `-mtp` build pins it today — the base build has no
    /// head, and neither does any other catalog model. Resolved
    /// from the live catalog by model id (or source URL), like the
    /// offload restrictions, so the pin reaches existing
    /// configurations without a catalog-revision bump. A model
    /// with no pin is not MTP-capable: capability is never
    /// inferred from a model's name or file.
    #[serde(default)]
    pub mtp: bool,
}

/// The curated catalog's default expert offload for a model: the
/// offload a new backend serving it starts with, from
/// [`ModelSpec::default_offload_layers`]. [`ExpertOffload::None`]
/// when the model pins no default.
pub fn catalog_default_offload(spec: &ModelSpec) -> ExpertOffload {
    match (spec.default_offload_layers, spec.moe_layers) {
        (Some(d), Some(l)) if l > 0 => memory::offload_for_layers(d.min(l), l),
        _ => ExpertOffload::None,
    }
}

/// The curated catalog's parameter count for a model being
/// registered (`model_add`): an exact id match wins; otherwise an
/// exact source-URL match — the same catalog file registered under
/// an operator-chosen id. `None` when the catalog knows neither the
/// id nor the URL, in which case registration must ask the caller
/// for the count rather than store a placeholder (config validation
/// requires a positive `params_b`, and the value feeds memory-fit
/// estimation that nothing re-derives later).
pub fn catalog_params_b(id: &str, source_url: &str) -> Option<f32> {
    let models = curated_models();
    if let Some(spec) = models.iter().find(|s| s.id == id) {
        return Some(spec.params_b);
    }
    models
        .iter()
        .find(|s| s.url == source_url)
        .map(|s| s.params_b)
}

/// The catalog's MoE layer count for a model, resolved the way
/// [`catalog_mtp_pin`] resolves its pin: by id first, then by
/// source URL. `None` when the model is outside the catalog or
/// the catalog carries no MoE layer count for it (a dense model).
pub fn catalog_moe_layers(id: &str, source_url: &str) -> Option<u32> {
    let models = curated_models();
    if let Some(spec) = models.iter().find(|s| s.id == id) {
        return spec.moe_layers;
    }
    if source_url.is_empty() {
        return None;
    }
    models
        .iter()
        .find(|s| s.url == source_url)
        .and_then(|s| s.moe_layers)
}

/// The curated catalog's MTP capability pin for a model (see
/// [`ModelSpec::mtp`]): `Some(pin)` when the catalog knows the
/// model — an exact id match wins; otherwise an exact source-URL
/// match, the same resolution [`catalog_params_b`] uses, so the
/// catalog's `-mtp` build registered under an operator-chosen id
/// still resolves. `None` when the catalog knows neither the id
/// nor the URL: the catalog pins nothing for such a model, and
/// capability falls to the model record's own declaration (see
/// [`effective_mtp`]).
pub fn catalog_mtp_pin(id: &str, source_url: &str) -> Option<bool> {
    let models = curated_models();
    if let Some(spec) = models.iter().find(|s| s.id == id) {
        return Some(spec.mtp);
    }
    if source_url.is_empty() {
        return None;
    }
    models
        .iter()
        .find(|s| s.url == source_url)
        .map(|s| s.mtp)
}

/// Whether the curated catalog pins MTP capability for a model
/// (see [`ModelSpec::mtp`]): `false` when the catalog knows
/// neither the id nor the URL — capability is a pinned fact,
/// never a guess.
pub fn catalog_mtp(id: &str, source_url: &str) -> bool {
    catalog_mtp_pin(id, source_url).unwrap_or(false)
}

/// A model's EFFECTIVE MTP capability: the curated catalog's pin
/// where the catalog knows the model (by id or source URL), else
/// the operator's declaration on the model record
/// (`ModelConfig::mtp`), else not capable. The same precedence
/// shape as the MoE figures: a catalog pin wins over the record,
/// because the pin is a verified property of the catalog build —
/// a declaration stored on a catalog model is shadowed by the
/// pin, never an override. Capability is never inferred from a
/// model's name or file.
pub fn effective_mtp(id: &str, source_url: &str, declared: Option<bool>) -> bool {
    match catalog_mtp_pin(id, source_url) {
        Some(pin) => pin,
        None => declared.unwrap_or(false),
    }
}

/// The curated catalog's revision. Bump whenever
/// [`curated_models`] changes: existing configurations append
/// newly added models exactly once per revision (see
/// `append_missing_catalog_models` in manager-config), so catalog
/// additions reach installed systems without resurrecting models
/// an operator deliberately deleted.
pub const CATALOG_REVISION: u32 = 3;

/// Download progress snapshot for the progress callback.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
}

impl DownloadProgress {
    pub fn fraction(&self) -> f64 {
        if self.total_bytes == 0 {
            0.0
        } else {
            self.downloaded_bytes as f64 / self.total_bytes as f64
        }
    }
}

/// The curated model catalog (spec §4.6, extended since).
///
/// SHA-256 values and sizes are the real values from the publisher's
/// Hugging Face file metadata (the LFS object hash and size for each
/// file, pinned 2026-10-15; the planner and coder replacements
/// 2026-10-16), so [`verify_sha256`] runs against a real pin from
/// the first download. Catalog revision 2 replaced two default
/// definitions: the planner slot — which carried the spec's
/// "Qwen3.8-27B" name over a Qwen3-32B artifact before that model
/// existed — now points at the real Qwen3.8-27B, and the coder slot
/// at Qwen3-Coder-Next. Existing configurations pick the
/// replacements up through
/// `manager_config::refresh_catalog_definition`. Qwen publishes no
/// official GGUF repo for Qwen3-Coder-30B-A3B-Instruct, so the coder
/// slot uses unsloth's GGUF conversion of the official weights.
/// Catalog revision 3 added one opt-in entry: the planner model's
/// `-mtp` build (`qwen3.8-27b-mtp`), the same GSQ-RCO IQ3_S weights
/// plus the MTP (multi-token prediction) block, as a separate model
/// so a Planner-MTP backend can be tested side by side with the
/// existing planner without touching it.
pub fn curated_models() -> Vec<ModelSpec> {
    vec![
        ModelSpec {
            id: "qwen3-27b-planner".to_string(),
            name: "Qwen3.8-27B".to_string(),
            quant: "IQ3_S".to_string(),
            size_bytes: 11_771_546_784,
            sha256: "64b53b64c7aa39f20a7e54bd80582fe595b1d745624ee8a72e92508c0326d810".to_string(),
            ctx_max: 262144,
            params_b: 27.0,
            enabled_by_default: true,
            expert_bytes: Some(0),
            moe_layers: None,
            // KV geometry from the file's GGUF header (pinned
            // 2026-10-21): qwen35, 64 blocks, full attention every
            // 4th layer (16 KV layers), 4 KV heads, key/value
            // lengths 256 — 16 * 4 * (256 + 256) * 2 B.
            kv_bytes_per_token: Some(65_536),
            default_offload_layers: None,
            offload_restrictions: Vec::new(),
            // The real Qwen3.8-27B (Alibaba, August 2026), dense
            // 27B, in ISTA-DASLab's GSQ-RCO quantization at IQ3_S —
            // fully GPU-resident on a 16 GB card. Replaced the
            // Qwen3-32B that filled this slot at revision 2.
            mtp: false,
            url: "https://huggingface.co/ISTA-DASLab/Qwen3.8-27B-GSQ-RCO-GGUF/resolve/main/Qwen3.8-27B-GSQ-RCO-IQ3_S.gguf"
                .to_string(),
        },
        // Opt-in variant of the planner model (added at catalog
        // revision 3): the same GSQ-RCO IQ3_S build plus the MTP
        // (multi-token prediction) block — block 64, 15 tensors,
        // head weights at Q6_K — which llama.cpp's
        // `--spec-type draft-mtp` self-speculative decoding drafts
        // with. A separate model entry on purpose: a Planner-MTP
        // backend can be created, tuned, and benchmarked against
        // the existing planner backend without modifying it (the
        // two backends are started on demand, never together —
        // 11,771,546,784 + 12,120,016,960 bytes of weights exceed
        // a 16 GiB card before KV cache). Seeds disabled and
        // unassigned, like the Nemotron opt-in below; no default
        // backend is seeded for it. SHA-256 and size are the
        // publisher's Hugging Face LFS metadata for this file
        // (X-Linked-ETag / X-Linked-Size, pinned 2026-10-17; the
        // same source reproduces the base planner file's pin
        // exactly).
        ModelSpec {
            id: "qwen3.8-27b-mtp".to_string(),
            name: "Qwen3.8-27B Planner-MTP (GSQ-RCO IQ3_S)".to_string(),
            quant: "IQ3_S".to_string(),
            size_bytes: 12_120_016_960,
            sha256: "58fd826723939933dc86f45b7fe04545cbc2de1c70f6fe2cdd3858c87a98c12f".to_string(),
            ctx_max: 262144,
            params_b: 27.0,
            enabled_by_default: false,
            expert_bytes: Some(0),
            moe_layers: None,
            // Same KV geometry as the base planner file: the
            // header's 65th block is the nextn prediction block,
            // which carries no KV in the main context, so the
            // trunk is the same 64 blocks / 16 KV layers.
            kv_bytes_per_token: Some(65_536),
            default_offload_layers: None,
            offload_restrictions: Vec::new(),
            // The one MTP-capable catalog entry: this build
            // carries the MTP head (see the entry comment above).
            mtp: true,
            url: "https://huggingface.co/ISTA-DASLab/Qwen3.8-27B-GSQ-RCO-GGUF/resolve/main/Qwen3.8-27B-GSQ-RCO-IQ3_S-mtp.gguf"
                .to_string(),
        },
        ModelSpec {
            id: "qwen3-coder-30b".to_string(),
            name: "Qwen3-Coder-Next-80B".to_string(),
            quant: "UD-Q4_K_M".to_string(),
            size_bytes: 49_301_055_488,
            sha256: "3a47167ad1bf9e9a79f233f799588d54f85bc65644e72c19251165823e7ae612".to_string(),
            ctx_max: 262144,
            params_b: 80.0,
            enabled_by_default: true,
            // Routed-expert total derived from the file's GGUF
            // tensor directory 2026-10-16 (the non-expert share is
            // 2,587,296,768 B; the directory sums reproduce the
            // pinned file size exactly).
            expert_bytes: Some(46_707_769_344),
            // 48 MoE layers (every block carries routed experts),
            // derived from the same tensor directory. The default
            // offload is all of them: full expert offload is this
            // model's shipped design point, so the green limit is
            // the whole range.
            moe_layers: Some(48),
            // KV geometry from the file's GGUF header (pinned
            // 2026-10-21): qwen3next, 48 blocks, full attention
            // every 4th layer (12 KV layers), 2 KV heads, key/value
            // lengths 256 — 12 * 2 * (256 + 256) * 2 B.
            kv_bytes_per_token: Some(24_576),
            default_offload_layers: Some(48),
            // Pascal guardrail (tracker #37): at --n-cpu-moe 46 on
            // a Tesla P100 (compute capability (6, 0)) the coder
            // died with a CUDA invalid-argument launch error; full
            // offload fixed it. Endpoints only on (6, 0) until
            // partial positions are verified per architecture.
            offload_restrictions: vec![OffloadRestriction {
                compute_capability: (6, 0),
                allowed: AllowedOffload::EndpointsOnly,
            }],
            // Qwen3-Coder-Next (80B total / ~3B active MoE),
            // served by expert offload — the coder backend's raw
            // flags carry `-ot exps=CPU`. Unsloth's single-file
            // UD-Q4_K_M: Qwen's official Q4_K_M ships as four
            // shards, which the single-file download/verify path
            // cannot consume. Replaced Qwen3-Coder-30B-A3B at
            // revision 2.
            mtp: false,
            url: "https://huggingface.co/unsloth/Qwen3-Coder-Next-GGUF/resolve/main/Qwen3-Coder-Next-UD-Q4_K_M.gguf"
                .to_string(),
        },
        ModelSpec {
            id: "qwen2.5-coder-7b".to_string(),
            name: "Qwen2.5-Coder-7B".to_string(),
            quant: "Q4_K_M".to_string(),
            size_bytes: 4_683_073_536,
            sha256: "509287f78cb4d4cf6b3843734733b914b2c158e43e22a7f4bf5e963800894d3c".to_string(),
            ctx_max: 131072,
            params_b: 7.0,
            enabled_by_default: true,
            expert_bytes: Some(0),
            moe_layers: None,
            // KV geometry from the file's GGUF header (pinned
            // 2026-10-21): qwen2, 28 dense blocks, 4 KV heads; the
            // header carries no key/value lengths, so the head
            // dimension is llama.cpp's default derivation,
            // embedding_length / head_count = 3584 / 28 = 128 —
            // 28 * 4 * (128 + 128) * 2 B.
            kv_bytes_per_token: Some(57_344),
            default_offload_layers: None,
            offload_restrictions: Vec::new(),
            mtp: false,
            url: "https://huggingface.co/Qwen/Qwen2.5-Coder-7B-Instruct-GGUF/resolve/main/qwen2.5-coder-7b-instruct-q4_k_m.gguf"
                .to_string(),
        },
        ModelSpec {
            id: "qwen3-8b".to_string(),
            name: "Qwen3-8B".to_string(),
            quant: "Q4_K_M".to_string(),
            size_bytes: 5_027_783_488,
            sha256: "d98cdcbd03e17ce47681435b5150e34c1417f50b5c0019dd560e4882c5745785".to_string(),
            ctx_max: 131072,
            params_b: 8.0,
            enabled_by_default: true,
            expert_bytes: Some(0),
            moe_layers: None,
            // KV geometry from the file's GGUF header (pinned
            // 2026-10-21): qwen3, 36 dense blocks, 8 KV heads,
            // key/value lengths 128 — 36 * 8 * (128 + 128) * 2 B.
            kv_bytes_per_token: Some(147_456),
            default_offload_layers: None,
            offload_restrictions: Vec::new(),
            mtp: false,
            url: "https://huggingface.co/Qwen/Qwen3-8B-GGUF/resolve/main/Qwen3-8B-Q4_K_M.gguf"
                .to_string(),
        },
        // Opt-in candidate for the coder role (added at catalog
        // revision 1): NVIDIA's agent-execution model — hybrid
        // Mamba-2 + MoE, 31.6B total / ~3.6B active. Seeds disabled
        // and unassigned; the pinned llama.cpp build carries the
        // nemotron_h_moe architecture. Mamba-2 execution on Pascal
        // (P100) is unvalidated — that is part of what the opt-in
        // evaluation is for.
        ModelSpec {
            id: "nemotron-3.5-lightning-30b".to_string(),
            name: "Nemotron 3.5 Lightning 30B-A3B".to_string(),
            quant: "Q4_K_M".to_string(),
            size_bytes: 25_266_255_936,
            sha256: "edcb5d4650796ed2fb412498de6f83b585862312c747ddb74f0ea04b22206181".to_string(),
            ctx_max: 262144,
            params_b: 31.6,
            enabled_by_default: false,
            // Routed experts are 22,829,334,528 of the pinned file's
            // 25,258,358,528 tensor bytes (24 MoE blocks of 53),
            // derived from the GGUF tensor directory 2026-10-16 —
            // expert offload (`--n-cpu-moe`, `--cpu-moe`) is this
            // model's fit path on 16 GiB GPUs.
            expert_bytes: Some(22_829_334_528),
            // 24 MoE blocks of 53 total. The default offload is 16
            // of the 24: the most offload that stays green — the
            // GPU-resident remainder (~9.3 GiB of weights) fits a
            // single 16 GiB GPU alongside the non-expert weights.
            moe_layers: Some(24),
            // KV geometry from the file's GGUF tensor directory
            // (pinned 2026-10-21): the header carries no
            // head_count_kv, so the pin comes from the directory —
            // 7 blocks carry attention tensors (5, 12, 19, 26, 33,
            // 42, 52), each attn_k/attn_v 256 wide at key/value
            // lengths 128 (2 KV heads) — 7 * 2 * (128 + 128) * 2 B.
            kv_bytes_per_token: Some(7_168),
            default_offload_layers: Some(16),
            offload_restrictions: Vec::new(),
            mtp: false,
            url: "https://huggingface.co/unsloth/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-GGUF/resolve/main/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-UD-Q4_K_M.gguf"
                .to_string(),
        },
    ]
}

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

/// Stream a file's SHA-256 hex digest with a bounded 1 MiB buffer,
/// regardless of file size. This is the one hashing implementation
/// for the whole app — the model verifier and the setup wizard both
/// use it. A whole-file read of a multi-GiB GGUF materializes the
/// file in memory, and a failed allocation aborts the process: the
/// wizard's former read-then-hash implementation did exactly that
/// on the 45.9 GiB Coder-Next file (the v0.1.31 "Verify checksum"
/// crash).
pub fn sha256_hex(path: &Path) -> Result<String, ModelError> {
    use std::io::Read;

    let mut file = std::fs::File::open(path).map_err(|e| ModelError::Io(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| ModelError::Io(e.to_string()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// SHA-256-verify a downloaded file against the catalog hash.
///
/// Returns `Ok(true)` on match, `Ok(false)` on mismatch (callers decide
/// whether to block "installed" / offer re-download). Malformed expected
/// hashes and I/O failures are errors.
pub async fn verify_sha256(path: &Path, expected_hex: &str) -> Result<bool, ModelError> {
    let expected = expected_hex.trim().to_lowercase();
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ModelError::InvalidChecksum(format!(
            "expected 64 hex chars, got {expected_hex:?}"
        )));
    }
    let owned = path.to_path_buf();
    let actual = tokio::task::spawn_blocking(move || sha256_hex(&owned))
        .await
        .map_err(|e| ModelError::Io(format!("hash task failed: {e}")))??;
    Ok(actual == expected)
}

// ---------------------------------------------------------------------------
// Download manager
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerState {
    Downloading,
    Paused,
    Cancelled,
    Done,
    Failed,
}

struct DownloadShared {
    state: Mutex<WorkerState>,
    notify: Notify,
    downloaded: AtomicU64,
    total: AtomicU64,
    result: Mutex<Option<Result<(), ModelError>>>,
    /// Set by the worker when it has parked in the paused state; lets
    /// `pause()` return only once the transfer is actually stopped.
    paused_ack: AtomicBool,
}

impl DownloadShared {
    fn current_state(&self) -> WorkerState {
        *mutex_lock(&self.state)
    }
}

/// Lock a mutex, recovering from poisoning instead of panicking.
/// Used for the state/result mutexes, which are only held across
/// non-panicking code — a poisoned lock here must not wedge the handle.
/// (The progress-callback mutex intentionally does NOT use this: a
/// panicking callback is skipped, see `report_progress`.)
fn mutex_lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

type ProgressCallback = Arc<Mutex<Box<dyn Fn(DownloadProgress) + Send + 'static>>>;

/// Handle to an in-flight download.
///
/// `download()` returns immediately after spawning the background task; use
/// [`DownloadHandle::wait`] to await completion, [`DownloadHandle::progress`]
/// to poll, and pause/resume/cancel to control the transfer.
#[derive(Clone)]
pub struct DownloadHandle {
    spec_id: String,
    shared: Arc<DownloadShared>,
}

impl DownloadHandle {
    /// Stable id of the model being downloaded.
    pub fn id(&self) -> &str {
        &self.spec_id
    }

    /// Pause the download (keeps the partial file for resume). Synchronous:
    /// returns only once the worker has parked, so no further bytes land
    /// after this returns. No-op once the download has reached a terminal
    /// state.
    pub async fn pause(&self) -> Result<(), ModelError> {
        let needs_ack = {
            let mut state = mutex_lock(&self.shared.state);
            if *state == WorkerState::Downloading {
                *state = WorkerState::Paused;
                self.shared.paused_ack.store(false, Ordering::SeqCst);
                self.shared.notify.notify_waiters();
                true
            } else {
                false
            }
        };
        if needs_ack {
            // Wait for the worker to acknowledge the parked state (or for a
            // concurrent terminal transition / overtake by resume(), so we
            // can't hang if the worker finished or resumed concurrently).
            loop {
                if self.shared.paused_ack.load(Ordering::SeqCst) {
                    break;
                }
                match self.shared.current_state() {
                    WorkerState::Done
                    | WorkerState::Failed
                    | WorkerState::Cancelled
                    | WorkerState::Downloading => break,
                    _ => {}
                }
                self.shared.notify.notified().await;
            }
        }
        Ok(())
    }

    /// Resume a paused download via HTTP range requests. No-op unless paused.
    pub async fn resume(&self) -> Result<(), ModelError> {
        let mut state = mutex_lock(&self.shared.state);
        if *state == WorkerState::Paused {
            *state = WorkerState::Downloading;
            self.shared.notify.notify_waiters();
        }
        Ok(())
    }

    /// Cancel and delete the partial file. No-op once done/failed.
    pub async fn cancel(&self) -> Result<(), ModelError> {
        let mut state = mutex_lock(&self.shared.state);
        match *state {
            WorkerState::Downloading | WorkerState::Paused => {
                *state = WorkerState::Cancelled;
                self.shared.notify.notify_waiters();
            }
            _ => {}
        }
        Ok(())
    }

    /// Whether the download has reached a terminal state (done, failed,
    /// or cancelled). Paused is NOT terminal: the transfer can resume.
    /// Callers use this to reap finished handles from their registries.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.shared.current_state(),
            WorkerState::Done | WorkerState::Failed | WorkerState::Cancelled
        )
    }

    /// Whether the download is currently paused (parked, partial file
    /// kept for resume).
    pub fn is_paused(&self) -> bool {
        self.shared.current_state() == WorkerState::Paused
    }

    /// Latest progress snapshot (lock-free read).
    pub fn progress(&self) -> DownloadProgress {
        DownloadProgress {
            downloaded_bytes: self.shared.downloaded.load(Ordering::SeqCst),
            total_bytes: self.shared.total.load(Ordering::SeqCst),
        }
    }

    /// Await the download reaching a terminal state.
    pub async fn wait(&self) -> Result<(), ModelError> {
        loop {
            if let Some(result) = mutex_lock(&self.shared.result).clone() {
                return result;
            }
            self.shared.notify.notified().await;
        }
    }
}

/// Start downloading `spec` to `dest`, invoking `on_progress` as chunks land.
///
/// Returns immediately with a [`DownloadHandle`]; the transfer runs on a
/// background task. Bytes land in a sibling partial file (`dest` with
/// `.part` appended) and are moved into place at `dest` only once the
/// transfer completes, so `dest` never holds a partial file. An existing
/// partial file is resumed with `Range` when the server honors it,
/// otherwise the transfer restarts from zero. Fails fast on an
/// unparseable URL or an unwritable destination.
pub async fn download<F>(
    spec: &ModelSpec,
    dest: &Path,
    on_progress: F,
) -> Result<DownloadHandle, ModelError>
where
    F: Fn(DownloadProgress) + Send + 'static,
{
    let _: reqwest::Url = spec
        .url
        .parse()
        .map_err(|e| ModelError::Network(format!("invalid url {:?}: {e}", spec.url)))?;

    // Fail fast on an unwritable destination: create the parent dir here,
    // before spawning the worker, rather than surfacing it via `wait()`.
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ModelError::Io(e.to_string()))?;
        }
    }

    let shared = Arc::new(DownloadShared {
        state: Mutex::new(WorkerState::Downloading),
        notify: Notify::new(),
        downloaded: AtomicU64::new(0),
        total: AtomicU64::new(0),
        result: Mutex::new(None),
        paused_ack: AtomicBool::new(false),
    });
    let callback: ProgressCallback = Arc::new(Mutex::new(Box::new(on_progress)));

    let worker_spec = spec.clone();
    let worker_dest = dest.to_path_buf();
    let worker_shared = Arc::clone(&shared);
    tokio::spawn(async move {
        download_worker(worker_spec, worker_dest, worker_shared, callback).await;
    });

    Ok(DownloadHandle {
        spec_id: spec.id.clone(),
        shared,
    })
}

fn report_progress(
    shared: &DownloadShared,
    callback: &ProgressCallback,
    downloaded: u64,
    total: u64,
) {
    shared.downloaded.store(downloaded, Ordering::SeqCst);
    shared.total.store(total, Ordering::SeqCst);
    let progress = DownloadProgress {
        downloaded_bytes: downloaded,
        total_bytes: total,
    };
    // A callback that panicked previously poisons its mutex; skip rather
    // than re-invoking it (which would panic the worker task and wedge
    // `wait()` forever). State/result mutexes use `mutex_lock` recovery
    // instead, since they are only held across non-panicking code.
    if let Ok(cb) = callback.lock() {
        cb(progress);
    }
}

async fn wait_while_paused(shared: &DownloadShared) {
    let mut first = true;
    while shared.current_state() == WorkerState::Paused {
        if first {
            // Acknowledge the parked state so `pause()` can return.
            shared.paused_ack.store(true, Ordering::SeqCst);
            shared.notify.notify_waiters();
            first = false;
        }
        shared.notify.notified().await;
    }
}

async fn download_worker(
    spec: ModelSpec,
    dest: PathBuf,
    shared: Arc<DownloadShared>,
    callback: ProgressCallback,
) {
    let outcome = download_inner(&spec, &dest, &shared, &callback).await;
    let terminal = match &outcome {
        Ok(()) => WorkerState::Done,
        Err(ModelError::Cancelled(_)) => WorkerState::Cancelled,
        Err(_) => WorkerState::Failed,
    };
    *mutex_lock(&shared.state) = terminal;
    *mutex_lock(&shared.result) = Some(outcome);
    shared.notify.notify_waiters();
}

/// The partial-file path for a download destination: `dest` with `.part`
/// appended to the file name. Transfers write here and are renamed into
/// place at `dest` only on success.
pub fn partial_path(dest: &Path) -> PathBuf {
    let mut os = dest.as_os_str().to_owned();
    os.push(".part");
    PathBuf::from(os)
}

/// Parse `Content-Range: bytes START-END/TOTAL` into `(start, total)`.
fn parse_content_range(header: &str) -> Option<(u64, u64)> {
    let rest = header.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, _end) = range.split_once('-')?;
    Some((start.trim().parse().ok()?, total.trim().parse().ok()?))
}

/// Stream a response body into the partial file (appending when
/// `append`, truncating otherwise), starting the byte counter at
/// `downloaded`. Pause parks the transfer; cancel deletes the partial
/// file. On a complete body the counter and the on-disk length must
/// agree, then the partial file is moved into place at `dest`.
#[allow(clippy::too_many_arguments)]
async fn transfer_body(
    response: &mut reqwest::Response,
    part: &Path,
    dest: &Path,
    append: bool,
    mut downloaded: u64,
    total: u64,
    spec: &ModelSpec,
    shared: &DownloadShared,
    callback: &ProgressCallback,
) -> Result<(), ModelError> {
    use tokio::io::AsyncWriteExt;

    let mut file = if append {
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(part)
            .await
            .map_err(|e| ModelError::Io(e.to_string()))?
    } else {
        tokio::fs::File::create(part)
            .await
            .map_err(|e| ModelError::Io(e.to_string()))?
    };
    report_progress(shared, callback, downloaded, total);

    loop {
        match shared.current_state() {
            WorkerState::Cancelled => {
                drop(file);
                let _ = tokio::fs::remove_file(part).await;
                return Err(ModelError::Cancelled(spec.id.clone()));
            }
            WorkerState::Paused => {
                wait_while_paused(shared).await;
                continue;
            }
            _ => {}
        }

        tokio::select! {
            chunk_result = response.chunk() => {
                match chunk_result {
                    Ok(Some(chunk)) => {
                        file.write_all(&chunk)
                            .await
                            .map_err(|e| ModelError::Io(e.to_string()))?;
                        downloaded += chunk.len() as u64;
                        report_progress(shared, callback, downloaded, total);
                    }
                    Ok(None) => break,
                    Err(e) => return Err(ModelError::Network(e.to_string())),
                }
            }
            _ = shared.notify.notified() => {
                // Control state changed (pause/cancel); re-check above.
                continue;
            }
        }
    }

    if total > 0 && downloaded < total {
        return Err(ModelError::Network(format!(
            "short read for {}: {downloaded}/{total} bytes",
            spec.id
        )));
    }
    // The byte counter and the file must agree before we report
    // success: never mark a download done with a short file.
    // `sync_all` first: the durability check below must observe
    // every byte `write_all` reported, even on filesystems where
    // close() completion and stat visibility can otherwise race
    // under heavy parallel load.
    file.sync_all()
        .await
        .map_err(|e| ModelError::Io(e.to_string()))?;
    drop(file);
    let file_len = tokio::fs::metadata(part)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    if file_len != downloaded {
        return Err(ModelError::Network(format!(
            "file size mismatch for {}: counted {downloaded} bytes but file is {file_len} bytes",
            spec.id
        )));
    }
    // Transfer complete: move the partial file into place. A previous
    // file at `dest` (e.g. an older download being replaced) is removed
    // first — rename does not overwrite on Windows.
    if tokio::fs::metadata(dest).await.is_ok() {
        tokio::fs::remove_file(dest)
            .await
            .map_err(|e| ModelError::Io(e.to_string()))?;
    }
    tokio::fs::rename(part, dest)
        .await
        .map_err(|e| ModelError::Io(e.to_string()))?;
    Ok(())
}

async fn download_inner(
    spec: &ModelSpec,
    dest: &Path,
    shared: &DownloadShared,
    callback: &ProgressCallback,
) -> Result<(), ModelError> {
    // Parent dir is created fail-fast in `download()`; the worker assumes it.
    let part = partial_path(dest);
    let client = reqwest::Client::new();
    let mut offset: u64 = match tokio::fs::metadata(&part).await {
        Ok(meta) => meta.len(),
        Err(_) => 0,
    };

    // Bounded attempts: a ranged try, plus fresh restarts after a
    // mismatched 206 range or a stale-partial 416.
    for _ in 0..3 {
        match shared.current_state() {
            WorkerState::Cancelled => {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(ModelError::Cancelled(spec.id.clone()));
            }
            WorkerState::Paused => {
                wait_while_paused(shared).await;
                if shared.current_state() == WorkerState::Cancelled {
                    let _ = tokio::fs::remove_file(&part).await;
                    return Err(ModelError::Cancelled(spec.id.clone()));
                }
            }
            _ => {}
        }

        let ranged = offset > 0;
        let mut request = client.get(&spec.url);
        if ranged {
            request = request.header("Range", format!("bytes={offset}-"));
        }
        let mut response = request
            .send()
            .await
            .map_err(|e| ModelError::Network(e.to_string()))?;
        let status = response.status();

        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            let (start, total) = response
                .headers()
                .get("Content-Range")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range)
                .ok_or_else(|| {
                    ModelError::Network("206 response without a parseable Content-Range".into())
                })?;
            if start != offset {
                // The server answered a different range than requested:
                // appending would corrupt the file. Discard the response
                // and the partial, and restart with a plain GET.
                drop(response);
                let _ = tokio::fs::remove_file(&part).await;
                offset = 0;
                continue;
            }
            return transfer_body(
                &mut response,
                &part,
                dest,
                true,
                offset,
                total,
                spec,
                shared,
                callback,
            )
            .await;
        } else if status == reqwest::StatusCode::OK {
            // A 200 to a ranged request means the server ignored the
            // Range header: this body is the whole file from byte zero,
            // so the partial is truncated and rewritten from scratch.
            let total = response
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(spec.size_bytes);
            return transfer_body(
                &mut response,
                &part,
                dest,
                false,
                0,
                total,
                spec,
                shared,
                callback,
            )
            .await;
        } else if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            // The partial is at or past the end of the resource. It is
            // the complete file only when its size matches the total
            // the server reports in `Content-Range: bytes */TOTAL`.
            let total = response
                .headers()
                .get("Content-Range")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range_total);
            if offset > 0 && total == Some(offset) {
                report_progress(shared, callback, offset, offset);
                if tokio::fs::metadata(dest).await.is_ok() {
                    tokio::fs::remove_file(dest)
                        .await
                        .map_err(|e| ModelError::Io(e.to_string()))?;
                }
                tokio::fs::rename(&part, dest)
                    .await
                    .map_err(|e| ModelError::Io(e.to_string()))?;
                return Ok(());
            }
            // Stale or oversized partial: delete it and retry fresh.
            let _ = tokio::fs::remove_file(&part).await;
            offset = 0;
            continue;
        } else {
            return Err(ModelError::Network(format!(
                "download of {} failed: HTTP {status}",
                spec.id
            )));
        }
    }

    Err(ModelError::Network(format!(
        "download of {} exhausted retries",
        spec.id
    )))
}

/// Parse the total length out of `Content-Range: bytes 123-456/789`.
fn parse_content_range_total(header: &str) -> Option<u64> {
    header.split('/').nth(1)?.trim().parse().ok()
}

// ---------------------------------------------------------------------------
// Model registry
// ---------------------------------------------------------------------------

/// A model file present on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledModel {
    pub spec: ModelSpec,
    pub path: PathBuf,
    /// RFC 3339 timestamp of the last successful checksum verification.
    pub verified_at: Option<String>,
    /// Backend ids this file is assigned to.
    pub backends: Vec<String>,
}

/// In-memory registry of installed models and their status.
///
/// Persistence of the registry across restarts is manager-config's job;
/// this tracks live install/verify/assign state.
#[derive(Debug, Default)]
pub struct ModelRegistry {
    models: HashMap<String, InstalledModel>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a downloaded model file.
    pub fn register(&mut self, spec: ModelSpec, path: PathBuf) -> &InstalledModel {
        let id = spec.id.clone();
        self.models.entry(id.clone()).or_insert(InstalledModel {
            spec,
            path,
            verified_at: None,
            backends: Vec::new(),
        })
    }

    /// Mark a model checksum-verified now. Call after [`verify_sha256`].
    pub fn mark_verified(&mut self, id: &str) -> Result<(), ModelError> {
        let model = self
            .models
            .get_mut(id)
            .ok_or_else(|| ModelError::UnknownModel(id.to_string()))?;
        model.verified_at = Some(chrono::Utc::now().to_rfc3339());
        Ok(())
    }

    /// Assign an installed model file to a backend.
    pub fn assign_backend(&mut self, id: &str, backend_id: &str) -> Result<(), ModelError> {
        let model = self
            .models
            .get_mut(id)
            .ok_or_else(|| ModelError::UnknownModel(id.to_string()))?;
        if !model.backends.iter().any(|b| b == backend_id) {
            model.backends.push(backend_id.to_string());
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&InstalledModel> {
        self.models.get(id)
    }

    pub fn list(&self) -> Vec<&InstalledModel> {
        let mut models: Vec<&InstalledModel> = self.models.values().collect();
        models.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));
        models
    }

    pub fn remove(&mut self, id: &str) -> Option<InstalledModel> {
        self.models.remove(id)
    }
}

// ---------------------------------------------------------------------------
// Guarded delete (spec §14 `model_delete`)
// ---------------------------------------------------------------------------

/// Report of a successful guarded model deletion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteReport {
    pub id: String,
    pub path: PathBuf,
    /// False when the file was already gone (the desired end state).
    pub file_existed: bool,
    pub bytes_freed: u64,
}

/// Path comparison key for the reference guard: separators unified and
/// case folded — the same normalization `manager_config::validate`
/// applies when comparing a backend's `model_file` to a model's
/// `gguf_path` (replicated here; the original lives in manager-config).
fn normalize_model_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/").to_lowercase()
}

/// Guarded model deletion.
///
/// Refuses when any backend references the model — either by pointing its
/// `model_file` at the model's `gguf_path` (compared after path
/// normalization), or by appearing in the model's `assigned_backends`.
/// Otherwise deletes the file at `file_path` (a missing file is not an
/// error: the desired end state is already reached) and returns a
/// [`DeleteReport`].
///
/// Removing the model from the caller's registry / config is the caller's
/// job, after this returns `Ok`.
pub fn delete_model_file(
    model: &manager_config::ModelConfig,
    backends: &[manager_config::BackendConfig],
    file_path: &Path,
) -> Result<DeleteReport, ModelError> {
    let target = normalize_model_path(&model.gguf_path);
    let referencing: Vec<String> = backends
        .iter()
        .filter(|b| {
            normalize_model_path(&b.model_file) == target
                || model.assigned_backends.iter().any(|a| a == &b.id)
        })
        .map(|b| b.id.clone())
        .collect();
    if !referencing.is_empty() {
        return Err(ModelError::InUse {
            id: model.id.clone(),
            backends: referencing,
        });
    }
    let (file_existed, bytes_freed) = match std::fs::metadata(file_path) {
        Ok(meta) => {
            let bytes = meta.len();
            std::fs::remove_file(file_path)?;
            (true, bytes)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (false, 0),
        Err(e) => return Err(ModelError::Io(e.to_string())),
    };
    Ok(DeleteReport {
        id: model.id.clone(),
        path: file_path.to_path_buf(),
        file_existed,
        bytes_freed,
    })
}

/// Model library errors.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ModelError {
    #[error("network error: {0}")]
    Network(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("checksum mismatch for {0}")]
    ChecksumMismatch(String),
    #[error("download cancelled: {0}")]
    Cancelled(String),
    #[error("unknown model: {0}")]
    UnknownModel(String),
    #[error("model {id} is still referenced by backend(s): {}", backends.join(", "))]
    InUse { id: String, backends: Vec<String> },
    #[error("invalid checksum: {0}")]
    InvalidChecksum(String),
    #[error("invalid tensor split: {0}")]
    InvalidSplit(String),
}

impl From<std::io::Error> for ModelError {
    fn from(e: std::io::Error) -> Self {
        ModelError::Io(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    // -- test helpers -------------------------------------------------------

    fn test_spec(id: &str, size_bytes: u64) -> ModelSpec {
        ModelSpec {
            id: id.to_string(),
            name: id.to_string(),
            quant: "Q4_K_M".to_string(),
            size_bytes,
            sha256: PLACEHOLDER_SHA256.to_string(),
            url: "https://example.com/x.gguf".to_string(),
            params_b: 8.0,
            ctx_max: 0,
            enabled_by_default: true,
            expert_bytes: None,
            moe_layers: None,
            kv_bytes_per_token: None,
            default_offload_layers: None,
            offload_restrictions: Vec::new(),
            mtp: false,
        }
    }

    #[test]
    fn model_spec_and_progress_construct() {
        let spec = ModelSpec {
            id: "qwen3-8b".to_string(),
            name: "Qwen3-8B".to_string(),
            quant: "Q4_K_M".to_string(),
            size_bytes: 4_700_000_000,
            sha256: "abc123".to_string(),
            url: "https://example.com/qwen3-8b-q4_k_m.gguf".to_string(),
            params_b: 8.0,
            ctx_max: 131_072,
            enabled_by_default: true,
            expert_bytes: None,
            moe_layers: None,
            kv_bytes_per_token: None,
            default_offload_layers: None,
            offload_restrictions: Vec::new(),
            mtp: false,
        };
        assert_eq!(spec.quant, "Q4_K_M");
        let p = DownloadProgress {
            downloaded_bytes: 2_350_000_000,
            total_bytes: 4_700_000_000,
        };
        assert!((p.fraction() - 0.5).abs() < f64::EPSILON);
        let verdict = FitVerdict::Fits { gpu: 0 };
        assert!(matches!(verdict, FitVerdict::Fits { gpu: 0 }));
    }

    // -- hermetic HTTP server -----------------------------------------------

    fn test_payload() -> &'static [u8] {
        static PAYLOAD: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        PAYLOAD.get_or_init(|| (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect())
    }

    struct TestServer {
        addr: std::net::SocketAddr,
        served_range: Arc<AtomicBool>,
        served_bad_range: Arc<AtomicBool>,
    }

    async fn start_server() -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served_range = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&served_range);
        let served_bad_range = Arc::new(AtomicBool::new(false));
        let bad_flag = Arc::clone(&served_bad_range);
        tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let flag = Arc::clone(&flag);
                let bad_flag = Arc::clone(&bad_flag);
                tokio::spawn(async move { handle_test_conn(socket, flag, bad_flag).await });
            }
        });
        TestServer {
            addr,
            served_range,
            served_bad_range,
        }
    }

    /// Minimal HTTP/1.1 server. `/file` honors `Range` (206); `/no-range`
    /// ignores it (200, no Accept-Ranges); `/bad-range` answers a Range
    /// request with a 206 whose Content-Range lies about the start (always
    /// claims 0). Body is drip-fed in 64 KiB chunks so pause/cancel can
    /// interleave deterministically.
    async fn handle_test_conn(
        mut socket: TcpStream,
        served_range: Arc<AtomicBool>,
        served_bad_range: Arc<AtomicBool>,
    ) {
        let mut req = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match socket.read(&mut buf).await {
                Ok(0) => return,
                Ok(n) => {
                    req.extend_from_slice(&buf[..n]);
                    if req.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    if req.len() > 65536 {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
        let req_str = String::from_utf8_lossy(&req);
        let mut lines = req_str.lines();
        let request_line = lines.next().unwrap_or("");
        let path = request_line.split_whitespace().nth(1).unwrap_or("/");
        let mut range_start: Option<u64> = None;
        for line in lines {
            if line.is_empty() {
                break;
            }
            // Header names are case-insensitive (hyper normalizes to
            // lowercase on the wire), so match against the lowercased line.
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("range:") {
                let value = value.trim();
                if let Some(rest) = value.strip_prefix("bytes=") {
                    if let Some((start, _)) = rest.split_once('-') {
                        range_start = start.parse().ok();
                    }
                }
            }
        }

        let payload = test_payload();
        let payload_len = payload.len() as u64;
        let supports_range = path == "/file" || path == "/bad-range";

        if supports_range {
            if let Some(start) = range_start {
                if start >= payload_len {
                    let headers = format!(
                        "HTTP/1.1 416 Range Not Satisfiable\r\n\
                         Content-Range: bytes */{payload_len}\r\n\
                         Connection: close\r\nContent-Length: 0\r\n\r\n"
                    );
                    let _ = socket.write_all(headers.as_bytes()).await;
                    return;
                }
                if path == "/bad-range" {
                    // Lie about the range: claim the body starts at 0
                    // whatever was requested, and send the whole payload.
                    served_bad_range.store(true, Ordering::SeqCst);
                    let headers = format!(
                        "HTTP/1.1 206 Partial Content\r\n\
                         Content-Length: {payload_len}\r\n\
                         Accept-Ranges: bytes\r\n\
                         Content-Range: bytes 0-{}/{payload_len}\r\n\
                         Connection: close\r\n\r\n",
                        payload_len - 1
                    );
                    if socket.write_all(headers.as_bytes()).await.is_err() {
                        return;
                    }
                    drip_body(&mut socket, payload, 0).await;
                    return;
                }
                served_range.store(true, Ordering::SeqCst);
                let body_len = payload_len - start;
                let headers = format!(
                    "HTTP/1.1 206 Partial Content\r\n\
                     Content-Length: {body_len}\r\n\
                     Accept-Ranges: bytes\r\n\
                     Content-Range: bytes {start}-{}/{payload_len}\r\n\
                     Connection: close\r\n\r\n",
                    payload_len - 1
                );
                if socket.write_all(headers.as_bytes()).await.is_err() {
                    return;
                }
                drip_body(&mut socket, payload, start as usize).await;
                return;
            }
        }

        // Full 200 response (range ignored on /no-range).
        let headers = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Length: {payload_len}\r\n\
             Connection: close\r\n"
        );
        let headers = if supports_range {
            format!("{headers}Accept-Ranges: bytes\r\n\r\n")
        } else {
            format!("{headers}\r\n")
        };
        if socket.write_all(headers.as_bytes()).await.is_err() {
            return;
        }
        drip_body(&mut socket, payload, 0).await;
    }

    async fn drip_body(socket: &mut TcpStream, payload: &[u8], from: usize) {
        let mut off = from;
        while off < payload.len() {
            let end = (off + 65536).min(payload.len());
            if socket.write_all(&payload[off..end]).await.is_err() {
                return;
            }
            off = end;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "manager-models-test-{}-{}-{name}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::SeqCst),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn download_spec(server: &TestServer, path: &str) -> ModelSpec {
        ModelSpec {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            quant: "Q4_K_M".to_string(),
            size_bytes: test_payload().len() as u64,
            sha256: PLACEHOLDER_SHA256.to_string(),
            url: format!("http://{}{path}", server.addr),
            params_b: 1.0,
            ctx_max: 0,
            enabled_by_default: true,
            expert_bytes: None,
            moe_layers: None,
            kv_bytes_per_token: None,
            default_offload_layers: None,
            offload_restrictions: Vec::new(),
            mtp: false,
        }
    }

    async fn wait_for_partial(handle: &DownloadHandle) {
        for _ in 0..200 {
            let p = handle.progress();
            if p.downloaded_bytes > 0 && p.total_bytes > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("download never made progress");
    }

    // -- download tests ------------------------------------------------------

    #[tokio::test]
    async fn download_completes_and_reports_progress() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-complete");
        let dest = dir.join("model.gguf");
        let spec = download_spec(&server, "/file");

        let fractions = Arc::new(Mutex::new(Vec::new()));
        let fractions2 = Arc::clone(&fractions);
        let handle = download(&spec, &dest, move |p| {
            fractions2.lock().unwrap().push(p.fraction());
        })
        .await
        .unwrap();
        handle.wait().await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), test_payload());
        assert!(
            !partial_path(&dest).exists(),
            "partial file must be moved into place on success"
        );
        let fractions = fractions.lock().unwrap();
        assert!(!fractions.is_empty());
        assert!((fractions.last().unwrap() - 1.0).abs() < 1e-9);
        assert!(fractions.windows(2).all(|w| w[1] >= w[0]));
        assert_eq!(handle.progress().total_bytes, test_payload().len() as u64);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_pause_resume() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-pause");
        let dest = dir.join("model.gguf");
        let spec = download_spec(&server, "/file");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        wait_for_partial(&handle).await;

        handle.pause().await.unwrap();
        let snap = handle.progress().downloaded_bytes;
        assert!(snap > 0);
        // While paused the bytes live only in the partial file; the
        // destination must not exist yet.
        assert!(handle.is_paused());
        assert!(partial_path(&dest).exists());
        assert!(!dest.exists(), "dest must not hold a partial download");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            handle.progress().downloaded_bytes,
            snap,
            "no bytes may land while paused"
        );

        handle.resume().await.unwrap();
        handle.wait().await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), test_payload());
        assert!(!partial_path(&dest).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_cancel_deletes_partial() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-cancel");
        let dest = dir.join("model.gguf");
        let spec = download_spec(&server, "/file");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        wait_for_partial(&handle).await;

        handle.cancel().await.unwrap();
        let result = handle.wait().await;
        assert!(
            matches!(result, Err(ModelError::Cancelled(_))),
            "expected Cancelled, got {result:?}"
        );
        assert!(!dest.exists(), "dest must never hold a partial file");
        assert!(
            !partial_path(&dest).exists(),
            "partial file must be removed on cancel"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_cancel_while_paused_deletes_partial() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-cancel-paused");
        let dest = dir.join("model.gguf");
        let spec = download_spec(&server, "/file");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        wait_for_partial(&handle).await;

        handle.pause().await.unwrap();
        assert!(partial_path(&dest).exists());
        handle.cancel().await.unwrap();
        let result = handle.wait().await;
        assert!(
            matches!(result, Err(ModelError::Cancelled(_))),
            "expected Cancelled, got {result:?}"
        );
        assert!(!dest.exists(), "dest must never hold a partial file");
        assert!(
            !partial_path(&dest).exists(),
            "partial file must be removed when a paused download is cancelled"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_resumes_partial_file_with_range() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-resume");
        let dest = dir.join("model.gguf");
        // Pre-write the first MiB of the partial file, as if from an
        // earlier interrupted download.
        std::fs::write(partial_path(&dest), &test_payload()[..1024 * 1024]).unwrap();
        let spec = download_spec(&server, "/file");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        handle.wait().await.unwrap();

        assert!(
            server.served_range.load(Ordering::SeqCst),
            "server should have seen a Range request"
        );
        assert!(!partial_path(&dest).exists());
        let got = std::fs::read(&dest).unwrap();
        let want = test_payload();
        if got != want {
            let div = got
                .iter()
                .zip(want.iter())
                .position(|(a, b)| a != b)
                .unwrap_or_else(|| got.len().min(want.len()));
            panic!(
                "downloaded file differs: got {} bytes, want {} bytes, first divergence at byte {}",
                got.len(),
                want.len(),
                div
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_finalizes_complete_partial_on_416() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-complete-part");
        let dest = dir.join("model.gguf");
        // The partial already holds the whole file (e.g. a previous
        // transfer finished writing but the app died before the rename).
        std::fs::write(partial_path(&dest), test_payload()).unwrap();
        let spec = download_spec(&server, "/file");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        handle.wait().await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), test_payload());
        assert!(!partial_path(&dest).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_restarts_when_server_ignores_range() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-norange");
        let dest = dir.join("model.gguf");
        std::fs::write(partial_path(&dest), &test_payload()[..1024]).unwrap();
        let spec = download_spec(&server, "/no-range");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        handle.wait().await.unwrap();

        assert!(
            !server.served_range.load(Ordering::SeqCst),
            "server must not see a Range request on /no-range"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), test_payload());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_restarts_on_mismatched_206_range() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-badrange");
        let dest = dir.join("model.gguf");
        // Partial from an earlier interrupted download: the client asks
        // for `Range: bytes=1048576-`, the server answers 206 claiming
        // the range starts at 0. Appending that body would corrupt the
        // file; the client must discard the partial and restart with a
        // plain GET.
        std::fs::write(partial_path(&dest), &test_payload()[..1024 * 1024]).unwrap();
        let spec = download_spec(&server, "/bad-range");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        handle.wait().await.unwrap();

        assert!(
            server.served_bad_range.load(Ordering::SeqCst),
            "server should have served the mismatched 206"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), test_payload());
        assert!(!partial_path(&dest).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_discards_stale_partial_on_416_total_mismatch() {
        let server = start_server().await;
        let dir = test_temp_dir("dl-stale416");
        let dest = dir.join("model.gguf");
        // Stale partial larger than the resource itself (e.g. the model
        // file changed upstream). The Range request starts past the end;
        // the server's 416 reports the real total, which does not match
        // the partial size, so the partial must be deleted — not renamed
        // into place — and the download retried fresh.
        let mut stale = test_payload().to_vec();
        stale.extend_from_slice(&[0xEE; 1024 * 1024]);
        std::fs::write(partial_path(&dest), &stale).unwrap();
        let spec = download_spec(&server, "/file");

        let handle = download(&spec, &dest, |_| {}).await.unwrap();
        handle.wait().await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), test_payload());
        assert!(!partial_path(&dest).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_rejects_bad_url() {
        let dir = test_temp_dir("dl-badurl");
        let mut spec = test_spec("bad", 10);
        spec.url = "://not-a-url".to_string();
        let result = download(&spec, &dir.join("x.gguf"), |_| {}).await;
        assert!(matches!(result, Err(ModelError::Network(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    // -- checksum tests ------------------------------------------------------

    #[tokio::test]
    async fn verify_sha256_pass_fail_and_malformed() {
        let dir = test_temp_dir("sha256");
        let file = dir.join("x.bin");
        std::fs::write(&file, b"hello manager-models").unwrap();

        let mut hasher = Sha256::new();
        hasher.update(b"hello manager-models");
        let hex: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        assert!(verify_sha256(&file, &hex).await.unwrap());
        assert!(verify_sha256(&file, &hex.to_uppercase()).await.unwrap());
        assert!(!verify_sha256(&file, &"0".repeat(64)).await.unwrap());
        assert!(matches!(
            verify_sha256(&file, "zzz").await,
            Err(ModelError::InvalidChecksum(_))
        ));
        assert!(matches!(
            verify_sha256(&dir.join("missing.bin"), &hex).await,
            Err(ModelError::Io(_))
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sha256_hex_matches_incremental_reference_across_chunks() {
        // 3 full 1 MiB buffers + a 17-byte tail, patterned so a
        // chunk-boundary or length bug changes the digest. The
        // reference digest is computed incrementally over odd-sized
        // slices of the same pattern.
        let dir = test_temp_dir("sha256hex");
        let file = dir.join("pattern.bin");
        let total = 3 * (1usize << 20) + 17;
        let pattern: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        std::fs::write(&file, &pattern).unwrap();
        let mut reference = Sha256::new();
        for chunk in pattern.chunks(65_537) {
            reference.update(chunk);
        }
        let expected: String = reference
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(sha256_hex(&file).unwrap(), expected);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The direct regression guard for the v0.1.31 crash class: the
    /// digest of a multi-GiB file must be computable with bounded
    /// memory. Writes ~5 GiB, so it is ignored by default; run it
    /// explicitly in release gates (`cargo test -p manager-models
    /// -- --ignored sha256_hex_streams`).
    #[test]
    #[ignore = "writes ~5 GiB; run explicitly in release gates"]
    fn sha256_hex_streams_multi_gib_file() {
        use std::io::Write;
        let dir = test_temp_dir("sha256big");
        let file = dir.join("big.bin");
        // One 1 MiB pattern block, repeated 5,120 times (~5 GiB).
        let block: Vec<u8> = (0..(1usize << 20)).map(|i| (i % 251) as u8).collect();
        let mut reference = Sha256::new();
        {
            let mut w = std::io::BufWriter::new(std::fs::File::create(&file).unwrap());
            for _ in 0..5_120 {
                w.write_all(&block).unwrap();
                reference.update(&block);
            }
            w.flush().unwrap();
        }
        let expected: String = reference
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(sha256_hex(&file).unwrap(), expected);
        std::fs::remove_dir_all(&dir).ok();
    }

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
        assert_eq!(
            fit_state(17_224, 16_376, true),
            (FitState::WddmPaged, 848)
        );
        // Over total on TCC — and on an unreported driver model,
        // which the caller passes as not-WDDM: does not fit.
        assert_eq!(
            fit_state(17_224, 16_376, false),
            (FitState::DoesNotFit, 0)
        );
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

    // -- catalog + registry tests ---------------------------------------------

    #[test]
    fn curated_models_are_six_with_pinned_hashes() {
        let models = curated_models();
        assert_eq!(models.len(), 6);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "qwen3-27b-planner",
                "qwen3.8-27b-mtp",
                "qwen3-coder-30b",
                "qwen2.5-coder-7b",
                "qwen3-8b",
                "nemotron-3.5-lightning-30b"
            ]
        );
        // The default fleet seeds enabled; the opt-in candidates do not.
        for m in &models {
            assert_eq!(
                m.enabled_by_default,
                m.id != "nemotron-3.5-lightning-30b" && m.id != "qwen3.8-27b-mtp",
                "{}",
                m.id
            );
        }
        // The revision-3 opt-in: the planner model's -mtp build,
        // pinned from the publisher's Hugging Face LFS metadata
        // (2026-10-17). Same weights plus the MTP block, so it is
        // larger than the base planner file and carries a different
        // hash — never the base file's pin.
        let mtp = &models[1];
        assert_eq!(mtp.params_b, 27.0);
        assert_eq!(mtp.size_bytes, 12_120_016_960);
        assert_eq!(mtp.ctx_max, 262144);
        assert_eq!(mtp.quant, "IQ3_S");
        assert_eq!(
            mtp.sha256,
            "58fd826723939933dc86f45b7fe04545cbc2de1c70f6fe2cdd3858c87a98c12f"
        );
        assert_ne!(mtp.sha256, models[0].sha256);
        assert!(mtp.url.ends_with("Qwen3.8-27B-GSQ-RCO-IQ3_S-mtp.gguf"));
        let nemotron = &models[5];
        assert_eq!(nemotron.params_b, 31.6);
        assert_eq!(nemotron.size_bytes, 25_266_255_936);
        assert_eq!(nemotron.expert_bytes, Some(22_829_334_528));
        // 24 MoE blocks; the catalog default offload is 16 of them
        // (the green limit the offload slider renders).
        assert_eq!(nemotron.moe_layers, Some(24));
        assert_eq!(nemotron.default_offload_layers, Some(16));
        // The revision-2 replacements: real Qwen3.8-27B planner,
        // Qwen3-Coder-Next coder (pins from the publishers' file
        // metadata, 2026-10-16).
        let planner = &models[0];
        assert_eq!(planner.params_b, 27.0);
        assert_eq!(planner.size_bytes, 11_771_546_784);
        assert_eq!(planner.ctx_max, 262144);
        let coder = &models[2];
        assert_eq!(coder.params_b, 80.0);
        assert_eq!(coder.size_bytes, 49_301_055_488);
        assert_eq!(coder.ctx_max, 262144);
        assert_eq!(coder.expert_bytes, Some(46_707_769_344));
        // 48 MoE layers; the default offload is all of them (the
        // shipped design point), so its green limit is the range.
        assert_eq!(coder.moe_layers, Some(48));
        assert_eq!(coder.default_offload_layers, Some(48));
        // The dense models carry no offload data.
        for m in [&models[0], &models[1], &models[3], &models[4]] {
            assert_eq!(m.expert_bytes, Some(0), "{}", m.id);
            assert_eq!(m.moe_layers, None, "{}", m.id);
            assert_eq!(m.default_offload_layers, None, "{}", m.id);
        }
        for m in &models {
            // Real pins from the publishers' Hugging Face file metadata:
            // 64 hex chars, never the placeholder.
            assert_eq!(m.sha256.len(), 64, "hash length for {}", m.id);
            assert!(
                m.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                "hash for {} is not hex",
                m.id
            );
            assert_ne!(m.sha256, PLACEHOLDER_SHA256, "hash for {}", m.id);
            assert!(!m.url.is_empty() && !m.quant.is_empty());
            assert!(m.size_bytes > 0);
        }
    }

    #[test]
    fn effective_mtp_resolves_catalog_pin_then_declaration() {
        let models = curated_models();
        let mtp_url = models[1].url.as_str();
        let coder_url = models[2].url.as_str();
        // The catalog pins exactly one model capable.
        assert_eq!(catalog_mtp_pin("qwen3.8-27b-mtp", ""), Some(true));
        assert!(catalog_mtp("qwen3.8-27b-mtp", ""));
        // A catalog model's pin is a fact even when it is false:
        // the coder pins NOT capable, and a record declaration
        // stored on it is shadowed by the pin, never an override.
        assert_eq!(catalog_mtp_pin("qwen3-coder-30b", coder_url), Some(false));
        assert!(!effective_mtp("qwen3-coder-30b", coder_url, Some(true)));
        assert!(!effective_mtp("qwen3-coder-30b", coder_url, None));
        // The pinned build registered under an operator-chosen id
        // still resolves by source URL.
        assert_eq!(catalog_mtp_pin("my-planner", mtp_url), Some(true));
        assert!(effective_mtp("my-planner", mtp_url, Some(false)));
        // A model the catalog does not know falls to the record
        // declaration; undeclared means not capable.
        assert_eq!(catalog_mtp_pin("user-model", "https://example.com/m.gguf"), None);
        assert!(effective_mtp("user-model", "https://example.com/m.gguf", Some(true)));
        assert!(!effective_mtp("user-model", "https://example.com/m.gguf", Some(false)));
        assert!(!effective_mtp("user-model", "https://example.com/m.gguf", None));
    }

    #[test]
    fn catalog_params_b_resolves_by_id_then_url() {
        // Exact id match.
        assert_eq!(
            catalog_params_b("qwen3-8b", "https://example.com/other.gguf"),
            Some(8.0)
        );
        // Exact source-URL match under an operator-chosen id: the
        // same catalog file, so the catalog count applies.
        assert_eq!(
            catalog_params_b(
                "my-qwen3-8b",
                "https://huggingface.co/Qwen/Qwen3-8B-GGUF/resolve/main/Qwen3-8B-Q4_K_M.gguf"
            ),
            Some(8.0)
        );
        // An id match wins over a URL that names a different model.
        assert_eq!(
            catalog_params_b(
                "qwen3-8b",
                "https://huggingface.co/Qwen/Qwen2.5-Coder-7B-Instruct-GGUF/resolve/main/qwen2.5-coder-7b-instruct-q4_k_m.gguf"
            ),
            Some(8.0)
        );
        // Neither known: no count to offer.
        assert_eq!(
            catalog_params_b("obscure-model", "https://example.com/obscure.gguf"),
            None
        );
    }

    #[test]
    fn catalog_kv_bytes_per_token_pins_match_the_gguf_geometry() {
        // Every curated entry pins its KV figure, computed from
        // the publisher file's real GGUF header geometry (headers
        // pinned 2026-10-21) with the parser's own derivation:
        // trunk KV layers (full_attention_interval folding, nextn
        // blocks excluded) * head_count_kv * (key + value lengths)
        // * 2 B (F16). A catalog model therefore resolves a KV
        // figure before download and under any parse failure.
        let models = curated_models();
        let get = |id: &str| models.iter().find(|m| m.id == id).unwrap();
        // qwen35: 64 blocks, interval 4 -> 16 layers, 4 KV heads,
        // 256/256.
        assert_eq!(get("qwen3-27b-planner").kv_bytes_per_token, Some(65_536));
        // The MTP build's 65th block is the nextn block: same
        // trunk, same figure — NOT the 69,632 B/token that
        // counting it as a KV layer would give.
        assert_eq!(get("qwen3.8-27b-mtp").kv_bytes_per_token, Some(65_536));
        // qwen3next: 48 blocks, interval 4 -> 12 layers, 2 KV
        // heads, 256/256.
        assert_eq!(get("qwen3-coder-30b").kv_bytes_per_token, Some(24_576));
        // qwen2: 28 dense layers, 4 KV heads, head dimension
        // 3584 / 28 = 128.
        assert_eq!(get("qwen2.5-coder-7b").kv_bytes_per_token, Some(57_344));
        // qwen3: 36 dense layers, 8 KV heads, 128/128.
        assert_eq!(get("qwen3-8b").kv_bytes_per_token, Some(147_456));
        // nemotron_h_moe: 7 attention blocks per the tensor
        // directory, 2 KV heads (attn_k 256 wide at length 128),
        // 128/128.
        assert_eq!(
            get("nemotron-3.5-lightning-30b").kv_bytes_per_token,
            Some(7_168)
        );
        for m in &models {
            assert!(m.kv_bytes_per_token.is_some(), "{}", m.id);
        }
        // The planner figures at the fleet's working context:
        // 65,536 B/token * 32,768 tokens = 2,048 MiB.
        assert_eq!(65_536u64 * 32_768, 2_048 * MIB);
    }

    #[test]
    fn model_spec_without_kv_pin_deserializes_as_unpinned() {
        // Specs persisted before the KV pin existed carry no
        // kv_bytes_per_token key; the serde default keeps them
        // readable with the figure unpinned.
        let spec = test_spec("legacy", 1_000);
        let mut v = serde_json::to_value(&spec).unwrap();
        v.as_object_mut().unwrap().remove("kv_bytes_per_token");
        let back: ModelSpec = serde_json::from_value(v).unwrap();
        assert_eq!(back.kv_bytes_per_token, None);
    }

    #[test]
    fn catalog_default_offloads_match_the_design_points() {
        let models = curated_models();
        let get = |id: &str| models.iter().find(|m| m.id == id).unwrap();
        // Coder-Next: all experts — its default backend's shipped
        // `-ot exps=CPU` form.
        let coder = get("qwen3-coder-30b");
        assert_eq!(catalog_default_offload(coder), ExpertOffload::AllExperts);
        assert_eq!(
            offload_flag_string(catalog_default_offload(coder)).as_deref(),
            Some("-ot exps=CPU")
        );
        // Nemotron: 16 of 24 MoE layers.
        let nemotron = get("nemotron-3.5-lightning-30b");
        assert_eq!(
            catalog_default_offload(nemotron),
            ExpertOffload::CpuMoeLayers(16)
        );
        assert_eq!(
            offload_flag_string(catalog_default_offload(nemotron)).as_deref(),
            Some("--n-cpu-moe 16")
        );
        // Dense models default to no offload.
        assert_eq!(
            catalog_default_offload(get("qwen3-27b-planner")),
            ExpertOffload::None
        );
        // The default sits exactly at the slider's green limit for
        // both MoE models: green_end == default / layers.
        for spec in [coder, nemotron] {
            let layers = spec.moe_layers.unwrap();
            let (green, _) = offload_bands(layers, spec.default_offload_layers.unwrap());
            let pos = catalog_default_offload(spec).layers(layers);
            assert_eq!(pos, spec.default_offload_layers.unwrap());
            assert!((green - pos as f64 / layers as f64).abs() < 1e-9);
        }
    }

    #[test]
    fn catalog_offload_restrictions_pin_coder_next_pascal_only() {
        // Tracker #37: exactly one curated restriction — the
        // coder's (6, 0) EndpointsOnly pin from the Pascal CUDA
        // invalid-argument incident at --n-cpu-moe 46. Every other
        // curated entry carries none; in particular Nemotron's
        // 16-of-24 partial design point stays unrestricted.
        let models = curated_models();
        let coder = models.iter().find(|m| m.id == "qwen3-coder-30b").unwrap();
        assert_eq!(
            coder.offload_restrictions,
            vec![OffloadRestriction {
                compute_capability: (6, 0),
                allowed: AllowedOffload::EndpointsOnly,
            }]
        );
        assert_eq!(
            restriction_for(coder, (6, 0)),
            Some(&coder.offload_restrictions[0])
        );
        for m in &models {
            if m.id != "qwen3-coder-30b" {
                assert!(m.offload_restrictions.is_empty(), "{}", m.id);
            }
        }
    }

    #[test]
    fn registry_lifecycle() {
        let mut registry = ModelRegistry::new();
        let spec = test_spec("qwen3-8b", 5_280_000_000);
        registry.register(spec.clone(), PathBuf::from("/models/qwen3-8b.gguf"));
        assert!(registry.get(&spec.id).unwrap().verified_at.is_none());

        registry.mark_verified(&spec.id).unwrap();
        assert!(registry.get(&spec.id).unwrap().verified_at.is_some());

        registry.assign_backend(&spec.id, "planner").unwrap();
        registry.assign_backend(&spec.id, "planner").unwrap(); // idempotent
        assert_eq!(
            registry.get(&spec.id).unwrap().backends,
            vec!["planner".to_string()]
        );

        assert!(matches!(
            registry.mark_verified("nope"),
            Err(ModelError::UnknownModel(_))
        ));
        assert_eq!(registry.list().len(), 1);
        assert!(registry.remove(&spec.id).is_some());
        assert!(registry.get(&spec.id).is_none());
    }

    // -- delete_model_file --------------------------------------------------

    fn test_model_config(id: &str, path: PathBuf) -> manager_config::ModelConfig {
        manager_config::ModelConfig {
            id: id.to_string(),
            gguf_path: path,
            quant: "Q4_K_M".to_string(),
            params_b: 8.0,
            sha256: "00".repeat(32),
            verified_at: None,
            verified_sha256: None,
            source_url: String::new(),
            assigned_backends: Vec::new(),
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
            mtp: None,
        }
    }

    fn test_backend(id: &str, model_file: PathBuf) -> manager_config::BackendConfig {
        manager_config::BackendConfig {
            id: id.to_string(),
            enabled: true,
            model_file,
            port: 8081,
            flags: manager_config::ServerFlags {
                n_ctx: 32768,
                n_batch: 512,
                tensor_split: Vec::new(),
                split_mode: manager_config::SplitMode::None,
            },
            raw_flags: None,
            flags_advanced: false,
            restart_policy: manager_config::RestartPolicy {
                max_retries: 5,
                backoff_base_secs: 2,
            },
            overrides_global: false,
            tool_call_probe: None,
            purpose: String::new(),
        }
    }

    fn temp_gguf(id: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("llm-manager-test-{id}.gguf"));
        std::fs::write(&path, bytes).expect("write temp gguf");
        path
    }

    #[test]
    fn delete_model_file_removes_file_and_reports_bytes() {
        let path = temp_gguf("delete-ok", b"gguf-bytes");
        let model = test_model_config("m1", path.clone());
        let backends = vec![test_backend("planner", PathBuf::from("/other/model.gguf"))];

        let report = delete_model_file(&model, &backends, &path).expect("delete");
        assert_eq!(report.id, "m1");
        assert_eq!(report.path, path);
        assert!(report.file_existed);
        assert_eq!(report.bytes_freed, 10);
        assert!(!path.exists(), "file must be gone");
    }

    #[test]
    fn delete_model_file_missing_file_is_not_an_error() {
        let path = std::env::temp_dir().join("llm-manager-test-delete-missing.gguf");
        let _ = std::fs::remove_file(&path);
        let model = test_model_config("m2", path);

        let report = delete_model_file(&model, &[], &model.gguf_path.clone()).expect("delete");
        assert!(!report.file_existed);
        assert_eq!(report.bytes_freed, 0);
    }

    #[test]
    fn delete_model_file_refuses_when_backend_points_at_file() {
        let path = temp_gguf("delete-refused-path", b"gguf-bytes");
        let model = test_model_config("m3", path.clone());
        let backends = vec![test_backend("planner", path.clone())];

        let err = delete_model_file(&model, &backends, &path).expect_err("must refuse");
        assert!(matches!(err, ModelError::InUse { .. }));
        assert!(err.to_string().contains("planner"));
        assert!(path.exists(), "refused delete must leave the file");
        std::fs::remove_file(&path).expect("cleanup");
    }

    #[test]
    fn delete_model_file_refuses_with_normalized_path_match() {
        let path = temp_gguf("delete-refused-normalized", b"gguf-bytes");
        let model = test_model_config("m5", path.clone());
        // Same file, spelled with different case and separators: the
        // guard compares normalized paths, so this still refuses.
        let spelled = path.to_string_lossy().replace('/', "\\").to_uppercase();
        let backends = vec![test_backend("planner", PathBuf::from(spelled))];

        let err = delete_model_file(&model, &backends, &path).expect_err("must refuse");
        assert!(matches!(err, ModelError::InUse { .. }));
        assert!(path.exists(), "refused delete must leave the file");
        std::fs::remove_file(&path).expect("cleanup");
    }

    #[test]
    fn delete_model_file_refuses_when_backend_is_assigned() {
        let path = temp_gguf("delete-refused-assign", b"gguf-bytes");
        let mut model = test_model_config("m4", path.clone());
        model.assigned_backends.push("coder".to_string());
        // Backend points at a *different* file: the assignment alone guards.
        let backends = vec![test_backend("coder", PathBuf::from("/other/model.gguf"))];

        let err = delete_model_file(&model, &backends, &path).expect_err("must refuse");
        assert!(matches!(err, ModelError::InUse { .. }));
        assert!(path.exists(), "refused delete must leave the file");
        std::fs::remove_file(&path).expect("cleanup");
    }
}
