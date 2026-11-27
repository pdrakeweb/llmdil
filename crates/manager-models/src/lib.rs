//! Model library: downloads, checksums, fit-check (spec §4.6).
//!
//! Curated GGUF catalog with SHA-256 verification before a model is marked
//! "installed". Downloads run on a background task and support progress
//! callbacks, pause/resume (HTTP range requests), and cancel (partial file
//! removed). Fit-check compares model size against discovered per-GPU free
//! VRAM and recommends a placement: single GPU, tensor split proportional to
//! free VRAM (fractions sum to 1.0), or too-large with the arithmetic shown.

pub mod cache;
mod catalog;
mod download;
mod fit;
pub mod flags;
pub mod gguf;
pub mod guardrails;
pub mod memory;
pub mod moe_cache;
pub mod mtp;
pub mod parallel;
pub mod placement;
mod registry;
#[cfg(test)]
mod test_fixtures;
pub mod tier;

pub use catalog::*;
pub use download::*;
pub use fit::*;
pub use registry::*;

pub use cache::{
    cache_selection, cache_tokens_present, kv_bytes_per_token, predicted_best, predicted_fits,
    predicted_shares, price_ladder, set_cache_selection, CacheFitInputs, CacheRung, CacheSelection,
    CacheType, FlashAttn, HEADROOM_RESERVE_PERCENT, PREDICTED_BEST_LADDER,
};
pub use flags::{raw_flag_errors, raw_flag_warnings, split_raw_flags, FlagWarning};
pub use gguf::{parse_gguf_layout, GgufError, GgufLayout, KvGeometry, RsGeometry};
pub use guardrails::{
    nearest_allowed, offload_status, pascal_partial_warning, restriction_for, AllowedOffload,
    OffloadRestriction, OffloadStatus,
};
pub use memory::{
    expert_offload, memory_requirement, offload_bands, offload_flag_string, offload_for_layers,
    offload_for_moe_layers, set_expert_offload, sys_bytes_by_position, ExpertOffload, MemoryBasis,
    MemoryRequirement, MoeBlockMap,
};
pub use moe_cache::{
    auto_moe_cache_budget, moe_cache_mib, moe_cache_shares, set_moe_cache,
    MOE_CACHE_AUTO_RESERVE_MIB, MOE_CACHE_DEFAULT_MIB, MOE_CACHE_PRESETS_MIB,
};
pub use mtp::{mtp_draft, set_mtp_draft, MtpDraft, LLAMA_CPP_N_MAX_DEFAULT};
pub use parallel::{effective_slots, parallel_slots, set_parallel_slots};
pub use placement::{
    apply_placement, propose_placement, propose_placement_full, propose_placement_resident,
    validate_split, PlacementProjection,
};

/// Bytes in one MiB.
pub const MIB: u64 = 1024 * 1024;

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
