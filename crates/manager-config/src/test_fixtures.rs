//! Shared helpers for the crate's unit tests.

use std::path::PathBuf;

use crate::{AppConfig, ModelConfig, SplitMode};

/// Config with one registered model and one enabled backend using it.
pub(crate) fn working_config() -> AppConfig {
    let mut cfg = AppConfig::default_config();
    cfg.models.push(ModelConfig {
        id: "qwen3-8b".to_string(),
        gguf_path: PathBuf::from("models/qwen3-8b.gguf"),
        quant: "Q4_K_M".to_string(),
        params_b: 8.0,
        sha256: "ab".repeat(32),
        verified_at: None,
        verified_sha256: None,
        source_url: "https://example.com/qwen3-8b.gguf".to_string(),
        assigned_backends: vec!["tool-runner".to_string()],
        enabled: true,
        display_name: None,
        moe_layers: None,
        default_offload_layers: None,
        mtp: None,
    });
    let backend = cfg
        .backends
        .iter_mut()
        .find(|b| b.id == "tool-runner")
        .unwrap();
    backend.enabled = true;
    backend.model_file = PathBuf::from("models/qwen3-8b.gguf");
    backend.flags.tensor_split = vec![0.6, 0.4];
    backend.flags.split_mode = SplitMode::Layer;
    cfg
}
