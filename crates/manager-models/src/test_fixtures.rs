//! Shared test fixtures for the manager-models unit tests.

use crate::*;

pub(crate) fn test_spec(id: &str, size_bytes: u64) -> ModelSpec {
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
