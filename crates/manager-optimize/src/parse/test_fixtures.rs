//! Shared helpers for the parse submodules' unit tests.

use std::collections::HashSet;

use chrono::Utc;
use manager_config::AppConfig;
use serde_json::{json, Value};

use super::{parse_plan, ParseError};
use crate::plan::Plan;

pub(super) fn parse(response: &str) -> Result<Plan, ParseError> {
    parse_plan(
        response,
        &AppConfig::default_config(),
        "planner",
        "hash",
        json!({}),
        &HashSet::new(),
        "",
        Utc::now(),
    )
}

/// A config that validates cleanly with exactly one registered
/// model — the same trim validate.rs's tests use, since
/// default_config() alone does not pass whole-config
/// validation (its backends reference unregistered files).
pub(super) fn cfg_one_model() -> AppConfig {
    let mut cfg = AppConfig::default_config();
    cfg.backends.retain(|b| b.id == "tool-runner");
    cfg.gateway.groups = vec![manager_config::ModelGroup {
        name: "tool-runner".to_string(),
        members: vec!["tool-runner".to_string()],
        strategy: manager_config::RoutingStrategy::SimpleShuffle,
        fallbacks: Vec::new(),
        purpose: String::new(),
    }];
    cfg.models.push(manager_config::ModelConfig {
        id: "qwen3-8b".to_string(),
        gguf_path: std::path::PathBuf::from("models/qwen3-8b.gguf"),
        quant: "Q4_K_M".to_string(),
        params_b: 8.0,
        sha256: String::new(),
        verified_at: None,
        verified_sha256: None,
        source_url: String::new(),
        assigned_backends: vec!["tool-runner".to_string()],
        enabled: true,
        display_name: None,
        moe_layers: None,
        default_offload_layers: None,
        mtp: None,
    });
    cfg.backends[0].model_file = std::path::PathBuf::from("models/qwen3-8b.gguf");
    cfg
}

pub(super) fn parse_with(
    response: &str,
    cfg: &AppConfig,
    context: Value,
) -> Result<Plan, ParseError> {
    parse_plan(
        response,
        cfg,
        "planner",
        "hash",
        context,
        &HashSet::new(),
        "",
        Utc::now(),
    )
}
