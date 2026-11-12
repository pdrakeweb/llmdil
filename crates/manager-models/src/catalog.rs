//! The curated GGUF model catalog: `ModelSpec`, catalog pins, and
//! catalog lookups.

use crate::*;
use serde::{Deserialize, Serialize};

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
    models.iter().find(|s| s.url == source_url).map(|s| s.mtp)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::*;

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
        assert_eq!(
            catalog_mtp_pin("user-model", "https://example.com/m.gguf"),
            None
        );
        assert!(effective_mtp(
            "user-model",
            "https://example.com/m.gguf",
            Some(true)
        ));
        assert!(!effective_mtp(
            "user-model",
            "https://example.com/m.gguf",
            Some(false)
        ));
        assert!(!effective_mtp(
            "user-model",
            "https://example.com/m.gguf",
            None
        ));
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
}
