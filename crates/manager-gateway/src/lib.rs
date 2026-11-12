//! LiteLLM gateway config generation and routing policy (spec §7).
//!
//! Generates LiteLLM `config.yaml` from [`AppConfig`]. The generated file
//! is never hand-edited: it is regenerated from the app config whenever
//! the gateway is (re)started from the Setup Wizard (the start-gateway
//! step writes a fresh file before spawning LiteLLM). [`diff_config_yaml`]
//! is a library utility for comparing two generated documents; no UI
//! flow currently calls it.
//!
//! Model groups and default strategies (spec §7):
//! `planner` (latency-based) → coder → OpenRouter;
//! `coder-fast` (least-busy) → coder → OpenRouter;
//! `coder` (simple-shuffle) → OpenRouter;
//! `tool-runner` (least-busy, + WinML :8090 if registered) → OpenRouter.
//!
//! Resolved spec ambiguities:
//! - LiteLLM's `router_settings.routing_strategy` is global, but our groups
//!   want per-group strategies. [`generate_config_yaml`] emits the global
//!   setting from the first multi-member group's strategy (falling back to the
//!   planner group's, then the first group's). Per-group strategies are
//!   recorded in the generated YAML for reference only: live traffic is
//!   routed by LiteLLM under that single global strategy. The app's
//!   [`pick_backend`] does not route live traffic — it backs only the
//!   routing-trace explainer behind the gateway view's test-routing action
//!   (`gateway_test_routing` in the Tauri layer).
//! - Rubric-based escalation is decided at runtime by the local planner model;
//!   this crate evaluates the verdict via [`RubricInput`].
//! - Overflow "for N seconds" is tracked by the app; this crate evaluates the
//!   rule via [`OverflowSignal`] against [`OverflowThresholds`].

mod config_yaml;
mod context_overflow;
mod diff;
mod fallback;
mod groups;
mod routing;
#[cfg(test)]
mod test_fixtures;

pub use config_yaml::*;
pub use context_overflow::*;
pub use diff::*;
pub use fallback::*;
pub use groups::*;
pub use routing::*;

/// Gateway errors.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("invalid gateway config: {0}")]
    ConfigInvalid(String),
    #[error("YAML serialization failed: {0}")]
    Serialization(#[from] serde_yaml::Error),
}
