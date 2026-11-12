//! Shared test fixtures for the gateway unit tests.

use crate::*;
use manager_config::{
    AppConfig, BackendConfig, CloudTierConfig, ModelGroup, RoutingStrategy, ServerFlags, SplitMode,
};
use std::path::PathBuf;

pub(crate) fn backend(id: &str, port: u16) -> BackendConfig {
    BackendConfig {
        id: id.to_string(),
        enabled: true,
        model_file: PathBuf::from(format!("{id}.gguf")),
        port,
        flags: ServerFlags {
            n_ctx: 32768,
            n_batch: 512,
            tensor_split: vec![],
            split_mode: SplitMode::None,
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

/// The 4-backend stack from the spec: planner :8081, coder :8082,
/// coder-fast :8083, tool-runner :8084, OpenRouter tier enabled.
/// The MCP registry is pinned to the two legacy Local servers:
/// the Phase 3 default registry also carries a Gateway-exposed
/// curated server, and these fixtures predate it — tests that
/// exercise gateway MCP emission set the registry explicitly.
pub(crate) fn four_backend_config() -> AppConfig {
    let mut cfg = AppConfig::default_config();
    cfg.mcp
        .servers
        .retain(|s| s.id == "github" || s.id == "gdrive");
    cfg.backends = vec![
        backend("planner", 8081),
        backend("coder", 8082),
        backend("coder-fast", 8083),
        backend("tool-runner", 8084),
    ];
    cfg.gateway.openrouter = CloudTierConfig {
        enabled: true,
        cred_ref: Some("openrouter-api-key".to_string()),
        daily_cap_usd: 5.0,
        model_allowlist: vec![
            "qwen/qwen3-235b-a22b-thinking".to_string(),
            "qwen/qwen3-coder-480b-a35b".to_string(),
        ],
    };
    cfg
}

pub(crate) fn group(name: &str, strategy: RoutingStrategy, members: &[&str]) -> ModelGroup {
    ModelGroup {
        name: name.to_string(),
        members: members.iter().map(|s| s.to_string()).collect(),
        strategy,
        fallbacks: vec![],
        purpose: String::new(),
    }
}

pub(crate) fn stats(id: &str, healthy: bool, load: Option<f64>, p50: Option<f64>) -> BackendStats {
    BackendStats {
        id: id.to_string(),
        healthy,
        queue_depth: 0,
        p50_latency_ms: p50,
        p99_latency_ms: p50.map(|p| p * 1.8),
        load,
    }
}
